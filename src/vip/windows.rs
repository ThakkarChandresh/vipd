//! Windows backend: `netsh` commands, plus a PowerShell duplicate-address check after attaching
//! (spec §7.3). Everything except the trait implementation and the link check is a pure function
//! tested on any OS.

use std::net::Ipv4Addr;
use std::time::Duration;

use super::{Vip, VipBackend, COMMAND_TIMEOUT};
use crate::exec;

const ATTACH_ATTEMPTS: u32 = 3;
const DHCP_HINT: &str = "if the adapter uses DHCP, run `netsh interface ipv4 set interface \
     interface=\"<adapter>\" dhcpstaticipcoexistence=enabled` once, or give it a static IP";

#[derive(Debug, Clone, Default)]
pub struct WindowsBackend;

impl WindowsBackend {
    pub fn new() -> Self {
        Self
    }
}

fn strings(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

pub fn interface_args(iface: &str) -> Vec<String> {
    strings(&["netsh", "interface", "ipv4", "show", "interfaces", iface])
}

pub fn find_args(iface: &str) -> Vec<String> {
    strings(&["netsh", "interface", "ipv4", "show", "ipaddresses", iface])
}

pub fn attach_args(vip: &Vip) -> Vec<String> {
    strings(&[
        "netsh",
        "interface",
        "ipv4",
        "add",
        "address",
        &vip.interface,
        &vip.ip.to_string(),
        &vip.mask().to_string(),
        "store=active",
        "skipassource=true",
    ])
}

pub fn detach_args(vip: &Vip) -> Vec<String> {
    strings(&["netsh", "interface", "ipv4", "delete", "address", &vip.interface, &vip.ip.to_string(), "store=active"])
}

/// PowerShell that waits up to 3 s for duplicate-address detection to finish for the VIP on its
/// interface, then prints the state. PowerShell runs by full path, because the service runs as
/// LocalSystem and a PATH lookup could pick up a planted `powershell.exe`. The adapter name is not
/// part of the script: the caller passes it in the `VIPD_IFACE` environment variable, and `-eq`
/// compares it exactly (no wildcards), so no character in it can change the script. The script
/// contains no double quotes, so Windows command-line quoting cannot change it either.
pub fn address_state_args(vip: &Vip) -> Vec<String> {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    let powershell = format!(r"{}\System32\WindowsPowerShell\v1.0\powershell.exe", root.to_string_lossy());
    let script = format!(
        "$d=(Get-Date).AddSeconds(3); do {{ $s=(Get-NetIPAddress -IPAddress '{ip}' \
         -ErrorAction SilentlyContinue | Where-Object InterfaceAlias -eq $env:VIPD_IFACE | \
         Select-Object -First 1).AddressState; if ($s -ne 'Tentative') {{ break }}; \
         Start-Sleep -Milliseconds 250 }} while ((Get-Date) -lt $d); [string]$s",
        ip = vip.ip
    );
    strings(&[&powershell, "-NoProfile", "-NonInteractive", "-Command", &script])
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddressState {
    Preferred,
    Duplicate,
    Tentative,
    Missing,
    Other(String),
}

pub fn parse_address_state(output: &str) -> AddressState {
    match output.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or("") {
        "" => AddressState::Missing,
        "Preferred" => AddressState::Preferred,
        "Duplicate" => AddressState::Duplicate,
        "Tentative" => AddressState::Tentative,
        other => AddressState::Other(other.to_string()),
    }
}

/// Runs the duplicate-address check. `Err` says why the check itself could not run.
async fn check_address_state(vip: &Vip) -> Result<AddressState, String> {
    match exec::run(&address_state_args(vip), COMMAND_TIMEOUT, &[("VIPD_IFACE", vip.interface.clone())]).await {
        Ok(out) if out.success => Ok(parse_address_state(&out.stdout)),
        Ok(out) => Err(out.failure()),
        Err(err) => Err(format!("{err:#}")),
    }
}

/// True if `ip` appears as a whole whitespace-separated token. Works in any Windows language.
pub fn output_has_ip(output: &str, ip: Ipv4Addr) -> bool {
    let wanted = ip.to_string();
    output.split_whitespace().any(|token| token == wanted)
}

/// windows-sys's `IfOperStatus*` and `MediaConnectState*` values, from
/// `windows_sys::Win32::NetworkManagement::Ndis`. They are here so that `link_state_is_up` compiles
/// and is tested on any OS; on Windows, the asserts below compare them with windows-sys's own.
#[cfg_attr(not(windows), allow(dead_code))]
mod ndis {
    pub const UP: i32 = 1;
    pub const DOWN: i32 = 2;
    pub const TESTING: i32 = 3;
    pub const UNKNOWN: i32 = 4;
    pub const DORMANT: i32 = 5;
    pub const NOT_PRESENT: i32 = 6;
    pub const LOWER_LAYER_DOWN: i32 = 7;
    pub const MEDIA_UNKNOWN: i32 = 0;
    pub const MEDIA_CONNECTED: i32 = 1;
    pub const MEDIA_DISCONNECTED: i32 = 2;
}

#[cfg(windows)]
const _: () = {
    use windows_sys::Win32::NetworkManagement::Ndis as sys;
    assert!(ndis::UP == sys::IfOperStatusUp);
    assert!(ndis::DOWN == sys::IfOperStatusDown);
    assert!(ndis::TESTING == sys::IfOperStatusTesting);
    assert!(ndis::UNKNOWN == sys::IfOperStatusUnknown);
    assert!(ndis::DORMANT == sys::IfOperStatusDormant);
    assert!(ndis::NOT_PRESENT == sys::IfOperStatusNotPresent);
    assert!(ndis::LOWER_LAYER_DOWN == sys::IfOperStatusLowerLayerDown);
    assert!(ndis::MEDIA_UNKNOWN == sys::MediaConnectStateUnknown);
    assert!(ndis::MEDIA_CONNECTED == sys::MediaConnectStateConnected);
    assert!(ndis::MEDIA_DISCONNECTED == sys::MediaConnectStateDisconnected);
};

/// Whether an adapter's link counts as up, from the operational status and media-connect state that
/// `GetIfEntry2` reports. Only positive evidence counts as down, as on Linux, where `unknown` is up:
/// a disconnected medium, or a status other than up or unknown (down, testing, dormant, not present
/// or lower layer down).
#[cfg_attr(not(windows), allow(dead_code))] // only Windows calls it, but its test runs on any OS
fn link_state_is_up(oper_status: i32, media_state: i32) -> bool {
    media_state != ndis::MEDIA_DISCONNECTED && matches!(oper_status, ndis::UP | ndis::UNKNOWN)
}

/// Whether the adapter's link is up, as `link_state_is_up` decides from what the IP Helper API
/// reports: two system calls, not a netsh or PowerShell process, every advert interval. Switching
/// Wi-Fi off leaves the adapter's address in place, so only this notices. An alias that cannot be
/// resolved, or an entry that cannot be read, counts as up ("cannot tell", see
/// `VipBackend::link_up`): the bind-address check still applies, and a renamed adapter must never
/// keep a node in Fault for good.
#[cfg(windows)]
fn adapter_connected(alias: &str) -> bool {
    use windows_sys::Win32::Foundation::NO_ERROR;
    use windows_sys::Win32::NetworkManagement::IpHelper::{ConvertInterfaceAliasToLuid, GetIfEntry2, MIB_IF_ROW2};

    let alias: Vec<u16> = alias.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `alias` is NUL-terminated and outlives both calls. `row` is plain data, for which all
    // zeroes is a valid value, and both calls only write into it through pointers to it that do not
    // outlive this block.
    unsafe {
        let mut row: MIB_IF_ROW2 = std::mem::zeroed();
        if ConvertInterfaceAliasToLuid(alias.as_ptr(), &mut row.InterfaceLuid) != NO_ERROR
            || GetIfEntry2(&mut row) != NO_ERROR
        {
            return true;
        }
        link_state_is_up(row.OperStatus, row.MediaConnectState)
    }
}

/// Off Windows this backend is only compiled and unit-tested, and cannot ask Windows about an
/// adapter, so it says yes, as a backend that cannot tell does.
#[cfg(not(windows))]
fn adapter_connected(_alias: &str) -> bool {
    true
}

impl VipBackend for WindowsBackend {
    async fn interface_exists(&self, iface: &str) -> anyhow::Result<bool> {
        Ok(exec::run(&interface_args(iface), COMMAND_TIMEOUT, &[]).await?.success)
    }

    async fn find(&self, vip: &Vip) -> anyhow::Result<Option<String>> {
        let out = exec::run_ok(&find_args(&vip.interface), COMMAND_TIMEOUT).await?;
        Ok(output_has_ip(&out.stdout, vip.ip).then(|| format!("{}/{}", vip.ip, vip.prefix)))
    }

    async fn attach(&self, vip: &Vip) -> anyhow::Result<()> {
        for attempt in 1..=ATTACH_ATTEMPTS {
            exec::run_ok(&attach_args(vip), COMMAND_TIMEOUT).await.map_err(|e| e.context(DHCP_HINT))?;
            let state = match check_address_state(vip).await {
                Ok(state) => state,
                Err(reason) => {
                    // Windows still runs duplicate-address detection itself; only the retry is lost.
                    tracing::warn!(vip = %vip.ip, %reason, "cannot check the VIP for a duplicate address; keeping it");
                    return Ok(());
                }
            };
            match state {
                AddressState::Preferred => return Ok(()),
                AddressState::Duplicate => {
                    tracing::warn!(vip = %vip.ip, attempt, "Windows marked the VIP Duplicate; removing it");
                    exec::run_ok(&detach_args(vip), COMMAND_TIMEOUT).await?;
                    if attempt < ATTACH_ATTEMPTS {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
                AddressState::Missing => {
                    // The script silences WMI/CIM errors, so an empty answer can also mean the check failed
                    // inside PowerShell. netsh added the address, so ask it before failing the attach.
                    if self.find(vip).await?.is_some() {
                        tracing::warn!(
                            vip = %vip.ip,
                            "the duplicate-address check did not see the VIP, but netsh does; keeping it"
                        );
                        return Ok(());
                    }
                    // Config validation rejects an adapter index, which the InterfaceAlias filter could not
                    // match, so a missing address means the attach really failed.
                    anyhow::bail!("{} is not on {} after adding it", vip.ip, vip.interface)
                }
                other => {
                    tracing::warn!(vip = %vip.ip, state = ?other, "VIP attached but not yet Preferred");
                    return Ok(());
                }
            }
        }
        anyhow::bail!("{} stayed Duplicate after {ATTACH_ATTEMPTS} attempts; another machine holds it", vip.ip)
    }

    async fn detach(&self, vip: &Vip, _found: &str) -> anyhow::Result<()> {
        exec::run_ok(&detach_args(vip), COMMAND_TIMEOUT).await.map(|_| ())
    }

    async fn announce(&self, _vip: &Vip) -> anyhow::Result<()> {
        // Windows announces a newly added address itself (to be confirmed on hardware, spec §15).
        Ok(())
    }

    async fn link_up(&self, iface: &str) -> bool {
        adapter_connected(iface)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vip() -> Vip {
        Vip { ip: Ipv4Addr::new(192, 168, 1, 201), prefix: 24, interface: "Ethernet 2".into() }
    }

    #[test]
    fn netsh_commands_keep_the_adapter_name_as_one_argument() {
        assert_eq!(
            attach_args(&vip()).join("|"),
            "netsh|interface|ipv4|add|address|Ethernet 2|192.168.1.201|255.255.255.0|store=active|skipassource=true"
        );
        assert_eq!(
            detach_args(&vip()).join("|"),
            "netsh|interface|ipv4|delete|address|Ethernet 2|192.168.1.201|store=active"
        );
        assert_eq!(find_args("Ethernet 2").join("|"), "netsh|interface|ipv4|show|ipaddresses|Ethernet 2");
        assert_eq!(interface_args("Ethernet 2").join("|"), "netsh|interface|ipv4|show|interfaces|Ethernet 2");
    }

    #[test]
    fn finds_the_ip_only_as_a_whole_token() {
        let out = "Addr Type  DAD State   Valid Life Pref. Life Address\n\
                   ---------  ----------- ---------- ---------- -------\n\
                   Manual     Preferred     infinite   infinite 192.168.1.201\n";
        assert!(output_has_ip(out, Ipv4Addr::new(192, 168, 1, 201)));
        assert!(!output_has_ip(out, Ipv4Addr::new(192, 168, 1, 20)));
    }

    #[test]
    fn parses_powershell_address_states() {
        assert_eq!(parse_address_state("Preferred\r\n"), AddressState::Preferred);
        assert_eq!(parse_address_state("\r\nDuplicate\r\n"), AddressState::Duplicate);
        assert_eq!(parse_address_state(""), AddressState::Missing);
        assert_eq!(parse_address_state("Tentative"), AddressState::Tentative);
        assert_eq!(parse_address_state("Deprecated"), AddressState::Other("Deprecated".into()));
    }

    #[test]
    fn only_positive_evidence_makes_a_windows_link_down() {
        use ndis::*;
        assert!(link_state_is_up(UP, MEDIA_CONNECTED));
        assert!(link_state_is_up(UP, MEDIA_UNKNOWN));
        assert!(link_state_is_up(UNKNOWN, MEDIA_UNKNOWN));
        assert!(!link_state_is_up(UP, MEDIA_DISCONNECTED));
        for down in [DOWN, TESTING, DORMANT, NOT_PRESENT, LOWER_LAYER_DOWN] {
            assert!(!link_state_is_up(down, MEDIA_CONNECTED), "status {down}");
        }
    }

    /// Runs only on Windows, in the release CI.
    #[cfg(windows)]
    #[test]
    fn an_adapter_windows_does_not_know_counts_as_connected() {
        // The FFI path runs, and "cannot tell" is up, so a renamed adapter never keeps a node in Fault.
        assert!(adapter_connected("vipd-no-such-adapter"));
    }

    #[test]
    fn the_powershell_script_targets_the_vip_on_its_interface() {
        let mut v = vip();
        v.interface = "Bob's NIC".into();
        let args = address_state_args(&v);
        assert!(args[0].ends_with(r"\System32\WindowsPowerShell\v1.0\powershell.exe"), "{}", args[0]);
        assert_eq!(&args[1..4], &["-NoProfile", "-NonInteractive", "-Command"]);
        assert!(args[4].contains("Get-NetIPAddress -IPAddress '192.168.1.201'"), "{}", args[4]);
        assert!(args[4].contains("Where-Object InterfaceAlias -eq $env:VIPD_IFACE"), "{}", args[4]);
        assert!(!args[4].contains("Bob"), "the adapter name stays out of the script: {}", args[4]);
        assert!(!args[4].contains('"'), "no double quotes: {}", args[4]);
    }
}
