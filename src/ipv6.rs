//! Is IPv6 actually usable on this machine?
//!
//! Three layers, cheapest first:
//!
//! 1. **Local check** (no packets): connecting a UDP socket only consults the
//!    routing table. The chosen source address must be native global unicast.
//!    Windows' Teredo tunnel (`2001:0::/32`), 6to4 (`2002::/16`), unique-local and
//!    link-local addresses do not count: they produce a "route" with no reliable
//!    way out, which used to make every IPv6 candidate wait for a connect timeout.
//! 2. **One real test** before IPv6 is trusted: a TCP handshake (no data) to two
//!    well-known public resolvers at most every 30 minutes, capped at 1.5 s. Until
//!    it succeeds IPv6 counts as unusable, so a broken network never delays the
//!    first page load; IPv4 is used meanwhile.
//! 3. **Real traffic**: three IPv6 connection failures in a row with no success
//!    in between switch IPv6 off for 10 minutes. Any success resets the count.
//!
//! "Re-route" (`reset`) forgets layers 2 and 3.
use std::{
    net::{Ipv6Addr, SocketAddr},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const ROUTE_CHECK_EVERY: Duration = Duration::from_secs(60);
const VERIFIED_FOR: Duration = Duration::from_secs(30 * 60);
const BROKEN_FOR: Duration = Duration::from_secs(10 * 60);
const TEST_TIMEOUT: Duration = Duration::from_millis(1500);
const FAILURES_TO_GIVE_UP: u32 = 3;
/// Public resolvers on IPv6. Only a TCP handshake is made; nothing is sent.
const TEST_TARGETS: [&str; 2] = ["[2606:4700:4700::1111]:443", "[2001:4860:4860::8888]:443"];

/// Native, globally routed unicast: not Teredo, 6to4, ULA, link-local or special.
pub fn native_global(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    (s[0] & 0xe000) == 0x2000 // 2000::/3 global unicast
        && !(s[0] == 0x2001 && s[1] == 0) // Teredo
        && s[0] != 0x2002 // 6to4
        && !(s[0] == 0x2001 && s[1] == 0x0db8) // documentation
}

/// Layer 1. Sends nothing.
fn local_route() -> bool {
    let Ok(socket) = std::net::UdpSocket::bind("[::]:0") else {
        return false;
    };
    if socket.connect("[2606:4700:4700::1111]:53").is_err() {
        return false;
    }
    matches!(socket.local_addr(), Ok(SocketAddr::V6(local)) if native_global(*local.ip()))
}

#[derive(Default)]
struct Inner {
    route: Option<(Instant, bool)>,
    /// Result of the last real test or of real traffic, and when.
    verdict: Option<(Instant, bool)>,
    testing: bool,
    failures: u32,
}

#[derive(Clone, Default)]
pub struct Ipv6 {
    inner: Arc<Mutex<Inner>>,
    /// Test hook: replaces the local route check.
    #[cfg(test)]
    pub forced_route: Option<bool>,
}

impl Ipv6 {
    #[cfg(test)]
    pub fn with_route(route: bool) -> Self {
        Self {
            forced_route: Some(route),
            ..Default::default()
        }
    }
    fn route(&self, inner: &mut Inner) -> bool {
        #[cfg(test)]
        if let Some(route) = self.forced_route {
            return route;
        }
        match inner.route {
            Some((at, route)) if at.elapsed() < ROUTE_CHECK_EVERY => route,
            _ => {
                let route = local_route();
                if inner.route.is_some_and(|(_, before)| before != route) {
                    // The network changed; earlier verdicts no longer apply.
                    inner.verdict = None;
                    inner.failures = 0;
                }
                inner.route = Some((Instant::now(), route));
                route
            }
        }
    }

    /// Cheap enough to call per connection: a lock, and at most one routing-table
    /// lookup per minute. May start a background test.
    pub fn usable(&self) -> bool {
        let mut inner = self.inner.lock().unwrap();
        if !self.route(&mut inner) {
            return false;
        }
        let (current, expired) = match inner.verdict {
            Some((at, true)) => (true, at.elapsed() >= VERIFIED_FOR),
            Some((at, false)) => (false, at.elapsed() >= BROKEN_FOR),
            None => (false, true),
        };
        if expired && !inner.testing {
            self.start_test(&mut inner);
        }
        current
    }

    fn start_test(&self, inner: &mut Inner) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        inner.testing = true;
        let this = self.clone();
        runtime.spawn(async move {
            let ok = handshake().await;
            let mut inner = this.inner.lock().unwrap();
            inner.testing = false;
            inner.verdict = Some((Instant::now(), ok));
            inner.failures = 0;
        });
    }

    /// Layer 3: the outcome of a real TCP connect to an IPv6 address.
    pub fn record(&self, connected: bool) {
        let mut inner = self.inner.lock().unwrap();
        if connected {
            inner.failures = 0;
            inner.verdict = Some((Instant::now(), true));
            return;
        }
        inner.failures += 1;
        if inner.failures >= FAILURES_TO_GIVE_UP {
            inner.failures = 0;
            inner.verdict = Some((Instant::now(), false));
        }
    }

    pub fn reset(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.route = None;
        inner.verdict = None;
        inner.failures = 0;
    }

    pub fn snapshot(&self) -> serde_json::Value {
        let usable = self.usable();
        let inner = self.inner.lock().unwrap();
        let verdict = match inner.verdict {
            None if inner.testing => "testing",
            None => "unknown",
            Some((_, true)) => "working",
            Some((_, false)) => "broken",
        };
        serde_json::json!({
            "usable": usable,
            "route": inner.route.map(|(_, r)| r),
            "verdict": verdict,
            "recent_failures": inner.failures,
        })
    }
}

/// Layer 2: either target completing a TCP handshake proves IPv6 works.
async fn handshake() -> bool {
    let attempts = TEST_TARGETS.map(|target| async move {
        let address: SocketAddr = target.parse().ok()?;
        tokio::net::TcpStream::connect(address)
            .await
            .ok()
            .map(|_| ())
    });
    let [a, b] = attempts;
    let race = async {
        tokio::pin!(a, b);
        tokio::select! {
            r = &mut a => if r.is_some() { true } else { b.await.is_some() },
            r = &mut b => if r.is_some() { true } else { a.await.is_some() },
        }
    };
    tokio::time::timeout(TEST_TIMEOUT, race)
        .await
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tunnels_and_local_ranges_are_not_native() {
        for (ip, native) in [
            ("2409:8a00:1:2::5", true),
            ("2606:4700:4700::1111", true),
            ("2001:0:4136:e378:8000:63bf:3fff:fdd2", false), // Teredo
            ("2002:c000:204::1", false),                     // 6to4
            ("fd00::1", false),
            ("fe80::1", false),
            ("::1", false),
            ("2001:db8::1", false),
        ] {
            assert_eq!(native_global(ip.parse().unwrap()), native, "{ip}");
        }
    }

    #[test]
    fn no_route_is_never_usable() {
        let v6 = Ipv6 {
            forced_route: Some(false),
            ..Default::default()
        };
        v6.record(true);
        assert!(!v6.usable());
    }

    #[test]
    fn unproven_is_unusable_and_traffic_decides() {
        let v6 = Ipv6 {
            forced_route: Some(true),
            ..Default::default()
        };
        // No runtime here, so no background test starts: still unproven.
        assert!(!v6.usable());
        v6.record(true);
        assert!(v6.usable());
        v6.record(false);
        v6.record(false);
        assert!(v6.usable(), "two failures are not yet a verdict");
        v6.record(true);
        for _ in 0..FAILURES_TO_GIVE_UP {
            v6.record(false);
        }
        assert!(!v6.usable(), "three failures in a row switch IPv6 off");
        v6.reset();
        v6.record(true);
        assert!(v6.usable());
    }
}
