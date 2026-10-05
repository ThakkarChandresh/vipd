//! The heartbeat wire format and replay protection (spec §6).

mod packet;

pub use packet::{vip_fingerprint, Codec, DecodeError, Heartbeat, PACKET_LEN};
