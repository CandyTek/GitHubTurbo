//! Public immutable/static resources only. No HTML/account/session cache.
use super::*;
use bytes::Bytes;
use hyper::{HeaderMap, Method, StatusCode};
struct Entry {
    headers: HeaderMap,
    body: Bytes,
    expires: Instant,
    stored: Instant,
}
#[derive(Default)]
struct Store {
    entries: HashMap<String, Entry>,
    bytes: usize,
}
pub struct Cache {
    pub collecting: Arc<Semaphore>,
    store: std::sync::Mutex<Store>,
    pub hits: AtomicU64,
    pub misses: AtomicU64,
}
impl Default for Cache {
    fn default() -> Self {
        Self {
            collecting: Arc::new(Semaphore::new(4)),
            store: Default::default(),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }
}
pub fn sensitive(uri: &hyper::Uri, headers: &HeaderMap) -> bool {
    if headers.contains_key("authorization")
        || headers.contains_key("cookie")
        || headers.contains_key("proxy-authorization")
    {
        return true;
    }
    uri.query().is_some_and(|query| {
        query.split('&').any(|pair| {
            let key = percent_encoding::percent_decode_str(pair.split('=').next().unwrap_or(""))
                .decode_utf8_lossy()
                .to_ascii_lowercase();
            key.starts_with("x-amz-")
                || [
                    "token",
                    "access_token",
                    "auth",
                    "authorization",
                    "jwt",
                    "sig",
                    "signature",
                    "key",
                    "credential",
                    "se",
                ]
                .contains(&key.as_str())
        })
    })
}
pub fn key(method: &Method, host: &str, uri: &hyper::Uri, headers: &HeaderMap) -> Option<String> {
    if *method != Method::GET
        || sensitive(uri, headers)
        || headers.contains_key("range")
        || headers.contains_key("if-none-match")
        || headers.contains_key("if-modified-since")
        || headers.contains_key("if-match")
        || headers.contains_key("if-unmodified-since")
    {
        return None;
    }
    if headers
        .get("cache-control")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| {
            let s = s.to_ascii_lowercase();
            s.contains("no-cache") || s.contains("no-store") || s.contains("max-age=0")
        })
        || headers.contains_key("pragma")
    {
        return None;
    }
    let static_host = host.ends_with(".githubassets.com")
        || host == "raw.githubusercontent.com"
        || host == "avatars.githubusercontent.com"
        || host == "media.githubusercontent.com";
    if !static_host {
        return None;
    }
    let variants = ["accept", "accept-encoding", "accept-language"].map(|name| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
    });
    Some(format!(
        "{host}{uri}\n{}\n{}\n{}",
        variants[0], variants[1], variants[2]
    ))
}
fn lifetime(status: StatusCode, headers: &HeaderMap) -> Option<Duration> {
    if status != StatusCode::OK || headers.contains_key("set-cookie") {
        return None;
    }
    if headers.get_all("vary").iter().any(|v| {
        v.to_str().map_or(true, |s| {
            s.split(',').any(|k| {
                !["accept", "accept-encoding", "accept-language"]
                    .contains(&k.trim().to_ascii_lowercase().as_str())
            })
        })
    }) {
        return None;
    }
    let directives = headers
        .get_all("cache-control")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect::<Vec<_>>()
        .join(",")
        .to_ascii_lowercase();
    if directives.split(',').any(|s| {
        matches!(
            s.trim().split('=').next().unwrap_or(""),
            "private" | "no-store" | "no-cache"
        )
    }) || !directives.split(',').any(|s| s.trim() == "public")
    {
        return None;
    }
    let max = directives.split(',').find_map(|s| {
        s.trim()
            .strip_prefix("max-age=")
            .and_then(|v| v.trim_matches('"').parse::<u64>().ok())
    })?;
    let age = headers
        .get("age")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let remaining = max.saturating_sub(age).min(3600);
    if remaining == 0 {
        None
    } else {
        Some(Duration::from_secs(remaining))
    }
}
impl Cache {
    pub const MAX_ENTRY: usize = 2 * 1024 * 1024;
    pub fn eligible(status: StatusCode, headers: &HeaderMap) -> bool {
        lifetime(status, headers).is_some()
    }
    pub fn get(&self, key: &str) -> Option<(HeaderMap, Bytes)> {
        let mut store = self.store.lock().unwrap();
        if let Some(entry) = store.entries.get(key) {
            if entry.expires > Instant::now() {
                self.hits.fetch_add(1, Ordering::Relaxed);
                let mut headers = entry.headers.clone();
                let old_age = headers
                    .get("age")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(0);
                headers.insert(
                    "age",
                    old_age
                        .saturating_add(entry.stored.elapsed().as_secs())
                        .to_string()
                        .parse()
                        .unwrap(),
                );
                return Some((headers, entry.body.clone()));
            }
        }
        if let Some(old) = store.entries.remove(key) {
            store.bytes -= old.body.len();
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        None
    }
    pub fn put(&self, key: String, status: StatusCode, headers: HeaderMap, body: Bytes) {
        let Some(life) = lifetime(status, &headers) else {
            return;
        };
        if body.len() > Self::MAX_ENTRY {
            return;
        }
        let mut store = self.store.lock().unwrap();
        if let Some(old) = store.entries.remove(&key) {
            store.bytes -= old.body.len();
        }
        while store.bytes + body.len() > 16 * 1024 * 1024 || store.entries.len() >= 128 {
            let Some(oldest) = store
                .entries
                .iter()
                .min_by_key(|(_, e)| e.stored)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            if let Some(old) = store.entries.remove(&oldest) {
                store.bytes -= old.body.len();
            }
        }
        store.bytes += body.len();
        let now = Instant::now();
        store.entries.insert(
            key,
            Entry {
                headers,
                body,
                expires: now + life,
                stored: now,
            },
        );
    }
    pub fn snapshot(&self) -> serde_json::Value {
        let store = self.store.lock().unwrap();
        serde_json::json!({"entries":store.entries.len(),"bytes":store.bytes,"limit_bytes":16*1024*1024,"hits":self.hits.load(Ordering::Relaxed),"misses":self.misses.load(Ordering::Relaxed)})
    }
    pub fn clear(&self) {
        *self.store.lock().unwrap() = Store::default();
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn privacy_and_variants() {
        let uri = "/asset.js".parse().unwrap();
        let mut h = HeaderMap::new();
        assert!(key(&Method::GET, "github.githubassets.com", &uri, &h).is_some());
        h.insert("Cookie", "session=x".parse().unwrap());
        assert!(key(&Method::GET, "github.githubassets.com", &uri, &h).is_none());
        h.clear();
        assert!(sensitive(&"/asset?%74oken=secret".parse().unwrap(), &h));
        assert!(key(&Method::GET, "github.com", &uri, &h).is_none());
        h.insert("Cache-Control", "public,max-age=20".parse().unwrap());
        assert!(Cache::eligible(StatusCode::OK, &h));
        h.insert("Set-Cookie", "x=y".parse().unwrap());
        assert!(!Cache::eligible(StatusCode::OK, &h));
    }
    #[test]
    fn bounded_storage_and_cache_control() {
        let cache = Cache::default();
        let mut h = HeaderMap::new();
        h.insert("cache-control", "public,max-age=20".parse().unwrap());
        cache.put(
            "a".into(),
            StatusCode::OK,
            h.clone(),
            Bytes::from_static(b"asset"),
        );
        assert_eq!(cache.get("a").unwrap().1, "asset");
        h.insert("cache-control", "private,max-age=20".parse().unwrap());
        cache.put(
            "b".into(),
            StatusCode::OK,
            h,
            Bytes::from_static(b"private"),
        );
        assert!(cache.get("b").is_none());
    }
}
