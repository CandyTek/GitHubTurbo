//! Sanitized route facts from GitPort (Apache-2.0); see third-party/gitport/NOTICE.
//! No spoofed SNI, verification override or mirrors are imported.
use super::*;
#[derive(Deserialize)]
struct Seed {
    ip: String,
    hosts: Vec<String>,
}
pub fn for_host(host: &str) -> Vec<SocketAddr> {
    static SEEDS: std::sync::OnceLock<Vec<Seed>> = std::sync::OnceLock::new();
    let seeds = SEEDS.get_or_init(|| {
        serde_json::from_str(
            include_str!("../assets/route-seeds.json").trim_start_matches('\u{feff}'),
        )
        .unwrap_or_default()
    });
    let mut out = Vec::new();
    let has_exact = seeds
        .iter()
        .any(|seed| seed.hosts.iter().any(|name| name == host));
    for seed in seeds.iter().filter(|seed| {
        if has_exact {
            return seed.hosts.iter().any(|name| name == host);
        }
        seed.hosts.iter().any(|name| {
            name == host
                || name.strip_prefix("*.").is_some_and(|suffix| {
                    host.strip_suffix(suffix)
                        .is_some_and(|prefix| prefix.ends_with('.'))
                })
        })
    }) {
        for ip in expand(&seed.ip) {
            let address = SocketAddr::new(ip, 443);
            if public_ip(ip) && !out.contains(&address) {
                out.push(address);
            }
        }
    }
    out.truncate(64);
    out
}
fn expand(value: &str) -> Vec<IpAddr> {
    if let Ok(ip) = value.parse() {
        return vec![ip];
    }
    let parts: Vec<_> = value.split('.').collect();
    if parts.len() != 4 {
        return vec![];
    }
    let mut result = vec![Vec::<u8>::new()];
    for part in parts {
        let range = if let Some((a, b)) = part.split_once('-') {
            match (a.parse::<u8>(), b.parse::<u8>()) {
                (Ok(a), Ok(b)) if a <= b => (a, b),
                _ => return vec![],
            }
        } else {
            match part.parse::<u8>() {
                Ok(a) => (a, a),
                _ => return vec![],
            }
        };
        if result.len() * (range.1 as usize - range.0 as usize + 1) > 64 {
            return vec![];
        }
        result = result
            .into_iter()
            .flat_map(|prefix| {
                (range.0..=range.1).map(move |b| {
                    let mut p = prefix.clone();
                    p.push(b);
                    p
                })
            })
            .collect();
    }
    result
        .into_iter()
        .map(|v| IpAddr::from([v[0], v[1], v[2], v[3]]))
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn range_and_scope() {
        assert_eq!(expand("140.82.112-114.3-4").len(), 6);
        assert!(expand("1-255.1-255.1.1").is_empty());
        assert!(!for_host("github.com").is_empty());
        assert!(for_host("github.com.evil.test").is_empty());
        let api = for_host("api.github.com");
        assert!(!api.is_empty());
        assert!(!api.iter().any(|ip| ip.to_string() == "140.82.112.3:443"));
    }
}
