use super::*;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{body::Incoming, service::service_fn, Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
struct Fixture {
    state: Arc<State>,
    addr: SocketAddr,
    tls: TlsConnector,
    data: Bytes,
    requests: Arc<AtomicU64>,
    connections: Arc<AtomicU64>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    dir: std::path::PathBuf,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        if self.dir.parent() == Some(std::env::temp_dir().as_path())
            && self
                .dir
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("turbo-transport-")
        {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}
impl Fixture {
    async fn new() -> Result<Self> {
        let dir = std::env::temp_dir().join(format!(
            "turbo-transport-{}",
            time::OffsetDateTime::now_utc().unix_timestamp_nanos()
        ));
        let mut config: Config = serde_json::from_str(include_str!("../config.json"))?;
        config.transparent.state_dir = dir.clone();
        config.mode = Mode::Direct;
        config.optimization.seed_routes = false;
        config
            .ip_overrides
            .insert("raw.githubusercontent.com".into(), vec!["1.1.1.1".parse()?]);
        transparent::init_ca(&config.transparent, config.listen)?;
        let root = native_tls::Certificate::from_pem(&std::fs::read(dir.join("ca.pem"))?)?;
        let tls: TlsConnector = native_tls::TlsConnector::builder()
            .add_root_certificate(root)
            .request_alpns(&["h2", "http/1.1"])
            .build()?
            .into();
        let mut state = State::new(config)?;
        Arc::get_mut(&mut state).unwrap().tls_http = tls.clone();
        Arc::get_mut(&mut state).unwrap().tls = native_tls::TlsConnector::builder()
            .add_root_certificate(native_tls::Certificate::from_pem(&std::fs::read(
                dir.join("ca.pem"),
            )?)?)
            .request_alpns(&["http/1.1"])
            .build()?
            .into();
        let accept = transparent::acceptor(&state.config.transparent)?;
        let backend = TcpListener::bind("127.0.0.1:0").await?;
        state
            .cache_route(
                "raw.githubusercontent.com",
                Route::Direct(backend.local_addr()?),
            )
            .await;
        let requests = Arc::new(AtomicU64::new(0));
        let connections = Arc::new(AtomicU64::new(0));
        let count = requests.clone();
        let conns = connections.clone();
        let backend_accept = accept.clone();
        let data = Bytes::from(
            (0..5 * 1024 * 1024)
                .map(|i| (i % 251) as u8)
                .collect::<Vec<_>>(),
        );
        let content = data.clone();
        let backend = tokio::spawn(async move {
            while let Ok((socket, _)) = backend.accept().await {
                let accept = backend_accept.clone();
                let count = count.clone();
                let content = content.clone();
                conns.fetch_add(1, Ordering::Relaxed);
                tokio::spawn(async move {
                    let Ok(stream) = accept.accept(socket).await else {
                        return;
                    };
                    let service = service_fn(move |mut request: Request<Incoming>| {
                        let count = count.clone();
                        let content = content.clone();
                        async move {
                            count.fetch_add(1, Ordering::Relaxed);
                            if request.uri().path() == "/upgrade" {
                                assert_eq!(request.version(), hyper::Version::HTTP_11);
                                let upgrade = hyper::upgrade::on(&mut request);
                                tokio::spawn(async move {
                                    if let Ok(stream) = upgrade.await {
                                        let mut stream = TokioIo::new(stream);
                                        let mut bytes = [0u8; 4];
                                        if stream.read_exact(&mut bytes).await.is_ok() {
                                            let _ = stream.write_all(&bytes).await;
                                            let _ = stream.shutdown().await;
                                        }
                                    }
                                });
                                return Ok::<_, std::convert::Infallible>(
                                    Response::builder()
                                        .status(101)
                                        .header("connection", "upgrade")
                                        .header("upgrade", "websocket")
                                        .body(Full::new(Bytes::new()))
                                        .unwrap(),
                                );
                            }
                            let path = request.uri().path();
                            let response =
                                if matches!(path, "/large" | "/bad-range" | "/slow-range") {
                                    if let Some(range) =
                                        request.headers().get("range").and_then(|v| v.to_str().ok())
                                    {
                                        if path == "/slow-range" {
                                            tokio::time::sleep(Duration::from_secs(4)).await;
                                        }
                                        let (a, b) = range
                                            .trim_start_matches("bytes=")
                                            .split_once('-')
                                            .unwrap();
                                        let a = a.parse::<usize>().unwrap();
                                        let b = b.parse::<usize>().unwrap();
                                        let start = if path == "/bad-range" { a + 1 } else { a };
                                        Response::builder()
                                            .status(206)
                                            .header(
                                                "content-range",
                                                format!("bytes {start}-{b}/{}", content.len()),
                                            )
                                            .header("etag", "\"entity1\"")
                                            .body(Full::new(content.slice(a..=b)))
                                            .unwrap()
                                    } else {
                                        Response::builder()
                                            .header("content-type", "image/png")
                                            .header("etag", "\"entity1\"")
                                            .header("accept-ranges", "bytes")
                                            .body(Full::new(content))
                                            .unwrap()
                                    }
                                } else if path == "/immutable.css" {
                                    Response::builder()
                                        .header("cache-control", "public,max-age=120")
                                        .header("content-type", "text/css")
                                        .body(Full::new(Bytes::from_static(b"body{color:blue}")))
                                        .unwrap()
                                } else {
                                    tokio::time::sleep(Duration::from_millis(40)).await;
                                    Response::new(Full::new(Bytes::from_static(b"ok")))
                                };
                            Ok::<_, std::convert::Infallible>(response)
                        }
                    });
                    let _ = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                        .serve_connection_with_upgrades(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        let front = TcpListener::bind("127.0.0.1:0").await?;
        let addr = front.local_addr()?;
        let front_state = state.clone();
        let front = tokio::spawn(async move {
            while let Ok((socket, _)) = front.accept().await {
                let accept = accept.clone();
                let state = front_state.clone();
                tokio::spawn(async move {
                    let _ = transparent::bridge(socket, state, accept).await;
                });
            }
        });
        Ok(Self {
            state,
            addr,
            tls,
            data,
            requests,
            connections,
            tasks: vec![front, backend],
            dir,
        })
    }
    async fn client(&self) -> Result<hyper::client::conn::http2::SendRequest<Full<Bytes>>> {
        let socket = TcpStream::connect(self.addr).await?;
        let stream = self
            .tls
            .connect("raw.githubusercontent.com", socket)
            .await?;
        let (sender, conn) =
            hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(stream))
                .await?;
        tokio::spawn(conn);
        Ok(sender)
    }
    fn request(path: &str) -> Request<Full<Bytes>> {
        Request::builder()
            .uri(format!("https://raw.githubusercontent.com{path}"))
            .body(Full::new(Bytes::new()))
            .unwrap()
    }
}
#[tokio::test]
async fn shared_h2_pool_and_public_cache_privacy() -> Result<()> {
    let f = Fixture::new().await?;
    let mut a = f.client().await?;
    let mut b = f.client().await?;
    let r = a.send_request(Fixture::request("/slow")).await?;
    assert_eq!(r.headers()["x-github-turbo-upstream"], "h2");
    r.into_body().collect().await?;
    let start = Instant::now();
    let mut jobs = JoinSet::new();
    for i in 0..12 {
        let mut client = if i % 2 == 0 { a.clone() } else { b.clone() };
        jobs.spawn(async move {
            client
                .send_request(Fixture::request("/slow"))
                .await
                .unwrap()
                .into_body()
                .collect()
                .await
                .unwrap();
        });
    }
    while jobs.join_next().await.is_some() {}
    assert!(start.elapsed() < Duration::from_millis(450));
    assert_eq!(f.connections.load(Ordering::Relaxed), 1);
    assert_eq!(f.state.pool.snapshot().await["h2_created"], 1);
    let first = a.send_request(Fixture::request("/immutable.css")).await?;
    first.into_body().collect().await?;
    let before = f.requests.load(Ordering::Relaxed);
    let cached = b.send_request(Fixture::request("/immutable.css")).await?;
    assert_eq!(cached.headers()["x-github-turbo-cache"], "hit");
    cached.into_body().collect().await?;
    assert_eq!(f.requests.load(Ordering::Relaxed), before);
    let mut req = Fixture::request("/immutable.css");
    req.headers_mut()
        .insert("cookie", "session=private".parse()?);
    let reply = b.send_request(req).await?;
    assert_eq!(reply.headers()["x-github-turbo-cache"], "miss");
    reply.into_body().collect().await?;
    assert_eq!(f.requests.load(Ordering::Relaxed), before + 1);
    Ok(())
}
#[tokio::test]
async fn segmented_bytes_exact_and_malformed_ranges_fall_back() -> Result<()> {
    let f = Fixture::new().await?;
    let mut client = f.client().await?;
    let response = client.send_request(Fixture::request("/large")).await?;
    assert_eq!(response.headers()["x-github-turbo-route"], "direct-split");
    let bytes = response.into_body().collect().await?.to_bytes();
    assert_eq!(bytes, f.data);
    assert!(f.connections.load(Ordering::Relaxed) >= 2);
    let response = client.send_request(Fixture::request("/bad-range")).await?;
    assert!(!response.headers().contains_key("x-github-turbo-route"));
    assert_eq!(response.into_body().collect().await?.to_bytes(), f.data);
    let before = f.requests.load(Ordering::Relaxed);
    let response = client.send_request(Fixture::request("/slow-range")).await?;
    assert_eq!(response.headers()["x-github-turbo-split"], "fallback");
    assert_eq!(response.into_body().collect().await?.to_bytes(), f.data);
    assert_eq!(
        f.requests.load(Ordering::Relaxed),
        before + 5,
        "fallback keeps the original GET"
    );
    Ok(())
}

#[tokio::test]
async fn upgrade_has_dedicated_http1_channel_alongside_h2() -> Result<()> {
    let f = Fixture::new().await?;
    let mut h2 = f.client().await?;
    h2.send_request(Fixture::request("/slow"))
        .await?
        .into_body()
        .collect()
        .await?;
    let stream = f
        .state
        .tls
        .connect(
            "raw.githubusercontent.com",
            TcpStream::connect(f.addr).await?,
        )
        .await?;
    let (mut client, connection) =
        hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
    let driver = tokio::spawn(connection.with_upgrades());
    let request = Request::builder()
        .uri("/upgrade")
        .header("host", "raw.githubusercontent.com")
        .header("connection", "upgrade")
        .header("upgrade", "websocket")
        .body(Full::new(Bytes::new()))?;
    let mut response = client.send_request(request).await?;
    assert_eq!(response.status(), 101);
    let mut stream =
        TokioIo::new(timeout(Duration::from_secs(3), hyper::upgrade::on(&mut response)).await??);
    stream.write_all(b"ping").await?;
    let mut bytes = [0u8; 4];
    timeout(Duration::from_secs(3), stream.read_exact(&mut bytes)).await??;
    assert_eq!(&bytes, b"ping");
    assert_eq!(f.connections.load(Ordering::Relaxed), 2);
    let response = h2.send_request(Fixture::request("/slow")).await?;
    assert_eq!(response.headers()["x-github-turbo-upstream"], "h2");
    response.into_body().collect().await?;
    driver.abort();
    Ok(())
}
