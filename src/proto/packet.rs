//! The 64-byte signed heartbeat packet (spec §6.2).

use std::net::Ipv4Addr;

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

pub const PACKET_LEN: usize = 64;
const MAGIC: &[u8; 4] = b"VIPD";
const VERSION: u8 = 1;
const KIND_HEARTBEAT: u8 = 1;
/// Bytes 0..32 are covered by the HMAC, which fills bytes 32..64.
const SIGNED_LEN: usize = 32;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Heartbeat {
    pub group_id: u16,
    pub priority: u8,
    pub interval_ms: u16,
    pub vip_fingerprint: u32,
    pub boot_id: u64,
    pub seq: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    #[error("packet is {0} bytes, expected 64")]
    Length(usize),
    #[error("not a vipd packet (bad magic)")]
    Magic,
    #[error("unsupported protocol version {0}")]
    Version(u8),
    #[error("unknown packet kind {0}")]
    Kind(u8),
    #[error("bad signature (do the auth_keys match?)")]
    Signature,
}

/// Signs and verifies packets with the shared `auth_key`.
#[derive(Clone)]
pub struct Codec {
    mac: HmacSha256,
}

impl Codec {
    pub fn new(key: &[u8]) -> Self {
        let mac = HmacSha256::new_from_slice(key).expect("HMAC accepts keys of any length");
        Self { mac }
    }

    pub fn encode(&self, hb: &Heartbeat) -> [u8; PACKET_LEN] {
        let mut buf = [0u8; PACKET_LEN];
        buf[0..4].copy_from_slice(MAGIC);
        buf[4] = VERSION;
        buf[5] = KIND_HEARTBEAT;
        buf[6..8].copy_from_slice(&hb.group_id.to_be_bytes());
        buf[8] = hb.priority;
        buf[9] = 0; // flags: reserved
        buf[10..12].copy_from_slice(&hb.interval_ms.to_be_bytes());
        buf[12..16].copy_from_slice(&hb.vip_fingerprint.to_be_bytes());
        buf[16..24].copy_from_slice(&hb.boot_id.to_be_bytes());
        buf[24..32].copy_from_slice(&hb.seq.to_be_bytes());
        let mut mac = self.mac.clone();
        mac.update(&buf[..SIGNED_LEN]);
        let tag = mac.finalize().into_bytes();
        buf[SIGNED_LEN..].copy_from_slice(&tag);
        buf
    }

    pub fn decode(&self, buf: &[u8]) -> Result<Heartbeat, DecodeError> {
        if buf.len() != PACKET_LEN {
            return Err(DecodeError::Length(buf.len()));
        }
        if buf[0..4] != MAGIC[..] {
            return Err(DecodeError::Magic);
        }
        if buf[4] != VERSION {
            return Err(DecodeError::Version(buf[4]));
        }
        if buf[5] != KIND_HEARTBEAT {
            return Err(DecodeError::Kind(buf[5]));
        }
        let mut mac = self.mac.clone();
        mac.update(&buf[..SIGNED_LEN]);
        mac.verify_slice(&buf[SIGNED_LEN..]).map_err(|_| DecodeError::Signature)?;
        Ok(Heartbeat {
            group_id: u16::from_be_bytes([buf[6], buf[7]]),
            priority: buf[8],
            interval_ms: u16::from_be_bytes([buf[10], buf[11]]),
            vip_fingerprint: u32::from_be_bytes(buf[12..16].try_into().unwrap()),
            boot_id: u64::from_be_bytes(buf[16..24].try_into().unwrap()),
            seq: u64::from_be_bytes(buf[24..32].try_into().unwrap()),
        })
    }
}

/// First 4 bytes of SHA-256 over the sorted `"ip/prefix"` strings joined with `,`.
pub fn vip_fingerprint(vips: &[(Ipv4Addr, u8)]) -> u32 {
    let mut items: Vec<String> = vips.iter().map(|(ip, prefix)| format!("{ip}/{prefix}")).collect();
    items.sort();
    let hash = Sha256::digest(items.join(",").as_bytes());
    u32::from_be_bytes([hash[0], hash[1], hash[2], hash[3]])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Heartbeat {
        Heartbeat {
            group_id: 51,
            priority: 150,
            interval_ms: 1000,
            vip_fingerprint: 0xdead_beef,
            boot_id: 1_759_000_000_000,
            seq: 42,
        }
    }

    #[test]
    fn encode_then_decode_round_trips() {
        let codec = Codec::new(b"a-long-random-shared-secret");
        let packet = codec.encode(&sample());
        assert_eq!(packet.len(), PACKET_LEN);
        assert_eq!(&packet[0..4], b"VIPD");
        assert_eq!(codec.decode(&packet), Ok(sample()));
    }

    #[test]
    fn tampered_packets_are_rejected() {
        let codec = Codec::new(b"a-long-random-shared-secret");
        let mut packet = codec.encode(&sample());
        packet[8] = 254; // try to raise the priority
        assert_eq!(codec.decode(&packet), Err(DecodeError::Signature));
    }

    #[test]
    fn a_different_key_is_rejected() {
        let packet = Codec::new(b"key-one-key-one-key-one").encode(&sample());
        assert_eq!(Codec::new(b"key-two-key-two-key-two").decode(&packet), Err(DecodeError::Signature));
    }

    #[test]
    fn structural_errors_are_reported() {
        let codec = Codec::new(b"a-long-random-shared-secret");
        assert_eq!(codec.decode(&[0u8; 10]), Err(DecodeError::Length(10)));
        let mut packet = codec.encode(&sample());
        packet[0] = b'X';
        assert_eq!(codec.decode(&packet), Err(DecodeError::Magic));
        let mut packet = codec.encode(&sample());
        packet[4] = 2;
        assert_eq!(codec.decode(&packet), Err(DecodeError::Version(2)));
    }

    #[test]
    fn fingerprint_ignores_vip_order() {
        let a = (Ipv4Addr::new(10, 0, 0, 1), 24);
        let b = (Ipv4Addr::new(10, 0, 0, 2), 32);
        assert_eq!(vip_fingerprint(&[a, b]), vip_fingerprint(&[b, a]));
        assert_ne!(vip_fingerprint(&[a]), vip_fingerprint(&[b]));
    }
}
