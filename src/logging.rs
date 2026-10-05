//! Log setup: stdout for consoles and systemd, a daily-rotated file for the Windows service.

use tracing_subscriber::EnvFilter;

fn filter(level: &str) -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level))
}

/// Logs to stdout. `RUST_LOG` overrides `level`.
pub fn init_stdout(level: &str) {
    let _ = tracing_subscriber::fmt().with_env_filter(filter(level)).try_init();
}

/// Logs to `<dir>/vipd.log`, rotated daily. Keep the returned guard alive so logs get flushed.
#[cfg(windows)]
pub fn init_file(dir: &std::path::Path, level: &str) -> anyhow::Result<tracing_appender::non_blocking::WorkerGuard> {
    std::fs::create_dir_all(dir)?;
    let appender = tracing_appender::rolling::daily(dir, "vipd.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let _ = tracing_subscriber::fmt().with_env_filter(filter(level)).with_ansi(false).with_writer(writer).try_init();
    Ok(guard)
}
