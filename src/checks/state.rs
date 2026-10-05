//! Rise/fall bookkeeping and the effective-priority calculation (spec §5.1, §8).

use crate::election::Health;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStatus {
    Unknown,
    Ok,
    Failing,
}

#[derive(Debug, Clone)]
pub struct CheckState {
    status: CheckStatus,
    streak: u32,
    fall: u32,
    rise: u32,
}

impl CheckState {
    pub fn new(fall: u32, rise: u32) -> Self {
        Self { status: CheckStatus::Unknown, streak: 0, fall: fall.max(1), rise: rise.max(1) }
    }

    pub fn status(&self) -> CheckStatus {
        self.status
    }

    /// Records one result and returns true if the status changed.
    pub fn record(&mut self, passed: bool) -> bool {
        let before = self.status;
        match (self.status, passed) {
            // The very first result decides on its own.
            (CheckStatus::Unknown, true) => self.status = CheckStatus::Ok,
            (CheckStatus::Unknown, false) => self.status = CheckStatus::Failing,
            // A result that agrees with the current status resets the streak.
            (CheckStatus::Ok, true) | (CheckStatus::Failing, false) => self.streak = 0,
            (CheckStatus::Ok, false) => {
                self.streak += 1;
                if self.streak >= self.fall {
                    self.status = CheckStatus::Failing;
                    self.streak = 0;
                }
            }
            (CheckStatus::Failing, true) => {
                self.streak += 1;
                if self.streak >= self.rise {
                    self.status = CheckStatus::Ok;
                    self.streak = 0;
                }
            }
        }
        self.status != before
    }
}

/// Combines the base priority with each check's `(weight, status)`.
pub fn aggregate(base: u8, checks: &[(i32, CheckStatus)]) -> Health {
    let mut fault = false;
    let mut sum: i32 = 0;
    for &(weight, status) in checks {
        match (weight, status) {
            (0, CheckStatus::Failing) => fault = true,
            (w, CheckStatus::Failing) if w < 0 => sum += w,
            (w, CheckStatus::Ok) if w > 0 => sum += w,
            _ => {}
        }
    }
    let effective = (i32::from(base) + sum).clamp(1, 254) as u8;
    Health { effective_priority: effective, fault }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_result_decides_the_initial_status() {
        let mut up = CheckState::new(3, 3);
        assert!(up.record(true));
        assert_eq!(up.status(), CheckStatus::Ok);
        let mut down = CheckState::new(3, 3);
        assert!(down.record(false));
        assert_eq!(down.status(), CheckStatus::Failing);
    }

    #[test]
    fn needs_fall_consecutive_failures() {
        let mut check = CheckState::new(2, 2);
        check.record(true);
        assert!(!check.record(false));
        assert!(!check.record(true)); // resets the count
        assert!(!check.record(false));
        assert!(check.record(false));
        assert_eq!(check.status(), CheckStatus::Failing);
    }

    #[test]
    fn needs_rise_consecutive_passes() {
        let mut check = CheckState::new(1, 3);
        check.record(false);
        assert!(!check.record(true));
        assert!(!check.record(true));
        assert!(check.record(true));
        assert_eq!(check.status(), CheckStatus::Ok);
    }

    #[test]
    fn a_negative_weight_applies_while_failing() {
        assert_eq!(aggregate(150, &[(-60, CheckStatus::Failing)]), Health { effective_priority: 90, fault: false });
        assert_eq!(aggregate(150, &[(-60, CheckStatus::Ok)]).effective_priority, 150);
    }

    #[test]
    fn a_positive_weight_applies_while_passing() {
        assert_eq!(aggregate(100, &[(20, CheckStatus::Ok)]).effective_priority, 120);
        assert_eq!(aggregate(100, &[(20, CheckStatus::Failing)]).effective_priority, 100);
    }

    #[test]
    fn a_failing_weight_zero_check_means_fault() {
        assert!(aggregate(100, &[(0, CheckStatus::Failing)]).fault);
        assert!(!aggregate(100, &[(0, CheckStatus::Ok), (0, CheckStatus::Unknown)]).fault);
    }

    #[test]
    fn the_effective_priority_is_clamped() {
        assert_eq!(aggregate(10, &[(-200, CheckStatus::Failing)]).effective_priority, 1);
        assert_eq!(aggregate(250, &[(100, CheckStatus::Ok)]).effective_priority, 254);
    }
}
