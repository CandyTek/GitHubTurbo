use super::*;
use crate::forward::HttpBody;
use hyper::{body::Incoming, Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use tokio::sync::Notify;
enum Sender {
    H1(Mutex<hyper::client::conn::http1::SendRequest<HttpBody>>),
    H2(hyper::client::conn::http2::SendRequest<HttpBody>),
}
struct Channel {
    _capacity: tokio::sync::OwnedSemaphorePermit,
    sender: Sender,
    route: Route,
    active: AtomicU64,
    last: std::sync::Mutex<Instant>,
    closed: Arc<std::sync::atomic::AtomicBool>,
    driver: tokio::task::AbortHandle,
    notify: Arc<Notify>,
}
impl Drop for Channel {
    fn drop(&mut self) {
        self.driver.abort();
    }
}
pub struct Lease(Arc<Channel>);
impl Lease {
    pub fn route(&self) -> &Route {
        &self.0.route
    }
    pub fn protocol(&self) -> &'static str {
        match self.0.sender {
            Sender::H1(_) => "http/1.1",
            Sender::H2(_) => "h2",
        }
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::Relaxed);
        *self.0.last.lock().unwrap() = Instant::now();
        self.0.notify.notify_one();
    }
}
struct HostPool {
    channels: std::sync::Mutex<Vec<Arc<Channel>>>,
    build: Mutex<()>,
    notify: Arc<Notify>,
}
impl Default for HostPool {
    fn default() -> Self {
        Self {
            channels: Default::default(),
            build: Mutex::new(()),
            notify: Arc::new(Notify::new()),
        }
    }
}
pub struct Pool {
    capacity: Arc<Semaphore>,
    hosts: Mutex<HashMap<String, Arc<HostPool>>>,
    pub created: AtomicU64,
    pub reused: AtomicU64,
    pub h2_created: AtomicU64,
}
impl Default for Pool {
    fn default() -> Self {
        Self {
            capacity: Arc::new(Semaphore::new(96)),
            hosts: Default::default(),
            created: AtomicU64::new(0),
            reused: AtomicU64::new(0),
            h2_created: AtomicU64::new(0),
        }
    }
}
#[derive(Clone)]
pub struct IndependentRange;
pub struct Exchange {
    pub response: Response<Incoming>,
    pub lease: Lease,
}
pub fn session_host(host: &str) -> bool {
    host == "github.com"
        || host == "api.github.com"
        || host == "gist.github.com"
        || host.starts_with("uploads.")
}
impl Pool {
    async fn host(&self, host: &str, traffic: Traffic) -> Result<Arc<HostPool>> {
        let key = traffic.key(host);
        let mut hosts = self.hosts.lock().await;
        if hosts.len() >= 64 && !hosts.contains_key(&key) {
            hosts.retain(|_, pool| {
                Arc::strong_count(pool) > 1
                    || pool
                        .channels
                        .lock()
                        .unwrap()
                        .iter()
                        .any(|c| c.active.load(Ordering::Relaxed) > 0)
            });
            if hosts.len() >= 64 {
                return Err(err("upstream host capacity reached"));
            }
        }
        Ok(hosts.entry(key).or_default().clone())
    }
    fn pick(
        pool: &HostPool,
        state: &State,
        host: &str,
        route: Option<&Route>,
        exclude_busy: bool,
    ) -> Option<Lease> {
        let mut channels = pool.channels.lock().unwrap();
        channels.retain(|c| {
            !c.closed.load(Ordering::Relaxed)
                && (c.active.load(Ordering::Relaxed) > 0
                    || (c.last.lock().unwrap().elapsed() < Duration::from_secs(90)
                        && !state.health.bad(host, &c.route)))
        });
        let channel = channels
            .iter()
            .filter(|c| {
                route.is_none_or(|r| r == &c.route)
                    && !state.health.bad(host, &c.route)
                    && !c.closed.load(Ordering::Relaxed)
            })
            .filter(|c| match c.sender {
                Sender::H1(_) => c.active.load(Ordering::Relaxed) == 0,
                Sender::H2(_) => {
                    c.active.load(Ordering::Relaxed) < 64
                        && (!exclude_busy || c.active.load(Ordering::Relaxed) == 0)
                }
            })
            .min_by(|a, b| {
                let score = |c: &Channel| {
                    state.health.score(host, &c.route)
                        * (1. + c.active.load(Ordering::Relaxed) as f64)
                };
                score(a).total_cmp(&score(b))
            })
            .cloned()?;
        channel.active.fetch_add(1, Ordering::Relaxed);
        Some(Lease(channel))
    }
    async fn acquire(
        &self,
        state: &Arc<State>,
        host: &str,
        route: Option<&Route>,
        independent: bool,
        traffic: Traffic,
    ) -> Result<Lease> {
        let pool = self.host(host, traffic).await?;
        // Re-evaluate bulk routes before reusing a connection: a previous slow
        // body must not keep winning merely because its headers arrived quickly.
        let selected_bulk = if traffic == Traffic::Bulk && route.is_none() {
            Some(state.select_for(host, traffic).await?)
        } else {
            None
        };
        let route = route.or(selected_bulk.as_ref());
        loop {
            let notified = pool.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(lease) = Self::pick(&pool, state, host, route, independent) {
                self.reused.fetch_add(1, Ordering::Relaxed);
                return Ok(lease);
            }
            let guard = pool.build.lock().await;
            if let Some(lease) = Self::pick(&pool, state, host, route, independent) {
                self.reused.fetch_add(1, Ordering::Relaxed);
                return Ok(lease);
            }
            let cap = if session_host(host) && traffic == Traffic::Latency {
                1
            } else {
                4
            };
            {
                let mut channels = pool.channels.lock().unwrap();
                if route.is_some() {
                    channels.retain(|c| {
                        c.active.load(Ordering::Relaxed) > 0 || route == Some(&c.route)
                    });
                }
            }
            let capacity = self.capacity.clone().try_acquire_owned().ok();
            let can_build = pool.channels.lock().unwrap().len() < cap && capacity.is_some();
            if can_build {
                let selected = match route {
                    Some(route) => route.clone(),
                    None => state.select_for(host, traffic).await?,
                };
                let start = Instant::now();
                let channel = match self
                    .build(
                        state,
                        host,
                        selected.clone(),
                        pool.notify.clone(),
                        capacity.unwrap(),
                        false,
                    )
                    .await
                {
                    Ok(c) => c,
                    Err(e) => {
                        state.health.fail(host, &selected, health::Failure::Network);
                        return Err(e);
                    }
                };
                state
                    .health
                    .success(host, &selected, start.elapsed().as_secs_f64() * 1000.);
                self.created.fetch_add(1, Ordering::Relaxed);
                if matches!(channel.sender, Sender::H2(_)) {
                    self.h2_created.fetch_add(1, Ordering::Relaxed);
                }
                channel.active.store(1, Ordering::Relaxed);
                let channel = Arc::new(channel);
                pool.channels.lock().unwrap().push(channel.clone());
                return Ok(Lease(channel));
            }
            if let Some(lease) = Self::pick(&pool, state, host, route, false) {
                self.reused.fetch_add(1, Ordering::Relaxed);
                return Ok(lease);
            }
            drop(guard);
            timeout(Duration::from_secs(15), notified)
                .await
                .map_err(|_| err("upstream stream capacity timeout"))?;
        }
    }
    async fn build(
        &self,
        state: &State,
        host: &str,
        route: Route,
        notify: Arc<Notify>,
        capacity: tokio::sync::OwnedSemaphorePermit,
        force_http1: bool,
    ) -> Result<Channel> {
        let h2 = state.optimization().http2 && !force_http1;
        let warm = if state.optimization().probe_reuse {
            state.warm.take(host, &route, h2).await
        } else {
            None
        };
        let stream = if let Some(stream) = warm {
            stream
        } else {
            let socket = state.dial(host, &route).await?;
            timeout(
                Duration::from_millis(state.config.connect_timeout_ms),
                state.connector(&route, h2).connect(host, socket),
            )
            .await??
        };
        let h2 = stream
            .get_ref()
            .negotiated_alpn()?
            .is_some_and(|value| value == b"h2");
        let io = TokioIo::new(tokio::io::BufStream::with_capacity(16384, 16384, stream));
        let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done = closed.clone();
        let done_notify = notify.clone();
        let (sender, driver) = if h2 {
            let mut builder = hyper::client::conn::http2::Builder::new(TokioExecutor::new());
            builder
                .timer(TokioTimer::new())
                .keep_alive_interval(Some(Duration::from_secs(30)))
                .keep_alive_timeout(Duration::from_secs(5))
                .keep_alive_while_idle(false)
                .initial_stream_window_size(1024 * 1024)
                .initial_connection_window_size(4 * 1024 * 1024)
                .max_send_buf_size(256 * 1024);
            let (sender, connection) =
                timeout(Duration::from_secs(5), builder.handshake(io)).await??;
            let task = tokio::spawn(async move {
                let _ = connection.await;
                done.store(true, Ordering::Relaxed);
                done_notify.notify_waiters();
            });
            (Sender::H2(sender), task.abort_handle())
        } else {
            let (sender, connection) = hyper::client::conn::http1::Builder::new()
                .writev(false)
                .title_case_headers(true)
                .handshake(io)
                .await?;
            let task = tokio::spawn(async move {
                let _ = connection.with_upgrades().await;
                done.store(true, Ordering::Relaxed);
                done_notify.notify_waiters();
            });
            (Sender::H1(Mutex::new(sender)), task.abort_handle())
        };
        Ok(Channel {
            _capacity: capacity,
            sender,
            route,
            active: AtomicU64::new(0),
            last: std::sync::Mutex::new(Instant::now()),
            closed,
            driver,
            notify,
        })
    }
    pub async fn send(
        &self,
        state: &Arc<State>,
        host: &str,
        mut request: Request<HttpBody>,
        route: Option<&Route>,
        budget: Option<Duration>,
    ) -> Result<Exchange> {
        let traffic = Traffic::classify(host, request.uri().path());
        let independent = request.extensions().get::<IndependentRange>().is_some();
        // Classic WebSocket upgrades need their own HTTP/1.1 connection. An h2
        // channel cannot carry an HTTP/1.1 Upgrade or become an exclusive tunnel.
        let lease = if request.headers().contains_key("upgrade") {
            let capacity = timeout(
                Duration::from_secs(15),
                self.capacity.clone().acquire_owned(),
            )
            .await??;
            let selected = match route {
                Some(route) => route.clone(),
                None => state.select_for(host, traffic).await?,
            };
            let channel = self
                .build(
                    state,
                    host,
                    selected,
                    Arc::new(Notify::new()),
                    capacity,
                    true,
                )
                .await?;
            channel.active.store(1, Ordering::Relaxed);
            self.created.fetch_add(1, Ordering::Relaxed);
            Lease(Arc::new(channel))
        } else {
            self.acquire(state, host, route, independent, traffic)
                .await?
        };
        let work = async {
            match &lease.0.sender {
                Sender::H1(sender) => {
                    let mut sender = sender.lock().await;
                    sender.ready().await?;
                    sender.send_request(request).await
                }
                Sender::H2(sender) => {
                    let mut sender = sender.clone();
                    let path = request
                        .uri()
                        .path_and_query()
                        .map(|p| p.as_str())
                        .unwrap_or("/");
                    *request.uri_mut() = format!("https://{host}{path}")
                        .parse()
                        .map_err(|_| err("invalid upstream URI"))?;
                    *request.version_mut() = hyper::Version::HTTP_2;
                    sender.ready().await?;
                    sender.send_request(request).await
                }
            }
            .map_err(Into::into)
        };
        let response: Result<_> = if let Some(budget) = budget {
            match timeout(budget, work).await {
                Ok(result) => result,
                Err(_) => Err(err("upstream response header timeout")),
            }
        } else {
            work.await
        };
        match response {
            Ok(response) => Ok(Exchange { response, lease }),
            Err(e) => {
                lease.0.closed.store(true, Ordering::Relaxed);
                state
                    .health
                    .fail(host, lease.route(), health::Failure::Network);
                Err(e)
            }
        }
    }
    pub async fn snapshot(&self) -> serde_json::Value {
        let hosts = self.hosts.lock().await;
        let entries:Vec<_>=hosts.iter().flat_map(|(host,pool)|pool.channels.lock().unwrap().iter().map(|c|serde_json::json!({"host":host.strip_prefix("bulk:").unwrap_or(host),"profile":if host.starts_with("bulk:"){"bulk"}else{"latency"},"route":c.route,"protocol":match c.sender{Sender::H1(_)=>"http/1.1",Sender::H2(_)=>"h2"},"active":c.active.load(Ordering::Relaxed),"idle_seconds":c.last.lock().unwrap().elapsed().as_secs(),"closed":c.closed.load(Ordering::Relaxed)})).collect::<Vec<_>>()).collect();
        serde_json::json!({"created":self.created.load(Ordering::Relaxed),"reused":self.reused.load(Ordering::Relaxed),"h2_created":self.h2_created.load(Ordering::Relaxed),"channels":entries})
    }
    pub async fn clear(&self) {
        self.hosts.lock().await.clear();
    }
    pub async fn prune(&self) {
        let mut hosts = self.hosts.lock().await;
        hosts.retain(|_, pool| {
            pool.channels.lock().unwrap().retain(|c| {
                c.active.load(Ordering::Relaxed) > 0
                    || (!c.closed.load(Ordering::Relaxed)
                        && c.last.lock().unwrap().elapsed() < Duration::from_secs(90))
            });
            Arc::strong_count(pool) > 1 || !pool.channels.lock().unwrap().is_empty()
        });
    }
}
