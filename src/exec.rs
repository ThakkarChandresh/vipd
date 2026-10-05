//! Command templates, and running external commands with a timeout.
//!
//! Used by the VIP command overrides, the health checks and the hooks. Nothing here uses a shell.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context};

/// Splits a command template into arguments.
///
/// Whitespace separates arguments. A `"double-quoted"` segment is kept together and the quotes
/// are removed. There are no escape sequences.
pub fn tokenize(template: &str) -> anyhow::Result<Vec<String>> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_arg = false;
    let mut in_quotes = false;
    for c in template.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                in_arg = true;
            }
            c if c.is_whitespace() && !in_quotes => {
                if in_arg {
                    args.push(std::mem::take(&mut current));
                    in_arg = false;
                }
            }
            c => {
                current.push(c);
                in_arg = true;
            }
        }
    }
    if in_quotes {
        bail!("unterminated quote in command: {template}");
    }
    if in_arg {
        args.push(current);
    }
    if args.is_empty() {
        bail!("command is empty");
    }
    Ok(args)
}

/// Replaces `{name}` placeholders inside each argument. Values are substituted one name at a time,
/// so a value must not itself contain another placeholder (vipd's values never do).
pub fn substitute(args: &[String], vars: &[(&str, String)]) -> Vec<String> {
    args.iter()
        .map(|arg| vars.iter().fold(arg.clone(), |acc, (name, value)| acc.replace(&format!("{{{name}}}"), value)))
        .collect()
}

#[derive(Debug, Clone)]
pub struct Output {
    pub success: bool,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Runs `args[0]` with the remaining arguments. The process is killed if it outlives `timeout`.
pub async fn run(args: &[String], timeout: Duration, envs: &[(&str, String)]) -> anyhow::Result<Output> {
    let (program, rest) = args.split_first().context("empty command")?;
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(rest).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    // A separate process group lets a timeout kill everything the command started (`sh -c` children too).
    #[cfg(target_os = "linux")]
    cmd.process_group(0);
    // The child inherits vipd's environment (checks and hooks need PATH); `envs` are added on top.
    for (key, value) in envs {
        cmd.env(key, value);
    }
    let child = cmd.spawn().with_context(|| format!("cannot start `{}`", args.join(" ")))?;
    let mut group = GroupGuard { pid: child.id(), finished: false };
    // On timeout the `wait_with_output` future is dropped, which drops (and kills) the child; `group`
    // then kills anything the child started. Cancelling `run` itself also drops `group`.
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(result) => {
            let out = result.with_context(|| format!("cannot wait for `{}`", args.join(" ")))?;
            group.finished = true;
            Ok(Output {
                success: out.status.success(),
                code: out.status.code(),
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            })
        }
        Err(_) => bail!("`{}` timed out after {:?} and was killed", args.join(" "), timeout),
    }
}

/// Kills a command's whole process group unless the command finished on its own. A descendant that
/// moves to a group or session of its own (`setsid`) still escapes.
struct GroupGuard {
    pid: Option<u32>,
    finished: bool,
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if let (Some(pid), false) = (self.pid, self.finished) {
            kill_group(pid);
        }
    }
}

#[cfg(target_os = "linux")]
fn kill_group(pgid: u32) {
    // SAFETY: kill(2) on the process group created for this command; errors are irrelevant.
    unsafe {
        libc::kill(-(pgid as i32), libc::SIGKILL);
    }
}

/// Windows has no process groups, so only the direct process is killed (by `kill_on_drop`); a Job
/// Object would be needed for the rest.
#[cfg(not(target_os = "linux"))]
fn kill_group(_pgid: u32) {}

/// Like [`run`], but a non-zero exit code is an error that includes the command's output.
pub async fn run_ok(args: &[String], timeout: Duration) -> anyhow::Result<Output> {
    let out = run(args, timeout, &[]).await?;
    if !out.success {
        // netsh prints its errors on stdout, so fall back to it.
        let detail = if out.stderr.trim().is_empty() { out.stdout.trim() } else { out.stderr.trim() };
        let code = out.code.map_or_else(|| "none (killed by a signal)".to_string(), |code| code.to_string());
        bail!("`{}` failed with exit code {}: {}", args.join(" "), code, detail);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn splits_on_whitespace() {
        assert_eq!(tokenize("ip addr  add\t10.0.0.1/24").unwrap(), strings(&["ip", "addr", "add", "10.0.0.1/24"]));
    }

    #[test]
    fn keeps_quoted_segments_together() {
        assert_eq!(
            tokenize(r#"netsh "Ethernet 2" name="x y""#).unwrap(),
            strings(&["netsh", "Ethernet 2", "name=x y"])
        );
    }

    #[test]
    fn empty_quotes_make_an_empty_argument() {
        assert_eq!(tokenize(r#"echo """#).unwrap(), strings(&["echo", ""]));
    }

    #[test]
    fn rejects_unterminated_quotes_and_empty_commands() {
        assert!(tokenize(r#"echo "oops"#).is_err());
        assert!(tokenize("   ").is_err());
    }

    #[test]
    fn substitutes_placeholders_inside_arguments() {
        let args = strings(&["ip", "addr", "add", "{ip}/{prefix}", "dev", "{iface}"]);
        let vars = [("ip", "10.0.0.5".to_string()), ("prefix", "24".to_string()), ("iface", "Ethernet 2".to_string())];
        assert_eq!(substitute(&args, &vars), strings(&["ip", "addr", "add", "10.0.0.5/24", "dev", "Ethernet 2"]));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_captures_output_and_exit_code() {
        let out = run(&strings(&["sh", "-c", "echo hi; exit 3"]), Duration::from_secs(5), &[]).await.unwrap();
        assert!(!out.success);
        assert_eq!(out.code, Some(3));
        assert_eq!(out.stdout.trim(), "hi");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_passes_environment_variables() {
        let envs = [("VIPD_STATE", "MASTER".to_string())];
        let out = run(&strings(&["sh", "-c", "echo $VIPD_STATE"]), Duration::from_secs(5), &envs).await.unwrap();
        assert_eq!(out.stdout.trim(), "MASTER");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_kills_commands_that_time_out() {
        let started = std::time::Instant::now();
        let err = run(&strings(&["sleep", "5"]), Duration::from_millis(200), &[]).await.unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_timeout_kills_everything_the_command_started() {
        let pidfile = std::env::temp_dir().join(format!("vipd-exec-group-{}.pid", std::process::id()));
        let script = format!("sleep 30 & echo $! > {}; wait", pidfile.display());
        let err = run(&strings(&["sh", "-c", &script]), Duration::from_millis(300), &[]).await.unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
        let pid = std::fs::read_to_string(&pidfile).unwrap().trim().to_string();
        let _ = std::fs::remove_file(&pidfile);
        // The background `sleep` must be gone (or a zombie about to be reaped), not still running.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let running = std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| !stat.contains(") Z "));
            if !running {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "background sleep {pid} survived the timeout");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_ok_turns_a_failure_into_an_error() {
        let err = run_ok(&strings(&["sh", "-c", "echo boom >&2; exit 1"]), Duration::from_secs(5)).await.unwrap_err();
        assert!(err.to_string().contains("boom"), "{err}");
        assert!(err.to_string().contains("exit code 1:"), "{err}");
    }
}
