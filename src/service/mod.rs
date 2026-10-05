//! Windows service integration. Linux uses the systemd unit in `packaging/vipd.service`.

use std::process::ExitCode;

use crate::cli::ServiceCommand;

#[cfg(windows)]
mod windows;

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
