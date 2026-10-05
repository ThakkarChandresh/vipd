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

#[cfg(target_os = "linux")]
#[test]
fn sighup_stops_vipd_cleanly() {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;

    // A free port, so the test does not depend on 18458 being unused.
    let port = std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let path = config_file("sighup", "lo");
    let text = std::fs::read_to_string(&path).unwrap().replace(":18458", &format!(":{port}"));
    std::fs::write(&path, text).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_vipd"))
        .args(["run", "--config", path.to_str().unwrap()])
        .env("RUST_LOG", "vipd=info")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines().map(Result::unwrap);
    // The signal handlers are in place before the election starts.
    assert!(lines.by_ref().any(|line| line.contains("starting election")));
    assert!(Command::new("kill").args(["-HUP", &child.id().to_string()]).status().unwrap().success());
    // Fail rather than hang if vipd ignores the signal.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            panic!("vipd was still running 20 s after SIGHUP");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    let rest: Vec<String> = lines.collect();
    assert_eq!(status.code(), Some(0), "{rest:?}");
    assert!(rest.iter().any(|line| line.contains("shutting down")), "{rest:?}");
    let _ = std::fs::remove_file(path);
}

#[cfg(not(windows))]
#[test]
fn service_commands_point_to_the_systemd_unit() {
    let out = vipd(&["service", "uninstall"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("packaging/vipd.service"), "{}", stderr(&out));
}
