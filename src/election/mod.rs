//! The election state machine. Pure logic: no sockets, no clock, no processes.

mod machine;
pub mod timers;

pub use machine::{Action, Event, Health, HookKind, Machine, MachineConfig, State};
