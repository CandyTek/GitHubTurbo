use super::{err, public_ip, Result};
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    sync::{Mutex, Semaphore},
    task::JoinSet,
};

type Cache = Mutex<HashMap<String, (Instant, Vec<IpAddr>)>>;
pub struct Resolver {
    clients: Vec<(reqwest::Client, &'static str, Option<&'static str>)>,
    cache: Arc<Cache>,
    slots: Arc<Semaphore>,
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
        ];
        let mut clients = Vec::new();
        for (name, ip, url) in providers {
            let client = reqwest::Client::builder()
                .no_proxy()
                .resolve(name, ip.parse::<SocketAddr>()?)
                .connect_timeout(Duration::from_secs(2))
                .timeout(Duration::from_secs(3))
                .redirect(reqwest::redirect::Policy::none())
                .pool_max_idle_per_host(2)
                .build()?;
            if name == "dns.alidns.com" {
                clients.push((client.clone(), url, Some("8.8.8.0/24")));
                clients.push((client.clone(), url, Some("208.67.222.0/24")));
            }
            clients.push((client, url, None));
        }
        Ok(Self {
            clients,
            cache: Arc::new(Mutex::new(HashMap::new())),
            slots: Arc::new(Semaphore::new(4)),
        })
    }
    pub async fn resolve(&self, host: &str) -> Result<Vec<IpAddr>> {
        if !super::allowed(host) {
            return Err(err("DNS scope denied"));
        }
        if let Some((expiry, ips)) = self.cache.lock().await.get(host) {
            if *expiry > Instant::now() {
                return Ok(ips.clone());
            }
        }
        let _permit = self.slots.clone().acquire_owned().await?;
        if let Some((expiry, ips)) = self.cache.lock().await.get(host) {
            if *expiry > Instant::now() {
                return Ok(ips.clone());
            }
        }
        let mut jobs = JoinSet::new();
        for (client, url, subnet) in &self.clients {
            for kind in ["A", "AAAA"] {
                let client = client.clone();
                let url = *url;
                let subnet = *subnet;
                let host = host.to_owned();
                jobs.spawn(async move {
                    let mut request = client
                        .get(url)
                        .query(&[("name", host.as_str()), ("type", kind)])
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
                    let mut ttl = 300;
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
                    Ok::<_, Box<dyn std::error::Error + Send + Sync>>((ips, ttl))
                });
            }
        }
        let mut ips = Vec::new();
        let mut ttl = 300;
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
        let cache = self.cache.clone();
        let host = host.to_owned();
        tokio::spawn(async move {
            let _permit = _permit;
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
async fn put(
    cache: &Mutex<HashMap<String, (Instant, Vec<IpAddr>)>>,
    host: &str,
    ips: Vec<IpAddr>,
    ttl: u64,
) {
    let mut cache = cache.lock().await;
    if cache.len() >= 256 && !cache.contains_key(host) {
        cache.retain(|_, (expiry, _)| *expiry > Instant::now());
        if cache.len() >= 256 {
            if let Some(key) = cache.keys().next().cloned() {
                cache.remove(&key);
            }
        }
    }
    cache.insert(
        host.to_owned(),
        (Instant::now() + Duration::from_secs(ttl.clamp(5, 300)), ips),
    );
}
#[cfg(test)]
mod tests {
    use super::*;
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
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        async fn server(delay: u64, ip: &'static str) -> Result<String> {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let url = format!("http://{}/resolve", listener.local_addr()?);
            tokio::spawn(async move {
                for _ in 0..2 {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    tokio::spawn(async move {
                        let mut request = [0; 4096];
                        let _ = socket.read(&mut request).await;
                        tokio::time::sleep(Duration::from_millis(delay)).await;
                        let body = format!(
                            r#"{{"Status":0,"Answer":[{{"type":1,"data":"{ip}","TTL":60}}]}}"#
                        );
                        let reply=format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len());
                        let _ = socket.write_all(reply.as_bytes()).await;
                    });
                }
            });
            Ok(url)
        }
        let fast = server(5, "8.8.8.8").await?;
        let slow = server(1200, "1.1.1.1").await?;
        // Leak only these two small test endpoint strings to match production's
        // fixed provider URLs; no externally supplied endpoint enters the app.
        let client = reqwest::Client::builder().no_proxy().build()?;
        let resolver = Resolver {
            clients: vec![
                (client.clone(), Box::leak(fast.into_boxed_str()), None),
                (client, Box::leak(slow.into_boxed_str()), None),
            ],
            cache: Arc::new(Mutex::new(HashMap::new())),
            slots: Arc::new(Semaphore::new(4)),
        };
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
}
