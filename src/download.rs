//! Bounded range capability memo. No URLs or credentials in diagnostics.
use super::*;
use std::hash::{Hash, Hasher};
#[derive(Default)]
pub struct Memo {
    entries: std::sync::Mutex<HashMap<u64, Instant>>,
    hits: AtomicU64,
}
impl Memo {
    pub fn key(host: &str, uri: &hyper::Uri, etag: &str, length: u64, route: &Route) -> u64 {
        let mut key = std::collections::hash_map::DefaultHasher::new();
        (host, uri.path(), etag, length, route).hash(&mut key);
        key.finish()
    }
    pub fn blocked(&self, key: u64) -> bool {
        let mut entries = self.entries.lock().unwrap();
        if entries
            .get(&key)
            .is_some_and(|expires| *expires > Instant::now())
        {
            self.hits.fetch_add(1, Ordering::Relaxed);
            true
        } else {
            entries.remove(&key);
            false
        }
    }
    pub fn reject(&self, key: u64) {
        let mut entries = self.entries.lock().unwrap();
        if entries.len() >= 512 && !entries.contains_key(&key) {
            entries.retain(|_, expires| *expires > Instant::now());
            if entries.len() >= 512 {
                if let Some(old) = entries
                    .iter()
                    .min_by_key(|(_, expires)| **expires)
                    .map(|(key, _)| *key)
                {
                    entries.remove(&old);
                }
            }
        }
        entries.insert(key, Instant::now() + Duration::from_secs(600));
    }
    pub fn clear(&self) {
        self.entries.lock().unwrap().clear();
    }
    pub fn snapshot(&self) -> serde_json::Value {
        serde_json::json!({"entries":self.entries.lock().unwrap().len(),"avoided_trials":self.hits.load(Ordering::Relaxed),"capacity":512,"ttl_seconds":600})
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn negative_memo_is_version_scoped_bounded_and_ephemeral() {
        let memo = Memo::default();
        let uri = "/asset?jwt=first".parse().unwrap();
        let route = Route::Direct("1.1.1.1:443".parse().unwrap());
        let key = Memo::key(
            "release-assets.githubusercontent.com",
            &uri,
            "\"v1\"",
            100,
            &route,
        );
        memo.reject(key);
        assert!(memo.blocked(Memo::key(
            "release-assets.githubusercontent.com",
            &"/asset?jwt=second".parse().unwrap(),
            "\"v1\"",
            100,
            &route
        )));
        assert!(!memo.blocked(Memo::key(
            "release-assets.githubusercontent.com",
            &uri,
            "\"v2\"",
            100,
            &route
        )));
        assert!(!memo.blocked(Memo::key(
            "release-assets.githubusercontent.com",
            &uri,
            "\"v1\"",
            100,
            &Route::Direct("8.8.8.8:443".parse().unwrap())
        )));
        for key in 0..600 {
            memo.reject(key);
        }
        assert_eq!(memo.snapshot()["entries"], 512);
        memo.clear();
        assert!(!memo.blocked(key));
    }
}
