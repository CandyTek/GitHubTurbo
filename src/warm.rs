//! Briefly retain already-verified probe TLS streams for the actual upstream request.
use super::*;
type Stream = tokio_native_tls::TlsStream<TcpStream>;
type Key = (String, Route, bool);
#[derive(Default)]
pub struct Warm {
    entries: Mutex<HashMap<Key, (Instant, Stream)>>,
    reused: AtomicU64,
}
impl Warm {
    pub async fn put(&self, host: &str, route: Route, h2: bool, stream: Stream) {
        let mut entries = self.entries.lock().await;
        entries.retain(|_, (at, _)| at.elapsed() < Duration::from_secs(3));
        if entries.len() >= 16 {
            if let Some(key) = entries
                .iter()
                .min_by_key(|(_, (at, _))| *at)
                .map(|(key, _)| key.clone())
            {
                entries.remove(&key);
            }
        }
        entries.insert((host.into(), route, h2), (Instant::now(), stream));
    }
    pub async fn take(&self, host: &str, route: &Route, h2: bool) -> Option<Stream> {
        let mut entries = self.entries.lock().await;
        entries.retain(|_, (at, _)| at.elapsed() < Duration::from_secs(3));
        let (_, stream) = entries.remove(&(host.into(), route.clone(), h2))?;
        self.reused.fetch_add(1, Ordering::Relaxed);
        Some(stream)
    }
    pub async fn clear(&self) {
        self.entries.lock().await.clear();
    }
    pub async fn snapshot(&self) -> serde_json::Value {
        self.prune().await;
        serde_json::json!({"retained":self.entries.lock().await.len(),"reused":self.reused.load(Ordering::Relaxed),"capacity":16,"max_age_seconds":3})
    }
    pub async fn prune(&self) {
        self.entries
            .lock()
            .await
            .retain(|_, (at, _)| at.elapsed() < Duration::from_secs(3));
    }
}
