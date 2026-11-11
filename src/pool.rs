use super::*;
use crate::forward::HttpBody;
use hyper::{body::Incoming, Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use tokio::sync::Notify;
// Web pages pay a TLS handshake for every reconnect, so hold those connections
// through a pause in browsing. Download connections are cheap to replace.
const LATENCY_IDLE: Duration = Duration::from_secs(300);
const BULK_IDLE: Duration = Duration::from_secs(90);
const HOST_CONNECTIONS: usize = 4;
// Speed is decided per connection, and every stream on a connection shares it.
// A page that asks for a hundred scripts over one connection gets one connection's
// speed for all of them. So web requests are spread: a connection takes this many
// at once, and further ones open more connections, up to WEB_CONNECTIONS.
const WEB_CONNECTIONS: usize = 6;
const WEB_SHARE: u64 = 4;
const H2_STREAMS: u64 = 64;
/// How long a request waits for a connection that is being opened before it
/// shares a busy one after all.
const SPREAD_PATIENCE: Duration = Duration::from_secs(2);
/// After an extra connection fails, none is tried for this long.
const GROWTH_PAUSE: Duration = Duration::from_secs(30);
// Extra connections are a convenience. They must leave room for other hosts.
const SPARE_HEADROOM: usize = 32;
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
    idle_limit: Duration,
    closed: Arc<std::sync::atomic::AtomicBool>,
    driver: tokio::task::AbortHandle,
    notify: Arc<Notify>,
}
impl Channel {
    fn expired(&self) -> bool {
        self.last.lock().unwrap().elapsed() >= self.idle_limit
    }
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
    /// Stop offering this connection to new requests. Its current streams finish.
    pub fn retire(&self) {
        self.0.closed.store(true, Ordering::Relaxed);
        self.0.notify.notify_one();
    }
    pub fn is_closed(&self) -> bool {
        self.0.closed.load(Ordering::Relaxed)
    }
    pub async fn request(
        &self,
        host: &str,
        mut request: Request<HttpBody>,
    ) -> Result<Response<Incoming>> {
        match &self.0.sender {
            Sender::H1(sender) => {
                let mut sender = sender.lock().await;
                sender.ready().await?;
                Ok(sender.send_request(request).await?)
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
                Ok(sender.send_request(request).await?)
            }
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
    /// Extra web connections being opened right now.
    building: AtomicU64,
    growth_paused_until: std::sync::Mutex<Option<Instant>>,
}
impl Default for HostPool {
    fn default() -> Self {
        Self {
            channels: Default::default(),
            build: Mutex::new(()),
            notify: Arc::new(Notify::new()),
            building: AtomicU64::new(0),
            growth_paused_until: Default::default(),
        }
    }
}
impl HostPool {
    /// Whether one more web connection may be opened next to those already open.
    /// The first connection is not opened this way: its failure must be reported.
    /// A `spare` is wanted only while no connection is idle or on its way.
    /// With `claim`, a true answer also counts the new connection as being opened.
    fn may_grow(&self, spare: bool, claim: bool) -> bool {
        let channels = self.channels.lock().unwrap();
        if self
            .growth_paused_until
            .lock()
            .unwrap()
            .is_some_and(|until| until > Instant::now())
        {
            return false;
        }
        let open = || {
            channels
                .iter()
                .filter(|c| !c.closed.load(Ordering::Relaxed))
        };
        let building = self.building.load(Ordering::Relaxed);
        let count = open().count();
        if count == 0 || count as u64 + building >= WEB_CONNECTIONS as u64 {
            return false;
        }
        if spare
            && (building > 0
                || open().any(|c| c.active.load(Ordering::Relaxed) == 0 && !c.expired()))
        {
            return false;
        }
        if claim {
            self.building.fetch_add(1, Ordering::Relaxed);
        }
        true
    }
}
/// Held while an extra connection is being opened, also if that is cancelled.
struct Building<'a>(&'a HostPool);
impl Drop for Building<'_> {
    fn drop(&mut self) {
        self.0.building.fetch_sub(1, Ordering::Relaxed);
        self.0.notify.notify_waiters();
    }
}
struct Shape {
    force_http1: bool,
    traffic: Traffic,
}
pub struct Pool {
    capacity: Arc<Semaphore>,
    hosts: Mutex<HashMap<String, Arc<HostPool>>>,
    pub created: AtomicU64,
    pub reused: AtomicU64,
    pub h2_created: AtomicU64,
    /// Whether web requests are spread over more than one connection per host.
    pub spread: std::sync::atomic::AtomicBool,
}
impl Default for Pool {
    fn default() -> Self {
        Self {
            capacity: Arc::new(Semaphore::new(96)),
            hosts: Default::default(),
            created: AtomicU64::new(0),
            reused: AtomicU64::new(0),
            h2_created: AtomicU64::new(0),
            spread: std::sync::atomic::AtomicBool::new(true),
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
        // Requests an HTTP/2 connection may carry, counting this one.
        room: u64,
    ) -> Option<Lease> {
        let mut channels = pool.channels.lock().unwrap();
        channels.retain(|c| {
            !c.closed.load(Ordering::Relaxed)
                && (c.active.load(Ordering::Relaxed) > 0
                    || (!c.expired() && !state.health.bad(host, &c.route)))
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
                Sender::H2(_) => c.active.load(Ordering::Relaxed) < room.min(H2_STREAMS),
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
        // Give each download its own TCP connection while there is room: on one
        // shared connection they would all inherit that connection's speed.
        let exclusive = independent || traffic == Traffic::Bulk;
        let spread = traffic == Traffic::Latency
            && !exclusive
            && route.is_none()
            && self.spread.load(Ordering::Relaxed);
        let room = if exclusive {
            1
        } else if spread {
            WEB_SHARE
        } else {
            H2_STREAMS
        };
        let patience = Instant::now() + SPREAD_PATIENCE;
        // Once the first connection exists, the others are opened side by side
        // instead of one after another under the build lock.
        let mut side_by_side = spread;
        loop {
            let notified = pool.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(lease) = Self::pick(&pool, state, host, route, room) {
                self.reused.fetch_add(1, Ordering::Relaxed);
                return Ok(lease);
            }
            if spread {
                if let Some(channel) = self.grow(state, host, &pool, false).await {
                    return Ok(Lease(channel));
                }
                let left = patience.saturating_duration_since(Instant::now());
                if pool.building.load(Ordering::Relaxed) > 0 && !left.is_zero() {
                    let _ = timeout(left, notified).await;
                    continue;
                }
                if let Some(lease) = Self::pick(&pool, state, host, route, H2_STREAMS) {
                    self.reused.fetch_add(1, Ordering::Relaxed);
                    return Ok(lease);
                }
            }
            let guard = pool.build.lock().await;
            if let Some(lease) = Self::pick(&pool, state, host, route, room) {
                self.reused.fetch_add(1, Ordering::Relaxed);
                return Ok(lease);
            }
            if side_by_side && pool.may_grow(false, false) {
                side_by_side = false;
                continue;
            }
            let cap = if traffic == Traffic::Latency {
                WEB_CONNECTIONS
            } else {
                HOST_CONNECTIONS
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
                        Shape {
                            force_http1: false,
                            traffic,
                        },
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
            if let Some(lease) = Self::pick(&pool, state, host, route, H2_STREAMS) {
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
        shape: Shape,
    ) -> Result<Channel> {
        let Shape {
            force_http1,
            traffic,
        } = shape;
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
                .keep_alive_while_idle(traffic == Traffic::Latency)
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
            idle_limit: if traffic == Traffic::Latency {
                LATENCY_IDLE
            } else {
                BULK_IDLE
            },
            closed,
            driver,
            notify,
        })
    }
    /// Open one more web connection to `host` in the background when all of them
    /// are in use, so the next request finds an idle one and does not have to wait
    /// for a handshake. The request that triggers this does not wait for it.
    fn reserve(state: &Arc<State>, host: &str, pool: &Arc<HostPool>) {
        if !state.pool.spread.load(Ordering::Relaxed) || !pool.may_grow(true, false) {
            return;
        }
        let (state, host, pool) = (state.clone(), host.to_owned(), pool.clone());
        tokio::spawn(async move {
            state.pool.grow(&state, &host, &pool, true).await;
        });
    }
    /// Open one more web connection next to those already open. A `spare` is left
    /// idle in the pool. Otherwise it is returned already counted as carrying the
    /// caller's request. None when no more are allowed or the attempt failed.
    async fn grow(
        &self,
        state: &Arc<State>,
        host: &str,
        pool: &HostPool,
        spare: bool,
    ) -> Option<Arc<Channel>> {
        if !self.spread.load(Ordering::Relaxed)
            || self.capacity.available_permits() <= SPARE_HEADROOM
            || !pool.may_grow(spare, true)
        {
            return None;
        }
        let _building = Building(pool);
        let capacity = self.capacity.clone().try_acquire_owned().ok()?;
        let built = async {
            let route = state.select_for(host, Traffic::Latency).await?;
            let shape = Shape {
                force_http1: false,
                traffic: Traffic::Latency,
            };
            self.build(state, host, route, pool.notify.clone(), capacity, shape)
                .await
        }
        .await;
        match built {
            Ok(channel) => {
                self.created.fetch_add(1, Ordering::Relaxed);
                if matches!(channel.sender, Sender::H2(_)) {
                    self.h2_created.fetch_add(1, Ordering::Relaxed);
                }
                channel.active.store(u64::from(!spare), Ordering::Relaxed);
                let channel = Arc::new(channel);
                pool.channels.lock().unwrap().push(channel.clone());
                Some(channel)
            }
            // Not held against the route: the connections already open show that
            // it works. The requests share those instead.
            Err(_) => {
                *pool.growth_paused_until.lock().unwrap() = Some(Instant::now() + GROWTH_PAUSE);
                None
            }
        }
    }
    /// A new connection owned by the caller, outside the shared pools. Dropping
    /// the lease closes it, so a slow connection can never be handed out again.
    pub async fn flow(&self, state: &State, host: &str, route: &Route) -> Result<Lease> {
        let capacity = timeout(
            Duration::from_secs(3),
            self.capacity.clone().acquire_owned(),
        )
        .await??;
        let channel = self
            .build(
                state,
                host,
                route.clone(),
                Arc::new(Notify::new()),
                capacity,
                Shape {
                    force_http1: false,
                    traffic: Traffic::Bulk,
                },
            )
            .await?;
        channel.active.store(1, Ordering::Relaxed);
        self.created.fetch_add(1, Ordering::Relaxed);
        if matches!(channel.sender, Sender::H2(_)) {
            self.h2_created.fetch_add(1, Ordering::Relaxed);
        }
        Ok(Lease(Arc::new(channel)))
    }
    /// Have a connection ready for web traffic to `host`. Returns whether a new one
    /// was opened. An existing one is left alone so its idle time keeps counting.
    pub async fn prewarm(&self, state: &Arc<State>, host: &str) -> Result<bool> {
        let ready = {
            let hosts = self.hosts.lock().await;
            hosts.get(&Traffic::Latency.key(host)).is_some_and(|pool| {
                pool.channels
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|c| !c.closed.load(Ordering::Relaxed) && !c.expired())
            })
        };
        if ready {
            return Ok(false);
        }
        self.acquire(state, host, None, false, Traffic::Latency)
            .await?;
        Ok(true)
    }
    pub async fn send(
        &self,
        state: &Arc<State>,
        host: &str,
        request: Request<HttpBody>,
        route: Option<&Route>,
        budget: Option<Duration>,
    ) -> Result<Exchange> {
        let traffic = Traffic::classify(host, request.uri().path());
        if traffic == Traffic::Latency {
            state.prewarm.touch(host);
        }
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
                    Shape {
                        force_http1: true,
                        traffic,
                    },
                )
                .await?;
            channel.active.store(1, Ordering::Relaxed);
            self.created.fetch_add(1, Ordering::Relaxed);
            Lease(Arc::new(channel))
        } else {
            let lease = self
                .acquire(state, host, route, independent, traffic)
                .await?;
            if traffic == Traffic::Latency && route.is_none() {
                if let Ok(pool) = self.host(host, traffic).await {
                    Self::reserve(state, host, &pool);
                }
            }
            lease
        };
        let work = lease.request(host, request);
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
                    || (!c.closed.load(Ordering::Relaxed) && !c.expired())
            });
            Arc::strong_count(pool) > 1 || !pool.channels.lock().unwrap().is_empty()
        });
    }
}
