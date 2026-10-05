//! The event loop: ties the election machine to UDP, timers, checks, the VIP worker and hooks
//! (spec §11).

mod hooks;
mod limiter;
mod vip_worker;

use std::future::Future;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};

use crate::checks::{self, CheckState};
use crate::config::{Config, HookCommands};
use crate::election::{Action, Event, Health, HookKind, Machine, MachineConfig};
use crate::proto::{self, Codec, Heartbeat, ReplayGuard, PACKET_LEN};
use crate::vip::{VipBackend, VipManager};
use limiter::WarnLimiter;
use vip_worker::{VipRequest, WorkerEvent};

const WORKER_FLUSH_TIMEOUT: Duration = Duration::from_secs(15);
const STOP_HOOK_TIMEOUT: Duration = Duration::from_secs(5);
const WARN_WINDOW: Duration = Duration::from_secs(60);

/// Runs one node until `shutdown` completes. Returns an error if start-up fails, if the VIP worker
/// dies, or if a VIP may still be attached when it stops.
pub async fn run<B: VipBackend>(
    cfg: Config,
    backend: B,
    shutdown: impl Future<Output = ()> + Send,
) -> anyhow::Result<()> {
    tokio::pin!(shutdown);
    let socket = UdpSocket::bind(cfg.bind).await.with_context(|| format!("cannot bind UDP {}", cfg.bind))?;
    let manager = Arc::new(VipManager::new(backend, cfg.vip_commands.clone()));

    // A stop cuts the interface check short; leftover VIPs are still removed below. Polling
    // `shutdown` here also installs its signal handlers before the slower steps.
    let mut stopping = tokio::select! {
        biased;
        () = &mut shutdown => true,
        checked = check_interfaces(&cfg, &manager) => {
            checked?;
            false
        }
    };
    // Leftover VIPs are always removed, even if a stop arrives meanwhile: the peer may already hold
    // them.
    {
        let cleanup = remove_leftover_vips(&cfg, &manager);
        tokio::pin!(cleanup);
        loop {
            tokio::select! {
                biased;
                () = &mut shutdown, if !stopping => stopping = true,
                cleaned = &mut cleanup => break cleaned?,
            }
        }
    }
    if stopping {
        return Ok(());
    }
    // A stop during the first round of checks just stops: nothing is held yet.
    let mut check_states = tokio::select! {
        biased;
        () = &mut shutdown => return Ok(()),
        states = first_check_round(&cfg) => states?,
    };
    let mut health = current_health(&cfg, &check_states);

    let (worker_events_tx, mut worker_events) = mpsc::unbounded_channel();
    let (vip_tx, _worker) = vip_worker::spawn(manager, cfg.vips.clone(), worker_events_tx);

    let (check_tx, mut check_results) = mpsc::channel(64);
    let check_loops = CheckLoops(
        cfg.checks
            .iter()
            .cloned()
            .enumerate()
            .map(|(index, spec)| checks::spawn_check_loop(index, spec, check_tx.clone()))
            .collect(),
    );
    drop(check_tx);

    let mut node = Node::new(&cfg, socket, vip_tx);
    let mut machine = Machine::new(MachineConfig::new(cfg.preempt, cfg.advert_interval(), *cfg.bind.ip()));
    tracing::info!(
        node = %cfg.node_name,
        bind = %cfg.bind,
        peers = cfg.peers.len(),
        priority = health.effective_priority,
        fault = health.fault,
        "starting election"
    );
    let actions = machine.handle(Event::Started { health }, Instant::now());
    node.execute(&machine, actions).await;

    let mut buf = [0u8; 2 * PACKET_LEN];
    loop {
        let deadline = machine.next_deadline();
        let event = tokio::select! {
            () = &mut shutdown => break,
            received = node.socket.recv_from(&mut buf) => match received {
                Ok((len, from)) => node.heartbeat_event(&buf[..len], from),
                Err(err) => {
                    node.receive_failed(&err);
                    None
                }
            },
            Some(result) = check_results.recv() => {
                if check_states[result.index].record(result.passed) {
                    tracing::info!(
                        check = %cfg.checks[result.index].name,
                        status = ?check_states[result.index].status(),
                        "check changed"
                    );
                }
                let new_health = current_health(&cfg, &check_states);
                if new_health == health {
                    None
                } else {
                    tracing::info!(
                        priority = new_health.effective_priority,
                        fault = new_health.fault,
                        "health changed"
                    );
                    health = new_health;
                    Some(Event::HealthChanged(health))
                }
            },
            worker_event = worker_events.recv() => match worker_event {
                Some(WorkerEvent::AttachFailed) => Some(Event::AttachFailed),
                // The worker only stops this early if it panicked. Without it no VIP can move, so
                // exit and let the service manager restart vipd; start-up removes any leftover VIP.
                None => anyhow::bail!("the VIP worker stopped unexpectedly"),
            },
            () = sleep_until(deadline) => Some(Event::TimerFired),
        };
        if let Some(event) = event {
            let before = machine.state();
            let actions = machine.handle(event, Instant::now());
            if machine.state() != before {
                tracing::info!(
                    from = ?before,
                    to = ?machine.state(),
                    priority = machine.health().effective_priority,
                    cause = ?event,
                    "state changed"
                );
                if event == Event::AttachFailed {
                    tracing::warn!(
                        "the VIPs could not be attached: this node will not preempt until it next becomes \
                         master on its own or restarts"
                    );
                }
            }
            node.execute(&machine, actions).await;
        }
    }

    tracing::info!("shutting down");
    drop(check_loops);
    let actions = machine.handle(Event::Shutdown, Instant::now());
    node.execute(&machine, actions).await;
    let flushed = flush_worker(&node.vip_tx).await;
    if let Some(stop_hook) = node.stop_hook.take() {
        if tokio::time::timeout(STOP_HOOK_TIMEOUT, stop_hook).await.is_err() {
            tracing::warn!("on_stop is still running after 5 s; it is stopped as vipd exits");
        }
    }
    // Returning lets the runtime drop every task that is still running, and dropping a command's
    // future kills its whole process group (hooks included). std::process::exit would skip that.
    flushed
}

/// Every VIP's interface must exist before the election starts (spec §10).
async fn check_interfaces<B: VipBackend>(cfg: &Config, manager: &VipManager<B>) -> anyhow::Result<()> {
    for vip in &cfg.vips {
        if !manager.interface_exists(&vip.interface).await? {
            anyhow::bail!("network interface {:?} (for VIP {}) does not exist", vip.interface, vip.ip);
        }
    }
    Ok(())
}

/// Removes VIPs left over from a crash, so a node never starts out holding one (spec §11.1).
async fn remove_leftover_vips<B: VipBackend>(cfg: &Config, manager: &VipManager<B>) -> anyhow::Result<()> {
    for vip in &cfg.vips {
        manager.ensure_detached(vip).await.with_context(|| format!("cannot remove leftover VIP {}", vip.ip))?;
    }
    Ok(())
}

/// The first round of checks, all at once, before the election starts (spec §8).
async fn first_check_round(cfg: &Config) -> anyhow::Result<Vec<CheckState>> {
    let mut check_states: Vec<CheckState> = cfg.checks.iter().map(|c| CheckState::new(c.fall, c.rise)).collect();
    let mut first_round = JoinSet::new();
    for (index, spec) in cfg.checks.iter().cloned().enumerate() {
        first_round.spawn(async move { (index, checks::run_once(&spec).await) });
    }
    while let Some(joined) = first_round.join_next().await {
        let (index, passed) = joined.context("a health check task panicked")?;
        check_states[index].record(passed);
    }
    Ok(check_states)
}

/// Waits up to 15 s for the VIP worker to finish every request, retries of a failed detach
/// included. An error means a VIP may still be attached.
async fn flush_worker(vip_tx: &mpsc::UnboundedSender<VipRequest>) -> anyhow::Result<()> {
    let (done, flushed) = oneshot::channel();
    anyhow::ensure!(
        vip_tx.send(VipRequest::Flush(done)).is_ok(),
        "the VIP worker had stopped, so a VIP may still be attached"
    );
    match tokio::time::timeout(WORKER_FLUSH_TIMEOUT, flushed).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) => anyhow::bail!("the VIP worker stopped before the VIPs were removed"),
        Err(_) => anyhow::bail!("the VIPs were still not removed after 15 s"),
    }
}

/// The health-check loops. Dropping this aborts them, so no way out of `run` leaves them running.
struct CheckLoops(Vec<JoinHandle<()>>);

impl Drop for CheckLoops {
    fn drop(&mut self) {
        for handle in &self.0 {
            handle.abort();
        }
    }
}

/// 16 random bits, from the random keys std seeds for `HashMap`.
fn random_u16() -> u16 {
    use std::hash::{BuildHasher, Hasher};
    std::collections::hash_map::RandomState::new().build_hasher().finish() as u16
}

fn current_health(cfg: &Config, states: &[CheckState]) -> Health {
    let weighted: Vec<_> = cfg.checks.iter().zip(states).map(|(spec, state)| (spec.weight, state.status())).collect();
    checks::aggregate(cfg.priority, &weighted)
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await,
        None => std::future::pending().await,
    }
}

/// Everything the loop needs to talk to the network, the VIP worker and the hooks.
struct Node {
    socket: UdpSocket,
    codec: Codec,
    group_id: u16,
    interval_ms: u16,
    fingerprint: u32,
    boot_id: u64,
    seq: u64,
    peers: Vec<SocketAddrV4>,
    replay: ReplayGuard,
    warnings: WarnLimiter,
    vip_tx: mpsc::UnboundedSender<VipRequest>,
    hooks: HookCommands,
    stop_hook: Option<JoinHandle<()>>,
}

impl Node {
    fn new(cfg: &Config, socket: UdpSocket, vip_tx: mpsc::UnboundedSender<VipRequest>) -> Self {
        let vips: Vec<(Ipv4Addr, u8)> = cfg.vips.iter().map(|v| (v.ip, v.prefix)).collect();
        // The start time keeps boot ids increasing across restarts, and the random low bits keep two
        // starts in the same millisecond (a host without a real-time clock) apart.
        let ms = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
        let boot_id = (ms << 16) | u64::from(random_u16());
        Self {
            socket,
            codec: Codec::new(cfg.auth_key.as_bytes()),
            group_id: cfg.group_id,
            interval_ms: cfg.advert_interval_ms,
            fingerprint: proto::vip_fingerprint(&vips),
            boot_id,
            seq: 0,
            peers: cfg.peers.clone(),
            replay: ReplayGuard::new(),
            warnings: WarnLimiter::new(WARN_WINDOW),
            vip_tx,
            hooks: cfg.hooks.clone(),
            stop_hook: None,
        }
    }

    /// Validates a received packet (spec §6.3) and turns it into an election event.
    fn heartbeat_event(&mut self, data: &[u8], from: SocketAddr) -> Option<Event> {
        let SocketAddr::V4(from) = from else { return None };
        let peer = *from.ip();
        let now = Instant::now();
        if !self.peers.iter().any(|p| *p.ip() == peer) {
            // Strangers share one limiter slot, so spoofed source addresses can neither grow the
            // limiter nor flood the log.
            if self.warnings.allow(Ipv4Addr::UNSPECIFIED, "stranger", now) {
                tracing::warn!(%peer, "dropping a packet from an address that is not in peers");
            }
            return None;
        }
        let hb = match self.codec.decode(data) {
            Ok(hb) => hb,
            Err(err) => {
                if self.warnings.allow(peer, "invalid packet", now) {
                    tracing::warn!(%peer, error = %err, "dropping an invalid packet");
                }
                return None;
            }
        };
        if hb.group_id != self.group_id {
            self.warn(peer, "dropping a heartbeat for a different group_id", now);
            return None;
        }
        let interval = Duration::from_millis(u64::from(hb.interval_ms));
        if !self.replay.accept(peer, hb.boot_id, hb.seq, interval, now) {
            self.warn(peer, "dropping a replayed or out-of-order heartbeat", now);
            return None;
        }
        if hb.interval_ms != self.interval_ms {
            self.warn(peer, "peer uses a different advert_interval_ms", now);
        }
        if hb.vip_fingerprint != self.fingerprint {
            self.warn(peer, "peer has a different VIP list", now);
        }
        Some(Event::Heartbeat { from: peer, priority: hb.priority, interval })
    }

    fn warn(&mut self, peer: Ipv4Addr, reason: &'static str, now: Instant) {
        if self.warnings.allow(peer, reason, now) {
            tracing::warn!(%peer, "{reason}");
        }
    }

    /// Windows reports an earlier heartbeat's ICMP "port unreachable" (a stopped peer) as a reset
    /// on the next receive, so that one is expected. Anything else is warned about once a minute.
    fn receive_failed(&mut self, err: &std::io::Error) {
        if err.kind() == std::io::ErrorKind::ConnectionReset {
            tracing::debug!(error = %err, "a peer's heartbeat port was unreachable");
        } else if self.warnings.allow(Ipv4Addr::UNSPECIFIED, "receive failed", Instant::now()) {
            tracing::warn!(error = %err, "receiving a heartbeat failed");
        }
    }

    async fn execute(&mut self, machine: &Machine, actions: Vec<Action>) {
        let effective = machine.health().effective_priority;
        for action in actions {
            match action {
                Action::SendHeartbeat { priority } => self.send_heartbeat(priority).await,
                Action::AttachVips => self.request(VipRequest::Attach),
                Action::DetachVips => self.request(VipRequest::Detach),
                Action::Announce => self.request(VipRequest::Announce),
                Action::RunHook(kind) => {
                    let handle = hooks::spawn(&self.hooks, kind, effective, self.group_id);
                    if kind == HookKind::Stop {
                        self.stop_hook = handle;
                    }
                }
            }
        }
    }

    fn request(&self, request: VipRequest) {
        if self.vip_tx.send(request).is_err() {
            tracing::error!("the VIP worker has stopped");
        }
    }

    async fn send_heartbeat(&mut self, priority: u8) {
        self.seq += 1;
        let packet = self.codec.encode(&Heartbeat {
            group_id: self.group_id,
            priority,
            interval_ms: self.interval_ms,
            vip_fingerprint: self.fingerprint,
            boot_id: self.boot_id,
            seq: self.seq,
        });
        let now = Instant::now();
        for peer in &self.peers {
            if let Err(err) = self.socket.send_to(&packet, *peer).await {
                if self.warnings.allow(*peer.ip(), "send failed", now) {
                    tracing::warn!(%peer, error = %err, "sending a heartbeat failed");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_node() -> Node {
        let cfg = Config::from_toml(
            r#"
node_name = "a"
group_id = 1
priority = 100
auth_key = "0123456789abcdef"
bind = "127.0.0.1:8458"
peers = ["127.0.0.2:8458"]

[[vip]]
ip = "10.99.0.1"
interface = "fake0"
"#,
        )
        .unwrap();
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let (vip_tx, _vip_rx) = mpsc::unbounded_channel();
        Node::new(&cfg, socket, vip_tx)
    }

    #[tokio::test]
    async fn strangers_share_one_warning_slot() {
        let mut node = test_node().await;
        for i in 1..=100u8 {
            let from = SocketAddr::from((Ipv4Addr::new(10, 0, 0, i), 8458));
            assert!(node.heartbeat_event(&[0; PACKET_LEN], from).is_none());
        }
        assert_eq!(node.warnings.len(), 1);
    }

    #[tokio::test]
    async fn receive_errors_are_rate_limited_and_resets_are_expected() {
        let mut node = test_node().await;
        for _ in 0..100 {
            node.receive_failed(&std::io::ErrorKind::ConnectionReset.into());
        }
        assert_eq!(node.warnings.len(), 0, "a reset follows a heartbeat sent to a stopped peer");
        for _ in 0..100 {
            node.receive_failed(&std::io::Error::other("boom"));
        }
        assert_eq!(node.warnings.len(), 1);
    }
}
