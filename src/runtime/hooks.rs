//! Runs the optional on_master / on_backup / on_fault / on_stop commands (spec §9).

use std::time::Duration;

use tokio::task::JoinHandle;

use crate::config::HookCommands;
use crate::election::HookKind;
use crate::exec;

const HOOK_TIMEOUT: Duration = Duration::from_secs(60);

pub fn state_name(kind: HookKind) -> &'static str {
    match kind {
        HookKind::Master => "MASTER",
        HookKind::Backup => "BACKUP",
        HookKind::Fault => "FAULT",
        HookKind::Stop => "STOP",
    }
}

fn command_for(hooks: &HookCommands, kind: HookKind) -> Option<&Vec<String>> {
    match kind {
        HookKind::Master => hooks.on_master.as_ref(),
        HookKind::Backup => hooks.on_backup.as_ref(),
        HookKind::Fault => hooks.on_fault.as_ref(),
        HookKind::Stop => hooks.on_stop.as_ref(),
    }
}

/// Starts the hook for `kind` in the background, if one is configured. Failures are only logged.
pub fn spawn(hooks: &HookCommands, kind: HookKind, priority: u8, group_id: u16) -> Option<JoinHandle<()>> {
    let command = command_for(hooks, kind)?.clone();
    let envs = vec![
        ("VIPD_STATE", state_name(kind).to_string()),
        ("VIPD_PRIORITY", priority.to_string()),
        ("VIPD_GROUP", group_id.to_string()),
    ];
    Some(tokio::spawn(async move {
        match exec::run(&command, HOOK_TIMEOUT, &envs).await {
            Ok(out) if out.success => {}
            Ok(out) => tracing::warn!(
                hook = state_name(kind),
                code = ?out.code,
                stderr = %out.stderr.trim(),
                "hook failed"
            ),
            Err(err) => tracing::warn!(hook = state_name(kind), error = %err, "hook failed"),
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_runs_without_a_configured_hook() {
        assert!(spawn(&HookCommands::default(), HookKind::Fault, 1, 1).is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_hook_gets_state_priority_and_group() {
        let out = std::env::temp_dir().join(format!("vipd-hook-{}.txt", std::process::id()));
        let hooks = HookCommands {
            on_master: Some(vec![
                "sh".into(),
                "-c".into(),
                format!("echo $VIPD_STATE $VIPD_PRIORITY $VIPD_GROUP > {}", out.display()),
            ]),
            ..HookCommands::default()
        };
        spawn(&hooks, HookKind::Master, 150, 51).unwrap().await.unwrap();
        assert_eq!(std::fs::read_to_string(&out).unwrap().trim(), "MASTER 150 51");
        let _ = std::fs::remove_file(out);
    }
}
