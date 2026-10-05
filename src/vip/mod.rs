//! Attaching and detaching VIPs with each OS's own commands (spec §7).

pub mod fake;

use std::future::Future;
use std::net::Ipv4Addr;
use std::time::Duration;

use crate::exec;

#[cfg(not(any(target_os = "linux", windows)))]
compile_error!("vipd supports Linux and Windows only");

/// Every OS command gets this long before it is killed.
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vip {
    pub ip: Ipv4Addr,
    pub prefix: u8,
    pub interface: String,
}

impl Vip {
    /// The dotted netmask for `prefix`, e.g. 24 → 255.255.255.0. Config validation keeps `prefix`
    /// within 1..=32; anything larger is treated as 32.
    pub fn mask(&self) -> Ipv4Addr {
        Ipv4Addr::from(u32::MAX.checked_shl(32u32.saturating_sub(u32::from(self.prefix))).unwrap_or(0))
    }

    /// Values for the `{ip}`, `{prefix}`, `{mask}` and `{iface}` placeholders.
    pub fn template_vars(&self) -> Vec<(&'static str, String)> {
        vec![
            ("ip", self.ip.to_string()),
            ("prefix", self.prefix.to_string()),
            ("mask", self.mask().to_string()),
            ("iface", self.interface.clone()),
        ]
    }
}

/// Replacement attach / detach commands from `[vip_commands]` (spec §7.4), already tokenized.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandOverrides {
    pub attach: Option<Vec<String>>,
    pub detach: Option<Vec<String>>,
}

/// The OS-specific operations. `VipManager` adds idempotency and overrides on top.
pub trait VipBackend: Send + Sync + 'static {
    /// Does this network interface exist?
    fn interface_exists(&self, iface: &str) -> impl Future<Output = anyhow::Result<bool>> + Send;
    /// The address as currently configured (`"ip/prefix"`), or `None` if the VIP is not attached.
    fn find(&self, vip: &Vip) -> impl Future<Output = anyhow::Result<Option<String>>> + Send;
    /// Adds the VIP. Only called when `find` returned `None`.
    fn attach(&self, vip: &Vip) -> impl Future<Output = anyhow::Result<()>> + Send;
    /// Removes the VIP. `found` is what `find` returned.
    fn detach(&self, vip: &Vip, found: &str) -> impl Future<Output = anyhow::Result<()>> + Send;
    /// Tells the LAN where the VIP lives now. A no-op where the OS does this itself.
    fn announce(&self, vip: &Vip) -> impl Future<Output = anyhow::Result<()>> + Send;
}

/// Wraps a backend: runs `find` first so attach and detach are idempotent, and applies the
/// optional `[vip_commands]` overrides (spec §7.4).
pub struct VipManager<B> {
    backend: B,
    overrides: CommandOverrides,
}

impl<B: VipBackend> VipManager<B> {
    pub fn new(backend: B, overrides: CommandOverrides) -> Self {
        Self { backend, overrides }
    }

    pub async fn interface_exists(&self, iface: &str) -> anyhow::Result<bool> {
        self.backend.interface_exists(iface).await
    }

    /// Attaches the VIP unless it is already there. Returns true if it had to be added.
    pub async fn ensure_attached(&self, vip: &Vip) -> anyhow::Result<bool> {
        if self.backend.find(vip).await?.is_some() {
            return Ok(false);
        }
        match &self.overrides.attach {
            Some(template) => run_override(template, vip).await?,
            None => self.backend.attach(vip).await?,
        }
        Ok(true)
    }

    /// Detaches the VIP if it is there. Returns true if it had to be removed.
    pub async fn ensure_detached(&self, vip: &Vip) -> anyhow::Result<bool> {
        let Some(found) = self.backend.find(vip).await? else {
            return Ok(false);
        };
        match &self.overrides.detach {
            Some(template) => run_override(template, vip).await?,
            None => self.backend.detach(vip, &found).await?,
        }
        Ok(true)
    }

    pub async fn announce(&self, vip: &Vip) -> anyhow::Result<()> {
        self.backend.announce(vip).await
    }
}

async fn run_override(template: &[String], vip: &Vip) -> anyhow::Result<()> {
    let args = exec::substitute(template, &vip.template_vars());
    exec::run_ok(&args, COMMAND_TIMEOUT).await.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::fake::FakeBackend;
    use super::*;

    fn vip() -> Vip {
        Vip { ip: Ipv4Addr::new(10, 0, 0, 200), prefix: 24, interface: "eth0".into() }
    }

    #[test]
    fn mask_comes_from_the_prefix() {
        let mut v = vip();
        assert_eq!(v.mask(), Ipv4Addr::new(255, 255, 255, 0));
        v.prefix = 32;
        assert_eq!(v.mask(), Ipv4Addr::new(255, 255, 255, 255));
        v.prefix = 1;
        assert_eq!(v.mask(), Ipv4Addr::new(128, 0, 0, 0));
        v.prefix = 0;
        assert_eq!(v.mask(), Ipv4Addr::new(0, 0, 0, 0));
        v.prefix = 33;
        assert_eq!(v.mask(), Ipv4Addr::new(255, 255, 255, 255));
    }

    #[tokio::test]
    async fn attach_and_detach_are_idempotent() {
        let fake = FakeBackend::new();
        let manager = VipManager::new(fake.clone(), CommandOverrides::default());
        assert!(manager.ensure_attached(&vip()).await.unwrap());
        assert!(!manager.ensure_attached(&vip()).await.unwrap());
        assert!(manager.ensure_detached(&vip()).await.unwrap());
        assert!(!manager.ensure_detached(&vip()).await.unwrap());
        assert_eq!(fake.calls(), vec!["attach 10.0.0.200", "detach 10.0.0.200"]);
    }

    #[tokio::test]
    async fn a_failed_attach_is_an_error_and_leaves_the_vip_detached() {
        let fake = FakeBackend::new();
        fake.set_fail_attach(true);
        let manager = VipManager::new(fake.clone(), CommandOverrides::default());
        assert!(manager.ensure_attached(&vip()).await.is_err());
        assert!(!fake.is_attached(vip().ip));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn overrides_replace_the_built_in_commands() {
        let fake = FakeBackend::new();
        let overrides = CommandOverrides {
            attach: Some(exec::tokenize(r#"sh -c "exit 0""#).unwrap()),
            detach: Some(exec::tokenize("false").unwrap()),
        };
        let manager = VipManager::new(fake.clone(), overrides);
        assert!(manager.ensure_attached(&vip()).await.unwrap());
        assert!(fake.calls().is_empty(), "the override ran instead of the backend");
        fake.attach(&vip()).await.unwrap();
        assert!(manager.ensure_detached(&vip()).await.is_err(), "`false` exits with 1");
        assert_eq!(fake.calls(), vec!["attach 10.0.0.200"]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn override_templates_get_the_vip_values() {
        let out = std::env::temp_dir().join(format!("vipd-override-{}.txt", std::process::id()));
        let template = format!(r#"sh -c "echo {{ip}} {{prefix}} {{mask}} {{iface}} > {}""#, out.display());
        let overrides = CommandOverrides { attach: Some(exec::tokenize(&template).unwrap()), detach: None };
        let manager = VipManager::new(FakeBackend::new(), overrides);
        manager.ensure_attached(&vip()).await.unwrap();
        assert_eq!(std::fs::read_to_string(&out).unwrap().trim(), "10.0.0.200 24 255.255.255.0 eth0");
        let _ = std::fs::remove_file(out);
    }
}
