//! Rate-limits repeated warnings to one per (peer, reason) per window.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

pub struct WarnLimiter {
    window: Duration,
    last: HashMap<(Ipv4Addr, &'static str), Instant>,
}

impl WarnLimiter {
    pub fn new(window: Duration) -> Self {
        Self { window, last: HashMap::new() }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.last.len()
    }

    /// True if this (peer, reason) warning should be logged now.
    pub fn allow(&mut self, peer: Ipv4Addr, reason: &'static str, now: Instant) -> bool {
        match self.last.get(&(peer, reason)) {
            Some(at) if now.duration_since(*at) < self.window => false,
            _ => {
                self.last.insert((peer, reason), now);
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_once_per_window_per_peer_and_reason() {
        let mut limiter = WarnLimiter::new(Duration::from_secs(60));
        let peer = Ipv4Addr::new(10, 0, 0, 1);
        let t = Instant::now();
        assert!(limiter.allow(peer, "bad signature", t));
        assert!(!limiter.allow(peer, "bad signature", t + Duration::from_secs(59)));
        assert!(limiter.allow(peer, "wrong group", t));
        assert!(limiter.allow(Ipv4Addr::new(10, 0, 0, 2), "bad signature", t));
        assert!(limiter.allow(peer, "bad signature", t + Duration::from_secs(60)));
        // A clock reading before the stored one counts as no time having passed.
        assert!(!limiter.allow(peer, "bad signature", t));
    }
}
