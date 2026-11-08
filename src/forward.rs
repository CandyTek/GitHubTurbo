use super::*;
use bytes::{Bytes, BytesMut};
use http_body_util::{combinators::UnsyncBoxBody, BodyExt, Full};
use hyper::{
    body::{Body, Incoming},
    service::service_fn,
    Method, Request, Response, StatusCode,
};
use hyper_util::rt::TokioIo;
pub(crate) type HttpBody = UnsyncBoxBody<Bytes, Box<dyn std::error::Error + Send + Sync>>;
struct CacheWrite {
    _permit: tokio::sync::OwnedSemaphorePermit,
    key: String,
    headers: hyper::HeaderMap,
    buffer: BytesMut,
}
struct CountedBody {
    inner: Incoming,
    state: Arc<State>,
    upload: bool,
    lease: Option<pool::Lease>,
    cache: Option<CacheWrite>,
}
impl Drop for CountedBody {
    fn drop(&mut self) {
        if self.inner.is_end_stream() {
            if let Some(write) = self.cache.take() {
                self.state.content_cache.put(
                    write.key,
                    StatusCode::OK,
                    write.headers,
                    write.buffer.freeze(),
                );
            }
        }
    }
}
impl Body for CountedBody {
    type Data = Bytes;
    type Error = Box<dyn std::error::Error + Send + Sync>;
    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<std::result::Result<hyper::body::Frame<Bytes>, Self::Error>>> {
        use std::task::Poll;
        match std::pin::Pin::new(&mut self.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    let counter = if self.upload {
                        &self.state.counters.uploaded
                    } else {
                        &self.state.counters.downloaded
                    };
                    counter.fetch_add(data.len() as u64, Ordering::Relaxed);
                    if let Some(write) = self.cache.as_mut() {
                        if write.buffer.len() + data.len() <= content_cache::Cache::MAX_ENTRY {
                            write.buffer.extend_from_slice(data);
                        } else {
                            self.cache = None;
                        }
                    }
                } else {
                    self.cache = None;
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(error))) => {
                self.cache = None;
                self.lease.take();
                Poll::Ready(Some(Err(error.into())))
            }
            Poll::Ready(None) => {
                if let Some(write) = self.cache.take() {
                    self.state.content_cache.put(
                        write.key,
                        StatusCode::OK,
                        write.headers,
                        write.buffer.freeze(),
                    );
                }
                self.lease.take();
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
    fn size_hint(&self) -> hyper::body::SizeHint {
        self.inner.size_hint()
    }
}
fn empty() -> HttpBody {
    Full::new(Bytes::new())
        .map_err(|never| match never {})
        .boxed_unsync()
}
fn error(code: StatusCode, text: &str) -> Response<HttpBody> {
    let mut reply = Response::new(
        Full::new(Bytes::copy_from_slice(text.as_bytes()))
            .map_err(|never| match never {})
            .boxed_unsync(),
    );
    *reply.status_mut() = code;
    reply
}
fn strip_hop(headers: &mut hyper::HeaderMap, upgrade: bool) {
    if upgrade {
        headers.remove("proxy-authorization");
        return;
    }
    let extra = headers
        .get(hyper::header::CONNECTION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(',')
        .map(|s| s.trim().to_owned())
        .collect::<Vec<_>>();
    for name in extra {
        headers.remove(name);
    }
    for name in [
        "connection",
        "proxy-connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
    ] {
        headers.remove(name);
    }
}
pub async fn serve(
    client: tokio_rustls::server::TlsStream<TcpStream>,
    state: Arc<State>,
    host: String,
) -> Result<()> {
    let service = service_fn(move |request| {
        let state = state.clone();
        let host = host.clone();
        async move {
            Ok::<_, std::convert::Infallible>(match forward(request, &state, &host).await {
                Ok(reply) => reply,
                Err(error_value) => {
                    state.counters.failures.fetch_add(1, Ordering::Relaxed);
                    state.health.event(&host, "request-failed");
                    eprintln!("HTTP {host}: {error_value}");
                    error(StatusCode::BAD_GATEWAY,"No healthy GitHub route. Check diagnostics or configure an optional upstream.\n")
                }
            })
        }
    });
    let mut builder =
        hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new());
    builder
        .http1()
        .max_buf_size(32768)
        .timer(hyper_util::rt::TokioTimer::new())
        .header_read_timeout(Duration::from_secs(15));
    builder
        .http2()
        .max_concurrent_streams(64)
        .initial_stream_window_size(1024 * 1024)
        .initial_connection_window_size(4 * 1024 * 1024)
        .max_send_buf_size(256 * 1024);
    let result = builder
        .serve_connection_with_upgrades(TokioIo::new(client), service)
        .await;
    if let Err(error) = result {
        if !error.to_string().contains("close_notify")
            && !error.to_string().contains("incomplete message")
            && !error.to_string().contains("timeout")
        {
            return Err(error);
        }
    }
    Ok(())
}
async fn forward(
    mut request: Request<Incoming>,
    state: &Arc<State>,
    host: &str,
) -> Result<Response<HttpBody>> {
    let authority = request
        .uri()
        .authority()
        .map(|a| a.as_str())
        .or_else(|| request.headers().get("host").and_then(|v| v.to_str().ok()))
        .unwrap_or("");
    if !authority.eq_ignore_ascii_case(host)
        && !authority.eq_ignore_ascii_case(&format!("{host}:443"))
    {
        return Ok(error(
            StatusCode::MISDIRECTED_REQUEST,
            "Host must match the TLS destination\n",
        ));
    }
    if request
        .headers()
        .get("host")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|value| {
            !value.eq_ignore_ascii_case(host) && !value.eq_ignore_ascii_case(&format!("{host}:443"))
        })
    {
        return Ok(error(StatusCode::MISDIRECTED_REQUEST, "Host mismatch\n"));
    }
    if request
        .uri()
        .scheme()
        .is_some_and(|s| s.as_str() != "https")
    {
        return Ok(error(
            StatusCode::BAD_REQUEST,
            "Expected HTTPS destination\n",
        ));
    }
    let is_upgrade = request.headers().contains_key("upgrade");
    let no_body = request.body().is_end_stream() || request.body().size_hint().exact() == Some(0);
    let replayable =
        matches!(*request.method(), Method::GET | Method::HEAD) && no_body && !is_upgrade;
    let downstream_upgrade = if is_upgrade {
        Some(hyper::upgrade::on(&mut request))
    } else {
        None
    };
    let method = request.method().clone();
    let uri: hyper::Uri = request
        .uri()
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/")
        .parse()?;
    let cache_key = if state.optimization().public_cache {
        content_cache::key(&method, host, &uri, request.headers())
    } else {
        None
    };
    if let Some((headers, body)) = cache_key
        .as_ref()
        .and_then(|key| state.content_cache.get(key))
    {
        let mut response = Response::new(
            Full::new(body)
                .map_err(|never| match never {})
                .boxed_unsync(),
        );
        *response.headers_mut() = headers;
        response
            .headers_mut()
            .insert("x-github-turbo-cache", "hit".parse()?);
        return Ok(response);
    }
    strip_hop(request.headers_mut(), is_upgrade);
    request.headers_mut().insert("host", host.parse()?);
    *request.uri_mut() = uri.clone();
    let headers = request.headers().clone();
    let (parts, body) = request.into_parts();
    let body = if no_body {
        empty()
    } else {
        CountedBody {
            inner: body,
            state: state.clone(),
            upload: true,
            lease: None,
            cache: None,
        }
        .boxed_unsync()
    };
    let mut first = Some(Request::from_parts(parts, body));
    let mut routes = Vec::new();
    let mut last = String::new();
    let mut api_tried = false;
    let budget = if replayable {
        Some(Duration::from_secs(if pool::session_host(host) {
            8
        } else {
            6
        }))
    } else {
        None
    };
    let mut downstream_upgrade = downstream_upgrade;
    for attempt in 0..5 {
        if attempt > 0 && routes.is_empty() {
            break;
        }
        let route = if attempt == 0 {
            None
        } else {
            Some(routes.remove(0))
        };
        let mut outgoing = if let Some(request) = first.take() {
            request
        } else {
            let mut req = Request::new(empty());
            *req.method_mut() = method.clone();
            *req.uri_mut() = uri.clone();
            *req.headers_mut() = headers.clone();
            req
        };
        *outgoing.version_mut() = hyper::Version::HTTP_11;
        let start = Instant::now();
        match state
            .pool
            .send(state, host, outgoing, route.as_ref(), budget)
            .await
        {
            Ok(exchange) => {
                let mut response = exchange.response;
                let lease = exchange.lease;
                let actual = lease.route().clone();
                let status = response.status();
                if status == StatusCode::MISDIRECTED_REQUEST {
                    state.health.fail(host, &actual, health::Failure::Rejected);
                } else if status == StatusCode::TOO_MANY_REQUESTS {
                    state.health.fail(host, &actual, health::Failure::Throttled);
                } else if status.is_server_error() {
                    state.health.fail(host, &actual, health::Failure::Server);
                } else {
                    state
                        .health
                        .success(host, &actual, start.elapsed().as_secs_f64() * 1000.);
                    state.cache_route(host, actual.clone()).await;
                }
                if (status.is_server_error() || status == StatusCode::MISDIRECTED_REQUEST)
                    && replayable
                {
                    if host == "github.com"
                        && !api_tried
                        && method == Method::GET
                        && !content_cache::sensitive(&uri, &headers)
                    {
                        api_tried = true;
                        if let Some(response) = release_api_fallback(state, &uri, &headers).await {
                            return Ok(response);
                        }
                    }
                    if attempt == 0 {
                        routes = state.alternatives(host, Some(&actual)).await;
                    }
                    if attempt < 4 && !routes.is_empty() {
                        state.health.event(host, "read-only-retry");
                        continue;
                    }
                }
                if status == StatusCode::SWITCHING_PROTOCOLS {
                    if let Some(downstream) = downstream_upgrade.take() {
                        let upstream = hyper::upgrade::on(&mut response);
                        let state = state.clone();
                        tokio::spawn(async move {
                            if let (Ok(a), Ok(b)) = tokio::join!(downstream, upstream) {
                                let _lease = lease;
                                let _ = relay(
                                    TokioIo::new(a),
                                    TokioIo::new(b),
                                    Duration::from_secs(state.config.idle_timeout_seconds),
                                    &state.counters,
                                )
                                .await;
                            }
                        });
                        let (parts, body) = response.into_parts();
                        return Ok(Response::from_parts(
                            parts,
                            body.map_err(Into::into).boxed_unsync(),
                        ));
                    }
                }
                strip_hop(response.headers_mut(), false);
                if state.optimization().segmented_downloads
                    && !matches!(actual, Route::Proxy)
                    && method == Method::GET
                    && status == StatusCode::OK
                    && segmented::eligible(host, response.headers(), &headers).is_some()
                {
                    // Keep the original response until the first Range batch proves useful.
                    // Falling back must not restart an already working single stream.
                    let head = response.headers().clone();
                    if let Some(split) = segmented::start(state, host, &uri, &headers, &head).await
                    {
                        return Ok(split);
                    }
                    response
                        .headers_mut()
                        .insert("x-github-turbo-split", "fallback".parse()?);
                }
                let cache = if state.optimization().public_cache
                    && content_cache::Cache::eligible(status, response.headers())
                {
                    cache_key.as_ref().and_then(|key| {
                        state
                            .content_cache
                            .collecting
                            .clone()
                            .try_acquire_owned()
                            .ok()
                            .map(|permit| CacheWrite {
                                _permit: permit,
                                key: key.clone(),
                                headers: response.headers().clone(),
                                buffer: BytesMut::new(),
                            })
                    })
                } else {
                    None
                };
                response
                    .headers_mut()
                    .insert("x-github-turbo-upstream", lease.protocol().parse()?);
                response
                    .headers_mut()
                    .insert("x-github-turbo-cache", "miss".parse()?);
                return Ok(response.map(|body| {
                    CountedBody {
                        inner: body,
                        state: state.clone(),
                        upload: false,
                        lease: Some(lease),
                        cache,
                    }
                    .boxed_unsync()
                }));
            }
            Err(error) => {
                last = error.to_string();
                state.routes.lock().await.remove(host);
                if attempt == 0 && replayable {
                    routes = state.alternatives(host, None).await;
                }
                if !replayable {
                    break;
                }
            }
        }
    }
    Err(err(&last))
}

// Anonymous official API fallback for public Release files. Never forward cookies or tokens.
async fn release_api_fallback(
    state: &Arc<State>,
    uri: &hyper::Uri,
    original: &hyper::HeaderMap,
) -> Option<Response<HttpBody>> {
    let pieces: Vec<_> = uri.path().trim_start_matches('/').split('/').collect();
    if pieces.len() != 6 || pieces[2] != "releases" || pieces[3] != "download" {
        return None;
    }
    let owner = pieces[0];
    let repo = pieces[1];
    let tag = pieces[4];
    if [owner, repo].iter().any(|name| {
        name.is_empty()
            || name.len() > 100
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    }) || tag.len() > 256
    {
        return None;
    }
    let name = percent_encoding::percent_decode_str(pieces[5])
        .decode_utf8()
        .ok()?
        .into_owned();
    let key = format!("{owner}/{repo}/{tag}");
    let cached = state
        .release_assets
        .lock()
        .await
        .get(&key)
        .filter(|(at, _)| at.elapsed() < Duration::from_secs(300))
        .map(|(_, assets)| assets.clone());
    let result = async {
        let assets = if let Some(assets) = cached {
            assets
        } else {
            let request = Request::builder()
                .uri(format!("/repos/{owner}/{repo}/releases/tags/{tag}"))
                .header("Host", "api.github.com")
                .header(
                    "User-Agent",
                    concat!("GitHubTurbo/", env!("CARGO_PKG_VERSION")),
                )
                .header("Accept", "application/vnd.github+json")
                .body(empty())?;
            let exchange = state
                .pool
                .send(
                    state,
                    "api.github.com",
                    request,
                    None,
                    Some(Duration::from_secs(10)),
                )
                .await?;
            let response = exchange.response;
            let _lease = exchange.lease;
            if !response.status().is_success() {
                return Err(err("Release metadata API unavailable"));
            }
            let body = http_body_util::Limited::new(response.into_body(), 2 * 1024 * 1024)
                .collect()
                .await?
                .to_bytes();
            let json: serde_json::Value = serde_json::from_slice(&body)?;
            let assets = json["assets"]
                .as_array()
                .ok_or_else(|| err("missing Release assets"))?
                .iter()
                .take(128)
                .filter_map(|asset| {
                    Some((
                        asset["name"]
                            .as_str()
                            .filter(|n| n.len() <= 256)?
                            .to_owned(),
                        asset["id"].as_u64()?,
                    ))
                })
                .collect::<HashMap<_, _>>();
            let mut cache = state.release_assets.lock().await;
            if cache.len() >= 32 {
                if let Some(key) = cache.keys().next().cloned() {
                    cache.remove(&key);
                }
            }
            cache.insert(key, (Instant::now(), assets.clone()));
            assets
        };
        let id = assets
            .get(&name)
            .ok_or_else(|| err("public Release asset not found"))?;

        let mut request = Request::builder()
            .uri(format!("/repos/{owner}/{repo}/releases/assets/{id}"))
            .header("Host", "api.github.com")
            .header(
                "User-Agent",
                concat!("GitHubTurbo/", env!("CARGO_PKG_VERSION")),
            )
            .header("Accept", "application/octet-stream")
            .body(empty())?;
        for header in [hyper::header::RANGE, hyper::header::IF_RANGE] {
            if let Some(value) = original.get(&header) {
                request.headers_mut().insert(header, value.clone());
            }
        }
        let exchange = state
            .pool
            .send(
                state,
                "api.github.com",
                request,
                None,
                Some(Duration::from_secs(12)),
            )
            .await?;
        let response = exchange.response;
        let _lease = exchange.lease;
        if !(response.status().is_success() || response.status().is_redirection()) {
            return Err(err("Release asset API unavailable"));
        }
        if let Some(location) = response.headers().get(hyper::header::LOCATION) {
            let url = reqwest::Url::parse(location.to_str()?)?;
            if url.scheme() != "https" || !url.host_str().is_some_and(allowed) {
                return Err(err("Release redirect outside supported HTTPS destinations"));
            }
        }
        eprintln!("Release download: used official API fallback");
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(
            response.map(|body| body.map_err(Into::into).boxed_unsync()),
        )
    }
    .await;
    match result {
        Ok(reply) => Some(reply),
        Err(error) => {
            eprintln!("Release fallback: {error}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn tls_streaming_range_host_scope_and_post_no_replay() -> Result<()> {
        let dir = std::env::temp_dir().join(format!(
            "github-turbo-test-{}-{}",
            std::process::id(),
            time::OffsetDateTime::now_utc().unix_timestamp_nanos()
        ));
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                if self.0.parent() == Some(std::env::temp_dir().as_path())
                    && self
                        .0
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with("github-turbo-test-")
                {
                    let _ = std::fs::remove_dir_all(&self.0);
                }
            }
        }
        let _cleanup = Cleanup(dir.clone());
        let mut config: Config = serde_json::from_str(include_str!("../config.json"))?;
        config.transparent.state_dir = dir.clone();
        config.mode = Mode::Direct;
        config
            .ip_overrides
            .insert("github.com".into(), vec!["1.1.1.1".parse()?]);
        transparent::init_ca(&config.transparent, config.listen)?;
        let root = native_tls::Certificate::from_pem(&std::fs::read(dir.join("ca.pem"))?)?;
        let tls: TlsConnector = native_tls::TlsConnector::builder()
            .add_root_certificate(root)
            .build()?
            .into();
        let mut state = State::new(config)?;
        Arc::get_mut(&mut state).unwrap().tls = tls.clone();
        Arc::get_mut(&mut state).unwrap().tls_http = tls.clone();
        Arc::get_mut(&mut state).unwrap().tls_http_no_sni = tls.clone();
        let accept = transparent::acceptor(&state.config.transparent)?;
        let backend = TcpListener::bind("127.0.0.1:0").await?;
        let address = backend.local_addr()?;
        state
            .cache_route("github.com", Route::Direct(address))
            .await;
        let count = Arc::new(AtomicU64::new(0));
        let backend_count = count.clone();
        let backend_accept = accept.clone();
        let backend_task = tokio::spawn(async move {
            while let Ok((socket, _)) = backend.accept().await {
                let accept = backend_accept.clone();
                let count = backend_count.clone();
                tokio::spawn(async move {
                    let Ok(stream) = accept.accept(socket).await else {
                        return;
                    };
                    let service = service_fn(move |request: Request<Incoming>| {
                        let count = count.clone();
                        async move {
                            count.fetch_add(1, Ordering::Relaxed);
                            let path = request.uri().path().to_owned();
                            let is_range = request
                                .headers()
                                .get("range")
                                .is_some_and(|v| v == "bytes=2-4");
                            let bytes = request.into_body().collect().await.unwrap().to_bytes();
                            let reply = if path == "/fail" {
                                Response::builder()
                                    .status(503)
                                    .body(Full::new(Bytes::from_static(b"unavailable")))
                                    .unwrap()
                            } else if is_range {
                                Response::builder()
                                    .status(206)
                                    .header("Content-Range", "bytes 2-4/10")
                                    .body(Full::new(Bytes::from_static(b"234")))
                                    .unwrap()
                            } else {
                                Response::new(Full::new(bytes))
                            };
                            Ok::<_, std::convert::Infallible>(reply)
                        }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        let front = TcpListener::bind("127.0.0.1:0").await?;
        let front_address = front.local_addr()?;
        let front_state = state.clone();
        let front_task = tokio::spawn(async move {
            while let Ok((socket, _)) = front.accept().await {
                let accept = accept.clone();
                let state = front_state.clone();
                tokio::spawn(async move {
                    if let Ok(stream) = accept.accept(socket).await {
                        let _ = serve(stream, state, "github.com".into()).await;
                    }
                });
            }
        });
        let socket = TcpStream::connect(front_address).await?;
        let stream = tls.connect("github.com", socket).await?;
        let (mut client, connection) =
            hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
        let driver = tokio::spawn(connection);
        let payload = Bytes::from(vec![0x81; 512 * 1024]);
        let request = Request::builder()
            .method("POST")
            .uri("/echo")
            .header("Host", "github.com")
            .body(Full::new(payload.clone()))?;
        let response = client.send_request(request).await?;
        assert_eq!(response.into_body().collect().await?.to_bytes(), payload);
        let request = Request::builder()
            .uri("/range")
            .header("Host", "github.com")
            .header("Range", "bytes=2-4")
            .body(Full::new(Bytes::new()))?;
        let response = client.send_request(request).await?;
        assert_eq!(response.status(), 206);
        assert_eq!(response.headers()["content-range"], "bytes 2-4/10");
        assert_eq!(response.into_body().collect().await?.to_bytes(), "234");
        let request = Request::builder()
            .method("POST")
            .uri("/fail")
            .header("Host", "github.com")
            .body(Full::new(Bytes::from_static(b"do not replay")))?;
        let response = client.send_request(request).await?;
        assert_eq!(response.status(), 503);
        let _ = response.into_body().collect().await?;
        assert_eq!(count.load(Ordering::Relaxed), 3);
        let request = Request::builder()
            .uri("/")
            .header("Host", "example.com")
            .body(Full::new(Bytes::new()))?;
        let response = client.send_request(request).await?;
        assert_eq!(response.status(), 421);
        let _ = response.into_body().collect().await?;
        assert_eq!(count.load(Ordering::Relaxed), 3);
        assert!(tls
            .connect(
                "wrong-name.example",
                TcpStream::connect(front_address).await?
            )
            .await
            .is_err());
        drop(client);
        driver.abort();
        front_task.abort();
        backend_task.abort();
        Ok(())
    }
}
