//! A circuit breaker per host.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Stops calling a host that keeps failing: after `threshold` failures in
/// a row (connection errors, timeouts, 5xx) calls fail at once for
/// `cooldown`; then one call is let through to probe, and its result
/// closes or reopens the circuit.
#[derive(Debug)]
pub(crate) struct Breaker {
    threshold: u32,
    cooldown: Duration,
    hosts: Mutex<HashMap<String, HostState>>,
}

#[derive(Debug, Default)]
struct HostState {
    failures: u32,
    open_until: Option<Instant>,
    probing: bool,
}

/// Whether a call may go ahead.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Permit {
    Go,
    /// The circuit is open; retry after this long.
    Open(Duration),
}

impl Breaker {
    pub(crate) fn new(threshold: u32, cooldown: Duration) -> Self {
        Self { threshold: threshold.max(1), cooldown, hosts: Mutex::new(HashMap::new()) }
    }

    pub(crate) fn before(&self, host: &str) -> Permit {
        let mut hosts = self.hosts.lock().unwrap_or_else(|e| e.into_inner());
        let state = hosts.entry(host.to_string()).or_default();
        match state.open_until {
            None => Permit::Go,
            Some(until) => {
                let now = Instant::now();
                if now < until {
                    Permit::Open(until - now)
                } else if state.probing {
                    // One probe at a time while half-open.
                    Permit::Open(Duration::ZERO)
                } else {
                    state.probing = true;
                    Permit::Go
                }
            }
        }
    }

    /// Whether calls to `host` are refused right now.
    pub(crate) fn is_open(&self, host: &str) -> bool {
        let hosts = self.hosts.lock().unwrap_or_else(|e| e.into_inner());
        hosts.get(host).and_then(|state| state.open_until).is_some_and(|until| Instant::now() < until)
    }

    /// Records a call's outcome; returns true when this opened the circuit.
    pub(crate) fn after(&self, host: &str, failed: bool) -> bool {
        let mut hosts = self.hosts.lock().unwrap_or_else(|e| e.into_inner());
        let state = hosts.entry(host.to_string()).or_default();
        let probe = std::mem::take(&mut state.probing);
        if !failed {
            *state = HostState::default();
            return false;
        }
        state.failures += 1;
        if probe || state.failures >= self.threshold {
            state.failures = 0;
            state.open_until = Some(Instant::now() + self.cooldown);
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_after_failures_then_probes() {
        let breaker = Breaker::new(3, Duration::from_millis(50));
        assert_eq!(breaker.before("a"), Permit::Go);
        assert!(!breaker.after("a", true));
        assert!(!breaker.after("a", true));
        assert!(!breaker.after("a", false), "a success resets the count");
        assert!(!breaker.after("a", true));
        assert!(!breaker.after("a", true));
        assert!(breaker.after("a", true), "third failure in a row opens");
        assert!(matches!(breaker.before("a"), Permit::Open(_)));
        assert_eq!(breaker.before("b"), Permit::Go, "per host");

        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(breaker.before("a"), Permit::Go, "half-open: one probe");
        assert_eq!(breaker.before("a"), Permit::Open(Duration::ZERO), "only one");
        assert!(breaker.after("a", true), "a failed probe reopens at once");
        assert!(matches!(breaker.before("a"), Permit::Open(_)));

        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(breaker.before("a"), Permit::Go);
        assert!(!breaker.after("a", false), "a good probe closes");
        assert_eq!(breaker.before("a"), Permit::Go);
        assert_eq!(breaker.before("a"), Permit::Go);
    }
}
