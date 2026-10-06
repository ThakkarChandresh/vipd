//! Gratuitous ARP: tells every device on the LAN "this IP is now at my MAC" (spec §7.2).

use std::net::Ipv4Addr;

/// Parses `aa:bb:cc:dd:ee:ff`.
pub fn parse_mac(text: &str) -> Option<[u8; 6]> {
    let mut mac = [0u8; 6];
    let mut parts = text.trim().split(':');
    for byte in &mut mac {
        *byte = u8::from_str_radix(parts.next()?, 16).ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    Some(mac)
}

/// An Ethernet broadcast ARP request in which sender IP = target IP = `ip` (RFC 826 layout).
pub fn build_frame(mac: [u8; 6], ip: Ipv4Addr) -> [u8; 42] {
    let mut frame = [0u8; 42];
    frame[0..6].copy_from_slice(&[0xff; 6]); // destination: broadcast
    frame[6..12].copy_from_slice(&mac); // source
    frame[12..14].copy_from_slice(&0x0806u16.to_be_bytes()); // EtherType: ARP
    frame[14..16].copy_from_slice(&1u16.to_be_bytes()); // hardware type: Ethernet
    frame[16..18].copy_from_slice(&0x0800u16.to_be_bytes()); // protocol type: IPv4
    frame[18] = 6; // hardware address length
    frame[19] = 4; // protocol address length
    frame[20..22].copy_from_slice(&1u16.to_be_bytes()); // operation: request
    frame[22..28].copy_from_slice(&mac); // sender MAC
    frame[28..32].copy_from_slice(&ip.octets()); // sender IP
                                                 // Bytes 32..38, the target MAC, stay zero.
    frame[38..42].copy_from_slice(&ip.octets()); // target IP
    frame
}

/// Sends `count` gratuitous ARPs for `ip` out of `iface`. Needs root or CAP_NET_RAW.
#[cfg(target_os = "linux")]
pub fn send(iface: &str, ip: Ipv4Addr, count: u32) -> anyhow::Result<()> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    use anyhow::{bail, Context};

    let mac_text = std::fs::read_to_string(format!("/sys/class/net/{iface}/address"))
        .with_context(|| format!("cannot read the MAC address of {iface}"))?;
    let mac = parse_mac(&mac_text).with_context(|| format!("unexpected MAC address {mac_text:?} on {iface}"))?;
    let name = CString::new(iface).context("interface name contains a NUL byte")?;
    // SAFETY: `name` is a valid NUL-terminated string.
    let ifindex = unsafe { libc::if_nametoindex(name.as_ptr()) };
    if ifindex == 0 {
        bail!("interface {iface} not found");
    }

    let protocol = (libc::ETH_P_ARP as u16).to_be();
    // SAFETY: a plain socket(2) call; the result is checked below.
    let fd = unsafe { libc::socket(libc::AF_PACKET, libc::SOCK_RAW, i32::from(protocol)) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error())
            .context("cannot open a raw packet socket (needs root or CAP_NET_RAW)");
    }
    // SAFETY: `fd` is a freshly opened socket that nothing else owns.
    let socket = unsafe { OwnedFd::from_raw_fd(fd) };

    // SAFETY: sockaddr_ll is plain old data, so all-zero is a valid starting value.
    let mut addr: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
    addr.sll_family = libc::AF_PACKET as u16;
    addr.sll_protocol = protocol;
    addr.sll_ifindex = ifindex as i32;
    addr.sll_halen = 6;
    addr.sll_addr[..6].copy_from_slice(&[0xff; 6]);

    let frame = build_frame(mac, ip);
    for _ in 0..count {
        // SAFETY: `frame` and `addr` are valid for the lengths passed.
        let sent = unsafe {
            libc::sendto(
                socket.as_raw_fd(),
                frame.as_ptr().cast(),
                frame.len(),
                0,
                std::ptr::addr_of!(addr).cast(),
                std::mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t,
            )
        };
        if sent < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("sending a gratuitous ARP on {iface} failed"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mac_addresses() {
        assert_eq!(parse_mac("aa:bb:cc:00:11:ff\n"), Some([0xaa, 0xbb, 0xcc, 0x00, 0x11, 0xff]));
        assert_eq!(parse_mac("AA:BB:CC:0:1:F"), Some([0xaa, 0xbb, 0xcc, 0x00, 0x01, 0x0f]));
        assert_eq!(parse_mac("aa:bb:cc:00:11"), None);
        assert_eq!(parse_mac("aa:bb:cc:00:11:ff:00"), None);
        assert_eq!(parse_mac("zz:bb:cc:00:11:ff"), None);
    }

    #[test]
    fn the_frame_layout_matches_rfc_826() {
        let mac = [0x02, 0, 0, 0, 0, 0x01];
        let frame = build_frame(mac, Ipv4Addr::new(192, 168, 1, 200));
        assert_eq!(&frame[0..6], &[0xff; 6]);
        assert_eq!(&frame[6..12], &mac);
        assert_eq!(&frame[12..14], &[0x08, 0x06]);
        assert_eq!(&frame[14..20], &[0x00, 0x01, 0x08, 0x00, 6, 4]);
        assert_eq!(&frame[20..22], &[0x00, 0x01]);
        assert_eq!(&frame[22..28], &mac);
        assert_eq!(&frame[28..32], &[192, 168, 1, 200]);
        assert_eq!(&frame[32..38], &[0; 6]);
        assert_eq!(&frame[38..42], &[192, 168, 1, 200]);
    }
}
