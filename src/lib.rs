//! vipd: keeps a virtual IP on one healthy node of a group, on Linux and Windows.

pub mod checks;
pub mod cli;
pub mod config;
pub mod election;
pub mod exec;
pub mod logging;
pub mod proto;
pub mod runtime;
pub mod service;
pub mod vip;
