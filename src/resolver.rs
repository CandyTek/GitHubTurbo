use super::{err, public_ip, Result};
use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    sync::{Mutex, Semaphore},
    task::JoinSet,
};

// A reachable provider answers in tens of milliseconds. Fail fast on the others:
// DNS must never be what a request is waiting for.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(800);
const QUERY_TIMEOUT: Duration = Duration::from_millis(1500);
// Skip a provider that just failed instead of paying its timeout on every lookup.
const PROVIDER_COOLDOWN: Duration = Duration::from_secs(60);
// Short CDN TTLs are extended: answers are only candidates, and every connection
// still validates the original hostname, so an old address cannot impersonate.
const MIN_FRESH_SECONDS: u64 = 300;
const MAX_FRESH_SECONDS: u64 = 3600;
// After expiry an answer is served immediately while a refresh runs behind it.
const STALE_GRACE: Duration = Duration::from_secs(6 * 3600);

struct Entry {
    fresh_until: Instant,
    stale_until: Instant,
    ips: Vec<IpAddr>,
}
type Cache = Mutex<HashMap<String, Entry>>;
type Provider = (reqwest::Client, &'static str, Option<&'static str>);
#[derive(Clone)]
pub struct Resolver {
    clients: Arc<Vec<Provider>>,
    down_until: Arc<std::sync::Mutex<Vec<Instant>>>,
    cache: Arc<Cache>,
    refreshing: Arc<std::sync::Mutex<HashSet<String>>>,
    slots: Arc<Semaphore>,
    pub ipv6: crate::ipv6::Ipv6,
}
impl Resolver {
    pub fn new() -> Result<Self> {
        let providers = [
            (
                "cloudflare-dns.com",
                "1.1.1.1:443",
                "https://cloudflare-dns.com/dns-query",
            ),
            ("dns.google", "8.8.8.8:443", "https://dns.google/resolve"),
            (
                "dns.alidns.com",
                "223.5.5.5:443",
                "https://dns.alidns.com/resolve",
            ),
            ("doh.pub", "1.12.12.12:443", "https://doh.pub/resolve"),
        ];
        let mut clients = Vec::new();
        for (name, ip, url) in providers {
            let client = reqwest::Client::builder()
                .no_proxy()
                .resolve(name, ip.parse::<SocketAddr>()?)
                .connect_timeout(CONNECT_TIMEOUT)
                .timeout(QUERY_TIMEOUT)
                .redirect(reqwest::redirect::Policy::none())
                .pool_max_idle_per_host(2)
                .build()?;
            if name == "dns.alidns.com" {
                clients.push((client.clone(), url, Some("8.8.8.0/24")));
                clients.push((client.clone(), url, Some("208.67.222.0/24")));
            }
            clients.push((client, url, None));
        }
        Ok(Self::with_providers(clients))
    }
    fn with_providers(clients: Vec<Provider>) -> Self {
        let now = Instant::now();
        Self {
            down_until: Arc::new(std::sync::Mutex::new(vec![now; clients.len()])),
            clients: Arc::new(clients),
            cache: Arc::new(Mutex::new(HashMap::new())),
            refreshing: Default::default(),
            slots: Arc::new(Semaphore::new(4)),
            ipv6: Default::default(),
        }
    }
    pub fn ipv6_usable(&self) -> bool {
        self.ipv6.usable()
    }
    /// Answers cached while IPv6 worked may still hold IPv6 addresses; hand them
    /// out only while it still does.
    pub async fn resolve(&self, host: &str) -> Result<Vec<IpAddr>> {
        let mut ips = self.resolve_any(host).await?;
        if !self.ipv6_usable() && ips.iter().any(IpAddr::is_ipv4) {
            ips.retain(IpAddr::is_ipv4);
        }
        Ok(ips)
    }
    async fn resolve_any(&self, host: &str) -> Result<Vec<IpAddr>> {
        if !super::allowed(host) {
            return Err(err("DNS scope denied"));
        }
        let now = Instant::now();
        let cached = self
            .cache
            .lock()
            .await
            .get(host)
            .filter(|entry| entry.stale_until > now)
            .map(|entry| (entry.fresh_until > now, entry.ips.clone()));
        match cached {
            Some((true, ips)) => Ok(ips),
            Some((false, ips)) => {
                self.refresh_behind(host);
                Ok(ips)
            }
            None => self.query(host).await,
        }
    }
    pub async fn clear(&self) {
        self.cache.lock().await.clear();
        self.ipv6.reset();
        let now = Instant::now();
        self.down_until.lock().unwrap().fill(now);
    }
    fn refresh_behind(&self, host: &str) {
        if !self.refreshing.lock().unwrap().insert(host.to_owned()) {
            return;
        }
        let this = self.clone();
        let host = host.to_owned();
        tokio::spawn(async move {
            let _ = this.query(&host).await;
            this.refreshing.lock().unwrap().remove(&host);
        });
    }
    fn usable_providers(&self) -> Vec<usize> {
        let now = Instant::now();
        let down = self.down_until.lock().unwrap();
        let up: Vec<_> = (0..down.len()).filter(|i| down[*i] <= now).collect();
        if up.is_empty() {
            // Everything failed recently; the network may be back, so try them all.
            (0..down.len()).collect()
        } else {
            up
        }
    }
    async fn query(&self, host: &str) -> Result<Vec<IpAddr>> {
        let permit = self.slots.clone().acquire_owned().await?;
        let now = Instant::now();
        if let Some(entry) = self.cache.lock().await.get(host) {
            if entry.fresh_until > now {
                return Ok(entry.ips.clone());
            }
        }
        let mut jobs = JoinSet::new();
        for index in self.usable_providers() {
            let (client, url, subnet) = &self.clients[index];
            let kinds: &[&str] = if self.ipv6_usable() {
                &["A", "AAAA"]
            } else {
                &["A"]
            };
            for kind in kinds.iter().copied() {
                let client = client.clone();
                let url = *url;
                let subnet = *subnet;
                let host = host.to_owned();
                let down_until = self.down_until.clone();
                jobs.spawn(async move {
                    let reply = lookup(client, url, subnet, &host, kind).await;
                    if reply.is_err() {
                        down_until.lock().unwrap()[index] = Instant::now() + PROVIDER_COOLDOWN;
                    }
                    reply
                });
            }
        }
        let mut ips = Vec::new();
        let mut ttl = MAX_FRESH_SECONDS;
        // A slow supplier must not hold up a usable DNS answer. Continue harvesting
        // the other views in the background for the next request.
        while let Some(reply) = jobs.join_next().await {
            if let Ok(Ok((found, valid))) = reply {
                if !found.is_empty() {
                    ttl = ttl.min(valid);
                    ips.extend(found);
                }
                if !ips.is_empty() {
                    break;
                }
            }
        }
        if ips.is_empty() {
            return Err(err("all independent DNS resolvers failed"));
        }
        let until = tokio::time::Instant::now() + Duration::from_millis(30);
        while let Ok(Some(reply)) = tokio::time::timeout_at(until, jobs.join_next()).await {
            if let Ok(Ok((found, valid))) = reply {
                if !found.is_empty() {
                    ips.extend(found);
                    ttl = ttl.min(valid);
                }
            }
        }
        let selected = balanced(&ips);
        put(&self.cache, host, selected.clone(), ttl).await;
        // The answer is in. Unreachable providers must not keep other hosts waiting.
        drop(permit);
        let cache = self.cache.clone();
        let host = host.to_owned();
        tokio::spawn(async move {
            while let Some(reply) = jobs.join_next().await {
                if let Ok(Ok((found, valid))) = reply {
                    if !found.is_empty() {
                        ips.extend(found);
                        ttl = ttl.min(valid);
                    }
                }
            }
            put(&cache, &host, balanced(&ips), ttl).await;
        });
        Ok(selected)
    }
}
async fn lookup(
    client: reqwest::Client,
    url: &str,
    subnet: Option<&str>,
    host: &str,
    kind: &str,
) -> Result<(Vec<IpAddr>, u64)> {
    let mut request = client
        .get(url)
        .query(&[("name", host), ("type", kind)])
        .header("Accept", "application/dns-json");
    if let Some(subnet) = subnet {
        request = request.query(&[("edns_client_subnet", subnet)]);
    }
    let mut response = request.send().await?.error_for_status()?;
    if response.content_length().is_some_and(|size| size > 65536) {
        return Err(err("DNS reply too large"));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len() + chunk.len() > 65536 {
            return Err(err("DNS reply too large"));
        }
        body.extend_from_slice(&chunk);
    }
    let json: serde_json::Value = serde_json::from_slice(&body)?;
    let mut ips = Vec::new();
    let mut ttl = MAX_FRESH_SECONDS;
    if json["Status"].as_u64() == Some(0) {
        if let Some(answers) = json["Answer"].as_array() {
            for record in answers {
                if matches!(record["type"].as_u64(), Some(1 | 28)) {
                    if let Some(ip) = record["data"]
                        .as_str()
                        .and_then(|s| s.parse::<IpAddr>().ok())
                        .filter(|ip| public_ip(*ip))
                    {
                        ips.push(ip);
                        ttl = ttl.min(record["TTL"].as_u64().unwrap_or(30));
                    }
                }
            }
        }
    }
    Ok((ips, ttl))
}
fn balanced(ips: &[IpAddr]) -> Vec<IpAddr> {
    let mut all = ips.to_vec();
    // Preserve answer arrival order. Alternate address families without sorting
    // a geographically useful answer behind a numerically smaller address.
    let mut seen = std::collections::HashSet::new();
    all.retain(|ip| seen.insert(*ip));
    let mut v4 = all.iter().filter(|ip| ip.is_ipv4());
    let mut v6 = all.iter().filter(|ip| ip.is_ipv6());
    let mut selected = Vec::new();
    for _ in 0..8 {
        if let Some(ip) = v4.next() {
            selected.push(*ip)
        }
        if let Some(ip) = v6.next() {
            selected.push(*ip)
        }
    }
    selected
}
async fn put(cache: &Cache, host: &str, ips: Vec<IpAddr>, ttl: u64) {
    let mut cache = cache.lock().await;
    if cache.len() >= 256 && !cache.contains_key(host) {
        let now = Instant::now();
        cache.retain(|_, entry| entry.stale_until > now);
        if cache.len() >= 256 {
            if let Some(key) = cache.keys().next().cloned() {
                cache.remove(&key);
            }
        }
    }
    let fresh_until =
        Instant::now() + Duration::from_secs(ttl.clamp(MIN_FRESH_SECONDS, MAX_FRESH_SECONDS));
    cache.insert(
        host.to_owned(),
        Entry {
            fresh_until,
            stale_until: fresh_until + STALE_GRACE,
            ips,
        },
    );
}
#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    async fn server(delay: u64, ip: &'static str) -> Result<&'static str> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}/resolve", listener.local_addr()?);
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut request = [0; 4096];
                    let _ = socket.read(&mut request).await;
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    let body =
                        format!(r#"{{"Status":0,"Answer":[{{"type":1,"data":"{ip}","TTL":60}}]}}"#);
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(reply.as_bytes()).await;
                });
            }
        });
        // Leak only these small test endpoint strings to match production's
        // fixed provider URLs; no externally supplied endpoint enters the app.
        Ok(Box::leak(url.into_boxed_str()))
    }
    fn client() -> Result<reqwest::Client> {
        Ok(reqwest::Client::builder().no_proxy().build()?)
    }
    #[tokio::test]
    async fn no_ipv6_route_means_no_aaaa_queries() -> Result<()> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}/resolve", listener.local_addr()?);
        let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = asked.clone();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let seen = seen.clone();
                tokio::spawn(async move {
                    let mut request = [0; 4096];
                    let n = socket.read(&mut request).await.unwrap_or(0);
                    seen.lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(&request[..n]).into_owned());
                    let body =
                        r#"{"Status":0,"Answer":[{"type":1,"data":"140.82.112.3","TTL":60}]}"#;
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(reply.as_bytes()).await;
                });
            }
        });
        let url: &'static str = Box::leak(url.into_boxed_str());
        let mut resolver = Resolver::with_providers(vec![(client()?, url, None)]);
        resolver.ipv6 = crate::ipv6::Ipv6::with_route(false);
        assert!(!resolver.ipv6_usable());
        resolver.resolve("github.com").await?;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let asked = asked.lock().unwrap();
        assert_eq!(asked.len(), 1);
        assert!(asked[0].contains("type=A") && !asked[0].contains("AAAA"));
        Ok(())
    }
    #[test]
    fn keep_first_answer_and_both_families() {
        let ips =
            ["8.8.8.8", "1.1.1.1", "8.8.8.8", "2606:4700:4700::1111"].map(|v| v.parse().unwrap());
        let out = balanced(&ips);
        assert_eq!(out[0], ips[0]);
        assert_eq!(out[1], ips[3]);
        assert_eq!(out.len(), 3);
    }
    #[tokio::test]
    async fn slow_supplier_does_not_block_and_is_harvested() -> Result<()> {
        let fast = server(5, "8.8.8.8").await?;
        let slow = server(1200, "1.1.1.1").await?;
        let client = client()?;
        let resolver =
            Resolver::with_providers(vec![(client.clone(), fast, None), (client, slow, None)]);
        let start = Instant::now();
        let first = resolver.resolve("github.com").await?;
        assert!(start.elapsed() < Duration::from_millis(700));
        assert_eq!(first, vec!["8.8.8.8".parse::<IpAddr>()?]);
        tokio::time::sleep(Duration::from_millis(1350)).await;
        assert!(resolver
            .resolve("github.com")
            .await?
            .contains(&"1.1.1.1".parse()?));
        Ok(())
    }
    #[tokio::test]
    async fn a_slow_supplier_does_not_hold_a_lookup_slot_after_the_answer() -> Result<()> {
        let fast = server(5, "8.8.8.8").await?;
        let slow = server(1500, "1.1.1.1").await?;
        let client = client()?;
        let mut resolver =
            Resolver::with_providers(vec![(client.clone(), fast, None), (client, slow, None)]);
        resolver.slots = Arc::new(Semaphore::new(1));
        let start = Instant::now();
        resolver.resolve("github.com").await?;
        resolver.resolve("api.github.com").await?;
        assert!(
            start.elapsed() < Duration::from_millis(700),
            "second host waited {:?} for the first host's slow provider",
            start.elapsed()
        );
        Ok(())
    }
    #[tokio::test]
    async fn expired_answer_is_served_at_once_and_refreshed_behind() -> Result<()> {
        let provider = server(150, "8.8.8.8").await?;
        let resolver = Resolver::with_providers(vec![(client()?, provider, None)]);
        let now = Instant::now();
        resolver.cache.lock().await.insert(
            "github.com".into(),
            Entry {
                fresh_until: now,
                stale_until: now + STALE_GRACE,
                ips: vec!["1.1.1.1".parse()?],
            },
        );
        let start = Instant::now();
        let old = resolver.resolve("github.com").await?;
        assert!(start.elapsed() < Duration::from_millis(100));
        assert_eq!(old, vec!["1.1.1.1".parse::<IpAddr>()?]);
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(
            resolver.resolve("github.com").await?,
            vec!["8.8.8.8".parse::<IpAddr>()?]
        );
        Ok(())
    }
    #[tokio::test]
    async fn short_ttl_is_extended_and_failed_provider_is_skipped() -> Result<()> {
        let good = server(5, "8.8.8.8").await?;
        // Answers every lookup with an HTTP error, the quickest failure to produce.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let dead: &'static str =
            Box::leak(format!("http://{}/resolve", listener.local_addr()?).into_boxed_str());
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut request = [0; 4096];
                    let _ = socket.read(&mut request).await;
                    let _ = socket
                        .write_all(
                            b"HTTP/1.1 503 Busy\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await;
                });
            }
        });
        let client = client()?;
        let resolver =
            Resolver::with_providers(vec![(client.clone(), dead, None), (client, good, None)]);
        assert_eq!(resolver.usable_providers(), vec![0, 1]);
        resolver.resolve("github.com").await?;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(resolver.usable_providers(), vec![1]);
        // The provider answered with TTL 60; the entry must outlive that.
        let cache = resolver.cache.lock().await;
        let entry = &cache["github.com"];
        assert!(entry.fresh_until > Instant::now() + Duration::from_secs(250));
        drop(cache);
        resolver.clear().await;
        assert_eq!(resolver.usable_providers(), vec![0, 1]);
        assert!(resolver.cache.lock().await.is_empty());
        Ok(())
    }
}
