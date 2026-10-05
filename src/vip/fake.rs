//! An in-memory backend for tests: remembers what is attached, records calls, can fail on demand.

use std::collections::BTreeSet;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

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

    fn record(&self, call: String) {
        self.inner.calls.lock().unwrap().push(call);
    }
}

impl VipBackend for FakeBackend {
    async fn interface_exists(&self, _iface: &str) -> anyhow::Result<bool> {
        Ok(true)
    }

    async fn find(&self, vip: &Vip) -> anyhow::Result<Option<String>> {
        Ok(self.is_attached(vip.ip).then(|| format!("{}/{}", vip.ip, vip.prefix)))
    }

    async fn attach(&self, vip: &Vip) -> anyhow::Result<()> {
        self.record(format!("attach {}", vip.ip));
        if self.inner.fail_attach.load(Ordering::SeqCst) {
            anyhow::bail!("simulated attach failure");
        }
        self.inner.attached.lock().unwrap().insert(vip.ip);
        Ok(())
    }

    async fn detach(&self, vip: &Vip, _found: &str) -> anyhow::Result<()> {
        self.record(format!("detach {}", vip.ip));
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
}
