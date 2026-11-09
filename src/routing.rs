use super::*;
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Traffic {
    Latency,
    Bulk,
}
impl Traffic {
    pub fn classify(host: &str, path: &str) -> Self {
        if matches!(
            host,
            "release-assets.githubusercontent.com"
                | "objects.githubusercontent.com"
                | "codeload.github.com"
        ) || path.ends_with("/git-upload-pack")
            || path.ends_with("/git-receive-pack")
            || path.ends_with(".git/info/refs")
            || path.contains("/archive/refs/")
        {
            Self::Bulk
        } else {
            Self::Latency
        }
    }
    pub fn key(self, host: &str) -> String {
        match self {
            Self::Latency => host.into(),
            Self::Bulk => format!("bulk:{host}"),
        }
    }
}
/// Below this a download connection is not worth keeping. Speed on one network
/// differs by 40x between connections to the same IP, so this judges the
/// connection only and says nothing about the address it reached.
pub const SLOW_FLOW_BYTES_PER_SECOND: f64 = 512. * 1024.;
pub fn slow_flow(bytes: u64, elapsed: Duration) -> bool {
    elapsed >= Duration::from_secs(2)
        && (bytes as f64 / elapsed.as_secs_f64()) < SLOW_FLOW_BYTES_PER_SECOND
}
struct Sample {
    rate: f64,
    bytes: u64,
    completed: u64,
    failed: bool,
    at: Instant,
}
#[derive(Default)]
pub struct Performance {
    samples: std::sync::Mutex<HashMap<(String, Route), Sample>>,
}
impl Performance {
    pub fn record(
        &self,
        host: &str,
        route: &Route,
        bytes: u64,
        duration: Duration,
        complete: bool,
    ) {
        // Tiny prefixes and experimental range pieces do not predict full-file speed.
        if complete && bytes < 4 * 1024 * 1024 {
            return;
        }
        let mut samples = self.samples.lock().unwrap();
        if samples.len() >= 512 && !samples.contains_key(&(host.into(), route.clone())) {
            if let Some(oldest) = samples
                .iter()
                .min_by_key(|(_, s)| s.at)
                .map(|(k, _)| k.clone())
            {
                samples.remove(&oldest);
            }
        }
        let rate = bytes as f64 / duration.as_secs_f64().max(0.1);
        let sample = samples
            .entry((host.into(), route.clone()))
            .or_insert(Sample {
                rate,
                bytes: 0,
                completed: 0,
                failed: false,
                at: Instant::now(),
            });
        if complete {
            // React immediately to a decline rather than letting an old fast
            // sample hide the user's most recent slow complete download.
            sample.rate = rate;
            sample.completed += 1;
            sample.bytes += bytes;
        }
        sample.failed = !complete;
        sample.at = Instant::now();
    }
    pub fn slow(&self, host: &str, route: &Route) -> bool {
        let samples = self.samples.lock().unwrap();
        let fresh = |s: &&Sample| s.at.elapsed() < Duration::from_secs(6 * 3600);
        let best = samples
            .iter()
            .filter(|((h, _), s)| h == host && !s.failed && fresh(s))
            .map(|(_, s)| s.rate)
            .fold(0., f64::max);
        samples.get(&(host.into(), route.clone())).is_some_and(|s| {
            s.at.elapsed() < Duration::from_secs(6 * 3600)
                && (s.failed || s.rate < 512. * 1024. || s.rate * 1.8 < best)
        })
    }
    pub fn preferred(&self, host: &str, candidates: &[SocketAddr]) -> Option<Route> {
        let samples = self.samples.lock().unwrap();
        samples
            .iter()
            .filter(|((h, r), s)| {
                h == host
                    && !s.failed
                    && s.completed > 0
                    && s.rate >= 512. * 1024.
                    && s.at.elapsed() < Duration::from_secs(6 * 3600)
                    && matches!(r,Route::Direct(a)|Route::DirectNoSni(a) if candidates.contains(a))
            })
            .max_by(|(_, a), (_, b)| a.rate.total_cmp(&b.rate))
            .map(|((_, r), _)| r.clone())
    }
    pub fn clear(&self) {
        self.samples.lock().unwrap().clear();
    }
    pub fn snapshot(&self) -> serde_json::Value {
        serde_json::json!(self.samples.lock().unwrap().iter().map(|((h,r),s)|serde_json::json!({"host":h,"route":r,"bytes":s.bytes,"completed":s.completed,"observed_bytes_per_second":s.rate as u64,"failed":s.failed,"age_seconds":s.at.elapsed().as_secs()})).collect::<Vec<_>>())
    }
}
/// Take representatives across the whole list instead of its first IP block.
pub fn spread<T: Clone>(items: &[T], limit: usize) -> Vec<T> {
    if limit == 0 {
        return vec![];
    }
    if limit == 1 {
        return items.first().cloned().into_iter().collect();
    }
    if items.len() <= limit {
        return items.to_vec();
    }
    (0..limit)
        .map(|i| items[i * (items.len() - 1) / (limit - 1)].clone())
        .collect()
}
/// Some GitHub addresses complete TLS with a valid certificate for these hosts but
/// serve something else, and refuse the request with a bare 400 or 403. Every real
/// answer from these hosts carries a request ID, even an error from their cache layer.
pub fn wrong_edge(host: &str, status: u16, headers: &hyper::HeaderMap) -> bool {
    matches!(host, "github.com" | "api.github.com")
        && matches!(status, 400 | 403)
        && !headers.contains_key("x-github-request-id")
}
/// While IPv6 does not work (see `ipv6.rs`) those addresses can only fail, wasting
/// probe slots and delaying the IPv4 candidates queued behind them. An IPv6-only
/// list becomes empty, so the caller moves on to other sources or the upstream.
pub fn reachable(addresses: Vec<SocketAddr>, ipv6: bool) -> Vec<SocketAddr> {
    if ipv6 {
        return addresses;
    }
    addresses.into_iter().filter(SocketAddr::is_ipv4).collect()
}
pub fn probe_delay(index: usize, ordered: bool, no_sni: bool) -> Duration {
    Duration::from_millis(
        (if ordered { index as u64 * 150 } else { 0 }) + if no_sni { 250 } else { 0 },
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wrong_edge_needs_a_covered_host_a_refusal_and_no_request_id() {
        let bare = hyper::HeaderMap::new();
        let mut real = hyper::HeaderMap::new();
        real.insert("x-github-request-id", "8CA1:415C2".parse().unwrap());
        assert!(wrong_edge("github.com", 400, &bare));
        assert!(wrong_edge("api.github.com", 403, &bare));
        assert!(!wrong_edge("github.com", 400, &real));
        assert!(!wrong_edge("github.com", 404, &bare));
        assert!(!wrong_edge("github.githubassets.com", 403, &bare));
    }
    #[test]
    fn unusable_ipv6_is_always_dropped() {
        let v4: SocketAddr = "140.82.112.3:443".parse().unwrap();
        let v6: SocketAddr = "[2606:50c0:8000::154]:443".parse().unwrap();
        assert_eq!(reachable(vec![v6, v4], false), [v4]);
        assert_eq!(reachable(vec![v6, v4], true), [v6, v4]);
        assert!(reachable(vec![v6], false).is_empty());
    }
    #[test]
    fn profiles_and_whole_transfer_learning() {
        assert_eq!(Traffic::classify("github.com", "/a/b"), Traffic::Latency);
        assert_eq!(
            Traffic::classify("github.com", "/a/b.git/git-upload-pack"),
            Traffic::Bulk
        );
        assert_eq!(
            Traffic::classify("release-assets.githubusercontent.com", "/a"),
            Traffic::Bulk
        );
        let p = Performance::default();
        let a = Route::Direct("1.1.1.1:443".parse().unwrap());
        let b = Route::Direct("8.8.8.8:443".parse().unwrap());
        p.record("github.com", &a, 1024 * 1024, Duration::from_secs(10), true);
        assert!(!p.slow("github.com", &a));
        p.record(
            "github.com",
            &a,
            8 * 1024 * 1024,
            Duration::from_secs(16),
            true,
        );
        p.record(
            "github.com",
            &b,
            8 * 1024 * 1024,
            Duration::from_secs(1),
            true,
        );
        assert!(p.slow("github.com", &a));
        assert!(!p.slow("api.github.com", &a));
        assert_eq!(
            p.preferred(
                "github.com",
                &[
                    "1.1.1.1:443".parse().unwrap(),
                    "8.8.8.8:443".parse().unwrap()
                ]
            ),
            Some(b)
        );
        assert!(!slow_flow(1024, Duration::from_secs(1)), "too early to say");
        assert!(slow_flow(1024 * 1024, Duration::from_secs(3)));
        assert!(!slow_flow(8 * 1024 * 1024, Duration::from_secs(3)));
        assert_eq!(spread(&(0..12).collect::<Vec<_>>(), 8).last(), Some(&11));
        assert_eq!(probe_delay(1, false, false), Duration::ZERO);
        assert_eq!(probe_delay(1, true, false), Duration::from_millis(150));
        assert_eq!(probe_delay(1, true, true), Duration::from_millis(400));
    }
}
