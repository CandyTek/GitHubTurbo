//! Per-destination circuit breakers and EWMA, inspired by GitPort's edge bookkeeping.
//! Independent implementation: no credential/session data and no shared certificate anchors.
use super::*;
use std::collections::VecDeque;
#[derive(Clone, Copy)]
pub enum Failure {
    Network,
    Rejected,
    Throttled,
    Server,
}
#[derive(Clone)]
struct Edge {
    failures: u32,
    good: u64,
    ewma: f64,
    until: Instant,
    sticky: bool,
}
#[derive(Default)]
pub struct Health {
    edges: std::sync::Mutex<HashMap<(String, Route), Edge>>,
    events: std::sync::Mutex<VecDeque<serde_json::Value>>,
}
impl Health {
    pub fn bad(&self, host: &str, route: &Route) -> bool {
        self.edges
            .lock()
            .unwrap()
            .get(&(host.to_owned(), route.clone()))
            .is_some_and(|edge| edge.until > Instant::now())
    }
    pub fn success(&self, host: &str, route: &Route, millis: f64) {
        let mut edges = self.edges.lock().unwrap();
        if edges.len() >= 2048 && !edges.contains_key(&(host.into(), route.clone())) {
            edges.retain(|_, e| e.until > Instant::now());
            if edges.len() >= 2048 {
                return;
            }
        }
        let edge = edges.entry((host.into(), route.clone())).or_insert(Edge {
            failures: 0,
            good: 0,
            ewma: millis,
            until: Instant::now(),
            sticky: false,
        });
        edge.good += 1;
        edge.ewma = edge.ewma * 0.8 + millis * 0.2;
        if !edge.sticky || edge.until <= Instant::now() {
            edge.failures = 0;
            edge.until = Instant::now();
            edge.sticky = false;
        }
    }
    pub fn fail(&self, host: &str, route: &Route, kind: Failure) {
        let now = Instant::now();
        let mut edges = self.edges.lock().unwrap();
        if edges.len() >= 2048 && !edges.contains_key(&(host.into(), route.clone())) {
            edges.retain(|_, e| e.until > now);
            if edges.len() >= 2048 {
                return;
            }
        }
        let edge = edges.entry((host.into(), route.clone())).or_insert(Edge {
            failures: 0,
            good: 0,
            ewma: 1000.,
            until: now,
            sticky: false,
        });
        edge.failures = edge.failures.saturating_add(1);
        let (secs, sticky) = match kind {
            Failure::Network => (
                (15u64.saturating_mul(1 << edge.failures.min(5))).min(300),
                false,
            ),
            Failure::Rejected => (3600, false),
            Failure::Throttled => (300, true),
            Failure::Server => (20, false),
        };
        if !edge.sticky || edge.until <= now || sticky {
            edge.until = now + Duration::from_secs(secs);
            edge.sticky = sticky;
        }
        drop(edges);
        self.event(host, "route-cooldown");
    }
    pub fn score(&self, host: &str, route: &Route) -> f64 {
        self.edges
            .lock()
            .unwrap()
            .get(&(host.into(), route.clone()))
            .map(|e| e.ewma)
            .unwrap_or(500.)
    }
    pub fn event(&self, host: &str, kind: &str) {
        let mut events = self.events.lock().unwrap();
        if events.len() >= 100 {
            events.pop_front();
        }
        events.push_back(serde_json::json!({"host":host,"event":kind,"utc":time::OffsetDateTime::now_utc().unix_timestamp()}));
    }
    pub fn snapshot(&self) -> serde_json::Value {
        let edges = self.edges.lock().unwrap();
        let now = Instant::now();
        serde_json::json!({"edges":edges.iter().map(|((host,route),e)|serde_json::json!({"host":host,"route":route,"successes":e.good,"failures":e.failures,"ewma_ms":e.ewma as u64,"cooldown_seconds":e.until.saturating_duration_since(now).as_secs()})).collect::<Vec<_>>(),"events":*self.events.lock().unwrap()})
    }
    pub fn forget(&self, host: &str) {
        self.edges
            .lock()
            .unwrap()
            .retain(|(name, _), _| name != host);
    }
    pub fn clear(&self) {
        self.edges.lock().unwrap().clear();
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn failures_are_scoped_and_throttle_is_sticky() {
        let h = Health::default();
        let r = Route::Direct("1.1.1.1:443".parse().unwrap());
        h.fail("github.com", &r, Failure::Throttled);
        h.success("github.com", &r, 1.);
        assert!(h.bad("github.com", &r));
        assert!(!h.bad("api.github.com", &r));
        h.fail("github.com", &r, Failure::Network);
        assert!(h.bad("github.com", &r));
        h.clear();
        assert!(!h.bad("github.com", &r));
    }
}
