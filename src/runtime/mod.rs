//! The event loop: ties the election machine to UDP, timers, checks, the VIP worker and hooks
//! (spec §11).

mod hooks;
mod limiter;
mod vip_worker;
