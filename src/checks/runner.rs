//! Runs check commands on their interval.

use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::exec;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckSpec {
    pub name: String,
    pub command: Vec<String>,
    pub interval: Duration,
    pub timeout: Duration,
    pub fall: u32,
    pub rise: u32,
    pub weight: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckResult {
    pub index: usize,
    pub passed: bool,
}

/// Runs the check once. Exit code 0 passes; any other code, a timeout or a start failure fails.
/// Each failure is logged at debug level with its reason; the runtime logs status changes.
pub async fn run_once(spec: &CheckSpec) -> bool {
    let reason = match exec::run(&spec.command, spec.timeout, &[]).await {
        Ok(out) if out.success => return true,
        Ok(out) => out.failure(),
        Err(err) => format!("{err:#}"),
    };
    tracing::debug!(check = %spec.name, %reason, "check failed");
    false
}

/// Runs the check every `interval`, starting one interval from now, and reports each result.
/// A tick that arrives while the previous run is still going is skipped.
pub fn spawn_check_loop(index: usize, spec: CheckSpec, results: mpsc::Sender<CheckResult>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let first = tokio::time::Instant::now() + spec.interval;
        let mut ticker = tokio::time::interval_at(first, spec.interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let passed = run_once(&spec).await;
            if results.send(CheckResult { index, passed }).await.is_err() {
                break;
            }
        }
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn spec(command: &[&str], timeout_ms: u64) -> CheckSpec {
        CheckSpec {
            name: "test".into(),
            command: command.iter().map(|s| s.to_string()).collect(),
            interval: Duration::from_millis(50),
            timeout: Duration::from_millis(timeout_ms),
            fall: 1,
            rise: 1,
            weight: 0,
        }
    }

    #[tokio::test]
    async fn the_exit_code_decides_pass_or_fail() {
        assert!(run_once(&spec(&["true"], 1000)).await);
        assert!(!run_once(&spec(&["false"], 1000)).await);
    }

    #[tokio::test]
    async fn a_timeout_or_a_missing_program_fails() {
        assert!(!run_once(&spec(&["sleep", "5"], 100)).await);
        assert!(!run_once(&spec(&["/nonexistent/vipd-check"], 1000)).await);
    }

    #[tokio::test]
    async fn the_loop_reports_results_with_its_index() {
        let (tx, mut rx) = mpsc::channel(4);
        let handle = spawn_check_loop(7, spec(&["true"], 1000), tx);
        let first = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await.unwrap().unwrap();
        assert_eq!(first, CheckResult { index: 7, passed: true });
        handle.abort();
    }

    #[tokio::test]
    async fn a_slow_run_is_not_followed_by_a_burst_of_catch_up_runs() {
        // The first run takes about two intervals and later runs are instant. Missed ticks are
        // skipped, so the third run waits for the next scheduled tick instead of starting at once.
        let marker = std::env::temp_dir().join(format!("vipd-slow-check-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let script = format!("[ -e {0} ] && exit 0; touch {0}; sleep 0.62", marker.display());
        let mut slow = spec(&["sh", "-c", &script], 2000);
        slow.interval = Duration::from_millis(300);
        let (tx, mut rx) = mpsc::channel(4);
        let handle = spawn_check_loop(0, slow, tx);
        let mut arrivals = Vec::new();
        for _ in 0..3 {
            tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.unwrap().unwrap();
            arrivals.push(tokio::time::Instant::now());
        }
        handle.abort();
        let _ = std::fs::remove_file(&marker);
        let gap = arrivals[2] - arrivals[0];
        assert!(gap >= Duration::from_millis(100), "missed ticks ran back to back: {gap:?}");
    }
}
