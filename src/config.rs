//! Loading and validating `vipd.toml` (spec §10).

use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::checks::CheckSpec;
use crate::exec;
use crate::proto::{MAX_INTERVAL_MS, MIN_INTERVAL_MS};
use crate::vip::{CommandOverrides, Vip};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Read { path: PathBuf, source: std::io::Error },
    #[error("invalid TOML: {0}")]
    Parse(String),
    #[error("invalid config:\n  - {}", .0.join("\n  - "))]
    Invalid(Vec<String>),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookCommands {
    pub on_master: Option<Vec<String>>,
    pub on_backup: Option<Vec<String>>,
    pub on_fault: Option<Vec<String>>,
    pub on_stop: Option<Vec<String>>,
}

/// The shared heartbeat key. Its `Debug` output is redacted so the secret cannot end up in a log.
#[derive(Clone, Deserialize)]
#[serde(transparent)]
pub struct AuthKey(String);

impl std::ops::Deref for AuthKey {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for AuthKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub node_name: String,
    pub group_id: u16,
    pub priority: u8,
    pub preempt: bool,
    pub advert_interval_ms: u16,
    pub auth_key: AuthKey,
    pub bind: SocketAddrV4,
    pub peers: Vec<SocketAddrV4>,
    pub log_level: String,
    pub log_dir: Option<PathBuf>,
    pub vips: Vec<Vip>,
    pub checks: Vec<CheckSpec>,
    pub hooks: HookCommands,
    pub vip_commands: CommandOverrides,
}

impl Config {
    pub fn advert_interval(&self) -> Duration {
        Duration::from_millis(u64::from(self.advert_interval_ms))
    }

    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text =
            std::fs::read_to_string(path).map_err(|source| ConfigError::Read { path: path.to_path_buf(), source })?;
        Self::from_toml(&text)
    }

    pub fn from_toml(text: &str) -> Result<Self, ConfigError> {
        let raw: RawConfig = toml::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))?;
        raw.validate()
    }
}

fn default_true() -> bool {
    true
}
fn default_advert_interval_ms() -> u16 {
    1000
}
fn default_log_level() -> String {
    "info".to_string()
}
fn default_prefix() -> u8 {
    32
}
fn default_check_interval_ms() -> u64 {
    1000
}
fn default_rise_fall() -> u32 {
    1
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    node_name: String,
    group_id: u16,
    priority: u8,
    #[serde(default = "default_true")]
    preempt: bool,
    #[serde(default = "default_advert_interval_ms")]
    advert_interval_ms: u16,
    auth_key: AuthKey,
    bind: SocketAddrV4,
    peers: Vec<SocketAddrV4>,
    #[serde(default = "default_log_level")]
    log_level: String,
    #[serde(default)]
    log_dir: Option<PathBuf>,
    #[serde(rename = "vip")]
    vips: Vec<RawVip>,
    #[serde(rename = "check", default)]
    checks: Vec<RawCheck>,
    #[serde(default)]
    hooks: RawHooks,
    #[serde(default)]
    vip_commands: RawCommands,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawVip {
    ip: Ipv4Addr,
    #[serde(default = "default_prefix")]
    prefix: u8,
    interface: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCheck {
    name: String,
    command: String,
    #[serde(default = "default_check_interval_ms")]
    interval_ms: u64,
    timeout_ms: Option<u64>,
    #[serde(default = "default_rise_fall")]
    fall: u32,
    #[serde(default = "default_rise_fall")]
    rise: u32,
    #[serde(default)]
    weight: i32,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHooks {
    on_master: Option<String>,
    on_backup: Option<String>,
    on_fault: Option<String>,
    on_stop: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCommands {
    attach: Option<String>,
    detach: Option<String>,
}

const LOG_LEVELS: [&str; 5] = ["trace", "debug", "info", "warn", "error"];

impl RawConfig {
    /// Checks every rule and reports all problems at once.
    fn validate(self) -> Result<Config, ConfigError> {
        let mut problems = Vec::new();

        if self.node_name.trim().is_empty() {
            problems.push("node_name must not be empty".to_string());
        }
        if self.group_id == 0 {
            problems.push("group_id must be between 1 and 65535".to_string());
        }
        if !(1..=254).contains(&self.priority) {
            problems.push(format!("priority must be between 1 and 254 (got {})", self.priority));
        }
        if !(MIN_INTERVAL_MS..=MAX_INTERVAL_MS).contains(&self.advert_interval_ms) {
            problems.push(format!(
                "advert_interval_ms must be between {MIN_INTERVAL_MS} and {MAX_INTERVAL_MS} (got {})",
                self.advert_interval_ms
            ));
        }
        if self.auth_key.chars().count() < 16 {
            problems.push("auth_key must be at least 16 characters".to_string());
        }
        if self.auth_key.trim().len() != self.auth_key.len() {
            problems.push("auth_key must not start or end with whitespace".to_string());
        }
        if self.bind.ip().is_unspecified() {
            problems.push("bind must use this node's real IP, not 0.0.0.0".to_string());
        }
        if self.bind.port() == 0 {
            problems.push("bind needs a real port, not 0 (peers send heartbeats to it)".to_string());
        }
        if self.peers.is_empty() {
            problems.push("peers must list at least one other node".to_string());
        }
        let mut peer_ips = HashSet::new();
        for peer in &self.peers {
            if !peer_ips.insert(*peer.ip()) {
                problems.push(format!("peer {} is listed more than once", peer.ip()));
            }
            if peer.ip() == self.bind.ip() {
                problems.push(format!("peers must not contain this node's own IP {}", peer.ip()));
            }
            if peer.port() == 0 {
                problems.push(format!("peer {peer} needs a real port, not 0"));
            }
        }
        if !LOG_LEVELS.contains(&self.log_level.as_str()) {
            problems.push(format!("log_level must be one of {} (got {:?})", LOG_LEVELS.join(", "), self.log_level));
        }

        if self.vips.is_empty() {
            problems.push("at least one [[vip]] is required".to_string());
        }
        let mut vip_ips = HashSet::new();
        let mut vips = Vec::new();
        for v in &self.vips {
            if !(1..=32).contains(&v.prefix) {
                problems.push(format!("vip {}: prefix must be between 1 and 32 (got {})", v.ip, v.prefix));
            }
            if v.interface.trim().is_empty() {
                problems.push(format!("vip {}: interface must not be empty", v.ip));
            } else if let Some(reason) = interface_name_problem(&v.interface) {
                problems.push(format!("vip {}: interface {:?} {reason}", v.ip, v.interface));
            }
            if !vip_ips.insert(v.ip) {
                problems.push(format!("vip {} is listed more than once", v.ip));
            }
            if v.ip.is_unspecified() || v.ip.is_loopback() || v.ip.is_multicast() || v.ip.is_broadcast() {
                problems.push(format!("vip {} is not a usable unicast address", v.ip));
            }
            // Start-up removes leftover VIPs, so a node address listed as a VIP would be deleted.
            if v.ip == *self.bind.ip() {
                problems.push(format!("vip {} must not be this node's own bind address", v.ip));
            } else if peer_ips.contains(&v.ip) {
                problems.push(format!("vip {} must not be a peer's address", v.ip));
            }
            vips.push(Vip { ip: v.ip, prefix: v.prefix, interface: v.interface.clone() });
        }

        let mut check_names = HashSet::new();
        let mut checks = Vec::new();
        for c in &self.checks {
            let label = format!("check {:?}", c.name);
            if c.name.trim().is_empty() {
                problems.push("every [[check]] needs a name".to_string());
            } else if !check_names.insert(c.name.clone()) {
                problems.push(format!("{label} is defined more than once"));
            }
            let timeout_ms = c.timeout_ms.unwrap_or(c.interval_ms);
            if !(100..=3_600_000).contains(&c.interval_ms) {
                problems.push(format!("{label}: interval_ms must be between 100 and 3600000"));
            }
            if !(100..=3_600_000).contains(&timeout_ms) {
                problems.push(format!("{label}: timeout_ms must be between 100 and 3600000"));
            }
            if c.fall == 0 || c.rise == 0 {
                problems.push(format!("{label}: fall and rise must be at least 1"));
            }
            if !(-253..=253).contains(&c.weight) {
                problems.push(format!("{label}: weight must be between -253 and 253"));
            }
            match exec::tokenize(&c.command) {
                Ok(command) => checks.push(CheckSpec {
                    name: c.name.clone(),
                    command,
                    interval: Duration::from_millis(c.interval_ms),
                    timeout: Duration::from_millis(timeout_ms),
                    fall: c.fall,
                    rise: c.rise,
                    weight: c.weight,
                }),
                Err(e) => problems.push(format!("{label}: {e}")),
            }
        }

        let mut command = |field: &str, value: &Option<String>| -> Option<Vec<String>> {
            let text = value.as_ref()?;
            match exec::tokenize(text) {
                Ok(args) => Some(args),
                Err(e) => {
                    problems.push(format!("{field}: {e}"));
                    None
                }
            }
        };
        let hooks = HookCommands {
            on_master: command("hooks.on_master", &self.hooks.on_master),
            on_backup: command("hooks.on_backup", &self.hooks.on_backup),
            on_fault: command("hooks.on_fault", &self.hooks.on_fault),
            on_stop: command("hooks.on_stop", &self.hooks.on_stop),
        };
        let vip_commands = CommandOverrides {
            attach: command("vip_commands.attach", &self.vip_commands.attach),
            detach: command("vip_commands.detach", &self.vip_commands.detach),
        };

        if !problems.is_empty() {
            return Err(ConfigError::Invalid(problems));
        }
        Ok(Config {
            node_name: self.node_name,
            group_id: self.group_id,
            priority: self.priority,
            preempt: self.preempt,
            advert_interval_ms: self.advert_interval_ms,
            auth_key: self.auth_key,
            bind: self.bind,
            peers: self.peers,
            log_level: self.log_level,
            log_dir: self.log_dir,
            vips,
            checks,
            hooks,
            vip_commands,
        })
    }
}

/// Why Linux would reject `name` as a network interface name (the kernel's `dev_valid_name`). This
/// also keeps names like `../x` out of the `/sys/class/net/{iface}` paths. Windows adapter names
/// may contain spaces and other characters, so only Linux checks.
#[cfg(target_os = "linux")]
fn interface_name_problem(name: &str) -> Option<&'static str> {
    if name.len() > 15 {
        Some("is longer than 15 bytes")
    } else if name == "." || name == ".." || name.contains(['/', ':', '\0']) || name.contains(char::is_whitespace) {
        Some("is not a valid Linux interface name")
    } else {
        None
    }
}

#[cfg(not(target_os = "linux"))]
fn interface_name_problem(name: &str) -> Option<&'static str> {
    windows_interface_name_problem(name)
}

/// netsh also accepts an adapter index, or a name with spaces around it, but the duplicate-address
/// check (src/vip/windows.rs) matches the adapter's name exactly.
#[cfg_attr(target_os = "linux", allow(dead_code))]
fn windows_interface_name_problem(name: &str) -> Option<&'static str> {
    if name.bytes().all(|b| b.is_ascii_digit()) {
        Some("is an adapter index; use the adapter's name")
    } else if name.trim() != name {
        Some("has leading or trailing whitespace")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
node_name = "web-a"
group_id = 51
priority = 150
auth_key = "0123456789abcdef"
bind = "192.168.1.13:8458"
peers = ["192.168.1.14:8458"]

[[vip]]
ip = "192.168.1.200"
interface = "eth0"
"#;

    fn problems(text: &str) -> Vec<String> {
        match Config::from_toml(text) {
            Err(ConfigError::Invalid(problems)) => problems,
            other => panic!("expected validation problems, got {other:?}"),
        }
    }

    #[test]
    fn a_minimal_config_gets_defaults() {
        let c = Config::from_toml(MINIMAL).unwrap();
        assert!(c.preempt);
        assert_eq!(c.advert_interval(), Duration::from_secs(1));
        assert_eq!(c.log_level, "info");
        assert_eq!(c.vips, vec![Vip { ip: Ipv4Addr::new(192, 168, 1, 200), prefix: 32, interface: "eth0".into() }]);
        assert!(c.checks.is_empty());
        assert_eq!(c.hooks, HookCommands::default());
        assert_eq!(c.vip_commands, CommandOverrides::default());
    }

    #[test]
    fn the_full_example_parses() {
        let text = format!(
            r#"{MINIMAL}
[[check]]
name = "nginx"
command = "curl -sf http://127.0.0.1/"
fall = 2
rise = 2
weight = -60

[hooks]
on_master = "/usr/local/bin/vip-alert.sh master"

[vip_commands]
attach = "ip addr add {{ip}}/{{prefix}} dev {{iface}}"
"#
        );
        let c = Config::from_toml(&text).unwrap();
        let check = &c.checks[0];
        assert_eq!(check.command, vec!["curl", "-sf", "http://127.0.0.1/"]);
        assert_eq!(check.interval, Duration::from_secs(1));
        assert_eq!(check.timeout, Duration::from_secs(1));
        assert_eq!((check.fall, check.rise, check.weight), (2, 2, -60));
        assert_eq!(c.hooks.on_master, Some(vec!["/usr/local/bin/vip-alert.sh".to_string(), "master".to_string()]));
        assert_eq!(c.vip_commands.attach.unwrap()[3], "{ip}/{prefix}");
    }

    #[test]
    fn every_problem_is_reported_at_once() {
        let text = MINIMAL
            .replace("priority = 150", "priority = 255")
            .replace("0123456789abcdef", "short")
            .replace("192.168.1.13:8458", "0.0.0.0:8458");
        let p = problems(&text);
        assert!(p.iter().any(|m| m.contains("priority")), "{p:?}");
        assert!(p.iter().any(|m| m.contains("auth_key")), "{p:?}");
        assert!(p.iter().any(|m| m.contains("0.0.0.0")), "{p:?}");
    }

    #[test]
    fn rejects_our_own_ip_and_duplicates_in_peers() {
        let text = MINIMAL.replace(
            r#"peers = ["192.168.1.14:8458"]"#,
            r#"peers = ["192.168.1.13:9000", "192.168.1.14:1", "192.168.1.14:2"]"#,
        );
        let p = problems(&text);
        assert!(p.iter().any(|m| m.contains("own IP")), "{p:?}");
        assert!(p.iter().any(|m| m.contains("more than once")), "{p:?}");
    }

    #[test]
    fn a_vip_must_not_be_a_node_address() {
        for (ip, expected) in [("192.168.1.13", "own bind address"), ("192.168.1.14", "a peer's address")] {
            let p = problems(&MINIMAL.replace(r#"ip = "192.168.1.200""#, &format!(r#"ip = "{ip}""#)));
            assert!(p.iter().any(|m| m.contains(expected)), "{ip}: {p:?}");
        }
    }

    #[test]
    fn a_duplicate_vip_ip_is_rejected() {
        let text = format!("{MINIMAL}\n[[vip]]\nip = \"192.168.1.200\"\ninterface = \"eth1\"\n");
        let p = problems(&text);
        assert!(p.iter().any(|m| m.contains("vip 192.168.1.200 is listed more than once")), "{p:?}");
    }

    #[test]
    fn every_rule_names_the_bad_setting() {
        let cases = [
            ("group_id = 51", "group_id = 0", "group_id"),
            ("priority = 150", "priority = 0", "priority"),
            ("priority = 150", "priority = 150\nadvert_interval_ms = 49", "advert_interval_ms"),
            ("priority = 150", "priority = 150\nadvert_interval_ms = 60001", "advert_interval_ms"),
            ("priority = 150", "priority = 150\nlog_level = \"loud\"", "log_level"),
            ("0123456789abcdef", "0123456789abcdef ", "whitespace"),
            ("192.168.1.13:8458", "192.168.1.13:0", "bind needs a real port"),
            ("192.168.1.14:8458", "192.168.1.14:0", "needs a real port"),
            (r#"ip = "192.168.1.200""#, r#"ip = "127.0.0.1""#, "not a usable unicast address"),
            (r#"ip = "192.168.1.200""#, r#"ip = "224.0.0.18""#, "not a usable unicast address"),
            (r#"interface = "eth0""#, "interface = \"eth0\"\nprefix = 33", "prefix"),
        ];
        for (from, to, expected) in cases {
            let p = problems(&MINIMAL.replace(from, to));
            assert!(p.iter().any(|m| m.contains(expected)), "{to}: {p:?}");
        }
    }

    #[test]
    fn every_check_rule_names_the_bad_setting() {
        let check = |extra: &str| format!("{MINIMAL}\n[[check]]\nname = \"web\"\ncommand = \"true\"\n{extra}\n");
        let cases = [
            ("interval_ms = 99", "interval_ms"),
            ("timeout_ms = 3600001", "timeout_ms"),
            ("fall = 0", "fall and rise"),
            ("rise = 0", "fall and rise"),
            ("weight = -254", "weight"),
            ("[[check]]\nname = \"web\"\ncommand = \"true\"", "more than once"),
        ];
        for (extra, expected) in cases {
            let p = problems(&check(extra));
            assert!(p.iter().any(|m| m.contains(expected)), "{extra}: {p:?}");
        }
    }

    #[test]
    fn an_empty_check_name_is_rejected() {
        let text = format!("{MINIMAL}\n[[check]]\nname = \"\"\ncommand = \"true\"\n");
        let p = problems(&text);
        assert!(p.iter().any(|m| m.contains("every [[check]] needs a name")), "{p:?}");
    }

    #[test]
    fn a_check_timeout_defaults_to_its_interval() {
        let text = format!("{MINIMAL}\n[[check]]\nname = \"web\"\ncommand = \"true\"\ninterval_ms = 2500\n");
        let c = Config::from_toml(&text).unwrap();
        assert_eq!(c.checks[0].timeout, Duration::from_millis(2500));
    }

    #[test]
    fn debug_output_hides_the_auth_key() {
        let c = Config::from_toml(MINIMAL).unwrap();
        assert_eq!(&*c.auth_key, "0123456789abcdef");
        assert!(!format!("{c:?}").contains("0123456789abcdef"));
    }

    #[test]
    fn rejects_bad_check_settings() {
        let text = format!(
            r#"{MINIMAL}
[[check]]
name = "x"
command = '"unterminated'
weight = 300
"#
        );
        let p = problems(&text);
        assert!(p.iter().any(|m| m.contains("unterminated quote")), "{p:?}");
        assert!(p.iter().any(|m| m.contains("weight")), "{p:?}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn rejects_interface_names_linux_would_reject() {
        let with = |name: &str| MINIMAL.replace(r#"interface = "eth0""#, &format!("interface = {name:?}"));
        for bad in ["../../etc", "eth0:1", "sixteen-bytes-00", "Ethernet 2", ".."] {
            let p = problems(&with(bad));
            assert!(p.iter().any(|m| m.contains("interface")), "{bad}: {p:?}");
        }
        assert!(Config::from_toml(&with("fifteen-bytes00")).is_ok(), "15 bytes is the kernel's limit");
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let text = format!("prioritty = 5\n{MINIMAL}");
        assert!(matches!(Config::from_toml(&text), Err(ConfigError::Parse(_))));
    }

    #[test]
    fn a_missing_vip_section_is_rejected() {
        let text = MINIMAL.split("[[vip]]").next().unwrap();
        assert!(matches!(Config::from_toml(text), Err(ConfigError::Parse(_))));
    }

    #[test]
    fn windows_interfaces_must_be_adapter_names() {
        assert_eq!(windows_interface_name_problem("Ethernet 2"), None);
        assert_eq!(windows_interface_name_problem("12"), Some("is an adapter index; use the adapter's name"));
        assert_eq!(windows_interface_name_problem(" Ethernet"), Some("has leading or trailing whitespace"));
    }
}
