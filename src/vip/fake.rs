//! An in-memory backend for tests: remembers what is attached, records calls, can fail on demand.

use std::collections::BTreeSet;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::{Vip, VipBackend};

/// Cloning shares the same state, so a test can keep a handle to inspect.
#[derive(Debug, Clone, Default)]
pub struct FakeBackend {
    inner: Arc<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    attached: Mutex<BTreeSet<Ipv4Addr>>,
    calls: Mutex<Vec<String>>,
    fail_attach: AtomicBool,
    fail_detach: AtomicBool,
    panic_attach: AtomicBool,
    detach_delay_ms: AtomicU64,
    attach_delay_ms: AtomicU64,
    missing_interface: Mutex<Option<String>>,
    /// Down rather than up, so the derived default leaves every link up.
    link_down: AtomicBool,
}

impl FakeBackend {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_attached(&self, ip: Ipv4Addr) -> bool {
        self.inner.attached.lock().unwrap().contains(&ip)
    }

    /// Every attach, detach and announce so far, e.g. `"attach 10.0.0.200"`.
    pub fn calls(&self) -> Vec<String> {
        self.inner.calls.lock().unwrap().clone()
    }

    pub fn set_fail_attach(&self, fail: bool) {
        self.inner.fail_attach.store(fail, Ordering::SeqCst);
    }

    pub fn set_fail_detach(&self, fail: bool) {
        self.inner.fail_detach.store(fail, Ordering::SeqCst);
    }

    /// Makes every detach take this long, like a slow OS command.
    pub fn set_detach_delay(&self, delay: Duration) {
        self.inner.detach_delay_ms.store(delay.as_millis() as u64, Ordering::SeqCst);
    }

    /// Makes every attach take this long before it succeeds or fails, like a slow OS command.
    pub fn set_attach_delay(&self, delay: Duration) {
        self.inner.attach_delay_ms.store(delay.as_millis() as u64, Ordering::SeqCst);
    }

    /// Makes attach panic, to test what happens when the task running the backend dies.
    pub fn set_panic_on_attach(&self, panic: bool) {
        self.inner.panic_attach.store(panic, Ordering::SeqCst);
    }

    /// Makes this interface missing: `interface_exists` says so, and `find` fails on it, as with the
    /// real backends.
    pub fn set_missing_interface(&self, iface: &str) {
        *self.inner.missing_interface.lock().unwrap() = Some(iface.to_string());
    }

    fn is_missing(&self, iface: &str) -> bool {
        self.inner.missing_interface.lock().unwrap().as_deref() == Some(iface)
    }

    /// Takes the link of every interface down, or brings it back up. Links start up.
    pub fn set_link_up(&self, up: bool) {
        self.inner.link_down.store(!up, Ordering::SeqCst);
    }

    /// Drops the VIP without recording a call, as when NetworkManager clears an interface's addresses.
    pub fn remove_externally(&self, ip: Ipv4Addr) {
        self.inner.attached.lock().unwrap().remove(&ip);
    }

    fn record(&self, call: String) {
        self.inner.calls.lock().unwrap().push(call);
    }
}

impl VipBackend for FakeBackend {
    async fn interface_exists(&self, iface: &str) -> anyhow::Result<bool> {
        Ok(!self.is_missing(iface))
    }

    async fn find(&self, vip: &Vip) -> anyhow::Result<Option<String>> {
        if self.is_missing(&vip.interface) {
            anyhow::bail!("simulated missing interface {}", vip.interface);
        }
        Ok(self.is_attached(vip.ip).then(|| format!("{}/{}", vip.ip, vip.prefix)))
    }

    async fn attach(&self, vip: &Vip) -> anyhow::Result<()> {
        self.record(format!("attach {}", vip.ip));
        assert!(!self.inner.panic_attach.load(Ordering::SeqCst), "simulated panic in attach");
        let delay = self.inner.attach_delay_ms.load(Ordering::SeqCst);
        if delay > 0 {
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
        if self.inner.fail_attach.load(Ordering::SeqCst) {
            anyhow::bail!("simulated attach failure");
        }
        self.inner.attached.lock().unwrap().insert(vip.ip);
        Ok(())
    }

    async fn detach(&self, vip: &Vip, _found: &str) -> anyhow::Result<()> {
        self.record(format!("detach {}", vip.ip));
        let delay = self.inner.detach_delay_ms.load(Ordering::SeqCst);
        if delay > 0 {
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
        if self.inner.fail_detach.load(Ordering::SeqCst) {
            anyhow::bail!("simulated detach failure");
        }
        self.inner.attached.lock().unwrap().remove(&vip.ip);
        Ok(())
    }

    async fn announce(&self, vip: &Vip) -> anyhow::Result<()> {
        self.record(format!("announce {}", vip.ip));
        Ok(())
    }

    async fn link_up(&self, _iface: &str) -> bool {
        !self.inner.link_down.load(Ordering::SeqCst)
    }
}
