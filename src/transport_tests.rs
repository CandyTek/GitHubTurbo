use super::*;
use bytes::{Bytes, BytesMut};
use http_body_util::{BodyExt, Full};
use hyper::{body::Incoming, service::service_fn, Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
type TestBody =
    http_body_util::combinators::UnsyncBoxBody<Bytes, Box<dyn std::error::Error + Send + Sync>>;
// 16 KiB per 100 ms is well under the rescue threshold. After `slow_for` the rest
// arrives at once, so a test that relies on rescue never waits for this to finish.
fn throttled(content: Bytes, slow_for: Duration) -> TestBody {
    let began = Instant::now();
    http_body_util::StreamBody::new(futures_util::stream::unfold(
        (content, began),
        move |(mut rest, began)| async move {
            if rest.is_empty() {
                return None;
            }
            let chunk = if began.elapsed() < slow_for {
                tokio::time::sleep(Duration::from_millis(100)).await;
                rest.split_to(rest.len().min(16 * 1024))
            } else {
                rest.split_to(rest.len())
            };
            Some((
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(hyper::body::Frame::data(chunk)),
                (rest, began),
            ))
        },
    ))
    .boxed_unsync()
}
fn entity(content_length: usize) -> hyper::http::response::Builder {
    Response::builder()
        .header("content-type", "image/png")
        .header("etag", "\"entity1\"")
        .header("content-length", content_length)
}
struct Fixture {
    state: Arc<State>,
    addr: SocketAddr,
    tls: TlsConnector,
    data: Bytes,
    requests: Arc<AtomicU64>,
    connections: Arc<AtomicU64>,
    overlap: Arc<std::sync::atomic::AtomicBool>,
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
        config.optimization.segmented_downloads = true;
        config.optimization.adaptive_split = false;
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
        let overlap = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let slow_active = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let backend_overlap = overlap.clone();
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
                let overlap = backend_overlap.clone();
                let slow_active = slow_active.clone();
                conns.fetch_add(1, Ordering::Relaxed);
                tokio::spawn(async move {
                    let Ok(stream) = accept.accept(socket).await else {
                        return;
                    };
                    let service = service_fn(move |mut request: Request<Incoming>| {
                        let count = count.clone();
                        let content = content.clone();
                        let overlap = overlap.clone();
                        let slow_active = slow_active.clone();
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
                                        .unwrap()
                                        .map(|body| {
                                            body.map_err(|never| -> Box<dyn std::error::Error+Send+Sync> { match never {} }).boxed_unsync()
                                        }),
                                );
                            }
                            let path = request.uri().path().to_owned();
                            let path = path.as_str();
                            let size = content.len();
                            let ranged = request.headers().contains_key("range");
                            let hour = Duration::from_secs(3600);
                            if matches!(path, "/wrong-edge" | "/real-refusal") {
                                let mut reply = Response::builder().status(400);
                                if path == "/real-refusal" {
                                    reply = reply.header("x-github-request-id", "8CA1:415C2");
                                }
                                return Ok(reply.body(Full::new(Bytes::new())).unwrap().map(
                                    |body| {
                                        body.map_err(
                                            |never| -> Box<dyn std::error::Error + Send + Sync> {
                                                match never {}
                                            },
                                        )
                                        .boxed_unsync()
                                    },
                                ));
                            }
                            match (path, ranged) {
                                // Never finishes unless the download moves elsewhere.
                                ("/trickle" | "/unlucky" | "/slow.git/git-upload-pack", false) => {
                                    return Ok(entity(size)
                                        .body(throttled(content, hour))
                                        .unwrap());
                                }
                                // Ignores Range, and speeds up after the rescue has begun.
                                ("/no-range", _) => {
                                    return Ok(entity(size)
                                        .body(throttled(content, Duration::from_millis(1800)))
                                        .unwrap());
                                }
                                ("/dies", false) => {
                                    use futures_util::StreamExt;
                                    let head = content.slice(..256 * 1024);
                                    // Paced, so the reset cannot overtake the headers.
                                    let body = futures_util::stream::iter([
                                        Ok(hyper::body::Frame::data(head)),
                                        Err(err("connection lost")),
                                    ])
                                    .then(|item| async move {
                                        tokio::time::sleep(Duration::from_millis(100)).await;
                                        item
                                    });
                                    return Ok(entity(size)
                                        .body(http_body_util::StreamBody::new(body).boxed_unsync())
                                        .unwrap());
                                }
                                // The first replacement connections are unlucky too.
                                ("/unlucky", true)
                                    if UNLUCKY.fetch_add(1, Ordering::SeqCst) < 2 =>
                                {
                                    let range = request.headers()["range"].to_str().unwrap();
                                    let (a, b) =
                                        range.trim_start_matches("bytes=").split_once('-').unwrap();
                                    let (a, b) = (a.parse().unwrap(), b.parse::<usize>().unwrap());
                                    return Ok(Response::builder()
                                        .status(206)
                                        .header("content-range", format!("bytes {a}-{b}/{size}"))
                                        .header("etag", "\"entity1\"")
                                        .body(throttled(content.slice(a..=b), hour))
                                        .unwrap());
                                }
                                _ => {}
                            }
                            if path == "/single-slow" && !request.headers().contains_key("range") {
                                let stream = futures_util::stream::once(async move {
                                    tokio::time::sleep(Duration::from_millis(200)).await;
                                    Ok::<_, std::convert::Infallible>(hyper::body::Frame::data(
                                        content,
                                    ))
                                });
                                return Ok(Response::builder()
                                    .header("content-type", "image/png")
                                    .header("etag", "\"entity1\"")
                                    .header("content-length", 5 * 1024 * 1024)
                                    .body(
                                        http_body_util::StreamBody::new(stream)
                                            .map_err(|never| -> Box<dyn std::error::Error+Send+Sync> { match never {} })
                                            .boxed_unsync(),
                                    )
                                    .unwrap());
                            }
                            let response = if matches!(
                                path,
                                "/large"
                                    | "/repo.git/git-upload-pack"
                                    | "/bad-range"
                                    | "/slow-range"
                                    | "/pipeline"
                                    | "/single-slow"
                                    | "/trickle"
                                    | "/unlucky"
                                    | "/dies"
                            ) {
                                if let Some(range) =
                                    request.headers().get("range").and_then(|v| v.to_str().ok())
                                {
                                    if path == "/slow-range" {
                                        tokio::time::sleep(Duration::from_secs(4)).await;
                                    }
                                    let (a, b) =
                                        range.trim_start_matches("bytes=").split_once('-').unwrap();
                                    let a = a.parse::<usize>().unwrap();
                                    let b = b.parse::<usize>().unwrap();
                                    if path == "/pipeline" {
                                        if a == 1024 * 1024 {
                                            slow_active.store(true, Ordering::SeqCst);
                                            tokio::time::sleep(Duration::from_millis(250)).await;
                                            slow_active.store(false, Ordering::SeqCst);
                                        } else if a >= 2 * 1024 * 1024
                                            && slow_active.load(Ordering::SeqCst)
                                        {
                                            overlap.store(true, Ordering::SeqCst);
                                        }
                                    }
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
                            Ok::<_, std::convert::Infallible>(response.map(|body| {
                                body.map_err(|never| -> Box<dyn std::error::Error + Send + Sync> {
                                    match never {}
                                })
                                .boxed_unsync()
                            }))
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
            overlap,
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
static UNLUCKY: AtomicU64 = AtomicU64::new(0);
async fn rescue_fixture() -> Result<Fixture> {
    let f = Fixture::new().await?;
    {
        let mut options = f.state.options.write().unwrap();
        options.segmented_downloads = false;
        options.flow_rescue = true;
    }
    Ok(f)
}
fn rescue_count(f: &Fixture, field: &str) -> u64 {
    f.state.rescue.snapshot()[field].as_u64().unwrap()
}
#[tokio::test]
async fn slow_download_moves_to_new_connections_with_identical_bytes() -> Result<()> {
    let f = rescue_fixture().await?;
    let mut client = f.client().await?;
    let start = Instant::now();
    let response = client.send_request(Fixture::request("/trickle")).await?;
    assert_eq!(response.headers()["content-length"], "5242880");
    assert_eq!(response.into_body().collect().await?.to_bytes(), f.data);
    // At the original speed this would take over five minutes.
    assert!(start.elapsed() < Duration::from_secs(10));
    assert_eq!(rescue_count(&f, "triggered"), 1);
    assert_eq!(rescue_count(&f, "completed"), 1);
    Ok(())
}
#[tokio::test]
async fn unlucky_replacement_connections_are_dropped_and_replaced() -> Result<()> {
    let f = rescue_fixture().await?;
    let mut client = f.client().await?;
    let response = client.send_request(Fixture::request("/unlucky")).await?;
    assert_eq!(response.into_body().collect().await?.to_bytes(), f.data);
    assert_eq!(rescue_count(&f, "completed"), 1);
    Ok(())
}
#[tokio::test]
async fn upstream_dying_mid_file_is_resumed_without_the_client_noticing() -> Result<()> {
    let f = rescue_fixture().await?;
    let mut client = f.client().await?;
    let response = client.send_request(Fixture::request("/dies")).await?;
    assert_eq!(response.into_body().collect().await?.to_bytes(), f.data);
    assert_eq!(rescue_count(&f, "completed"), 1);
    Ok(())
}
#[tokio::test]
async fn server_refusing_range_still_delivers_over_the_original_get() -> Result<()> {
    let f = rescue_fixture().await?;
    let mut client = f.client().await?;
    let response = client.send_request(Fixture::request("/no-range")).await?;
    assert_eq!(response.into_body().collect().await?.to_bytes(), f.data);
    assert_eq!(rescue_count(&f, "triggered"), 1);
    Ok(())
}
#[tokio::test]
async fn healthy_download_or_slow_client_opens_nothing_extra() -> Result<()> {
    let f = rescue_fixture().await?;
    let mut client = f.client().await?;
    let response = client.send_request(Fixture::request("/large")).await?;
    let mut body = response.into_body();
    let mut received = BytesMut::new();
    // A client that stops reading must not look like a slow upstream.
    received.extend_from_slice(body.frame().await.unwrap()?.data_ref().unwrap());
    tokio::time::sleep(Duration::from_millis(2500)).await;
    received.extend_from_slice(&body.collect().await?.to_bytes());
    assert_eq!(received.freeze(), f.data);
    assert_eq!(rescue_count(&f, "triggered"), 0);
    assert_eq!(f.requests.load(Ordering::Relaxed), 1);
    assert_eq!(f.connections.load(Ordering::Relaxed), 1);
    Ok(())
}
#[tokio::test]
async fn wrong_edge_refusal_cools_the_route_but_a_real_error_does_not() -> Result<()> {
    let f = rescue_fixture().await?;
    let host = "github.com";
    let route = f.state.routes.lock().await["raw.githubusercontent.com"]
        .route
        .clone();
    f.state.cache_route(host, route.clone()).await;
    let socket = TcpStream::connect(f.addr).await?;
    let stream = f.tls.connect(host, socket).await?;
    let (mut client, conn) =
        hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(stream)).await?;
    tokio::spawn(conn);
    // A POST is never replayed, so the reply stays visible and no other address is tried.
    let post = |path: &str| {
        Request::builder()
            .method("POST")
            .uri(format!("https://github.com{path}"))
            .body(Full::new(Bytes::from_static(b"x")))
            .unwrap()
    };
    let response = client.send_request(post("/real-refusal")).await?;
    assert_eq!(response.status(), 400);
    let _ = response.into_body().collect().await?;
    assert!(!f.state.health.bad(host, &route));
    let response = client.send_request(post("/wrong-edge")).await?;
    assert_eq!(response.status(), 400);
    let _ = response.into_body().collect().await?;
    assert!(f.state.health.bad(host, &route));
    assert!(
        f.state.select("github.com").await.ok().as_ref() != Some(&route),
        "the refused route must not be served again from the cache"
    );
    Ok(())
}
#[tokio::test]
async fn prewarmed_connection_serves_the_first_request() -> Result<()> {
    let f = rescue_fixture().await?;
    f.state.options.write().unwrap().flow_rescue = false;
    let host = "raw.githubusercontent.com";
    prewarm::connect_for_test(&f.state, host).await;
    prewarm::connect_for_test(&f.state, host).await;
    let snapshot = f.state.prewarm.snapshot();
    assert_eq!(snapshot["opened"], 1, "a second pass must not open another");
    assert_eq!(f.connections.load(Ordering::Relaxed), 1);
    assert_eq!(
        f.requests.load(Ordering::Relaxed),
        0,
        "nothing is sent early"
    );
    let mut client = f.client().await?;
    let response = client.send_request(Fixture::request("/no-range")).await?;
    assert_eq!(response.into_body().collect().await?.to_bytes(), f.data);
    assert_eq!(f.connections.load(Ordering::Relaxed), 1);
    Ok(())
}
#[tokio::test]
async fn failed_prewarm_leaves_no_cooldown_behind() -> Result<()> {
    let f = Fixture::new().await?;
    let host = "raw.githubusercontent.com";
    let closed = TcpListener::bind("127.0.0.1:0").await?.local_addr()?;
    let route = Route::Direct(closed);
    f.state.cache_route(host, route.clone()).await;
    prewarm::connect_for_test(&f.state, host).await;
    assert_eq!(f.state.prewarm.snapshot()["failed"], 1);
    assert!(!f.state.health.bad(host, &route));
    Ok(())
}
/// A second upstream that answers every request at full speed with `content`,
/// recording each request body it receives.
async fn fast_backend(
    f: &Fixture,
    content: Bytes,
) -> Result<(
    SocketAddr,
    Arc<std::sync::Mutex<Vec<Bytes>>>,
    tokio::task::JoinHandle<()>,
)> {
    let accept = transparent::acceptor(&f.state.config.transparent)?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = bodies.clone();
    let task = tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let (accept, content, seen) = (accept.clone(), content.clone(), seen.clone());
            tokio::spawn(async move {
                let Ok(stream) = accept.accept(socket).await else {
                    return;
                };
                let service = service_fn(move |request: Request<Incoming>| {
                    let (content, seen) = (content.clone(), seen.clone());
                    async move {
                        let body = request.into_body().collect().await.unwrap().to_bytes();
                        seen.lock().unwrap().push(body);
                        Ok::<_, std::convert::Infallible>(Response::new(Full::new(content)))
                    }
                });
                let _ = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    Ok((addr, bodies, task))
}
fn race_count(f: &Fixture, field: &str) -> u64 {
    f.state.race.snapshot()[field].as_u64().unwrap()
}
#[tokio::test]
async fn git_race_keeps_the_faster_connection_and_its_exact_bytes() -> Result<()> {
    let f = rescue_fixture().await?;
    let host = "github.com";
    let slow = f.state.routes.lock().await["raw.githubusercontent.com"]
        .route
        .clone();
    let (fast, bodies, task) = fast_backend(&f, f.data.clone()).await?;
    let uri: hyper::Uri = "/slow.git/git-upload-pack".parse()?;
    let request =
        Bytes::from_static(b"0032want 7fd1a60b01f91b314f59955a4e4d4e80d8edf11d\n00000009done\n");
    let start = Instant::now();
    // The usual choice trickles forever; the other address is fast.
    let outcome = race::contest(
        &f.state,
        host,
        vec![slow, Route::Direct(fast)],
        &uri,
        &hyper::HeaderMap::new(),
        request.clone(),
    )
    .await;
    let race::Outcome::Kept { response, lease } = outcome else {
        panic!("the race should keep a reply");
    };
    assert_eq!(lease.route(), &Route::Direct(fast));
    assert_eq!(response.into_body().collect().await?.to_bytes(), f.data);
    assert!(start.elapsed() < Duration::from_secs(10));
    assert_eq!(bodies.lock().unwrap().as_slice(), [request]);
    assert_eq!(race_count(&f, "kept_other"), 1);
    assert_eq!(race_count(&f, "kept_first"), 0);
    task.abort();
    Ok(())
}
#[tokio::test]
async fn git_race_falls_back_to_the_survivor_when_one_address_is_dead() -> Result<()> {
    let f = rescue_fixture().await?;
    let dead = TcpListener::bind("127.0.0.1:0").await?.local_addr()?;
    let small = Bytes::from_static(b"0008NAK\n0000");
    let (fast, _, task) = fast_backend(&f, small.clone()).await?;
    let uri: hyper::Uri = "/repo.git/git-upload-pack".parse()?;
    let outcome = race::contest(
        &f.state,
        "github.com",
        vec![Route::Direct(dead), Route::Direct(fast)],
        &uri,
        &hyper::HeaderMap::new(),
        Bytes::from_static(b"0000"),
    )
    .await;
    let race::Outcome::Kept { response, .. } = outcome else {
        panic!("the live address should win");
    };
    // A reply shorter than the decision size wins as soon as it is complete.
    assert_eq!(response.into_body().collect().await?.to_bytes(), small);
    task.abort();
    Ok(())
}
#[tokio::test]
async fn git_race_is_off_by_default_and_only_for_small_upload_pack_posts() -> Result<()> {
    let f = rescue_fixture().await?;
    let mut headers = hyper::HeaderMap::new();
    assert!(race::wants_objects(&headers, b"0011command=fetch0001000dthin-pack0032want 7fd1a60b01f91b314f59955a4e4d4e80d8edf11d\n0009done\n0000"));
    assert!(!race::wants_objects(
        &headers,
        b"0014command=ls-refs\n0001000bsymrefs0000"
    ));
    headers.insert("content-encoding", "gzip".parse()?);
    assert!(race::wants_objects(&headers, b"\x1f\x8b"));
    headers.remove("content-encoding");
    headers.insert("content-length", "120".parse()?);
    let path = "/a/b.git/git-upload-pack";
    let post = hyper::Method::POST;
    assert!(!race::eligible(
        &f.state,
        "github.com",
        &post,
        path,
        &headers
    ));
    f.state.options.write().unwrap().git_race = true;
    assert!(race::eligible(
        &f.state,
        "github.com",
        &post,
        path,
        &headers
    ));
    assert!(!race::eligible(
        &f.state,
        "github.com",
        &hyper::Method::GET,
        path,
        &headers
    ));
    assert!(!race::eligible(
        &f.state,
        "github.com",
        &post,
        "/a/b.git/git-receive-pack",
        &headers
    ));
    assert!(!race::eligible(
        &f.state,
        "api.github.com",
        &post,
        path,
        &headers
    ));
    headers.insert(
        "content-length",
        (race::MAX_REQUEST + 1).to_string().parse()?,
    );
    assert!(!race::eligible(
        &f.state,
        "github.com",
        &post,
        path,
        &headers
    ));
    headers.remove("content-length");
    assert!(
        !race::eligible(&f.state, "github.com", &post, path, &headers),
        "chunked bodies are not buffered"
    );
    Ok(())
}
#[tokio::test]
async fn disabling_rescue_leaves_a_plain_stream() -> Result<()> {
    let f = rescue_fixture().await?;
    f.state.options.write().unwrap().flow_rescue = false;
    let mut client = f.client().await?;
    let response = client.send_request(Fixture::request("/no-range")).await?;
    assert_eq!(response.into_body().collect().await?.to_bytes(), f.data);
    assert_eq!(rescue_count(&f, "triggered"), 0);
    assert_eq!(f.requests.load(Ordering::Relaxed), 1);
    Ok(())
}
#[tokio::test]
async fn concurrent_downloads_get_separate_connections() -> Result<()> {
    let f = rescue_fixture().await?;
    f.state.options.write().unwrap().flow_rescue = false;
    let host = "raw.githubusercontent.com";
    let route = f.state.routes.lock().await[host].route.clone();
    f.state.cache_route_for(host, Traffic::Bulk, route).await;
    let mut client = f.client().await?;
    let path = "/slow.git/git-upload-pack";
    let first = client.send_request(Fixture::request(path)).await?;
    let second = client.send_request(Fixture::request(path)).await?;
    let pools = f.state.pool.snapshot().await;
    let bulk = pools["channels"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["profile"] == "bulk")
        .count();
    assert_eq!(bulk, 2);
    drop((first, second));
    Ok(())
}
#[tokio::test]
async fn connection_of_a_slow_cancelled_download_is_not_reused() -> Result<()> {
    let f = rescue_fixture().await?;
    f.state.options.write().unwrap().flow_rescue = false;
    let host = "raw.githubusercontent.com";
    let route = f.state.routes.lock().await[host].route.clone();
    f.state.cache_route_for(host, Traffic::Bulk, route).await;
    let mut client = f.client().await?;
    let path = "/slow.git/git-upload-pack";
    let mut response = client.send_request(Fixture::request(path)).await?;
    let until = Instant::now() + Duration::from_millis(2300);
    while Instant::now() < until {
        response.frame().await.unwrap()?;
    }
    drop(response);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let before = f.connections.load(Ordering::Relaxed);
    let again = client.send_request(Fixture::request(path)).await?;
    assert_eq!(f.connections.load(Ordering::Relaxed), before + 1);
    // Cancellation says nothing about the address: it must not be blacklisted.
    assert_eq!(f.state.performance.snapshot().as_array().unwrap().len(), 0);
    drop(again);
    Ok(())
}
#[tokio::test]
async fn bulk_keeps_dns_preference_while_web_chooses_first_verified_tls() -> Result<()> {
    let mut f = Fixture::new().await?;
    let accept = transparent::acceptor(&f.state.config.transparent)?;
    let mut addresses = Vec::new();
    for delay in [80, 5] {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        addresses.push(listener.local_addr()?);
        let accept = accept.clone();
        f.tasks.push(tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let accept = accept.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    let _ = accept.accept(socket).await;
                });
            }
        }));
    }
    let web = f
        .state
        .probe_addresses_for("raw.githubusercontent.com", addresses.clone(), false)
        .await?;
    assert_eq!(web, Route::Direct(addresses[1]));
    let bulk = f
        .state
        .probe_addresses_for("raw.githubusercontent.com", addresses.clone(), true)
        .await?;
    assert_eq!(bulk, Route::Direct(addresses[0]));
    Ok(())
}
#[tokio::test]
async fn bulk_pool_is_separate_and_only_complete_large_body_teaches_routing() -> Result<()> {
    let f = Fixture::new().await?;
    f.state.options.write().unwrap().segmented_downloads = false;
    let host = "raw.githubusercontent.com";
    let route = f.state.routes.lock().await[host].route.clone();
    f.state.cache_route_for(host, Traffic::Bulk, route).await;
    let mut client = f.client().await?;
    client
        .send_request(Fixture::request("/slow"))
        .await?
        .into_body()
        .collect()
        .await?;
    let mut partial = Fixture::request("/repo.git/git-upload-pack");
    partial
        .headers_mut()
        .insert("range", "bytes=0-4194303".parse()?);
    let response = client.send_request(partial).await?;
    assert_eq!(response.status(), 206);
    assert_eq!(
        response.into_body().collect().await?.to_bytes().len(),
        4 * 1024 * 1024
    );
    assert_eq!(f.state.performance.snapshot().as_array().unwrap().len(), 0);
    let response = client
        .send_request(Fixture::request("/repo.git/git-upload-pack"))
        .await?;
    assert_eq!(response.into_body().collect().await?.to_bytes(), f.data);
    let samples = f.state.performance.snapshot();
    assert_eq!(samples[0]["completed"], 1);
    assert_eq!(samples[0]["bytes"], 5 * 1024 * 1024);
    let pools = f.state.pool.snapshot().await;
    assert!(pools["channels"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["profile"] == "bulk"));
    assert!(pools["channels"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["profile"] == "latency"));
    assert_eq!(f.connections.load(Ordering::Relaxed), 2);
    Ok(())
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
    let start = Instant::now();
    let before = f.requests.load(Ordering::Relaxed);
    let response = client.send_request(Fixture::request("/slow-range")).await?;
    assert_eq!(response.into_body().collect().await?.to_bytes(), f.data);
    assert_eq!(
        f.requests.load(Ordering::Relaxed),
        before + 1,
        "memo avoids failed Range probes"
    );
    assert!(start.elapsed() < Duration::from_secs(2));
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

#[tokio::test]
async fn verified_probe_stream_is_used_without_a_second_handshake() -> Result<()> {
    let f = Fixture::new().await?;
    let host = "raw.githubusercontent.com";
    let route = f.state.routes.lock().await[host].route.clone();
    let socket = f.state.dial(host, &route).await?;
    let stream = f.state.tls_http.connect(host, socket).await?;
    f.state.warm.put(host, route.clone(), true, stream).await;
    let mut client = f.client().await?;
    let response = client.send_request(Fixture::request("/slow")).await?;
    assert_eq!(response.headers()["x-github-turbo-upstream"], "h2");
    response.into_body().collect().await?;
    assert_eq!(f.connections.load(Ordering::Relaxed), 1);
    assert_eq!(f.state.warm.snapshot().await["reused"], 1);
    f.state.pool.clear().await;
    let socket = f.state.dial(host, &route).await?;
    let stream = f.state.tls_http.connect(host, socket).await?;
    f.state.warm.put(host, route, true, stream).await;
    f.state.options.write().unwrap().http2 = false;
    let response = client.send_request(Fixture::request("/slow")).await?;
    assert_eq!(response.headers()["x-github-turbo-upstream"], "http/1.1");
    response.into_body().collect().await?;
    assert_eq!(
        f.state.warm.snapshot().await["reused"],
        1,
        "protocol mismatch must not reuse an h2 probe"
    );
    Ok(())
}

#[tokio::test]
async fn range_pipeline_advances_past_a_straggler_with_bounded_reordering() -> Result<()> {
    for enabled in [false, true] {
        let f = Fixture::new().await?;
        f.state.options.write().unwrap().range_pipeline = enabled;
        let mut client = f.client().await?;
        let response = client.send_request(Fixture::request("/pipeline")).await?;
        assert_eq!(response.into_body().collect().await?.to_bytes(), f.data);
        assert_eq!(
            f.overlap.load(Ordering::SeqCst),
            enabled,
            "moving window should schedule later chunks while the head is stalled"
        );
    }
    Ok(())
}

#[tokio::test]
async fn adaptive_split_keeps_the_faster_stream_and_restores_inspected_bytes() -> Result<()> {
    let f = Fixture::new().await?;
    f.state.options.write().unwrap().adaptive_split = true;
    let mut client = f.client().await?;
    let response = client.send_request(Fixture::request("/large")).await?;
    assert!(!response.headers().contains_key("x-github-turbo-route"));
    assert_eq!(response.into_body().collect().await?.to_bytes(), f.data);
    let response = client
        .send_request(Fixture::request("/single-slow"))
        .await?;
    assert_eq!(response.headers()["x-github-turbo-route"], "direct-split");
    assert_eq!(response.into_body().collect().await?.to_bytes(), f.data);
    Ok(())
}
