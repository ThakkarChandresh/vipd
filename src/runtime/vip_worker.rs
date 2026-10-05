//! One task owns the VIP backend and applies attach / detach / announce requests in order, so the
//! event loop never waits for an OS command (spec §11.3).

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::vip::{Vip, VipBackend, VipManager};

const DETACH_RETRY: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub enum VipRequest {
    Attach,
    Detach,
    Announce,
    /// Answered once every earlier request has been processed.
    Flush(oneshot::Sender<()>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerEvent {
    AttachFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Desired {
    Attached,
    Detached,
}

pub fn spawn<B: VipBackend>(
    manager: Arc<VipManager<B>>,
    vips: Vec<Vip>,
    events: mpsc::UnboundedSender<WorkerEvent>,
) -> (mpsc::UnboundedSender<VipRequest>, JoinHandle<()>) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let handle = tokio::spawn(async move {
        let mut desired = Desired::Detached;
        let mut detach_pending = false;
        loop {
            let request = if detach_pending {
                match tokio::time::timeout(DETACH_RETRY, rx.recv()).await {
                    Ok(Some(request)) => Some(request),
                    Ok(None) => break,
                    Err(_) => None, // time to retry the failed detach
                }
            } else {
                match rx.recv().await {
                    Some(request) => Some(request),
                    None => break,
                }
            };
            match request {
                None => detach_pending = !detach_all(&manager, &vips).await,
                Some(VipRequest::Attach) => {
                    desired = Desired::Attached;
                    detach_pending = false;
                    if !attach_all(&manager, &vips).await {
                        let _ = events.send(WorkerEvent::AttachFailed);
                    }
                }
                Some(VipRequest::Detach) => {
                    desired = Desired::Detached;
                    detach_pending = !detach_all(&manager, &vips).await;
                }
                Some(VipRequest::Announce) => {
                    if desired == Desired::Attached {
                        announce_all(&manager, &vips).await;
                    }
                }
                Some(VipRequest::Flush(done)) => {
                    let _ = done.send(());
                }
            }
        }
    });
    (tx, handle)
}

async fn attach_all<B: VipBackend>(manager: &VipManager<B>, vips: &[Vip]) -> bool {
    for vip in vips {
        match manager.ensure_attached(vip).await {
            Ok(true) => tracing::info!(vip = %vip.ip, interface = %vip.interface, "VIP attached"),
            Ok(false) => {}
            Err(err) => {
                tracing::error!(vip = %vip.ip, interface = %vip.interface, error = %format!("{err:#}"), "attaching the VIP failed");
                return false;
            }
        }
    }
    announce_all(manager, vips).await;
    true
}

async fn detach_all<B: VipBackend>(manager: &VipManager<B>, vips: &[Vip]) -> bool {
    let mut ok = true;
    for vip in vips {
        match manager.ensure_detached(vip).await {
            Ok(true) => tracing::info!(vip = %vip.ip, interface = %vip.interface, "VIP detached"),
            Ok(false) => {}
            Err(err) => {
                tracing::error!(vip = %vip.ip, interface = %vip.interface, error = %format!("{err:#}"), "detaching the VIP failed; retrying in 2 s");
                ok = false;
            }
        }
    }
    ok
}

async fn announce_all<B: VipBackend>(manager: &VipManager<B>, vips: &[Vip]) {
    for vip in vips {
        if let Err(err) = manager.announce(vip).await {
            tracing::warn!(vip = %vip.ip, error = %format!("{err:#}"), "announcing the VIP failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;
    use crate::vip::fake::FakeBackend;
    use crate::vip::CommandOverrides;

    const VIP_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 200);

    fn setup() -> (FakeBackend, mpsc::UnboundedSender<VipRequest>, mpsc::UnboundedReceiver<WorkerEvent>) {
        let fake = FakeBackend::new();
        let manager = Arc::new(VipManager::new(fake.clone(), CommandOverrides::default()));
        let vips = vec![Vip { ip: VIP_IP, prefix: 24, interface: "eth0".into() }];
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let (tx, _handle) = spawn(manager, vips, events_tx);
        (fake, tx, events_rx)
    }

    async fn flush(tx: &mpsc::UnboundedSender<VipRequest>) {
        let (done, wait) = oneshot::channel();
        tx.send(VipRequest::Flush(done)).unwrap();
        wait.await.unwrap();
    }

    #[tokio::test]
    async fn applies_requests_in_order() {
        let (fake, tx, _events) = setup();
        tx.send(VipRequest::Attach).unwrap();
        tx.send(VipRequest::Announce).unwrap();
        tx.send(VipRequest::Detach).unwrap();
        tx.send(VipRequest::Announce).unwrap(); // ignored: the VIP is no longer wanted
        flush(&tx).await;
        assert_eq!(
            fake.calls(),
            vec!["attach 10.0.0.200", "announce 10.0.0.200", "announce 10.0.0.200", "detach 10.0.0.200"]
        );
        assert!(!fake.is_attached(VIP_IP));
    }

    #[tokio::test]
    async fn reports_attach_failures() {
        let (fake, tx, mut events) = setup();
        fake.set_fail_attach(true);
        tx.send(VipRequest::Attach).unwrap();
        assert_eq!(events.recv().await, Some(WorkerEvent::AttachFailed));
    }
}
