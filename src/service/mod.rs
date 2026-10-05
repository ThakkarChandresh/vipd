//! Windows service integration. Linux uses the systemd unit in `packaging/vipd.service`.

use std::process::ExitCode;

use crate::cli::ServiceCommand;

#[cfg(windows)]
mod windows;

/// What a service run reports to the Service Control Manager. A stop the operator asked for is 0
/// even if it failed, because any other code makes the SCM restart the service it was just told to
/// stop. Otherwise an invalid config is 2 and any other failure 1, as on the command line.
#[cfg(any(windows, test))]
fn exit_code(result: &anyhow::Result<()>, stop_requested: bool) -> u32 {
    match result {
        _ if stop_requested => 0,
        Ok(()) => 0,
        Err(err) if err.downcast_ref::<crate::config::ConfigError>().is_some() => 2,
        Err(_) => 1,
    }
}

pub fn handle(command: ServiceCommand) -> ExitCode {
    #[cfg(windows)]
    {
        windows::handle(command)
    }
    #[cfg(not(windows))]
    {
        let _ = command;
        eprintln!(
            "`vipd service` is only available on Windows. On Linux, install packaging/vipd.service with systemd."
        );
        ExitCode::from(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConfigError;

    #[test]
    fn a_requested_stop_is_never_reported_as_a_failure() {
        assert_eq!(exit_code(&Ok(()), false), 0);
        assert_eq!(exit_code(&Err(anyhow::anyhow!("the VIPs were still not removed after 15 s")), true), 0);
        assert_eq!(exit_code(&Err(anyhow::anyhow!("the VIP worker stopped unexpectedly")), false), 1);
        assert_eq!(exit_code(&Err(ConfigError::Parse("bad".into()).into()), false), 2);
    }
}
