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
use tokio::time::MissedTickBehavior;

use crate::checks::{self, CheckState, CheckStatus};
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
    // A port that is already in use most likely means that another vipd runs on this node, and the
    // cleanup below would remove the VIPs it holds. Any other bind failure, such as a `bind` IP that
    // is gone, is reported after the cleanup.
    let bound = UdpSocket::bind(cfg.bind).await;
    if let Err(err) = &bound {
        if err.kind() == std::io::ErrorKind::AddrInUse {
            anyhow::bail!("cannot bind UDP {}: {err}; is another vipd running on this node?", cfg.bind);
        }
    }
    let manager = Arc::new(VipManager::new(backend, cfg.vip_commands.clone()));

    // A stop cuts the interface check short; leftover VIPs are still removed below. Polling
    // `shutdown` here also installs its signal handlers before the slower steps.
    let (mut stopping, checked) = tokio::select! {
        biased;
        () = &mut shutdown => (true, Ok(())),
        checked = check_interfaces(&cfg, &manager) => (false, checked),
    };
    // Leftover VIPs are always removed, even if an interface is missing or a stop arrives meanwhile:
    // the peer may already hold them.
    let cleaned = {
        let cleanup = remove_leftover_vips(&cfg, &manager);
        tokio::pin!(cleanup);
        loop {
            tokio::select! {
                biased;
                () = &mut shutdown, if !stopping => stopping = true,
                cleaned = &mut cleanup => break cleaned,
            }
        }
    };
    // A missing interface is reported before a failed removal, which it may well have caused.
    checked.and(cleaned)?;
    if stopping {
        return Ok(());
    }
    let socket = bound.with_context(|| format!("cannot bind UDP {}", cfg.bind))?;
    // A stop during the first round of checks just stops: nothing is held yet.
    let mut check_states = tokio::select! {
        biased;
        () = &mut shutdown => return Ok(()),
        states = first_check_round(&cfg) => states?,
    };
    // The network is tracked like a check with fall = 2 and rise = 1, so one bad sample (a Wi-Fi
    // interface briefly `dormant` while it re-keys) does not cause a failover (spec §8). As with a
    // check, the first result decides on its own: a node whose network is down starts in Fault.
    let mut network = CheckState::new(2, 1);
    let problem = network_problem(&cfg, &manager).await;
    if let Some(reason) = &problem {
        log_network_down(reason);
    }
    network.record(problem.is_none());
    let mut health = current_health(&cfg, &check_states, network.status() == CheckStatus::Ok);

    let (worker_events_tx, mut worker_events) = mpsc::unbounded_channel();
    // While the VIPs are wanted, the worker checks every 5 advert intervals that they are still there.
    let verify_interval = cfg.advert_interval() * 5;
    let (vip_tx, _worker) =
        vip_worker::spawn(Arc::clone(&manager), cfg.vips.clone(), verify_interval, worker_events_tx);

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

    // Start-up has just checked the network, so the next check is one advert interval away.
    let mut network_checks =
        tokio::time::interval_at(tokio::time::Instant::now() + cfg.advert_interval(), cfg.advert_interval());
    network_checks.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let mut buf = [0u8; 2 * PACKET_LEN];
    loop {
        let deadline = machine.next_deadline();
        // Unbiased on purpose: with `biased;` a flood of packets could starve the branches after
        // `recv_from`, the timer included.
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
                update_health(&mut health, current_health(&cfg, &check_states, network.status() == CheckStatus::Ok))
            },
            // The check runs after the tick, so cancelling the branch loses nothing. It is fast: a
            // file read and a bind per tick.
            _ = network_checks.tick() => {
                let problem = network_problem(&cfg, &manager).await;
                if network.record(problem.is_none()) {
                    match &problem {
                        Some(reason) => log_network_down(reason),
                        None => tracing::info!("the network is back"),
                    }
                    let network_ok = network.status() == CheckStatus::Ok;
                    update_health(&mut health, current_health(&cfg, &check_states, network_ok))
                } else {
                    None
                }
            },
            worker_event = worker_events.recv() => match worker_event {
                // Only the newest attach counts. An older one belongs to a term this node has since
                // left, and the attach queued after it may still succeed.
                Some(WorkerEvent::AttachFailed(id)) => (id == node.last_attach).then_some(Event::AttachFailed),
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
            tracing::warn!(
                "on_stop is still running after {} s; it is stopped as vipd exits",
                STOP_HOOK_TIMEOUT.as_secs()
            );
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

/// Removes VIPs left over from a crash, so a node never starts out holding one (spec §11.1). Tries
/// every VIP, even after one fails, and returns the first error.
async fn remove_leftover_vips<B: VipBackend>(cfg: &Config, manager: &VipManager<B>) -> anyhow::Result<()> {
    let mut result = Ok(());
    for vip in &cfg.vips {
        let removed =
            manager.ensure_detached(vip).await.with_context(|| format!("cannot remove leftover VIP {}", vip.ip));
        if let Ok(true) = removed {
            tracing::warn!(vip = %vip.ip, interface = %vip.interface, "removed a VIP left over from an earlier run");
        }
        result = result.and(removed.map(|_| ())); // keeps the first error
    }
    result
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
        Err(_) => anyhow::bail!("the VIPs were still not removed after {} s", WORKER_FLUSH_TIMEOUT.as_secs()),
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

/// The checks' health, in fault while the network is down. The checks still set the priority.
fn current_health(cfg: &Config, states: &[CheckState], network_ok: bool) -> Health {
    let weighted: Vec<_> = cfg.checks.iter().zip(states).map(|(spec, state)| (spec.weight, state.status())).collect();
    let health = checks::aggregate(cfg.priority, &weighted);
    Health { fault: health.fault || !network_ok, ..health }
}

/// Stores `new` as the node's health. Returns the event that tells the machine, or `None` if the
/// health did not change.
fn update_health(health: &mut Health, new: Health) -> Option<Event> {
    if new == *health {
        return None;
    }
    tracing::info!(priority = new.effective_priority, fault = new.fault, "health changed");
    *health = new;
    Some(Event::HealthChanged(new))
}

/// Why this node's network is unusable, or `None` if it is fine (spec §8).
async fn network_problem<B: VipBackend>(cfg: &Config, manager: &VipManager<B>) -> Option<String> {
    let ip = *cfg.bind.ip();
    if !bind_ip_present(ip) {
        return Some(format!("bind address {ip} is not on this machine"));
    }
    for vip in &cfg.vips {
        if !manager.link_up(&vip.interface).await {
            return Some(format!("interface {} is down", vip.interface));
        }
    }
    None
}

fn log_network_down(reason: &str) {
    tracing::warn!(reason = %reason, "the network is down: this node leaves the election until it is back");
}

/// Whether `ip` is still an address of this machine. NetworkManager clears a Wi-Fi interface's
/// addresses when it loses its network, and Windows does the same for a disconnected adapter. A
/// host with net.ipv4.ip_nonlocal_bind set always passes; the link check still applies there.
fn bind_ip_present(ip: Ipv4Addr) -> bool {
    // Port 0 cannot collide with vipd's own socket.
    std::net::UdpSocket::bind((ip, 0)).is_ok()
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
    /// The number of the newest attach request.
    last_attach: u64,
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
            last_attach: 0,
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
                Action::AttachVips => {
                    self.last_attach += 1;
                    self.request(VipRequest::Attach(self.last_attach));
                }
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

    const TEST_CONFIG: &str = r#"
node_name = "a"
group_id = 1
priority = 100
auth_key = "0123456789abcdef"
bind = "127.0.0.1:8458"
peers = ["127.0.0.2:8458"]

[[vip]]
ip = "10.99.0.1"
interface = "fake0"
"#;

    async fn test_node() -> Node {
        let cfg = Config::from_toml(TEST_CONFIG).unwrap();
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let (vip_tx, _vip_rx) = mpsc::unbounded_channel();
        Node::new(&cfg, socket, vip_tx)
    }

    #[test]
    fn an_address_of_this_machine_is_present() {
        assert!(bind_ip_present(Ipv4Addr::LOCALHOST));
    }

    #[test]
    fn a_lost_network_is_a_fault_and_the_checks_still_set_the_priority() {
        let check = "[[check]]\nname = \"web\"\ncommand = \"true\"\nweight = -60\n";
        let cfg = Config::from_toml(&format!("{TEST_CONFIG}\n{check}")).unwrap();
        let mut failing = CheckState::new(1, 1);
        failing.record(false);
        let states = [failing];
        assert_eq!(current_health(&cfg, &states, true), Health { effective_priority: 40, fault: false });
        assert_eq!(current_health(&cfg, &states, false), Health { effective_priority: 40, fault: true });
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

    #[tokio::test]
    async fn a_heartbeat_for_another_group_is_dropped_before_the_replay_check() {
        let mut node = test_node().await;
        let codec = Codec::new(b"0123456789abcdef"); // test_node's auth_key, so the HMAC passes
        let peer = Ipv4Addr::new(127, 0, 0, 2);
        let from = SocketAddr::from((peer, 8458));
        let packet = |group_id: u16, seq: u64| {
            codec.encode(&Heartbeat { group_id, priority: 200, interval_ms: 1000, vip_fingerprint: 0, boot_id: 1, seq })
        };
        // test_node is group 1, so group 2 is dropped...
        assert_eq!(node.heartbeat_event(&packet(2, 5), from), None);
        // ...without reaching the replay guard: seq 1 of the same run is still accepted.
        let accepted = Event::Heartbeat { from: peer, priority: 200, interval: Duration::from_secs(1) };
        assert_eq!(node.heartbeat_event(&packet(1, 1), from), Some(accepted));
    }

    #[tokio::test]
    async fn a_heartbeat_carries_the_peer_interval_and_a_replay_is_dropped() {
        let mut node = test_node().await;
        let codec = Codec::new(b"0123456789abcdef"); // test_node's auth_key, so the HMAC passes
        let peer = Ipv4Addr::new(127, 0, 0, 2);
        let from = SocketAddr::from((peer, 8458));
        // The peer advertises every 2 s; this node's own interval is 1 s.
        let hb = Heartbeat { group_id: 1, priority: 200, interval_ms: 2000, vip_fingerprint: 0, boot_id: 1, seq: 5 };
        let packet = codec.encode(&hb);
        let event = Event::Heartbeat { from: peer, priority: 200, interval: Duration::from_secs(2) };
        assert_eq!(node.heartbeat_event(&packet, from), Some(event));
        assert_eq!(node.heartbeat_event(&packet, from), None, "the same packet again is a replay");
    }
}
