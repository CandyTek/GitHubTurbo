use super::{err, public_ip, Result};
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    time::{Duration, Instant},
};
use tokio::{
    sync::{Mutex, Semaphore},
    task::JoinSet,
};

pub struct Resolver {
    clients: Vec<(reqwest::Client, &'static str, Option<&'static str>)>,
    cache: Mutex<HashMap<String, (Instant, Vec<IpAddr>)>>,
    slots: Semaphore,
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
            cache: Mutex::new(HashMap::new()),
            slots: Semaphore::new(4),
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
        let _permit = self.slots.acquire().await?;
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
        while let Some(reply) = jobs.join_next().await {
            if let Ok(Ok((found, valid))) = reply {
                ips.extend(found);
                ttl = ttl.min(valid);
            }
        }
        ips.sort();
        ips.dedup();
        // Keep both families represented, rather than truncating all IPv6 addresses.
        let mut v4 = ips.iter().filter(|ip| ip.is_ipv4());
        let mut v6 = ips.iter().filter(|ip| ip.is_ipv6());
        let mut selected = Vec::new();
        for _ in 0..4 {
            if let Some(ip) = v4.next() {
                selected.push(*ip);
            }
            if let Some(ip) = v6.next() {
                selected.push(*ip);
            }
        }
        if selected.is_empty() {
            return Err(err("all independent DNS resolvers failed; check network or configure an optional upstream"));
        }
        let mut cache = self.cache.lock().await;
        if cache.len() >= 256 {
            cache.retain(|_, (expiry, _)| *expiry > Instant::now());
            if cache.len() >= 256 {
                if let Some(key) = cache.keys().next().cloned() {
                    cache.remove(&key);
                }
            }
        }
        cache.insert(
            host.to_owned(),
            (
                Instant::now() + Duration::from_secs(ttl.clamp(5, 300)),
                selected.clone(),
            ),
        );
        Ok(selected)
    }
}
