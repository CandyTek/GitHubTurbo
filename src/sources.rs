//! Remote hosts files supply untrusted address candidates, never system DNS rules.
use super::*;
const URLS: [&str; 3] = [
    "https://raw.githubusercontent.com/jianboy/github-host/master/hosts",
    "https://raw.githubusercontent.com/521xueweihan/GitHub520/main/hosts",
    "https://raw.githubusercontent.com/maxiaof/github-hosts/master/hosts",
];
const LIMIT: usize = 256 * 1024;
#[derive(Clone, Default, Deserialize, Serialize)]
struct Feed {
    fetched: i64,
    hosts: HashMap<String, Vec<IpAddr>>,
}
#[derive(Default)]
pub struct Sources {
    feeds: std::sync::RwLock<Vec<Feed>>,
    errors: std::sync::Mutex<Vec<bool>>,
}
fn parse(body: &[u8]) -> Result<HashMap<String, Vec<IpAddr>>> {
    if body.len() > LIMIT {
        return Err(err("hosts source too large"));
    }
    let text = std::str::from_utf8(body)?.trim_start_matches('\u{feff}');
    let mut hosts: HashMap<String, Vec<IpAddr>> = HashMap::new();
    for line in text.lines() {
        let mut words = line
            .split('#')
            .next()
            .unwrap_or("")
            .split_ascii_whitespace();
        let Some(ip) = words
            .next()
            .and_then(|v| v.parse::<IpAddr>().ok())
            .filter(|ip| public_ip(*ip))
        else {
            continue;
        };
        for name in words.take(32) {
            let name = name.trim_end_matches('.').to_ascii_lowercase();
            if target(&format!("{name}:443")).is_err()
                || hosts.len() >= 512 && !hosts.contains_key(&name)
            {
                continue;
            }
            let ips = hosts.entry(name).or_default();
            if ips.len() < 16 && !ips.contains(&ip) {
                ips.push(ip);
            }
        }
    }
    if hosts.is_empty() {
        return Err(err("hosts source has no in-scope public candidates"));
    }
    Ok(hosts)
}
impl Sources {
    pub fn candidates(&self, host: &str) -> Vec<SocketAddr> {
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let feeds = self.feeds.read().unwrap();
        let groups: Vec<_> = feeds
            .iter()
            .filter(|f| (0..=86400).contains(&(now.saturating_sub(f.fetched))))
            .filter_map(|f| f.hosts.get(host))
            .collect();
        // Round robin across independent sources; list order is not a priority score.
        let mut out = Vec::new();
        for n in 0..16 {
            for ips in &groups {
                if let Some(ip) = ips.get(n) {
                    let address = SocketAddr::new(*ip, 443);
                    if !out.contains(&address) {
                        out.push(address);
                    }
                }
            }
        }
        out
    }
    pub fn snapshot(&self) -> serde_json::Value {
        let feeds = self.feeds.read().unwrap();
        let errors = self.errors.lock().unwrap();
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        serde_json::json!(URLS.iter().enumerate().map(|(i,url)| {
            let f = feeds.get(i);
            serde_json::json!({"url":url,"fetched":f.map(|f|f.fetched).unwrap_or(0),"hosts":f.map(|f|f.hosts.len()).unwrap_or(0),"usable":f.is_some_and(|f|(0..=86400).contains(&(now.saturating_sub(f.fetched)))),"refresh_failed":errors.get(i).copied().unwrap_or(false)})
        }).collect::<Vec<_>>())
    }
    pub fn restore(&self, dir: &std::path::Path) {
        let Ok(data) = std::fs::read(dir.join("host-sources.json")) else {
            return;
        };
        if data.len() > 1024 * 1024 {
            return;
        }
        let Ok(feeds) = serde_json::from_slice::<Vec<Feed>>(&data) else {
            return;
        };
        if feeds.len() != URLS.len() {
            return;
        }
        // The state file is also untrusted. Revalidate every restored fact.
        let feeds = feeds
            .into_iter()
            .map(|f| Feed {
                fetched: f.fetched,
                hosts: f
                    .hosts
                    .into_iter()
                    .take(512)
                    .filter(|(h, _)| target(&format!("{h}:443")).is_ok())
                    .map(|(h, ips)| {
                        (
                            h,
                            ips.into_iter()
                                .filter(|ip| public_ip(*ip))
                                .take(16)
                                .collect(),
                        )
                    })
                    .collect(),
            })
            .collect();
        *self.feeds.write().unwrap() = feeds;
    }
    pub async fn refresh(
        &self,
        resolver: &resolver::Resolver,
        dir: &std::path::Path,
    ) -> Result<()> {
        let ips = resolver
            .resolve("raw.githubusercontent.com")
            .await
            .unwrap_or_else(|_| {
                seeds::for_host("raw.githubusercontent.com")
                    .into_iter()
                    .map(|a| a.ip())
                    .collect()
            });
        let addresses: Vec<_> = ips.into_iter().map(|ip| SocketAddr::new(ip, 443)).collect();
        if addresses.is_empty() {
            return Err(err("no hosts source bootstrap address"));
        }
        let client = reqwest::Client::builder()
            .no_proxy()
            .resolve_to_addrs("raw.githubusercontent.com", &addresses)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(12))
            .build()?;
        let mut jobs = JoinSet::new();
        for (i, url) in URLS.iter().enumerate() {
            let client = client.clone();
            let url = *url;
            jobs.spawn(async move {
                let result = async {
                    let mut response = client.get(url).send().await?.error_for_status()?;
                    if response.content_length().is_some_and(|n| n > LIMIT as u64) {
                        return Err(err("hosts source too large"));
                    }
                    let mut body = Vec::new();
                    while let Some(chunk) = response.chunk().await? {
                        if body.len() + chunk.len() > LIMIT {
                            return Err(err("hosts source too large"));
                        }
                        body.extend_from_slice(&chunk);
                    }
                    parse(&body)
                }
                .await;
                (i, result)
            });
        }
        while let Some(Ok((i, result))) = jobs.join_next().await {
            let mut feeds = self.feeds.write().unwrap();
            feeds.resize_with(URLS.len(), Feed::default);
            let mut errors = self.errors.lock().unwrap();
            errors.resize(URLS.len(), false);
            errors[i] = result.is_err();
            if let Ok(hosts) = result {
                feeds[i] = Feed {
                    fetched: time::OffsetDateTime::now_utc().unix_timestamp(),
                    hosts,
                };
            }
        }
        let data = serde_json::to_vec(&*self.feeds.read().unwrap())?;
        tokio::fs::create_dir_all(dir).await?;
        let temp = dir.join("host-sources.tmp");
        tokio::fs::write(&temp, data).await?;
        tokio::fs::rename(temp, dir.join("host-sources.json")).await?;
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hostile_lines_and_multi_names() {
        let h=parse(b"\xef\xbb\xbf# hosts\n1.1.1.1 github.com RAW.githubusercontent.com. evil.test github.com.evil.test\n127.0.0.1 github.com\n::1 api.github.com\n100.64.0.1 api.github.com\n8.8.8.8 github.com # 9.9.9.9\n").unwrap();
        assert_eq!(h.len(), 2);
        assert_eq!(h["github.com"].len(), 2);
        assert_eq!(h["raw.githubusercontent.com"].len(), 1);
        assert!(parse(b"<html>failure</html>").is_err());
        assert!(parse(&vec![b'x'; LIMIT + 1]).is_err());
    }
    #[test]
    fn expire_and_interleave() {
        let s = Sources::default();
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        *s.feeds.write().unwrap() = vec![
            Feed {
                fetched: now,
                hosts: parse(b"1.1.1.1 github.com\n8.8.8.8 github.com").unwrap(),
            },
            Feed {
                fetched: now,
                hosts: parse(b"9.9.9.9 github.com").unwrap(),
            },
            Feed {
                fetched: now - 86401,
                hosts: parse(b"4.2.2.2 github.com").unwrap(),
            },
        ];
        assert_eq!(
            s.candidates("github.com")
                .iter()
                .map(|a| a.ip().to_string())
                .collect::<Vec<_>>(),
            vec!["1.1.1.1", "9.9.9.9", "8.8.8.8"]
        );
    }
}
