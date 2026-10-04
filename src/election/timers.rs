//! VRRP timer arithmetic (RFC 5798 §6.1, spec §5.2).

use std::time::Duration;

/// `(256 - priority) / 256 × interval`. A higher priority gets a shorter skew.
pub fn skew(priority: u8, interval: Duration) -> Duration {
    interval * (256 - u32::from(priority)) / 256
}

/// `3 × interval + skew`: how long a backup waits before taking over.
pub fn down_interval(priority: u8, interval: Duration) -> Duration {
    interval * 3 + skew(priority, interval)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skew_for_priority_100_at_one_second() {
        assert_eq!(skew(100, Duration::from_secs(1)), Duration::from_nanos(609_375_000));
    }

    #[test]
    fn higher_priority_has_shorter_skew() {
        let interval = Duration::from_secs(1);
        assert!(skew(150, interval) < skew(100, interval));
    }

    #[test]
    fn down_interval_is_three_intervals_plus_skew() {
        let interval = Duration::from_secs(1);
        assert_eq!(down_interval(100, interval), Duration::from_nanos(3_609_375_000));
        assert_eq!(down_interval(150, interval), Duration::from_nanos(3_414_062_500));
    }
}
