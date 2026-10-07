//! Log setup: stdout for consoles and systemd, a daily-rotated file for the Windows service.

use tracing_subscriber::EnvFilter;

fn filter(level: &str) -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level))
}

/// Logs panics like everything else, since the Windows service has no stderr. The default hook
/// still runs afterwards.
fn log_panics() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!("{info}");
        default(info);
    }));
}

/// Logs to stdout, with colours only on a terminal, so none end up in the journal or in piped output.
/// `RUST_LOG` overrides `level`.
pub fn init_stdout(level: &str) {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter(level))
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .try_init();
    log_panics();
}

/// Logs to `<dir>/vipd.YYYY-MM-DD.log`, a new file each day (UTC), keeping the last 14. Writes are
/// synchronous, so nothing is lost when the service process ends right after its last line.
#[cfg(windows)]
pub fn init_file(dir: &std::path::Path, level: &str) -> anyhow::Result<()> {
    use tracing_appender::rolling::{RollingFileAppender, Rotation};
    std::fs::create_dir_all(dir)?;
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("vipd")
        .filename_suffix("log")
        .max_log_files(14)
        .build(dir)?;
    let _ = tracing_subscriber::fmt().with_env_filter(filter(level)).with_ansi(false).with_writer(appender).try_init();
    log_panics();
    Ok(())
}
