//! Runs vipd as a native Windows service (spec §12, §13).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::sync::Notify;
use windows_service::service::{
    Service, ServiceAccess, ServiceAction, ServiceActionType, ServiceControl, ServiceControlAccept,
    ServiceErrorControl, ServiceExitCode, ServiceFailureActions, ServiceFailureResetPeriod, ServiceInfo,
    ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{define_windows_service, service_dispatcher};

use super::exit_code;
use crate::cli::{default_config_path, ServiceCommand};
use crate::config::Config;
use crate::vip::PlatformBackend;
use crate::{logging, runtime};

const SERVICE_NAME: &str = "vipd";
const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;
/// How long a stop may take: 15 s for the VIPs plus 5 s for on_stop (spec §11.4), or, during
/// start-up, up to 20 s per VIP to remove leftovers first.
const STOP_WAIT_HINT: Duration = Duration::from_secs(60);

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
    let service =
        manager.create_service(&info, ServiceAccess::CHANGE_CONFIG | ServiceAccess::START | ServiceAccess::DELETE)?;
    // A half-configured service would make the next install fail with "already exists".
    if let Err(err) = configure(&service) {
        let _ = service.delete();
        return Err(err);
    }
    println!("Installed the vipd service. Start it with: sc.exe start vipd");
    Ok(())
}

fn configure(service: &Service) -> anyhow::Result<()> {
    service.set_description("Keeps a virtual IP on one healthy node of a group")?;
    // Restart 5 s after a failure, twice; after that once a minute, so a broken config does not flood
    // the event log. The count resets after a day without failures.
    let restart = |secs| ServiceAction { action_type: ServiceActionType::Restart, delay: Duration::from_secs(secs) };
    service.update_failure_actions(ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(86_400)),
        reboot_msg: None,
        command: None,
        actions: Some(vec![restart(5), restart(5), restart(60)]),
    })?;
    // Restart after an error exit too, not only after a crash: vipd exits with an error when start-up
    // fails or its VIP worker dies, and start-up cleanup then removes any leftover VIP.
    service.set_failure_actions_on_non_crash_failures(true)?;
    // When Windows shuts down, give vipd time to hand the VIPs over before other services stop.
    service.set_preshutdown_timeout(Duration::from_secs(25))?;
    Ok(())
}

fn uninstall() -> anyhow::Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = manager
        .open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE)?;
    // Stop it, waiting up to a minute, so an install right after this does not find the old service
    // still "marked for deletion". A service that is still starting is stopped once it runs.
    let mut stopped = false;
    for _ in 0..120 {
        match service.query_status()?.current_state {
            ServiceState::Stopped => {
                stopped = true;
                break;
            }
            ServiceState::Running => {
                let _ = service.stop();
            }
            _ => {}
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    service.delete()?;
    if stopped {
        println!("Removed the vipd service.");
    } else {
        println!("Marked the vipd service for deletion; Windows removes it once it has stopped.");
    }
    Ok(())
}

define_windows_service!(ffi_service_main, service_main);

fn service_main(_arguments: Vec<OsString>) {
    run_service();
}

fn run_service() {
    let config_path = CONFIG_PATH.get().cloned().unwrap_or_else(default_config_path);
    let stop = Arc::new(Notify::new());
    let stop_requested = Arc::new(AtomicBool::new(false));
    let handler = {
        let (stop, stop_requested) = (stop.clone(), stop_requested.clone());
        move |control: ServiceControl| match control {
            // Preshutdown comes when Windows shuts down: hand the VIPs over just like for a stop.
            ServiceControl::Stop | ServiceControl::Preshutdown => {
                stop_requested.store(true, Ordering::SeqCst);
                stop.notify_one();
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };
    // Without a status handle nothing can be reported, and logging is not set up yet.
    let Ok(status) = service_control_handler::register(SERVICE_NAME, handler) else {
        return;
    };
    let set_state = move |state: ServiceState, code: u32, wait_hint: Duration| {
        status.set_service_status(ServiceStatus {
            service_type: SERVICE_TYPE,
            current_state: state,
            controls_accepted: if state == ServiceState::Running {
                ServiceControlAccept::STOP | ServiceControlAccept::PRESHUTDOWN
            } else {
                ServiceControlAccept::empty()
            },
            exit_code: match code {
                0 => ServiceExitCode::Win32(0),
                code => ServiceExitCode::ServiceSpecific(code),
            },
            checkpoint: 0,
            wait_hint,
            process_id: None,
        })
    };

    // Log to the configured directory. Fall back to the default one when the config cannot be loaded
    // or its log_dir cannot be used, so those errors are recorded too.
    let config = Config::load(&config_path);
    let (log_dir, log_level) = match &config {
        Ok(config) => (config.log_dir.clone().unwrap_or_else(default_log_dir), config.log_level.clone()),
        Err(_) => (default_log_dir(), "info".to_string()),
    };
    if let Err(err) = logging::init_file(&log_dir, &log_level) {
        let _ = logging::init_file(&default_log_dir(), &log_level);
        tracing::error!(
            dir = %log_dir.display(),
            error = %format!("{err:#}"),
            "cannot log to log_dir; using the default"
        );
    }

    let result = (|| -> anyhow::Result<()> {
        let config = config?;
        set_state(ServiceState::Running, 0, Duration::ZERO)?;
        tracing::info!(config = %config_path.display(), "vipd service started");
        let shutdown = async move {
            stop.notified().await;
            let _ = set_state(ServiceState::StopPending, 0, STOP_WAIT_HINT);
        };
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        rt.block_on(runtime::run(config, PlatformBackend::new(), shutdown))
    })();
    if let Err(err) = &result {
        tracing::error!(error = %format!("{err:#}"), "the vipd service failed");
    }
    // Logging is synchronous, so everything above is on disk before the SCM may end the process.
    let _ = set_state(ServiceState::Stopped, exit_code(&result, stop_requested.load(Ordering::SeqCst)), Duration::ZERO);
}

/// A directory of its own, because the 14-file limit prunes every `vipd*.log` file in it.
fn default_log_dir() -> PathBuf {
    PathBuf::from(r"C:\ProgramData\vipd\logs")
}
