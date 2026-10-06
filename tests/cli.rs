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

#[cfg(target_os = "linux")]
#[test]
fn check_config_rejects_a_bind_address_this_machine_does_not_have() {
    let path = config_file("foreign-bind", "lo");
    // 192.0.2.0/24 is TEST-NET-1, never a local address.
    let text = std::fs::read_to_string(&path).unwrap().replace("127.0.0.1:18458", "192.0.2.13:18458");
    std::fs::write(&path, text).unwrap();
    let out = vipd(&["check-config", "--config", path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("is not an address of this machine"), "{}", stderr(&out));
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

/// A running `vipd run` on `lo` and a free port, once its election has started.
#[cfg(target_os = "linux")]
struct Running {
    child: std::process::Child,
    lines: std::io::Lines<std::io::BufReader<std::process::ChildStdout>>,
    config: std::path::PathBuf,
}

/// Starts vipd with SIGHUP set to `sighup`, whatever the test runner inherited (it may run under
/// nohup), and waits until the election runs, by when its signal handlers are in place.
#[cfg(target_os = "linux")]
fn start_vipd(name: &str, sighup: libc::sighandler_t) -> Running {
    use std::io::{BufRead, BufReader};
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;

    // A free port, so the test does not depend on 18458 being unused.
    let port = std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let config = config_file(name, "lo");
    let text = std::fs::read_to_string(&config).unwrap().replace(":18458", &format!(":{port}"));
    std::fs::write(&config, text).unwrap();

    let mut command = Command::new(env!("CARGO_BIN_EXE_vipd"));
    command.args(["run", "--config", config.to_str().unwrap()]).env("RUST_LOG", "vipd=info").stdout(Stdio::piped());
    // SAFETY: signal(2) is async-signal-safe, so it may run between fork and exec.
    unsafe {
        command.pre_exec(move || {
            libc::signal(libc::SIGHUP, sighup);
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut seen = Vec::new();
    let started = lines.by_ref().map(Result::unwrap).any(|line| {
        let found = line.contains("starting election");
        seen.push(line);
        found
    });
    assert!(started, "vipd stopped before the election started: {seen:?}");
    Running { child, lines, config }
}

#[cfg(target_os = "linux")]
fn send(child: &std::process::Child, signal: &str) {
    assert!(Command::new("kill").args([signal, &child.id().to_string()]).status().unwrap().success());
}

/// Waits up to 20 s for vipd to exit, and returns its exit status and the rest of its log.
#[cfg(target_os = "linux")]
fn wait_for_exit(mut vipd: Running) -> (std::process::ExitStatus, Vec<String>) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let status = loop {
        if let Some(status) = vipd.child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() > deadline {
            let _ = vipd.child.kill();
            panic!("vipd was still running 20 s after the signal");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    let rest = vipd.lines.map(Result::unwrap).collect();
    let _ = std::fs::remove_file(&vipd.config);
    (status, rest)
}

#[cfg(target_os = "linux")]
#[test]
fn sighup_stops_vipd_cleanly() {
    let vipd = start_vipd("sighup", libc::SIG_DFL);
    send(&vipd.child, "-HUP");
    let (status, rest) = wait_for_exit(vipd);
    assert_eq!(status.code(), Some(0), "{rest:?}");
    assert!(rest.iter().any(|line| line.contains("shutting down")), "{rest:?}");
}

#[cfg(target_os = "linux")]
#[test]
fn an_ignored_sighup_stays_ignored() {
    // As under `nohup vipd run`: a hangup must not stop vipd.
    let mut vipd = start_vipd("nohup", libc::SIG_IGN);
    send(&vipd.child, "-HUP");
    std::thread::sleep(std::time::Duration::from_secs(1));
    assert!(vipd.child.try_wait().unwrap().is_none(), "vipd stopped on an ignored SIGHUP");
    send(&vipd.child, "-TERM");
    let (status, rest) = wait_for_exit(vipd);
    assert_eq!(status.code(), Some(0), "{rest:?}");
}

#[cfg(not(windows))]
#[test]
fn service_commands_point_to_the_systemd_unit() {
    let out = vipd(&["service", "uninstall"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("packaging/vipd.service"), "{}", stderr(&out));
}
