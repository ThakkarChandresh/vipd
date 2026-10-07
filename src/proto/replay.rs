//! Replay protection (spec §6.4).

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
struct Seen {
    /// The last packet accepted.
    boot_id: u64,
    seq: u64,
    at: Instant,
    interval: Duration,
    /// The newest run of the peer ever accepted and its highest `seq`. A packet from that run
    /// always needs a higher `seq`, even after an older run has been accepted since.
    newest_boot: u64,
    newest_seq: u64,
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
    ///
    /// Within the newest run of a peer (its highest `boot_id`), and within the last run accepted,
    /// only a higher `seq` is accepted, however long the peer has been silent. Any other, older
    /// `boot_id` is accepted after three silent intervals: a peer restarted with its clock set
    /// back.
    ///
    /// Known limit: once one packet of an older run is accepted, the rest of that run is too, and
    /// a freshly started node accepts any run. So whoever captured a stretch of a peer's
    /// heartbeats can replay it while that peer is silent, and keep this node from becoming master
    /// for as long as the capture lasts. Closing this needs a boot counter that survives restarts.
    ///
    /// Preconditions (both guaranteed by the runtime): `interval` comes from a decoded heartbeat,
    /// which `Codec::decode` limits to 50 ms – 60 s, and `peer` is one of the configured peers,
    /// which keeps this map small.
    pub fn accept(&mut self, peer: Ipv4Addr, boot_id: u64, seq: u64, interval: Duration, now: Instant) -> bool {
        let seen = self.peers.get(&peer).copied();
        let fresh = match seen {
            None => true,
            Some(s) if boot_id == s.newest_boot => seq > s.newest_seq,
            Some(s) if boot_id > s.newest_boot => true,
            Some(s) if boot_id == s.boot_id => seq > s.seq,
            Some(s) => now.duration_since(s.at) >= s.interval * 3,
        };
        if fresh {
            let (newest_boot, newest_seq) = match seen {
                Some(s) if boot_id < s.newest_boot => (s.newest_boot, s.newest_seq),
                _ => (boot_id, seq),
            };
            self.peers.insert(peer, Seen { boot_id, seq, at: now, interval, newest_boot, newest_seq });
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
    fn an_older_boot_is_accepted_after_three_silent_intervals() {
        let mut guard = ReplayGuard::new();
        let t = Instant::now();
        assert!(guard.accept(PEER, 200, 9, SEC, t));
        assert!(!guard.accept(PEER, 100, 1, SEC, t + SEC * 2));
        assert!(guard.accept(PEER, 100, 1, SEC, t + SEC * 3));
    }

    #[test]
    fn a_captured_packet_stays_rejected_however_long_the_peer_is_silent() {
        let mut guard = ReplayGuard::new();
        let t = Instant::now();
        assert!(guard.accept(PEER, 100, 5, SEC, t));
        assert!(guard.accept(PEER, 100, 6, SEC, t + SEC)); // the goodbye
        assert!(!guard.accept(PEER, 100, 5, SEC, t + SEC * 60));
    }

    #[test]
    fn alternating_two_runs_cannot_replay_the_newest_one() {
        let mut guard = ReplayGuard::new();
        let t = Instant::now();
        assert!(guard.accept(PEER, 100, 7, SEC, t)); // an older run, captured
        assert!(guard.accept(PEER, 200, 5, SEC, t + SEC)); // the newest run, also captured
        assert!(guard.accept(PEER, 200, 9, SEC, t + SEC * 2));
        // The peer goes silent: the older run gets in after three intervals...
        assert!(guard.accept(PEER, 100, 7, SEC, t + SEC * 6));
        // ...but the newest run's old packets stay out.
        assert!(!guard.accept(PEER, 200, 5, SEC, t + SEC * 7));
        assert!(!guard.accept(PEER, 200, 9, SEC, t + SEC * 12));
        assert!(guard.accept(PEER, 200, 10, SEC, t + SEC * 13)); // the real peer resumes
    }

    #[test]
    fn restarts_with_the_clock_set_back_still_work() {
        let mut guard = ReplayGuard::new();
        let t = Instant::now();
        assert!(guard.accept(PEER, 500, 40, SEC, t));
        assert!(!guard.accept(PEER, 300, 1, SEC, t + SEC));
        assert!(guard.accept(PEER, 300, 1, SEC, t + SEC * 4));
        assert!(guard.accept(PEER, 300, 2, SEC, t + SEC * 5));
        // Restarted again, still behind the first run.
        assert!(guard.accept(PEER, 400, 1, SEC, t + SEC * 9));
        assert!(guard.accept(PEER, 400, 2, SEC, t + SEC * 10));
    }

    #[test]
    fn peers_are_tracked_separately() {
        let mut guard = ReplayGuard::new();
        let t = Instant::now();
        assert!(guard.accept(PEER, 100, 5, SEC, t));
        assert!(guard.accept(Ipv4Addr::new(10, 0, 0, 9), 100, 1, SEC, t));
    }

    #[test]
    fn an_earlier_now_does_not_count_as_silence() {
        let mut guard = ReplayGuard::new();
        let t = Instant::now() + SEC * 10;
        assert!(guard.accept(PEER, 200, 9, SEC, t));
        // `duration_since` saturates to zero, so going back in time never opens rule 4.
        assert!(!guard.accept(PEER, 100, 1, SEC, t - SEC * 5));
    }
}
