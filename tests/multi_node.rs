//! Several in-process nodes on 127.0.0.x with fake VIP backends and 50 ms heartbeats
//! (spec §16 item 5). Linux routes all of 127.0.0.0/8 to the loopback interface.

use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket as StdUdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use vipd::config::Config;
use vipd::proto::{Codec, Heartbeat};
use vipd::runtime;
use vipd::vip::fake::FakeBackend;
use vipd::vip::VipBackend;

const VIP: Ipv4Addr = Ipv4Addr::new(10, 99, 0, 1);
const KEY: &str = "integration-test-key";

/// One free UDP address per IP. All sockets are held until every port is chosen, so the ports are
/// distinct. They stay free only until this returns, so bind any other socket on these IPs first.
fn free_addrs(ips: &[Ipv4Addr]) -> Vec<SocketAddrV4> {
    let sockets: Vec<StdUdpSocket> = ips.iter().map(|ip| StdUdpSocket::bind((*ip, 0)).unwrap()).collect();
    sockets
        .iter()
        .map(|s| match s.local_addr().unwrap() {
            std::net::SocketAddr::V4(addr) => addr,
            other => panic!("unexpected address {other}"),
        })
        .collect()
}

fn config(bind: SocketAddrV4, peers: &[SocketAddrV4], priority: u8) -> Config {
    config_with(bind, peers, priority, "")
}

/// `extra` is appended after the `[[vip]]` table, e.g. a `[[check]]` or `[hooks]` table.
fn config_with(bind: SocketAddrV4, peers: &[SocketAddrV4], priority: u8, extra: &str) -> Config {
    let peers: Vec<String> = peers.iter().map(|p| format!("\"{p}\"")).collect();
    let text = format!(
        r#"
node_name = "node-{priority}"
group_id = 7
priority = {priority}
advert_interval_ms = 50
auth_key = "{KEY}"
bind = "{bind}"
peers = [{peers}]

[[vip]]
ip = "{VIP}"
prefix = 24
interface = "fake0"
{extra}
"#,
        peers = peers.join(", ")
    );
    Config::from_toml(&text).unwrap()
}

struct Node {
    fake: FakeBackend,
    stop: oneshot::Sender<()>,
    task: JoinHandle<anyhow::Result<()>>,
}

impl Node {
    fn start(cfg: Config) -> Self {
        Self::start_with(cfg, FakeBackend::new())
    }

    fn start_with(cfg: Config, fake: FakeBackend) -> Self {
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(runtime::run(cfg, fake.clone(), async move {
            let _ = stopped.await;
        }));
        Self { fake, stop, task }
    }

    fn holds_vip(&self) -> bool {
        self.fake.is_attached(VIP)
    }

    /// A clean stop: the node says goodbye and detaches. Returns its backend for inspection.
    async fn stop(self) -> FakeBackend {
        let _ = self.stop.send(());
        self.task.await.unwrap().unwrap();
        self.fake
    }

    /// A crash: the node vanishes without a goodbye. Returns its backend for inspection.
    fn crash(self) -> FakeBackend {
        self.task.abort();
        self.fake
    }
}

async fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn failover_preemption_and_crash() {
    let addrs = free_addrs(&[Ipv4Addr::new(127, 0, 0, 11), Ipv4Addr::new(127, 0, 0, 12), Ipv4Addr::new(127, 0, 0, 13)]);
    let (a, b, c) = (addrs[0], addrs[1], addrs[2]);
    let node_a = Node::start(config(a, &[b, c], 150));
    let node_b = Node::start(config(b, &[a, c], 100));
    let node_c = Node::start(config(c, &[a, b], 50));
    wait_until("A becomes master", || node_a.holds_vip() && !node_b.holds_vip() && !node_c.holds_vip()).await;

    // A stops cleanly: it removes the VIP and says goodbye, so B takes over after just its skew.
    let stopped = node_a.stop().await;
    assert!(!stopped.is_attached(VIP), "a clean stop removes the VIP");
    wait_until("B takes over", || node_b.holds_vip() && !node_c.holds_vip()).await;

    // A returns with the higher priority and takes the VIP back.
    let node_a = Node::start(config(a, &[b, c], 150));
    wait_until("A preempts B", || node_a.holds_vip() && !node_b.holds_vip() && !node_c.holds_vip()).await;

    // A crashes without a goodbye; B takes over when its down timer runs out.
    let crashed = node_a.crash();
    wait_until("B takes over after the crash", || node_b.holds_vip() && !node_c.holds_vip()).await;
    assert!(crashed.is_attached(VIP), "a crashed node never gets to detach");

    node_b.stop().await;
    node_c.stop().await;
}

#[tokio::test]
async fn a_clean_stop_says_goodbye_with_priority_0() {
    let addrs = free_addrs(&[Ipv4Addr::new(127, 0, 0, 41), Ipv4Addr::new(127, 0, 0, 42)]);
    // A plain socket stands in for the peer. It never answers, so the node becomes master.
    let peer = UdpSocket::bind(addrs[1]).await.unwrap();
    let node = Node::start(config(addrs[0], &[addrs[1]], 150));
    wait_until("the node becomes master", || node.holds_vip()).await;
    let backend = node.stop().await;
    assert!(!backend.is_attached(VIP), "a clean stop removes the VIP");

    let codec = Codec::new(KEY.as_bytes());
    let mut buf = [0u8; 128];
    let mut last = None;
    while let Ok(Ok((len, _))) = tokio::time::timeout(Duration::from_millis(200), peer.recv_from(&mut buf)).await {
        last = Some(codec.decode(&buf[..len]).unwrap());
    }
    assert_eq!(last.expect("the node sent heartbeats").priority, 0, "the last heartbeat is the goodbye");
}

#[tokio::test]
async fn start_up_removes_a_leftover_vip_or_refuses_to_run() {
    let addrs = free_addrs(&[Ipv4Addr::new(127, 0, 0, 51), Ipv4Addr::new(127, 0, 0, 52)]);
    let cfg = config(addrs[0], &[addrs[1]], 100);
    let fake = FakeBackend::new();
    fake.attach(&cfg.vips[0]).await.unwrap(); // left over from a crash
    runtime::run(cfg.clone(), fake.clone(), tokio::time::sleep(Duration::from_millis(100))).await.unwrap();
    assert_eq!(fake.calls()[1], format!("detach {VIP}"), "the leftover VIP is removed first");
    assert!(!fake.is_attached(VIP));

    fake.attach(&cfg.vips[0]).await.unwrap();
    fake.set_fail_detach(true);
    let err = runtime::run(cfg, fake, std::future::pending()).await.unwrap_err();
    assert!(format!("{err:#}").contains("cannot remove leftover VIP"), "{err:#}");
}

#[cfg(unix)]
#[tokio::test]
async fn a_failing_check_hands_the_vip_to_the_peer_and_hooks_run() {
    let read = |path: &std::path::Path| std::fs::read_to_string(path).unwrap_or_default();
    let dir = std::env::temp_dir();
    let flag = dir.join(format!("vipd-test-flag-{}", std::process::id()));
    let hook_log = dir.join(format!("vipd-test-hooks-{}", std::process::id()));
    let _ = std::fs::remove_file(&flag);
    let _ = std::fs::remove_file(&hook_log);
    let extra = format!(
        r#"
[[check]]
name = "flag"
command = 'sh -c "! test -e {flag}"'
interval_ms = 100
weight = -60

[hooks]
on_master = 'sh -c "echo $VIPD_STATE $VIPD_PRIORITY >> {hook_log}"'
"#,
        flag = flag.display(),
        hook_log = hook_log.display()
    );
    let addrs = free_addrs(&[Ipv4Addr::new(127, 0, 0, 61), Ipv4Addr::new(127, 0, 0, 62)]);
    let node_a = Node::start(config_with(addrs[0], &[addrs[1]], 150, &extra));
    let node_b = Node::start(config(addrs[1], &[addrs[0]], 100));
    wait_until("A becomes master", || node_a.holds_vip() && !node_b.holds_vip()).await;
    wait_until("A's on_master hook ran", || read(&hook_log).starts_with("MASTER 150")).await;

    // The check now fails, so A drops to 150 - 60 = 90, below B.
    std::fs::write(&flag, "").unwrap();
    wait_until("B takes over", || node_b.holds_vip() && !node_a.holds_vip()).await;

    // The check passes again: A is back at 150 and preempts B.
    std::fs::remove_file(&flag).unwrap();
    wait_until("A takes the VIP back", || node_a.holds_vip() && !node_b.holds_vip()).await;

    node_a.stop().await;
    node_b.stop().await;
    let _ = std::fs::remove_file(hook_log);
}

#[tokio::test]
async fn a_stop_during_start_up_still_removes_a_leftover_vip() {
    let addrs = free_addrs(&[Ipv4Addr::new(127, 0, 0, 81), Ipv4Addr::new(127, 0, 0, 82)]);
    let cfg = config(addrs[0], &[addrs[1]], 100);
    let fake = FakeBackend::new();
    // A stop that has already happened...
    fake.attach(&cfg.vips[0]).await.unwrap(); // left over from a crash
    runtime::run(cfg.clone(), fake.clone(), async {}).await.unwrap();
    assert!(!fake.is_attached(VIP), "an immediate stop still removes the leftover VIP");
    // ...and one that comes during a slow detach.
    fake.attach(&cfg.vips[0]).await.unwrap();
    fake.set_detach_delay(Duration::from_millis(300));
    runtime::run(cfg, fake.clone(), tokio::time::sleep(Duration::from_millis(50))).await.unwrap();
    assert!(!fake.is_attached(VIP), "the cleanup finished before run returned");
}

#[tokio::test]
async fn shutdown_waits_for_a_slow_detach() {
    let addrs = free_addrs(&[Ipv4Addr::new(127, 0, 0, 91), Ipv4Addr::new(127, 0, 0, 92)]);
    let fake = FakeBackend::new();
    let node = Node::start_with(config(addrs[0], &[addrs[1]], 150), fake.clone());
    wait_until("the node becomes master", || node.holds_vip()).await;
    fake.set_detach_delay(Duration::from_millis(300));
    let backend = node.stop().await;
    assert!(!backend.is_attached(VIP), "run returned only after the slow detach");
}

#[tokio::test]
async fn a_node_that_cannot_attach_leaves_the_vip_to_its_peer() {
    let addrs = free_addrs(&[Ipv4Addr::new(127, 0, 0, 71), Ipv4Addr::new(127, 0, 0, 72)]);
    let broken = FakeBackend::new();
    broken.set_fail_attach(true);
    let node_a = Node::start_with(config(addrs[0], &[addrs[1]], 150), broken);
    let node_b = Node::start(config(addrs[1], &[addrs[0]], 100));
    wait_until("B holds the VIP", || node_b.holds_vip() && !node_a.holds_vip()).await;
    node_a.stop().await;
    node_b.stop().await;
}

/// The two sockets of a one-way UDP relay. Binding them before `free_addrs` keeps them from taking
/// a port chosen for a node.
struct RelaySockets {
    inbound: UdpSocket,
    outbound: UdpSocket,
}

impl RelaySockets {
    async fn bind(listen_ip: Ipv4Addr, source_ip: Ipv4Addr) -> Self {
        Self {
            inbound: UdpSocket::bind((listen_ip, 0)).await.unwrap(),
            outbound: UdpSocket::bind((source_ip, 0)).await.unwrap(),
        }
    }

    fn listen_addr(&self) -> SocketAddrV4 {
        let std::net::SocketAddr::V4(addr) = self.inbound.local_addr().unwrap() else { unreachable!() };
        addr
    }

    /// Forwards what arrives to `target`, sending from the source IP so the receiver sees the
    /// original sender's IP. Drops everything while `cut` is set.
    fn spawn(self, target: SocketAddrV4, cut: Arc<AtomicBool>) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut buf = [0u8; 256];
            while let Ok((len, _)) = self.inbound.recv_from(&mut buf).await {
                if !cut.load(Ordering::SeqCst) {
                    let _ = self.outbound.send_to(&buf[..len], target).await;
                }
            }
        })
    }
}

#[tokio::test]
async fn split_brain_forms_and_heals() {
    let (ip_a, ip_b) = (Ipv4Addr::new(127, 0, 0, 21), Ipv4Addr::new(127, 0, 0, 22));
    // A talks to B through one relay, B to A through another.
    let to_b = RelaySockets::bind(ip_b, ip_a).await;
    let to_a = RelaySockets::bind(ip_a, ip_b).await;
    let addrs = free_addrs(&[ip_a, ip_b]);
    let (a, b) = (addrs[0], addrs[1]);
    let cut = Arc::new(AtomicBool::new(false));

    let node_a = Node::start(config(a, &[to_b.listen_addr()], 150));
    let node_b = Node::start(config(b, &[to_a.listen_addr()], 100));
    let relay_ab = to_b.spawn(b, cut.clone());
    let relay_ba = to_a.spawn(a, cut.clone());
    wait_until("A becomes master", || node_a.holds_vip() && !node_b.holds_vip()).await;

    cut.store(true, Ordering::SeqCst);
    wait_until("split brain: both hold the VIP", || node_a.holds_vip() && node_b.holds_vip()).await;

    cut.store(false, Ordering::SeqCst);
    wait_until("healed: only A holds the VIP", || node_a.holds_vip() && !node_b.holds_vip()).await;

    node_a.stop().await;
    node_b.stop().await;
    relay_ab.abort();
    relay_ba.abort();
}

#[tokio::test]
async fn a_dead_vip_worker_stops_the_node() {
    let addrs = free_addrs(&[Ipv4Addr::new(127, 0, 0, 31), Ipv4Addr::new(127, 0, 0, 32)]);
    let fake = FakeBackend::new();
    fake.set_panic_on_attach(true);
    // The only peer never answers, so the node becomes master and the worker panics attaching.
    let node = runtime::run(config(addrs[0], &[addrs[1]], 100), fake, std::future::pending());
    let result = tokio::time::timeout(Duration::from_secs(5), node).await.expect("the node stops by itself");
    let err = result.expect_err("a dead VIP worker is fatal");
    assert!(err.to_string().contains("VIP worker"), "{err:#}");
}

#[tokio::test]
async fn a_late_attach_failure_from_an_earlier_term_does_not_fault_the_new_master() {
    let addrs = free_addrs(&[Ipv4Addr::new(127, 0, 0, 101), Ipv4Addr::new(127, 0, 0, 102)]);
    // A plain socket plays the peer, so the test decides exactly when it is master.
    let peer = UdpSocket::bind(addrs[1]).await.unwrap();
    let fake = FakeBackend::new();
    fake.set_attach_delay(Duration::from_millis(500));
    fake.set_fail_attach(true);
    let node = Node::start_with(config(addrs[0], &[addrs[1]], 100), fake.clone());
    let attaches = |f: &FakeBackend| f.calls().iter().filter(|c| c.starts_with("attach")).count();
    wait_until("the first attach starts", || attaches(&fake) == 1).await;

    // While that attach runs, a higher master appears and then says goodbye: the node steps down
    // and becomes master again, so a second attach is queued behind the first.
    let codec = Codec::new(KEY.as_bytes());
    let heartbeat = |priority, seq| {
        codec.encode(&Heartbeat { group_id: 7, priority, interval_ms: 50, vip_fingerprint: 0, boot_id: 1, seq })
    };
    peer.send_to(&heartbeat(200, 1), addrs[0]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    peer.send_to(&heartbeat(0, 2), addrs[0]).await.unwrap();

    // The first attach fails; the second, for the new term, succeeds.
    wait_until("the second attach starts", || attaches(&fake) == 2).await;
    fake.set_fail_attach(false);
    wait_until("the node holds the VIP", || node.holds_vip()).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(node.holds_vip(), "the first term's failure was charged to the second term");
    node.stop().await;
}

#[tokio::test]
async fn a_slow_backend_does_not_replay_a_backlog_after_the_election_settles() {
    let addrs = free_addrs(&[Ipv4Addr::new(127, 0, 0, 111), Ipv4Addr::new(127, 0, 0, 112)]);
    // A plain socket plays the peer, so the test decides exactly when it is master.
    let peer = UdpSocket::bind(addrs[1]).await.unwrap();
    let fake = FakeBackend::new();
    fake.set_attach_delay(Duration::from_millis(300));
    fake.set_detach_delay(Duration::from_millis(300));
    let node = Node::start_with(config(addrs[0], &[addrs[1]], 100), fake.clone());
    wait_until("the node holds the VIP", || node.holds_vip()).await;

    let codec = Codec::new(KEY.as_bytes());
    let mut seq = 0;
    let mut packet = |priority| {
        seq += 1;
        codec.encode(&Heartbeat { group_id: 7, priority, interval_ms: 50, vip_fingerprint: 0, boot_id: 1, seq })
    };
    // For 2 s the peer takes over and says goodbye about every 100 ms, so the node steps down and
    // takes over again each time, much faster than its backend can follow.
    let flapping_ends = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < flapping_ends {
        peer.send_to(&packet(200), addrs[0]).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        peer.send_to(&packet(0), addrs[0]).await.unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
    }
    // Then the peer stays master, so the node is a backup and should let go within one detach.
    let mut held = 0;
    for _ in 0..100 {
        peer.send_to(&packet(200), addrs[0]).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        held += usize::from(node.holds_vip());
    }
    assert!(held <= 10, "as a backup the node still held the VIP in {held} of 100 samples over 5 s");
    node.crash();
}

#[tokio::test]
async fn a_failed_bind_still_removes_a_leftover_vip() {
    let addrs = free_addrs(&[Ipv4Addr::new(127, 0, 0, 121), Ipv4Addr::new(127, 0, 0, 122)]);
    let _squatter = UdpSocket::bind(addrs[0]).await.unwrap(); // something else holds the port
    let cfg = config(addrs[0], &[addrs[1]], 100);
    let fake = FakeBackend::new();
    fake.attach(&cfg.vips[0]).await.unwrap(); // left over from a crash
    let err = runtime::run(cfg, fake.clone(), std::future::pending()).await.unwrap_err();
    assert!(format!("{err:#}").contains("cannot bind"), "{err:#}");
    assert!(!fake.is_attached(VIP), "vipd cannot start, and the leftover VIP stays on this node");
}

#[tokio::test]
async fn a_clean_stop_is_not_stuck_behind_a_backlog() {
    let addrs = free_addrs(&[Ipv4Addr::new(127, 0, 0, 131), Ipv4Addr::new(127, 0, 0, 132)]);
    let peer = UdpSocket::bind(addrs[1]).await.unwrap();
    let fake = FakeBackend::new();
    fake.set_attach_delay(Duration::from_millis(300));
    fake.set_detach_delay(Duration::from_millis(300));
    let node = Node::start_with(config(addrs[0], &[addrs[1]], 100), fake.clone());
    wait_until("the node holds the VIP", || node.holds_vip()).await;
    let codec = Codec::new(KEY.as_bytes());
    let mut seq = 0;
    let mut packet = |priority| {
        seq += 1;
        codec.encode(&Heartbeat { group_id: 7, priority, interval_ms: 50, vip_fingerprint: 0, boot_id: 1, seq })
    };
    // 4 s of flapping queues about 24 s of backend work, more than the 15 s a stop waits for it.
    let flapping_ends = tokio::time::Instant::now() + Duration::from_secs(4);
    while tokio::time::Instant::now() < flapping_ends {
        peer.send_to(&packet(200), addrs[0]).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        peer.send_to(&packet(0), addrs[0]).await.unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
    }
    let started = tokio::time::Instant::now();
    let _ = node.stop.send(());
    let result = node.task.await.unwrap();
    assert!(result.is_ok(), "the stop failed after {:.1} s: {result:?}", started.elapsed().as_secs_f64());
}
