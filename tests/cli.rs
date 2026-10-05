//! Runs the built `vipd` binary's one-shot commands and checks their exit codes (spec §12).

use std::process::{Command, Output};

fn vipd(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_vipd")).args(args).output().unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// A valid config with one VIP on `interface`, in a file only this test uses.
#[cfg(target_os = "linux")]
fn config_file(name: &str, interface: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("vipd-cli-test-{}-{name}.toml", std::process::id()));
    let text = format!(
        r#"
node_name = "cli"
group_id = 1
priority = 100
auth_key = "cli-test-key-0123456789"
bind = "127.0.0.1:18458"
peers = ["127.0.0.2:18458"]

[[vip]]
ip = "10.250.0.1"
interface = "{interface}"
"#
    );
    std::fs::write(&path, text).unwrap();
    path
}

#[cfg(target_os = "linux")]
#[test]
fn check_config_accepts_a_valid_file() {
    let path = config_file("valid", "lo");
    let out = vipd(&["check-config", "--config", path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(String::from_utf8_lossy(&out.stdout).ends_with(": OK\n"));
    let _ = std::fs::remove_file(path);
}

#[cfg(target_os = "linux")]
#[test]
fn check_config_rejects_a_missing_interface_with_exit_code_2() {
    let path = config_file("no-interface", "vipd-none0");
    let out = vipd(&["check-config", "--config", path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("interface \"vipd-none0\" does not exist on this machine"), "{}", stderr(&out));
    let _ = std::fs::remove_file(path);
}

#[test]
fn check_config_rejects_a_missing_file_with_exit_code_2() {
    let missing = std::env::temp_dir().join("vipd-cli-test-no-such-file.toml");
    let out = vipd(&["check-config", "--config", missing.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    let message = stderr(&out);
    assert_eq!(message.matches("vipd-cli-test-no-such-file.toml").count(), 1, "the path appears once: {message}");
}

#[cfg(not(windows))]
#[test]
fn service_commands_point_to_the_systemd_unit() {
    let out = vipd(&["service", "uninstall"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("packaging/vipd.service"), "{}", stderr(&out));
}
