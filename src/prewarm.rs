//! Open the connections a browser is about to need before it asks for them.
//!
//! A cold page load pays for DNS, route probing and TLS on each host at once. This
//! connects the hosts nearly every page uses when the engine starts, then keeps a
//! connection ready for any of them used within the last hour. Nothing is sent over
//! these connections until a real request arrives, and once GitHub has gone unused
//! for an hour the engine stops opening connections to it.
use super::*;
use tokio::task::{AbortHandle, JoinSet};

const HOSTS: &[&str] = &[
    "github.com",
    "api.github.com",
    "github.githubassets.com",
    "avatars.githubusercontent.com",
    "raw.githubusercontent.com",
];
const STOP_AFTER_IDLE: Duration = Duration::from_secs(3600);
// Route probes share a small pool, and real requests must not queue behind these.
const AT_ONCE: usize = 2;

#[derive(Default)]
pub struct Prewarm {
    used: std::sync::Mutex<HashMap<&'static str, Instant>>,
    task: std::sync::Mutex<Option<AbortHandle>>,
    opened: AtomicU64,
    failed: AtomicU64,
}
impl Prewarm {
    pub fn touch(&self, host: &str) {
        if let Some(name) = HOSTS.iter().find(|name| **name == host) {
            self.used.lock().unwrap().insert(name, Instant::now());
        }
    }
    fn in_use(&self) -> Vec<&'static str> {
        let mut used = self.used.lock().unwrap();
        used.retain(|_, at| at.elapsed() < STOP_AFTER_IDLE);
        used.keys().copied().collect()
    }
    /// Forget everything and cancel connecting. After a reset the network may have
    /// changed, so a connection still being opened must not repopulate the routes.
    pub fn clear(&self) {
        self.used.lock().unwrap().clear();
        if let Some(task) = self.task.lock().unwrap().take() {
            task.abort();
        }
    }
    pub fn snapshot(&self) -> serde_json::Value {
        serde_json::json!({
            "opened": self.opened.load(Ordering::Relaxed),
            "failed": self.failed.load(Ordering::Relaxed),
            "hosts_in_use": self.in_use().len(),
        })
    }
}
fn enabled(state: &State) -> bool {
    // Only the transparent front end sends requests through the shared pools.
    state.config.transparent.enabled && state.optimization().prewarm
}
pub fn start(state: &Arc<State>) {
    launch(state, HOSTS);
}
pub fn keep_hot(state: &Arc<State>) {
    launch(state, &state.prewarm.in_use());
}
fn launch(state: &Arc<State>, hosts: &[&'static str]) {
    if hosts.is_empty() || !enabled(state) {
        return;
    }
    let mut task = state.prewarm.task.lock().unwrap();
    if task.as_ref().is_some_and(|task| !task.is_finished()) {
        return;
    }
    let worker = state.clone();
    let hosts = hosts.to_vec();
    *task = Some(tokio::spawn(async move { connect(&worker, &hosts).await }).abort_handle());
}
async fn connect(state: &Arc<State>, hosts: &[&'static str]) {
    let slots = Arc::new(Semaphore::new(AT_ONCE));
    // Dropping the set when this task is aborted also cancels each connection attempt.
    let mut jobs = JoinSet::new();
    for host in hosts.iter().copied() {
        let state = state.clone();
        let slots = slots.clone();
        jobs.spawn(async move {
            let _slot = slots.acquire().await;
            match state.pool.prewarm(&state, host).await {
                Ok(true) => {
                    state.prewarm.opened.fetch_add(1, Ordering::Relaxed);
                }
                Ok(false) => {}
                Err(error) => {
                    // The machine may simply not be online yet. Cooldowns from this
                    // attempt must not make the first real request fail outright.
                    state.health.forget(host);
                    state.prewarm.failed.fetch_add(1, Ordering::Relaxed);
                    eprintln!("prewarm {host}: {error}");
                }
            }
        });
    }
    while jobs.join_next().await.is_some() {}
}
#[cfg(test)]
pub async fn connect_for_test(state: &Arc<State>, host: &'static str) {
    connect(state, &[host]).await;
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_known_hosts_in_recent_use_are_kept_hot() {
        let prewarm = Prewarm::default();
        assert!(prewarm.in_use().is_empty());
        prewarm.touch("codeload.github.com");
        prewarm.touch("gist.github.com");
        assert!(prewarm.in_use().is_empty());
        prewarm.touch("github.com");
        assert_eq!(prewarm.in_use(), ["github.com"]);
        if let Some(earlier) = Instant::now().checked_sub(STOP_AFTER_IDLE) {
            prewarm.used.lock().unwrap().insert("github.com", earlier);
            assert!(prewarm.in_use().is_empty());
        }
        prewarm.touch("github.com");
        prewarm.clear();
        assert!(prewarm.in_use().is_empty());
    }
}
