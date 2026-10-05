//! One task owns the VIP backend and applies attach / detach / announce requests in order, so the
//! event loop never waits for an OS command (spec §11.3).

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::vip::{Vip, VipBackend, VipManager};

const DETACH_RETRY: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub enum VipRequest {
    Attach,
    Detach,
    Announce,
    /// Answered once every earlier request has been processed and no failed detach is still being
    /// retried, so a shutdown that waits for it also waits for the retries.
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

/// Starts the worker. Requests are applied strictly in order; a failed detach is retried every
/// 2 s until it succeeds or an attach replaces it. The worker stops when every request sender is
/// dropped, and `events` closes when it stops, so a caller can treat that as the worker dying.
pub fn spawn<B: VipBackend>(
    manager: Arc<VipManager<B>>,
    vips: Vec<Vip>,
    events: mpsc::UnboundedSender<WorkerEvent>,
) -> (mpsc::UnboundedSender<VipRequest>, JoinHandle<()>) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let handle = tokio::spawn(async move {
        let mut desired = Desired::Detached;
        // When the failed detach is tried again. New requests do not move it.
        let mut retry_at: Option<Instant> = None;
        let mut flushes = Vec::new();
        loop {
            let request = tokio::select! {
                request = rx.recv() => match request {
                    Some(request) => Some(request),
                    None => break,
                },
                () = sleep_until(retry_at) => None,
            };
            match request {
                None => retry_at = retry_after(detach_all(&manager, &vips).await),
                Some(VipRequest::Attach) => {
                    desired = Desired::Attached;
                    retry_at = None;
                    if !attach_all(&manager, &vips).await {
                        let _ = events.send(WorkerEvent::AttachFailed);
                    }
                }
                Some(VipRequest::Detach) => {
                    desired = Desired::Detached;
                    retry_at = retry_after(detach_all(&manager, &vips).await);
                }
                Some(VipRequest::Announce) => {
                    if desired == Desired::Attached {
                        announce_all(&manager, &vips).await;
                    }
                }
                Some(VipRequest::Flush(done)) => flushes.push(done),
            }
            if retry_at.is_none() {
                for done in flushes.drain(..) {
                    let _ = done.send(());
                }
            }
        }
    });
    (tx, handle)
}

/// `None` if the detach succeeded, otherwise when to try it again.
fn retry_after(detached: bool) -> Option<Instant> {
    (!detached).then(|| Instant::now() + DETACH_RETRY)
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
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

    #[tokio::test(start_paused = true)]
    async fn a_failed_detach_is_retried_and_holds_back_flush() {
        let (fake, tx, _events) = setup();
        tx.send(VipRequest::Attach).unwrap();
        flush(&tx).await;
        fake.set_fail_detach(true);
        tx.send(VipRequest::Detach).unwrap();
        let (done, mut flushed) = oneshot::channel();
        tx.send(VipRequest::Flush(done)).unwrap();
        tx.send(VipRequest::Announce).unwrap(); // does not postpone the retry
        tokio::time::sleep(Duration::from_millis(4500)).await;
        assert!(flushed.try_recv().is_err(), "flush waits while the detach is retried");
        let detaches = fake.calls().iter().filter(|c| c.starts_with("detach")).count();
        assert_eq!(detaches, 3, "the first attempt plus retries at 2 s and 4 s");
        fake.set_fail_detach(false);
        flushed.await.unwrap();
        assert!(!fake.is_attached(VIP_IP));
    }

    #[tokio::test(start_paused = true)]
    async fn an_attach_cancels_a_pending_detach_retry() {
        let (fake, tx, _events) = setup();
        tx.send(VipRequest::Attach).unwrap();
        flush(&tx).await;
        fake.set_fail_detach(true);
        tx.send(VipRequest::Detach).unwrap();
        tx.send(VipRequest::Attach).unwrap();
        flush(&tx).await; // answered at once: nothing is pending any more
        fake.set_fail_detach(false);
        tokio::time::sleep(Duration::from_secs(10)).await;
        assert!(fake.is_attached(VIP_IP), "no retry ran after the attach");
    }

    #[tokio::test]
    async fn reports_attach_failures() {
        let (fake, tx, mut events) = setup();
        fake.set_fail_attach(true);
        tx.send(VipRequest::Attach).unwrap();
        assert_eq!(events.recv().await, Some(WorkerEvent::AttachFailed));
    }
}
