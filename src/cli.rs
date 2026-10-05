//! Command-line interface (spec §12).

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "vipd", version, about = "Virtual IP failover daemon for Linux and Windows")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run in the foreground until SIGTERM, SIGINT or Ctrl+C.
    Run {
        #[arg(long, default_value_os_t = default_config_path())]
        config: PathBuf,
    },
    /// Validate the config file; exit code 0 means valid, 2 means invalid.
    CheckConfig {
        #[arg(long, default_value_os_t = default_config_path())]
        config: PathBuf,
    },
    /// Manage the Windows service.
    #[command(subcommand)]
    Service(ServiceCommand),
}

#[derive(Debug, Subcommand)]
pub enum ServiceCommand {
    /// Register vipd as an automatic-start Windows service.
    Install {
        #[arg(long, default_value_os_t = default_config_path())]
        config: PathBuf,
    },
    /// Remove the Windows service.
    Uninstall,
    /// Entry point for the Windows Service Control Manager.
    #[command(hide = true)]
    Run {
        #[arg(long)]
        config: PathBuf,
    },
}

pub fn default_config_path() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(r"C:\ProgramData\vipd\vipd.toml")
    } else {
        PathBuf::from("/etc/vipd/vipd.toml")
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use clap::CommandFactory;

    use super::*;

    #[test]
    fn the_cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn run_uses_the_default_config_path() {
        let cli = Cli::try_parse_from(["vipd", "run"]).unwrap();
        match cli.command {
            Command::Run { config } => assert_eq!(config, default_config_path()),
            other => panic!("unexpected command {other:?}"),
        }
    }

    #[test]
    fn check_config_accepts_a_path() {
        let cli = Cli::try_parse_from(["vipd", "check-config", "--config", "x.toml"]).unwrap();
        assert!(matches!(cli.command, Command::CheckConfig { config } if config.as_path() == Path::new("x.toml")));
    }
}
