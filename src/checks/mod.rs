//! Health checks: pure rise/fall bookkeeping plus an async runner (spec §8).

mod state;

pub use state::{aggregate, CheckState, CheckStatus};
