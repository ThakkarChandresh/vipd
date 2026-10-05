//! Runs vipd as a native Windows service (spec §12, §13).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::sync::Notify;
use windows_service::service::{
    ServiceAccess, ServiceAction, ServiceActionType, ServiceControl, ServiceControlAccept, ServiceErrorControl,
    ServiceExitCode, ServiceFailureActions, ServiceFailureResetPeriod, ServiceInfo, ServiceStartType, ServiceState,
    ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{define_windows_service, service_dispatcher};

use crate::cli::{default_config_path, ServiceCommand};
use crate::config::Config;
use crate::vip::PlatformBackend;
use crate::{logging, runtime};

const SERVICE_NAME: &str = "vipd";
const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;

/// The SCM passes the config path as a process argument; the service entry point reads it here.
static CONFIG_PATH: OnceLock<PathBuf> = OnceLock::new();

pub fn handle(command: ServiceCommand) -> ExitCode {
    let result = match command {
        ServiceCommand::Install { config } => install(&config),
        ServiceCommand::Uninstall => uninstall(),
        ServiceCommand::Run { config } => {
            let _ = CONFIG_PATH.set(config);
            service_dispatcher::start(SERVICE_NAME, ffi_service_main).map_err(anyhow::Error::from)
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::from(1)
        }
    }
}

fn install(config: &Path) -> anyhow::Result<()> {
    let config = std::path::absolute(config)?;
    Config::load(&config)?; // refuse to install with a broken config
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )?;
    let info = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from("vipd virtual IP failover"),
        service_type: SERVICE_TYPE,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: std::env::current_exe()?,
        launch_arguments: vec![
            OsString::from("service"),
            OsString::from("run"),
            OsString::from("--config"),
            config.into_os_string(),
        ],
        dependencies: vec![],
        account_name: None, // LocalSystem
        account_password: None,
    };
    let service = manager.create_service(&info, ServiceAccess::CHANGE_CONFIG | ServiceAccess::START)?;
    service.set_description("Keeps a virtual IP on one healthy node of a group")?;
    service.update_failure_actions(ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(86_400)),
        reboot_msg: None,
        command: None,
        actions: Some(vec![ServiceAction { action_type: ServiceActionType::Restart, delay: Duration::from_secs(5) }]),
    })?;
    // Restart after an error exit too, not only after a crash: vipd exits with an error when start-up
    // fails or its VIP worker dies, and start-up cleanup then removes any leftover VIP.
    service.set_failure_actions_on_non_crash_failures(true)?;
    println!("Installed the vipd service. Start it with: sc start vipd");
    Ok(())
}

fn uninstall() -> anyhow::Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = manager
        .open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE)?;
    if service.query_status()?.current_state != ServiceState::Stopped {
        service.stop()?;
    }
    service.delete()?;
    println!("Removed the vipd service.");
    Ok(())
}

define_windows_service!(ffi_service_main, service_main);

fn service_main(_arguments: Vec<OsString>) {
    // run_service logs its own errors, while its log file is still open.
    let _ = run_service();
}

fn run_service() -> anyhow::Result<()> {
    let config_path = CONFIG_PATH.get().cloned().unwrap_or_else(default_config_path);
    let stop = Arc::new(Notify::new());
    let stop_from_scm = stop.clone();
    let status = service_control_handler::register(SERVICE_NAME, move |control| match control {
        ServiceControl::Stop => {
            stop_from_scm.notify_one();
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    })?;
    let set_state = move |state: ServiceState, exit_code: u32, wait_hint: Duration| {
        status.set_service_status(ServiceStatus {
            service_type: SERVICE_TYPE,
            current_state: state,
            controls_accepted: if state == ServiceState::Running {
                ServiceControlAccept::STOP
            } else {
                ServiceControlAccept::empty()
            },
            exit_code: ServiceExitCode::Win32(exit_code),
            checkpoint: 0,
            wait_hint,
            process_id: None,
        })
    };

    // Log to the configured directory, or to the default one when the config cannot be loaded, so
    // that error is recorded too.
    let config = Config::load(&config_path);
    let (log_dir, log_level) = match &config {
        Ok(config) => (config.log_dir.clone().unwrap_or_else(default_log_dir), config.log_level.clone()),
        Err(_) => (default_log_dir(), "info".to_string()),
    };
    let _log_guard = logging::init_file(&log_dir, &log_level);
    let stop_requested = Arc::new(AtomicBool::new(false));
    let result = (|| -> anyhow::Result<()> {
        let config = config?;
        set_state(ServiceState::Running, 0, Duration::ZERO)?;
        tracing::info!(config = %config_path.display(), "vipd service started");
        let shutdown = {
            let stop_requested = stop_requested.clone();
            async move {
                stop.notified().await;
                stop_requested.store(true, Ordering::SeqCst);
                // Up to 15 s for the VIPs and 5 s for on_stop (spec §11.4).
                let _ = set_state(ServiceState::StopPending, 0, Duration::from_secs(25));
            }
        };
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        rt.block_on(runtime::run(config, PlatformBackend::new(), shutdown))
    })();
    if let Err(err) = &result {
        tracing::error!(error = %format!("{err:#}"), "the vipd service failed");
    }
    // A stop the operator asked for is not a failure, even if a VIP could not be removed: with
    // restart-on-failure the SCM would otherwise start the service it was just told to stop.
    let exit_code = if result.is_ok() || stop_requested.load(Ordering::SeqCst) { 0 } else { 1 };
    set_state(ServiceState::Stopped, exit_code, Duration::ZERO)?;
    result
}

fn default_log_dir() -> PathBuf {
    PathBuf::from(r"C:\ProgramData\vipd")
}
