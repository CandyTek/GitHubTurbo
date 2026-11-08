use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    io,
    net::{IpAddr, SocketAddr},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Mutex, Semaphore},
    task::JoinSet,
    time::timeout,
};
use tokio_native_tls::{native_tls, TlsConnector};
mod content_cache;
mod control;
mod forward;
mod health;
mod pool;
mod resolver;
mod seeds;
mod segmented;
mod transparent;
#[cfg(test)]
mod transport_tests;
#[cfg(windows)]
mod tray;
mod watchdog;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
type ReleaseAssetCache = HashMap<String, (Instant, HashMap<String, u64>)>;
const DOMAINS: &[&str] = &[
    "github.com",
    "githubusercontent.com",
    "githubassets.com",
    "github.io",
    "githubapp.com",
    "github-com.s3.amazonaws.com",
    "github-cloud.s3.amazonaws.com",
    "github-production-release-asset-2e65be.s3.amazonaws.com",
    "github-production-repository-file-5c1aeb.s3.amazonaws.com",
    "github-production-user-asset-6210df.s3.amazonaws.com",
];
const DIAG_HOSTS: &[&str] = &[
    "github.com",
    "api.github.com",
    "raw.githubusercontent.com",
    "codeload.github.com",
    "release-assets.githubusercontent.com",
    "avatars.githubusercontent.com",
];

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    listen: SocketAddr,
    upstream: Option<SocketAddr>,
    mode: Mode,
    #[serde(default)]
    host_modes: HashMap<String, Mode>,
    connect_timeout_ms: u64,
    probe_timeout_ms: u64,
    cache_seconds: u64,
    max_connections: usize,
    max_routes: usize,
    idle_timeout_seconds: u64,
    ip_overrides: HashMap<String, Vec<IpAddr>>,
    #[serde(default)]
    transparent: transparent::Config,
    #[serde(default)]
    optimization: Optimization,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct Optimization {
    http2: bool,
    public_cache: bool,
    segmented_downloads: bool,
    seed_routes: bool,
}
impl Default for Optimization {
    fn default() -> Self {
        Self {
            http2: true,
            public_cache: true,
            segmented_downloads: true,
            seed_routes: true,
        }
    }
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Mode {
    Auto,
    Direct,
    Proxy,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(tag = "kind", content = "address", rename_all = "lowercase")]
enum Route {
    Direct(SocketAddr),
    DirectNoSni(SocketAddr),
    Proxy,
}
#[derive(Clone)]
struct Cached {
    route: Route,
    at: Instant,
}
#[derive(Default)]
struct Counters {
    accepted: AtomicU64,
    completed: AtomicU64,
    failures: AtomicU64,
    uploaded: AtomicU64,
    downloaded: AtomicU64,
}
struct State {
    config: Config,
    tls: TlsConnector,
    tls_no_sni: TlsConnector,
    tls_http: TlsConnector,
    tls_http_no_sni: TlsConnector,
    resolver: resolver::Resolver,
    routes: Mutex<HashMap<String, Cached>>,
    // Serialize cold probes for each bounded host entry, rather than all traffic.
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    probes: Semaphore,
    counters: Counters,
    release_assets: Mutex<ReleaseAssetCache>,
    health: health::Health,
    pool: pool::Pool,
    content_cache: Arc<content_cache::Cache>,
    split_slots: Arc<Semaphore>,
    started: Instant,
    options: std::sync::RwLock<Optimization>,
    config_file: std::sync::Mutex<Option<std::path::PathBuf>>,
    option_update: Mutex<()>,
    route_file: Mutex<()>,
}

fn err(message: &str) -> Box<dyn std::error::Error + Send + Sync> {
    io::Error::other(message).into()
}
fn allowed(host: &str) -> bool {
    DOMAINS.iter().any(|d| {
        host == *d
            || host
                .strip_suffix(d)
                .is_some_and(|prefix| prefix.ends_with('.'))
    })
}
fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            !v.is_private()
                && !v.is_loopback()
                && !v.is_link_local()
                && !v.is_broadcast()
                && !v.is_documentation()
                && !v.is_unspecified()
                && !v.is_multicast()
                && v.octets()[0] != 0
                && v.octets()[0] < 240
                && !(v.octets()[0] == 100 && (64..=127).contains(&v.octets()[1]))
                && !(v.octets()[0] == 198 && (18..=19).contains(&v.octets()[1]))
        }
        IpAddr::V6(v) => {
            let s = v.segments();
            (s[0] & 0xe000) == 0x2000 && !(s[0] == 0x2001 && (s[1] == 0x0db8 || s[1] == 0))
        }
    }
}
fn target(authority: &str) -> Result<String> {
    let (host, port) = authority
        .rsplit_once(':')
        .ok_or_else(|| err("expected host:443"))?;
    let host = host.to_ascii_lowercase();
    if port != "443"
        || host.len() > 253
        || !host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
        || !allowed(&host)
    {
        return Err(err("only GitHub HTTPS destinations are allowed"));
    }
    Ok(host)
}
impl Config {
    fn mode_for(&self, host: &str) -> Mode {
        self.host_modes.get(host).copied().unwrap_or(self.mode)
    }
    fn validate(&self) -> Result<()> {
        if !self.listen.ip().is_loopback()
            || self.listen.port() == 0
            || self.upstream.is_some_and(|u| {
                !u.ip().is_loopback() || u.port() == 0 || self.listen.port() == u.port()
            })
        {
            return Err(err(
                "use distinct nonzero loopback ports for listen and upstream",
            ));
        }
        self.transparent.validate(self.listen)?;
        if self.upstream.is_none()
            && (self.mode == Mode::Proxy || self.host_modes.values().any(|m| *m == Mode::Proxy))
        {
            return Err(err("proxy mode requires an upstream"));
        }
        if !(1..=1024).contains(&self.max_connections)
            || !(1..=1024).contains(&self.max_routes)
            || !(100..=30000).contains(&self.connect_timeout_ms)
            || !(100..=30000).contains(&self.probe_timeout_ms)
            || !(1..=3600).contains(&self.cache_seconds)
            || !(1..=86400).contains(&self.idle_timeout_seconds)
        {
            return Err(err("configuration limits out of range"));
        }
        for host in self.ip_overrides.keys().chain(self.host_modes.keys()) {
            if target(&format!("{host}:443"))? != *host {
                return Err(err("configured hostnames must be lowercase"));
            }
        }
        for (host, ips) in &self.ip_overrides {
            target(&format!("{host}:443"))?;
            if ips.is_empty() || ips.len() > 8 || ips.iter().any(|ip| !public_ip(*ip)) {
                return Err(err("IP overrides must contain 1-8 public IPs"));
            }
        }
        Ok(())
    }
}
impl State {
    fn optimization(&self) -> Optimization {
        self.options.read().unwrap().clone()
    }
    fn new(config: Config) -> Result<Arc<Self>> {
        config.validate()?;
        let options = std::sync::RwLock::new(config.optimization.clone());
        Ok(Arc::new(Self {
            config,
            options,
            config_file: std::sync::Mutex::new(None),
            option_update: Mutex::new(()),
            route_file: Mutex::new(()),
            tls: native_tls::TlsConnector::builder()
                .request_alpns(&["http/1.1"])
                .build()?
                .into(),
            tls_no_sni: native_tls::TlsConnector::builder()
                .use_sni(false)
                .request_alpns(&["http/1.1"])
                .build()?
                .into(),
            resolver: resolver::Resolver::new()?,
            tls_http: native_tls::TlsConnector::builder()
                .request_alpns(&["h2", "http/1.1"])
                .build()?
                .into(),
            tls_http_no_sni: native_tls::TlsConnector::builder()
                .use_sni(false)
                .request_alpns(&["h2", "http/1.1"])
                .build()?
                .into(),
            routes: Mutex::new(HashMap::new()),
            locks: Mutex::new(HashMap::new()),
            probes: Semaphore::new(4),
            counters: Counters::default(),
            release_assets: Mutex::new(HashMap::new()),
            health: health::Health::default(),
            pool: pool::Pool::default(),
            content_cache: Arc::new(content_cache::Cache::default()),
            split_slots: Arc::new(Semaphore::new(2)),
            started: Instant::now(),
        }))
    }
    async fn addresses(&self, host: &str) -> Result<Vec<SocketAddr>> {
        let mut addresses: Vec<_> = if let Some(ips) = self.config.ip_overrides.get(host) {
            ips.iter().map(|ip| SocketAddr::new(*ip, 443)).collect()
        } else {
            self.resolver
                .resolve(host)
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|ip| SocketAddr::new(ip, 443))
                .collect()
        };
        if self.optimization().seed_routes && !self.config.ip_overrides.contains_key(host) {
            for address in seeds::for_host(host) {
                if !addresses.contains(&address) {
                    addresses.push(address);
                }
            }
        }
        addresses.retain(|a| public_ip(a.ip()));
        addresses.truncate(32);
        if addresses.is_empty() {
            return Err(err("no public address resolved"));
        }
        Ok(addresses)
    }
    async fn dial(&self, host: &str, route: &Route) -> Result<TcpStream> {
        timeout(
            Duration::from_millis(self.config.connect_timeout_ms),
            async {
                let mut stream = match route {
                    Route::Direct(address) | Route::DirectNoSni(address) => {
                        TcpStream::connect(address).await?
                    }
                    Route::Proxy => {
                        TcpStream::connect(
                            self.config
                                .upstream
                                .ok_or_else(|| err("no upstream configured"))?,
                        )
                        .await?
                    }
                };
                stream.set_nodelay(true)?;
                if matches!(route, Route::Proxy) {
                    stream
                        .write_all(
                            format!("CONNECT {host}:443 HTTP/1.1\r\nHost: {host}:443\r\n\r\n")
                                .as_bytes(),
                        )
                        .await?;
                    let header = read_header(&mut stream).await?;
                    let line = std::str::from_utf8(&header)?.lines().next().unwrap_or("");
                    let mut parts = line.split_ascii_whitespace();
                    if !matches!(parts.next(), Some("HTTP/1.0" | "HTTP/1.1"))
                        || parts.next() != Some("200")
                    {
                        return Err(err("upstream CONNECT rejected"));
                    }
                }
                Ok(stream)
            },
        )
        .await?
    }
    async fn probe(&self, host: &str, route: &Route) -> Result<u128> {
        let start = Instant::now();
        timeout(Duration::from_millis(self.config.probe_timeout_ms), async {
            let stream = self.dial(host, route).await?;
            let connector = if matches!(route,Route::DirectNoSni(_)) { &self.tls_no_sni } else { &self.tls };
            let mut tls = connector.connect(host, stream).await?;
            tls.write_all(format!("HEAD / HTTP/1.1\r\nHost: {host}\r\nUser-Agent: GitHubTurbo/0.1\r\nConnection: close\r\n\r\n").as_bytes()).await?;
            let header = read_header(&mut tls).await?;
            let code = std::str::from_utf8(&header)?.split_ascii_whitespace().nth(1).unwrap_or("").parse::<u16>()?;
            // 4xx on a CDN root still proves HTTPS reachability, not file availability.
            if !(200..500).contains(&code) { return Err(err("probe received an unhealthy HTTP status")); }
            Ok(start.elapsed().as_millis())
        }).await?
    }
    async fn best_direct(self: &Arc<Self>, host: &str) -> Result<Route> {
        if self.config.ip_overrides.contains_key(host) || !self.optimization().seed_routes {
            return self
                .probe_addresses(host, self.addresses(host).await?)
                .await;
        }
        let mut seed = seeds::for_host(host);
        seed.truncate(8);
        let from_seed = self.probe_addresses(host, seed);
        let from_dns = async {
            let ips = self.resolver.resolve(host).await?;
            self.probe_addresses(
                host,
                ips.into_iter().map(|ip| SocketAddr::new(ip, 443)).collect(),
            )
            .await
        };
        tokio::pin!(from_seed, from_dns);
        tokio::select! {result=&mut from_seed=>match result{Ok(route)=>Ok(route),Err(_)=>from_dns.await},result=&mut from_dns=>match result{Ok(route)=>Ok(route),Err(_)=>from_seed.await}}
    }
    async fn probe_addresses(
        self: &Arc<Self>,
        host: &str,
        addresses: Vec<SocketAddr>,
    ) -> Result<Route> {
        let mut jobs = JoinSet::new();
        for address in addresses.into_iter().take(8) {
            for no_sni in [false, true] {
                if no_sni
                    && (!self.config.transparent.enabled
                        || !self.config.transparent.try_without_sni)
                {
                    continue;
                }
                let route = if no_sni {
                    Route::DirectNoSni(address)
                } else {
                    Route::Direct(address)
                };
                if self.health.bad(host, &route) {
                    continue;
                }
                let state = self.clone();
                let host = host.to_owned();
                jobs.spawn(async move {
                    if no_sni {
                        tokio::time::sleep(Duration::from_millis(250)).await;
                    }
                    match state.probe_candidate(&host, &route).await {
                        Ok(()) => Ok(route),
                        Err(error) => {
                            state.health.fail(&host, &route, health::Failure::Network);
                            Err(error)
                        }
                    }
                });
            }
        }
        while let Some(result) = jobs.join_next().await {
            if let Ok(Ok(route)) = result {
                return Ok(route);
            }
        }
        Err(err("all direct TLS/HTTP probes failed"))
    }
    async fn probe_candidate(&self, host: &str, route: &Route) -> Result<()> {
        if !self.config.transparent.enabled {
            return self.probe(host, route).await.map(|_| ());
        }
        // A failing homepage must not disqualify otherwise usable repository/API paths.
        // System mode evaluates the actual HTTP response in the forwarder.
        timeout(Duration::from_millis(self.config.probe_timeout_ms), async {
            let socket = self.dial(host, route).await?;
            let connector = if matches!(route, Route::DirectNoSni(_)) {
                &self.tls_no_sni
            } else {
                &self.tls
            };
            connector.connect(host, socket).await?;
            Ok(())
        })
        .await?
    }
    async fn select(self: &Arc<Self>, host: &str) -> Result<Route> {
        if self.config.mode_for(host) == Mode::Proxy {
            return Ok(Route::Proxy);
        }
        if let Some(cached) = self.routes.lock().await.get(host) {
            if cached.at.elapsed().as_secs() < self.config.cache_seconds
                && !self.health.bad(host, &cached.route)
            {
                return Ok(cached.route.clone());
            }
        }
        let lock = {
            let mut locks = self.locks.lock().await;
            if !locks.contains_key(host) && locks.len() >= self.config.max_routes {
                locks.retain(|_, lock| Arc::strong_count(lock) > 1);
            }
            if !locks.contains_key(host) && locks.len() >= self.config.max_routes {
                return Err(err("route probe capacity reached"));
            }
            locks
                .entry(host.to_owned())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let _guard = lock.lock().await;
        if let Some(cached) = self.routes.lock().await.get(host) {
            if cached.at.elapsed().as_secs() < self.config.cache_seconds
                && !self.health.bad(host, &cached.route)
            {
                return Ok(cached.route.clone());
            }
        }
        let _permit = self.probes.acquire().await?;
        let route = if self.config.mode_for(host) == Mode::Direct || self.config.upstream.is_none()
        {
            self.best_direct(host).await?
        } else {
            let direct = self.best_direct(host);
            let proxy = self.probe(host, &Route::Proxy);
            tokio::pin!(direct, proxy);
            tokio::select! {
                result = &mut direct => match result { Ok(route) => route, Err(_) => { proxy.await?; Route::Proxy } },
                result = &mut proxy => match result { Ok(_) => Route::Proxy, Err(_) => direct.await? },
            }
        };
        self.cache_route(host, route.clone()).await;
        eprintln!("route {host}: {route:?}");
        Ok(route)
    }
    async fn cache_route(&self, host: &str, route: Route) {
        let mut cache = self.routes.lock().await;
        if !cache.contains_key(host) && cache.len() >= self.config.max_routes {
            if let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, item)| item.at)
                .map(|(key, _)| key.clone())
            {
                cache.remove(&oldest);
            }
        }
        cache.insert(
            host.to_owned(),
            Cached {
                route,
                at: Instant::now(),
            },
        );
    }
    async fn alternatives(&self, host: &str, current: Option<&Route>) -> Vec<Route> {
        if self.config.mode_for(host) == Mode::Proxy {
            return vec![Route::Proxy];
        }
        let addresses = self.addresses(host).await.unwrap_or_default();
        let mut routes: Vec<_> = addresses.iter().copied().map(Route::Direct).collect();
        if self.config.transparent.enabled && self.config.transparent.try_without_sni {
            routes.extend(addresses.into_iter().map(Route::DirectNoSni));
        }
        routes.retain(|route| Some(route) != current && !self.health.bad(host, route));
        routes.sort_by(|a, b| {
            self.health
                .score(host, a)
                .total_cmp(&self.health.score(host, b))
        });
        routes.truncate(4);
        if self.config.upstream.is_some() && self.config.mode_for(host) == Mode::Auto {
            routes.truncate(3);
            routes.push(Route::Proxy);
        }
        routes
    }
    async fn persist_routes(&self) -> Result<()> {
        if !self.config.transparent.enabled {
            return Ok(());
        }
        let _write = self.route_file.lock().await;
        let routes = self.routes.lock().await;
        let entries: Vec<_> = routes
            .iter()
            .filter(|(host, entry)| {
                !matches!(entry.route, Route::Proxy) && !self.health.bad(host, &entry.route)
            })
            .map(|(host, entry)| serde_json::json!({"host":host,"route":entry.route}))
            .collect();
        drop(routes);
        let dir = self.config.transparent.state_dir.clone();
        let data = serde_json::to_vec(
            &serde_json::json!({"saved":time::OffsetDateTime::now_utc().unix_timestamp(),"routes":entries}),
        )?;
        tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&dir)?;
            let temp = dir.join("routes-good.tmp");
            std::fs::write(&temp, data)?;
            std::fs::rename(temp, dir.join("routes-good.json"))
        })
        .await??;
        Ok(())
    }
    async fn restore_routes(&self) {
        let file = self.config.transparent.state_dir.join("routes-good.json");
        let Ok(bytes) = std::fs::read(file) else {
            return;
        };
        if bytes.len() > 128 * 1024 {
            return;
        }
        let Ok(doc) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            return;
        };
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let age = now.saturating_sub(doc["saved"].as_i64().unwrap_or(0));
        if !(0..=86400).contains(&age) {
            return;
        }
        if let Some(entries) = doc["routes"].as_array() {
            for entry in entries.iter().take(self.config.max_routes) {
                let Some(host) = entry["host"].as_str() else {
                    continue;
                };
                if target(&format!("{host}:443")).is_err() {
                    continue;
                }
                let Ok(route) = serde_json::from_value::<Route>(entry["route"].clone()) else {
                    continue;
                };
                if matches!(&route,Route::Direct(address)|Route::DirectNoSni(address) if public_ip(address.ip())&&address.port()==443)
                {
                    self.cache_route(host, route).await;
                }
            }
        }
    }
    async fn connect(self: &Arc<Self>, host: &str) -> Result<TcpStream> {
        let route = self.select(host).await?;
        match self.dial(host, &route).await {
            Ok(stream) => Ok(stream),
            Err(first) => {
                self.routes.lock().await.remove(host);
                if self.config.mode_for(host) != Mode::Auto {
                    return Err(first);
                }
                let fallback = match route {
                    Route::Direct(_) | Route::DirectNoSni(_) => {
                        if self.config.upstream.is_none() {
                            return Err(first);
                        }
                        self.probe(host, &Route::Proxy).await?;
                        Route::Proxy
                    }
                    Route::Proxy => self.best_direct(host).await?,
                };
                eprintln!("fallback {host}: {fallback:?}");
                let stream = self.dial(host, &fallback).await?;
                self.cache_route(host, fallback).await;
                Ok(stream)
            }
        }
    }
}

// Read exactly through CRLFCRLF; never swallow pipelined TLS bytes.
async fn read_header<S: AsyncRead + Unpin>(stream: &mut S) -> Result<Vec<u8>> {
    let mut header = Vec::with_capacity(1024);
    while header.len() < 16384 {
        header.push(stream.read_u8().await?);
        if header.ends_with(b"\r\n\r\n") {
            return Ok(header);
        }
    }
    Err(err("HTTP headers exceed 16 KiB"))
}
async fn respond(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &str,
) -> Result<()> {
    stream.write_all(format!("HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nX-Frame-Options: DENY\r\nContent-Security-Policy: default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; connect-src 'self'; frame-ancestors 'none'\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\r\n{body}", body.len()).as_bytes()).await?;
    Ok(())
}
fn pac(listen: SocketAddr) -> String {
    format!("function FindProxyForURL(url, host) {{\n  host = host.toLowerCase();\n  var domains = {};\n  for (var i = 0; i < domains.length; i++) {{\n    if (host === domains[i] || dnsDomainIs(host, '.' + domains[i])) return 'PROXY {}';\n  }}\n  return 'DIRECT';\n}}\n", serde_json::to_string(DOMAINS).unwrap(), listen)
}
// A shared activity clock makes this an idle deadline, not a download duration limit.
async fn relay<A, B>(client: A, remote: B, idle: Duration, counters: &Counters) -> Result<()>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    let (mut cr, mut cw) = tokio::io::split(client);
    let (mut rr, mut rw) = tokio::io::split(remote);
    let start = Instant::now();
    let last = AtomicU64::new(0);
    async fn pump<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
        reader: &mut R,
        writer: &mut W,
        start: Instant,
        last: &AtomicU64,
        bytes: &AtomicU64,
    ) -> io::Result<()> {
        let mut buffer = [0u8; 16384];
        loop {
            let n = reader.read(&mut buffer).await?;
            if n == 0 {
                writer.shutdown().await?;
                return Ok(());
            }
            writer.write_all(&buffer[..n]).await?;
            // Upgraded HTTP connections can retain a buffered TLS writer.
            // Flush interactive frames before waiting for the peer's reply.
            writer.flush().await?;
            bytes.fetch_add(n as u64, Ordering::Relaxed);
            last.store(start.elapsed().as_millis() as u64, Ordering::Relaxed);
        }
    }
    let copy = async {
        tokio::try_join!(
            pump(&mut cr, &mut rw, start, &last, &counters.uploaded),
            pump(&mut rr, &mut cw, start, &last, &counters.downloaded)
        )?;
        Ok::<_, io::Error>(())
    };
    let watchdog = async {
        loop {
            tokio::time::sleep(idle).await;
            if (start.elapsed().as_millis() as u64).saturating_sub(last.load(Ordering::Relaxed))
                >= idle.as_millis() as u64
            {
                break;
            }
        }
    };
    tokio::select! { result = copy => { result?; Ok(()) }, _ = watchdog => Err(err("tunnel idle timeout")) }
}
async fn handle(mut client: TcpStream, state: Arc<State>) -> Result<()> {
    let header = timeout(Duration::from_secs(10), read_header(&mut client)).await??;
    let text = std::str::from_utf8(&header)?;
    let words: Vec<_> = text
        .lines()
        .next()
        .unwrap_or("")
        .split_ascii_whitespace()
        .collect();
    if words.len() != 3 || !matches!(words[2], "HTTP/1.0" | "HTTP/1.1") {
        respond(
            &mut client,
            "400 Bad Request",
            "text/plain",
            "Invalid request\n",
        )
        .await?;
        return Ok(());
    }
    let headers: HashMap<String, String> = text
        .lines()
        .skip(1)
        .filter_map(|line| {
            line.split_once(':')
                .map(|(key, value)| (key.to_ascii_lowercase(), value.trim().to_owned()))
        })
        .collect();
    if words[0] != "CONNECT"
        && !control::local_host(
            headers.get("host").map(String::as_str).unwrap_or(""),
            state.config.listen,
        )
    {
        return respond(
            &mut client,
            "403 Forbidden",
            "text/plain",
            "Local management Host required\n",
        )
        .await;
    }
    if words[0] == "GET" && words[1] == "/ca.crl" && state.config.transparent.enabled {
        let crl = std::fs::read(state.config.transparent.state_dir.join("ca.crl"))?;
        client.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/pkix-crl\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",crl.len()).as_bytes()).await?;
        client.write_all(&crl).await?;
        return Ok(());
    }
    if words[0] == "GET" && words[1] == "/proxy.pac" {
        return respond(
            &mut client,
            "200 OK",
            "application/x-ns-proxy-autoconfig",
            &pac(state.config.listen),
        )
        .await;
    }
    if words[0] == "GET" && words[1] == "/" {
        return respond(
            &mut client,
            "200 OK",
            "text/html; charset=utf-8",
            include_str!("dashboard.html"),
        )
        .await;
    }
    if words[0] == "GET" && matches!(words[1], "/status" | "/api/status" | "/api/diagnostics") {
        let body = serde_json::to_string_pretty(&control::status(&state).await)?;
        return respond(&mut client, "200 OK", "application/json", &body).await;
    }
    if words[0] == "POST" && words[1].starts_with("/api/") {
        if !control::write_allowed(&headers, state.config.listen) {
            return respond(
                &mut client,
                "403 Forbidden",
                "text/plain",
                "Same-origin management header required\n",
            )
            .await;
        }
        if headers.get("content-length").is_some_and(|v| v != "0")
            || headers.contains_key("transfer-encoding")
        {
            return respond(
                &mut client,
                "400 Bad Request",
                "text/plain",
                "Bodyless operation required\n",
            )
            .await;
        }
        return control::write(words[1], &mut client, &state).await;
    }
    if words[0] != "CONNECT" {
        return respond(
            &mut client,
            "405 Method Not Allowed",
            "text/plain",
            "Use HTTPS CONNECT or GET /proxy.pac\n",
        )
        .await;
    }
    let host = match target(words[1]) {
        Ok(host) => host,
        Err(_) => {
            return respond(
                &mut client,
                "403 Forbidden",
                "text/plain",
                "Only GitHub HTTPS destinations are allowed\n",
            )
            .await
        }
    };
    if state.config.transparent.enabled {
        let accept = transparent::acceptor(&state.config.transparent)?;
        client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await?;
        return transparent::bridge(client, state, accept).await;
    }
    let remote = match state.connect(&host).await {
        Ok(remote) => remote,
        Err(error) => {
            respond(
                &mut client,
                "502 Bad Gateway",
                "text/plain",
                "No available route\n",
            )
            .await?;
            return Err(error);
        }
    };
    client
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await?;
    // After 200, never retry/replay opaque TLS or an authenticated Git request.
    let result = relay(
        client,
        remote,
        Duration::from_secs(state.config.idle_timeout_seconds),
        &state.counters,
    )
    .await;
    if result.is_err() {
        state.routes.lock().await.remove(&host);
    }
    result
}
async fn diagnose(state: Arc<State>) -> Result<()> {
    let mut rows = Vec::new();
    for host in DIAG_HOSTS {
        let direct = async {
            let start = Instant::now();
            let route = state.best_direct(host).await?;
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>((route, start.elapsed().as_millis()))
        };
        let (d, p) = tokio::join!(direct, state.probe(host, &Route::Proxy));
        rows.push(serde_json::json!({"host": host, "direct": match d { Ok((route, ms)) => serde_json::json!({"route": route, "ms": ms}), Err(e) => serde_json::json!({"error": e.to_string()}) }, "proxy": match p { Ok(ms) => serde_json::json!({"ms": ms}), Err(e) => serde_json::json!({"error": e.to_string()}) }}));
    }
    println!("{}", serde_json::to_string_pretty(&rows)?);
    Ok(())
}
#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|s| s == "--help" || s == "-h") {
        println!("GitHubTurbo 0.3\nUsage: github-turbo [enable|disable|recover|serve|diagnose|pac|init-ca] [config.json]\nDefault: enable system integration. Config: beside executable, otherwise ./config.json");
        return Ok(());
    }
    let command = args.first().map(String::as_str).unwrap_or("enable");
    if command == "watch" {
        return watchdog::run(
            args.get(1)
                .ok_or_else(|| err("watch requires a process ID"))?
                .parse()?,
        );
    }
    if matches!(command, "enable" | "disable" | "recover") {
        let script = std::env::current_exe()?.with_file_name("windows.ps1");
        let status = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(script)
            .args(["-Action", command])
            .status()?;
        if !status.success() {
            return Err(err(
                "Windows integration failed; see state/session.json and use Recover.cmd",
            ));
        }
        return Ok(());
    }
    if !matches!(
        command,
        "serve" | "diagnose" | "pac" | "init-ca" | "ssh-tunnel"
    ) || args.len() > 2
    {
        return Err(err("unknown command; use --help"));
    }
    let sibling = std::env::current_exe()?.with_file_name("config.json");
    let path = args
        .get(1)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            if sibling.exists() {
                sibling
            } else {
                "config.json".into()
            }
        });
    let mut config: Config = serde_json::from_slice(&std::fs::read(&path)?)?;
    if config.transparent.state_dir.is_relative() {
        config.transparent.state_dir = path
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .join(&config.transparent.state_dir);
    }
    if command == "init-ca" {
        return transparent::init_ca(&config.transparent, config.listen);
    }
    let state = State::new(config)?;
    *state.config_file.lock().unwrap() = Some(path.clone());
    if command == "ssh-tunnel" {
        let result = transparent::ssh_stdio(state).await;
        if let Err(error) = result {
            eprintln!("SSH: {error}");
            std::process::exit(1);
        }
        std::process::exit(0);
    }
    if command == "diagnose" {
        return diagnose(state).await;
    }
    if command == "pac" {
        println!("{}", pac(state.config.listen));
        return Ok(());
    }
    state.restore_routes().await;
    let maintenance = state.clone();
    let maintenance = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            maintenance.pool.prune().await;
            let _ = maintenance.persist_routes().await;
        }
    });
    let listener = TcpListener::bind(state.config.listen).await?;
    let mut transparent_task = if state.config.transparent.enabled {
        let listeners = transparent::Listeners::bind(&state.config.transparent).await?;
        let state = state.clone();
        Some(tokio::spawn(async move { listeners.run(state).await }))
    } else {
        None
    };
    eprintln!(
        "GitHubTurbo listening on http://{} (optional upstream {:?})",
        state.config.listen, state.config.upstream
    );
    let slots = Arc::new(Semaphore::new(state.config.max_connections));
    let mut tasks = JoinSet::new();
    loop {
        tokio::select! {
            result=async {match transparent_task.as_mut(){Some(task)=>Some(task.await),None=>std::future::pending().await}}=>{return Err(err(&format!("transparent listener stopped: {result:?}")));},
            _ = tokio::signal::ctrl_c() => break,
            Some(_) = tasks.join_next(), if !tasks.is_empty() => {},
            incoming = listener.accept() => {
                let (stream, _) = incoming?;
                // Reject overflow immediately; no unbounded waiting task queue.
                let permit = match slots.clone().try_acquire_owned() { Ok(p) => p, Err(_) => { drop(stream); continue; } };
                let state = state.clone(); state.counters.accepted.fetch_add(1, Ordering::Relaxed);
                tasks.spawn(async move { let _permit = permit; if let Err(error) = handle(stream, state.clone()).await { state.counters.failures.fetch_add(1, Ordering::Relaxed); eprintln!("connection: {error}"); } state.counters.completed.fetch_add(1, Ordering::Relaxed); });
            }
        }
    }
    tasks.abort_all();
    maintenance.abort();
    let _ = state.persist_routes().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config() -> Config {
        serde_json::from_str(include_str!("../config.json")).unwrap()
    }
    #[test]
    fn config_limits_and_modes() {
        let mut cfg = config();
        cfg.validate().unwrap();
        assert_eq!(cfg.mode_for("github.com"), Mode::Auto);
        assert_eq!(cfg.mode_for("api.github.com"), Mode::Auto);
        cfg.listen = "0.0.0.0:18080".parse().unwrap();
        assert!(cfg.validate().is_err());
        cfg = config();
        cfg.max_connections = 0;
        assert!(cfg.validate().is_err());
        cfg = config();
        cfg.upstream = Some(cfg.listen);
        assert!(cfg.validate().is_err());
    }
    #[tokio::test]
    async fn route_cache_is_bounded_and_updates() {
        let mut cfg = config();
        cfg.max_routes = 2;
        let state = State::new(cfg).unwrap();
        state.cache_route("github.com", Route::Proxy).await;
        state.cache_route("api.github.com", Route::Proxy).await;
        state.cache_route("api.github.com", Route::Proxy).await;
        assert_eq!(state.routes.lock().await.len(), 2);
        state
            .cache_route("raw.githubusercontent.com", Route::Proxy)
            .await;
        let cache = state.routes.lock().await;
        assert_eq!(cache.len(), 2);
        assert!(!cache.contains_key("github.com"));
    }
    #[test]
    fn scope_and_authority() {
        for host in [
            "github.com:443",
            "RAW.githubusercontent.com:443",
            "release-assets.githubusercontent.com:443",
        ] {
            assert!(target(host).is_ok());
        }
        for host in [
            "github.com.evil.com:443",
            "evilgithub.com:443",
            "127.0.0.1:443",
            "github.com:80",
            "github.com:443\r\nInjected: yes",
            "github.com.:443",
            "a..github.com:443",
            "-a.github.com:443",
        ] {
            assert!(target(host).is_err(), "{host}");
        }
    }
    #[test]
    fn reject_local_addresses() {
        for ip in [
            "127.0.0.1",
            "10.0.0.1",
            "169.254.169.254",
            "100.64.0.1",
            "198.18.0.1",
            "::1",
            "::ffff:127.0.0.1",
            "fd00::1",
            "2001:db8::1",
        ] {
            assert!(!public_ip(ip.parse().unwrap()), "{ip}");
        }
        assert!(public_ip("140.82.112.3".parse().unwrap()));
        assert!(public_ip("2606:50c0:8000::154".parse().unwrap()));
    }
    #[tokio::test]
    async fn header_preserves_tunnel_bytes() {
        let mut input: &[u8] = b"HTTP/1.1 200 OK\r\n\r\nTLS bytes";
        assert_eq!(
            read_header(&mut input).await.unwrap(),
            b"HTTP/1.1 200 OK\r\n\r\n"
        );
        assert_eq!(input, b"TLS bytes");
        let data = vec![b'x'; 16385];
        assert!(read_header(&mut data.as_slice()).await.is_err());
    }
    #[tokio::test]
    async fn bidirectional_and_half_close() {
        let (mut client, inside) = tokio::io::duplex(1024);
        let (outside, mut server) = tokio::io::duplex(1024);
        let task = tokio::spawn(async move {
            let counts = Counters::default();
            relay(inside, outside, Duration::from_secs(2), &counts)
                .await
                .unwrap();
            assert_eq!(counts.uploaded.load(Ordering::Relaxed), 4);
            assert_eq!(counts.downloaded.load(Ordering::Relaxed), 5);
        });
        client.write_all(b"ping").await.unwrap();
        client.shutdown().await.unwrap();
        let mut data = Vec::new();
        server.read_to_end(&mut data).await.unwrap();
        assert_eq!(data, b"ping");
        server.write_all(b"pong!").await.unwrap();
        server.shutdown().await.unwrap();
        data.clear();
        client.read_to_end(&mut data).await.unwrap();
        assert_eq!(data, b"pong!");
        task.await.unwrap();
    }
}
