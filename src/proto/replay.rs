//! Replay protection (spec §6.4).

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
struct Seen {
    boot_id: u64,
    seq: u64,
    at: Instant,
    interval: Duration,
}

/// Remembers the newest packet accepted from each peer.
#[derive(Debug, Default)]
pub struct ReplayGuard {
    peers: HashMap<Ipv4Addr, Seen>,
}

impl ReplayGuard {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns true, and records the packet, if it is newer than anything accepted from `peer`.
    pub fn accept(&mut self, peer: Ipv4Addr, boot_id: u64, seq: u64, interval: Duration, now: Instant) -> bool {
        let fresh = match self.peers.get(&peer) {
            None => true,
            Some(last) => {
                (boot_id == last.boot_id && seq > last.seq)
                    || boot_id > last.boot_id
                    || now.duration_since(last.at) >= last.interval * 3
            }
        };
        if fresh {
            self.peers.insert(peer, Seen { boot_id, seq, at: now, interval });
        }
        fresh
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PEER: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 1);
    const SEC: Duration = Duration::from_secs(1);

    #[test]
    fn the_first_packet_from_a_peer_is_accepted() {
        let mut guard = ReplayGuard::new();
        assert!(guard.accept(PEER, 100, 1, SEC, Instant::now()));
    }

    #[test]
    fn the_same_boot_needs_a_higher_seq() {
        let mut guard = ReplayGuard::new();
        let t = Instant::now();
        assert!(guard.accept(PEER, 100, 5, SEC, t));
        assert!(!guard.accept(PEER, 100, 5, SEC, t));
        assert!(!guard.accept(PEER, 100, 4, SEC, t));
        assert!(guard.accept(PEER, 100, 6, SEC, t));
    }

    #[test]
    fn a_newer_boot_is_accepted_and_an_older_one_is_not() {
        let mut guard = ReplayGuard::new();
        let t = Instant::now();
        assert!(guard.accept(PEER, 100, 50, SEC, t));
        assert!(guard.accept(PEER, 200, 1, SEC, t));
        assert!(!guard.accept(PEER, 100, 51, SEC, t));
    }

    #[test]
    fn any_boot_is_accepted_after_three_silent_intervals() {
        let mut guard = ReplayGuard::new();
        let t = Instant::now();
        assert!(guard.accept(PEER, 200, 9, SEC, t));
        assert!(!guard.accept(PEER, 100, 1, SEC, t + SEC * 2));
        assert!(guard.accept(PEER, 100, 1, SEC, t + SEC * 3));
    }

    #[test]
    fn peers_are_tracked_separately() {
        let mut guard = ReplayGuard::new();
        let t = Instant::now();
        assert!(guard.accept(PEER, 100, 5, SEC, t));
        assert!(guard.accept(Ipv4Addr::new(10, 0, 0, 9), 100, 1, SEC, t));
    }
}
