//! Linux backend: `ip addr` commands plus gratuitous ARP (spec §7.2).

use std::net::Ipv4Addr;
use std::path::Path;

use super::{garp, Vip, VipBackend, COMMAND_TIMEOUT};
use crate::exec;

const GARP_COUNT: u32 = 5;

#[derive(Debug, Clone, Default)]
pub struct LinuxBackend;

impl LinuxBackend {
    pub fn new() -> Self {
        Self
    }
}

fn strings(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

pub fn find_args(iface: &str) -> Vec<String> {
    strings(&["ip", "-o", "-4", "addr", "show", "dev", iface])
}

pub fn attach_args(vip: &Vip) -> Vec<String> {
    strings(&["ip", "addr", "add", &format!("{}/{}", vip.ip, vip.prefix), "dev", &vip.interface])
}

pub fn detach_args(vip: &Vip, found: &str) -> Vec<String> {
    strings(&["ip", "addr", "del", found, "dev", &vip.interface])
}

/// Finds `ip` in `ip -o -4 addr show` output and returns its `ip/prefix` token. A point-to-point
/// address prints as `inet LOCAL peer PEER/PREFIX`; its bare `LOCAL` is not an `ip/prefix` token, so
/// it never matches.
pub fn parse_find(output: &str, ip: Ipv4Addr) -> Option<String> {
    let wanted = ip.to_string();
    output.lines().find_map(|line| {
        let mut tokens = line.split_whitespace();
        tokens.find(|token| *token == "inet")?;
        let addr = tokens.next()?;
        let (address, _prefix) = addr.split_once('/')?;
        (address == wanted).then(|| addr.to_string())
    })
}

/// Whether a `/sys/class/net/{iface}/operstate` value means the link is up. `unknown` counts as up,
/// because loopback, dummy and some drivers never report more. Everything else is down, such as
/// `dormant` for Wi-Fi that is not associated, or `lowerlayerdown`.
pub fn operstate_is_up(state: &str) -> bool {
    matches!(state.trim(), "up" | "unknown")
}

impl VipBackend for LinuxBackend {
    async fn interface_exists(&self, iface: &str) -> anyhow::Result<bool> {
        Ok(Path::new("/sys/class/net").join(iface).exists())
    }

    async fn find(&self, vip: &Vip) -> anyhow::Result<Option<String>> {
        let out = exec::run_ok(&find_args(&vip.interface), COMMAND_TIMEOUT).await?;
        Ok(parse_find(&out.stdout, vip.ip))
    }

    async fn attach(&self, vip: &Vip) -> anyhow::Result<()> {
        exec::run_ok(&attach_args(vip), COMMAND_TIMEOUT).await.map(|_| ())
    }

    async fn detach(&self, vip: &Vip, found: &str) -> anyhow::Result<()> {
        exec::run_ok(&detach_args(vip, found), COMMAND_TIMEOUT).await.map(|_| ())
    }

    async fn announce(&self, vip: &Vip) -> anyhow::Result<()> {
        let (iface, ip) = (vip.interface.clone(), vip.ip);
        tokio::task::spawn_blocking(move || garp::send(&iface, ip, GARP_COUNT)).await?
    }

    async fn link_up(&self, iface: &str) -> bool {
        // config.rs's Linux name rules keep `iface` inside /sys/class/net. An unreadable file means
        // the interface is gone, so its link is down.
        let operstate = Path::new("/sys/class/net").join(iface).join("operstate");
        std::fs::read_to_string(operstate).is_ok_and(|state| operstate_is_up(&state))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
2: eth0    inet 192.168.1.13/24 brd 192.168.1.255 scope global dynamic eth0\\       valid_lft 85000sec preferred_lft 85000sec
2: eth0    inet 192.168.1.200/32 scope global secondary eth0\\       valid_lft forever preferred_lft forever
";

    fn vip() -> Vip {
        Vip { ip: Ipv4Addr::new(192, 168, 1, 200), prefix: 24, interface: "eth0".into() }
    }

    #[test]
    fn finds_the_vip_with_its_actual_prefix() {
        assert_eq!(parse_find(SAMPLE, Ipv4Addr::new(192, 168, 1, 200)), Some("192.168.1.200/32".into()));
        assert_eq!(parse_find(SAMPLE, Ipv4Addr::new(192, 168, 1, 13)), Some("192.168.1.13/24".into()));
        assert_eq!(parse_find(SAMPLE, Ipv4Addr::new(192, 168, 1, 20)), None);
        assert_eq!(parse_find("", Ipv4Addr::new(192, 168, 1, 20)), None);
        let peer = "5: tun0    inet 10.10.0.1 peer 10.10.0.2/32 scope global tun0\\       valid_lft forever";
        assert_eq!(parse_find(peer, Ipv4Addr::new(10, 10, 0, 1)), None);
    }

    #[tokio::test]
    async fn checks_whether_an_interface_exists() {
        let backend = LinuxBackend::new();
        assert!(backend.interface_exists("lo").await.unwrap());
        assert!(!backend.interface_exists("vipd-no-such0").await.unwrap());
    }

    #[test]
    fn only_up_and_unknown_operstates_are_up() {
        for state in ["up", "unknown", "up\n", "unknown\n"] {
            assert!(operstate_is_up(state), "{state:?}");
        }
        for state in ["down", "dormant", "lowerlayerdown", "notpresent", "testing", "down\n", ""] {
            assert!(!operstate_is_up(state), "{state:?}");
        }
    }

    #[tokio::test]
    async fn reads_the_link_state_from_sysfs() {
        let backend = LinuxBackend::new();
        assert!(backend.link_up("lo").await, "lo reports `unknown`");
        assert!(!backend.link_up("vipd-no-such0").await, "an unreadable operstate means down");
    }

    #[test]
    fn builds_ip_commands() {
        assert_eq!(attach_args(&vip()).join(" "), "ip addr add 192.168.1.200/24 dev eth0");
        assert_eq!(detach_args(&vip(), "192.168.1.200/32").join(" "), "ip addr del 192.168.1.200/32 dev eth0");
        assert_eq!(find_args("eth0").join(" "), "ip -o -4 addr show dev eth0");
    }

    /// Needs root: creates and deletes the dummy interface `vipdtest0`.
    #[tokio::test]
    #[ignore = "needs root"]
    async fn attaches_and_detaches_on_a_dummy_interface() {
        let sh = |cmd: &str| std::process::Command::new("sh").arg("-c").arg(cmd).status().unwrap();
        sh("ip link del vipdtest0 2>/dev/null");
        assert!(sh("ip link add vipdtest0 type dummy && ip link set vipdtest0 up").success());
        let backend = LinuxBackend::new();
        let vip = Vip { ip: Ipv4Addr::new(10, 255, 255, 1), prefix: 24, interface: "vipdtest0".into() };
        assert!(backend.interface_exists("vipdtest0").await.unwrap());
        assert_eq!(backend.find(&vip).await.unwrap(), None);
        backend.attach(&vip).await.unwrap();
        let found = backend.find(&vip).await.unwrap().expect("the VIP is attached");
        assert_eq!(found, "10.255.255.1/24");
        backend.announce(&vip).await.unwrap();
        backend.detach(&vip, &found).await.unwrap();
        assert_eq!(backend.find(&vip).await.unwrap(), None);
        sh("ip link del vipdtest0");
    }
}
