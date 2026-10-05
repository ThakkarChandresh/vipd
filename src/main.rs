use std::path::Path;
use std::process::ExitCode;

use clap::Parser;
use vipd::cli::{Cli, Command};
use vipd::config::{Config, ConfigError};
use vipd::vip::{PlatformBackend, VipBackend};
use vipd::{logging, runtime, service};

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Run { config } => run(&config),
        Command::CheckConfig { config } => check_config(&config),
        Command::Service(command) => service::handle(command),
    }
}

/// Loads the config, or prints why it is invalid and returns exit code 2.
fn load(path: &Path) -> Result<Config, ExitCode> {
    Config::load(path).map_err(|err| {
        match err {
            // A read error already names the file.
            ConfigError::Read { .. } => eprintln!("{err}"),
            _ => eprintln!("{}: {err}", path.display()),
        }
        ExitCode::from(2)
    })
}

fn tokio_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread().enable_all().build().expect("cannot start the tokio runtime")
}

fn run(path: &Path) -> ExitCode {
    let config = match load(path) {
        Ok(config) => config,
        Err(code) => return code,
    };
    logging::init_stdout(&config.log_level);
    match tokio_runtime().block_on(runtime::run(config, PlatformBackend::new(), shutdown_signal())) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!(error = %format!("{err:#}"), "vipd stopped");
            ExitCode::from(1)
        }
    }
}

fn check_config(path: &Path) -> ExitCode {
    let config = match load(path) {
        Ok(config) => config,
        Err(code) => return code,
    };
    let backend = PlatformBackend::new();
    let problems: Vec<String> = tokio_runtime().block_on(async {
        let mut problems = Vec::new();
        for vip in &config.vips {
            match backend.interface_exists(&vip.interface).await {
                Ok(true) => {}
                Ok(false) => problems
                    .push(format!("vip {}: interface {:?} does not exist on this machine", vip.ip, vip.interface)),
                Err(err) => {
                    problems.push(format!("vip {}: cannot check interface {:?}: {err:#}", vip.ip, vip.interface))
                }
            }
        }
        problems
    });
    if problems.is_empty() {
        println!("{}: OK", path.display());
        ExitCode::SUCCESS
    } else {
        eprintln!("{}: invalid config:\n  - {}", path.display(), problems.join("\n  - "));
        ExitCode::from(2)
    }
}

/// Completes on Ctrl+C, or on SIGTERM on Unix (what systemd sends).
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("cannot listen for SIGTERM");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
