//! Windows backend: `netsh` commands, plus a PowerShell duplicate-address check after attaching
//! (spec §7.3). Everything except the trait implementation is a pure function tested on any OS.

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
/// interface, then prints the state. The script contains no double quotes, so Windows command-line
/// quoting cannot change it. A `'` in the adapter name is doubled, as single-quoted PowerShell
/// strings require, and `-eq` compares the name exactly (no wildcards).
pub fn address_state_args(vip: &Vip) -> Vec<String> {
    let alias = vip.interface.replace('\'', "''");
    let script = format!(
        "$d=(Get-Date).AddSeconds(3); do {{ $s=(Get-NetIPAddress -IPAddress '{ip}' \
         -ErrorAction SilentlyContinue | Where-Object InterfaceAlias -eq '{alias}' | \
         Select-Object -First 1).AddressState; if ($s -ne 'Tentative') {{ break }}; \
         Start-Sleep -Milliseconds 250 }} while ((Get-Date) -lt $d); [string]$s",
        ip = vip.ip
    );
    strings(&["powershell", "-NoProfile", "-NonInteractive", "-Command", &script])
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
    match exec::run(&address_state_args(vip), COMMAND_TIMEOUT, &[]).await {
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
                    // netsh also accepts an adapter index as `interface`, which the check's InterfaceAlias
                    // filter cannot match. Failing would leave this node unable to ever hold the VIP.
                    tracing::warn!(
                        vip = %vip.ip,
                        interface = %vip.interface,
                        "the duplicate-address check did not find the VIP; keeping it (set `interface` to the adapter's name)"
                    );
                    return Ok(());
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
    fn the_powershell_script_targets_the_vip_on_its_interface() {
        let mut v = vip();
        v.interface = "Bob's NIC".into();
        let args = address_state_args(&v);
        assert_eq!(&args[..4], &["powershell", "-NoProfile", "-NonInteractive", "-Command"]);
        assert!(args[4].contains("Get-NetIPAddress -IPAddress '192.168.1.201'"), "{}", args[4]);
        assert!(args[4].contains("Where-Object InterfaceAlias -eq 'Bob''s NIC'"), "{}", args[4]);
        assert!(!args[4].contains('"'), "no double quotes: {}", args[4]);
    }
}
