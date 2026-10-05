//! Health checks: pure rise/fall bookkeeping plus an async runner (spec §8).

mod runner;
mod state;

pub use runner::{run_once, spawn_check_loop, CheckResult, CheckSpec};
pub use state::{aggregate, CheckState, CheckStatus};
