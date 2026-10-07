//! One task owns the VIP backend, so the event loop never waits for an OS command (spec §11.3). It
//! applies attach / detach / announce requests in order, but skips any that a later attach or detach
//! supersedes. While the VIPs are wanted, it also checks now and then that they are still attached,
//! and adds back and announces any that something else removed.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::vip::{Vip, VipBackend, VipManager};

const DETACH_RETRY: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub enum VipRequest {
    /// Attach every VIP. The number comes back in `AttachFailed`, so the runtime can tell a failure
    /// of an attach it has since superseded.
    Attach(u64),
    Detach,
    Announce,
    /// Never skipped. Answered once every earlier request has been applied or skipped and no failed
    /// detach is still being retried, so a shutdown that waits for it also waits for the retries.
    Flush(oneshot::Sender<()>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerEvent {
    /// The attach with this number failed, or a later check could not add back a VIP it attached.
    AttachFailed(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Desired {
    Attached,
    Detached,
}

/// What the worker does next.
enum Work {
    Request(VipRequest),
    /// Try the failed detach again.
    RetryDetach,
    /// Check that the wanted VIPs are still attached.
    Verify,
}

/// Starts the worker. Requests are applied in order, except that an attach, detach or announce is
/// skipped when a later attach or detach is already queued; a failed detach is retried every 2 s
/// until it succeeds or an attach replaces it. From a successful attach until a detach or a failed
/// attach, the VIPs are checked every `verify_interval`: any that something else removed is added
/// back and announced, and if that fails the worker reports `AttachFailed` with the number of the
/// attach that made the VIPs wanted, as for a failed attach. The worker stops when every request
/// sender is dropped, and `events` closes when it stops, so a caller can treat that as the worker
/// dying.
pub fn spawn<B: VipBackend>(
    manager: Arc<VipManager<B>>,
    vips: Vec<Vip>,
    verify_interval: Duration,
    events: mpsc::UnboundedSender<WorkerEvent>,
) -> (mpsc::UnboundedSender<VipRequest>, JoinHandle<()>) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let handle = tokio::spawn(async move {
        let mut desired = Desired::Detached;
        // The number of the attach that set `desired` to attached. A failed verification reports it.
        let mut current_attach = 0;
        // When the failed detach is tried again. New requests do not move it.
        let mut retry_at: Option<Instant> = None;
        // When the wanted VIPs are next checked.
        let mut verify_at: Option<Instant> = None;
        let mut flushes = Vec::new();
        // Requests already taken off the channel, oldest first.
        let mut queue = VecDeque::new();
        loop {
            let work = match queue.pop_front() {
                Some(request) => Work::Request(request),
                None => tokio::select! {
                    request = rx.recv() => match request {
                        Some(request) => Work::Request(request),
                        None => break,
                    },
                    () = sleep_until(retry_at) => Work::RetryDetach,
                    () = sleep_until(verify_at) => Work::Verify,
                },
            };
            // A later attach or detach supersedes this one, and makes an announce before it moot.
            // Skipping them keeps a slow backend from replaying old terms after the election moved on.
            if matches!(work, Work::Request(VipRequest::Attach(_) | VipRequest::Detach | VipRequest::Announce))
                && attach_or_detach_waiting(&mut rx, &mut queue)
            {
                continue;
            }
            match work {
                Work::RetryDetach => retry_at = retry_after(detach_all(&manager, &vips).await),
                Work::Verify => {
                    // A waiting attach or detach decides whether the VIPs are wanted, so it goes first.
                    // Otherwise a verification could put back a VIP that the detach is about to remove.
                    if attach_or_detach_waiting(&mut rx, &mut queue) {
                        verify_at = Some(Instant::now() + verify_interval);
                        continue;
                    }
                    let verified = verify_all(&manager, &vips).await;
                    verify_at = Some(Instant::now() + verify_interval);
                    if !verified {
                        // As for a failed attach: the node faults and the peer takes over.
                        let _ = events.send(WorkerEvent::AttachFailed(current_attach));
                    }
                }
                Work::Request(VipRequest::Attach(id)) => {
                    desired = Desired::Attached;
                    current_attach = id;
                    retry_at = None;
                    if attach_all(&manager, &vips).await {
                        verify_at = Some(Instant::now() + verify_interval);
                    } else {
                        verify_at = None;
                        let _ = events.send(WorkerEvent::AttachFailed(id));
                    }
                }
                Work::Request(VipRequest::Detach) => {
                    desired = Desired::Detached;
                    verify_at = None;
                    retry_at = retry_after(detach_all(&manager, &vips).await);
                }
                Work::Request(VipRequest::Announce) => {
                    if desired == Desired::Attached {
                        announce_all(&manager, &vips).await;
                    }
                }
                Work::Request(VipRequest::Flush(done)) => flushes.push(done),
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

/// Moves every waiting request into `queue`, and says whether an attach or detach is among them.
fn attach_or_detach_waiting(rx: &mut mpsc::UnboundedReceiver<VipRequest>, queue: &mut VecDeque<VipRequest>) -> bool {
    while let Ok(more) = rx.try_recv() {
        queue.push_back(more);
    }
    queue.iter().any(|r| matches!(r, VipRequest::Attach(_) | VipRequest::Detach))
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
                tracing::error!(
                    vip = %vip.ip,
                    interface = %vip.interface,
                    error = %format!("{err:#}"),
                    "attaching the VIP failed"
                );
                return false;
            }
        }
    }
    announce_all(manager, vips).await;
    true
}

/// Adds back any VIP that something else removed, such as NetworkManager clearing an interface,
/// and then announces them all. Returns false as soon as a VIP cannot be checked or added back.
async fn verify_all<B: VipBackend>(manager: &VipManager<B>, vips: &[Vip]) -> bool {
    let mut added_back = false;
    for vip in vips {
        match manager.ensure_attached(vip).await {
            Ok(true) => {
                tracing::warn!(
                    vip = %vip.ip,
                    interface = %vip.interface,
                    "the VIP was removed outside vipd; added it back"
                );
                added_back = true;
            }
            Ok(false) => {}
            Err(err) => {
                tracing::error!(
                    vip = %vip.ip,
                    interface = %vip.interface,
                    error = %format!("{err:#}"),
                    "verifying the VIP failed"
                );
                return false;
            }
        }
    }
    if added_back {
        announce_all(manager, vips).await;
    }
    true
}

async fn detach_all<B: VipBackend>(manager: &VipManager<B>, vips: &[Vip]) -> bool {
    let mut ok = true;
    for vip in vips {
        match manager.ensure_detached(vip).await {
            Ok(true) => tracing::info!(vip = %vip.ip, interface = %vip.interface, "VIP detached"),
            Ok(false) => {}
            Err(err) => {
                tracing::error!(
                    vip = %vip.ip,
                    interface = %vip.interface,
                    error = %format!("{err:#}"),
                    "detaching the VIP failed; retrying in 2 s"
                );
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
    /// The default: 5 advert intervals of 1 s.
    const VERIFY: Duration = Duration::from_secs(5);

    fn setup() -> (FakeBackend, mpsc::UnboundedSender<VipRequest>, mpsc::UnboundedReceiver<WorkerEvent>) {
        let fake = FakeBackend::new();
        let manager = Arc::new(VipManager::new(fake.clone(), CommandOverrides::default()));
        let vips = vec![Vip { ip: VIP_IP, prefix: 24, interface: "eth0".into() }];
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let (tx, _handle) = spawn(manager, vips, VERIFY, events_tx);
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
        // The last announce is ignored: the VIP is no longer wanted.
        for request in [VipRequest::Attach(1), VipRequest::Announce, VipRequest::Detach, VipRequest::Announce] {
            tx.send(request).unwrap();
            flush(&tx).await;
        }
        assert_eq!(
            fake.calls(),
            vec!["attach 10.0.0.200", "announce 10.0.0.200", "announce 10.0.0.200", "detach 10.0.0.200"]
        );
        assert!(!fake.is_attached(VIP_IP));
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_detach_is_retried_and_holds_back_flush() {
        let (fake, tx, _events) = setup();
        tx.send(VipRequest::Attach(1)).unwrap();
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
        tx.send(VipRequest::Attach(1)).unwrap();
        flush(&tx).await;
        fake.set_fail_detach(true);
        tx.send(VipRequest::Detach).unwrap();
        // Let the detach fail first: queued together with it, the attach would supersede it.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let detaches = fake.calls().iter().filter(|c| c.starts_with("detach")).count();
        assert_eq!(detaches, 1, "the detach failed, so a retry is pending");
        tx.send(VipRequest::Attach(2)).unwrap();
        let flushed = tokio::time::timeout(Duration::from_secs(1), flush(&tx)).await;
        assert!(flushed.is_ok(), "flush is answered at once: nothing is pending any more");
        fake.set_fail_detach(false);
        tokio::time::sleep(Duration::from_secs(10)).await;
        assert!(fake.is_attached(VIP_IP), "no retry ran after the attach");
    }

    #[tokio::test]
    async fn a_later_attach_or_detach_supersedes_queued_ones() {
        let (fake, tx, _events) = setup();
        // All queued before the worker runs: only the last detach still matters, and the VIP was
        // never attached, so nothing needs doing.
        for request in [VipRequest::Attach(1), VipRequest::Announce, VipRequest::Detach, VipRequest::Attach(2)] {
            tx.send(request).unwrap();
        }
        tx.send(VipRequest::Detach).unwrap();
        flush(&tx).await;
        assert!(fake.calls().is_empty(), "{:?}", fake.calls());
    }

    #[tokio::test]
    async fn reports_attach_failures() {
        let (fake, tx, mut events) = setup();
        fake.set_fail_attach(true);
        tx.send(VipRequest::Attach(1)).unwrap();
        assert_eq!(events.recv().await, Some(WorkerEvent::AttachFailed(1)));
    }

    #[tokio::test(start_paused = true)]
    async fn a_vip_removed_outside_vipd_is_added_back_and_announced() {
        let (fake, tx, _events) = setup();
        tx.send(VipRequest::Attach(1)).unwrap();
        flush(&tx).await;
        fake.remove_externally(VIP_IP);
        let before = fake.calls().len();
        tokio::time::sleep(VERIFY + Duration::from_millis(1)).await;
        assert!(fake.is_attached(VIP_IP), "the VIP was not added back within one verify interval");
        assert_eq!(fake.calls()[before..], ["attach 10.0.0.200", "announce 10.0.0.200"]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_re_add_reports_the_current_attach() {
        let (fake, tx, mut events) = setup();
        tx.send(VipRequest::Attach(7)).unwrap();
        flush(&tx).await;
        fake.remove_externally(VIP_IP);
        fake.set_fail_attach(true);
        let event = tokio::time::timeout(VERIFY + Duration::from_millis(1), events.recv()).await;
        assert_eq!(event.expect("no event within one verify interval"), Some(WorkerEvent::AttachFailed(7)));
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_is_verified_while_the_vips_are_not_wanted() {
        let (fake, tx, _events) = setup();
        tx.send(VipRequest::Attach(1)).unwrap();
        flush(&tx).await;
        tx.send(VipRequest::Detach).unwrap();
        flush(&tx).await;
        let calls = fake.calls();
        tokio::time::sleep(VERIFY * 3).await;
        assert_eq!(fake.calls(), calls, "the detached VIP was verified");
    }

    #[tokio::test(start_paused = true)]
    async fn a_verification_yields_to_a_queued_detach() {
        for _ in 0..20 {
            let (fake, tx, _events) = setup();
            tx.send(VipRequest::Attach(1)).unwrap();
            flush(&tx).await;
            fake.remove_externally(VIP_IP);
            let before = fake.calls().len();
            // The verification falls due and wakes the worker, and a detach arrives before it runs.
            // Sent first, the detach would be taken before the timer fires, and nothing would race.
            tokio::time::advance(VERIFY + Duration::from_millis(1)).await;
            tx.send(VipRequest::Detach).unwrap();
            flush(&tx).await;
            let after = &fake.calls()[before..];
            assert!(!after.iter().any(|call| call.starts_with("attach")), "the VIP was added back: {after:?}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_is_verified_after_a_failed_attach() {
        let (fake, tx, mut events) = setup();
        tx.send(VipRequest::Attach(1)).unwrap();
        flush(&tx).await;
        // The next attach has to add the VIP again, and fails.
        fake.remove_externally(VIP_IP);
        fake.set_fail_attach(true);
        tx.send(VipRequest::Attach(2)).unwrap();
        assert_eq!(events.recv().await, Some(WorkerEvent::AttachFailed(2)));
        tokio::time::sleep(VERIFY * 3).await;
        assert!(events.try_recv().is_err(), "a verification retried the failed attach");
    }
}
