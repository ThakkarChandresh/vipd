//! Loading and validating `vipd.toml` (spec §10).

use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::checks::CheckSpec;
use crate::exec;
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

#[derive(Debug, Clone)]
pub struct Config {
    pub node_name: String,
    pub group_id: u16,
    pub priority: u8,
    pub preempt: bool,
    pub advert_interval_ms: u16,
    pub auth_key: String,
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
    auth_key: String,
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
        if !(50..=60_000).contains(&self.advert_interval_ms) {
            problems.push(format!("advert_interval_ms must be between 50 and 60000 (got {})", self.advert_interval_ms));
        }
        if self.auth_key.chars().count() < 16 {
            problems.push("auth_key must be at least 16 characters".to_string());
        }
        if self.bind.ip().is_unspecified() {
            problems.push("bind must use this node's real IP, not 0.0.0.0".to_string());
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
            }
            if !vip_ips.insert(v.ip) {
                problems.push(format!("vip {} is listed more than once", v.ip));
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
}
