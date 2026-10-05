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
use vipd::runtime;
use vipd::vip::fake::FakeBackend;

const VIP: Ipv4Addr = Ipv4Addr::new(10, 99, 0, 1);

/// One free UDP address per IP. All sockets are held until every port is chosen, so the
/// ports are distinct.
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
    let peers: Vec<String> = peers.iter().map(|p| format!("\"{p}\"")).collect();
    let text = format!(
        r#"
node_name = "node-{priority}"
group_id = 7
priority = {priority}
advert_interval_ms = 50
auth_key = "integration-test-key"
bind = "{bind}"
peers = [{peers}]

[[vip]]
ip = "{VIP}"
prefix = 24
interface = "fake0"
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
        let fake = FakeBackend::new();
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(runtime::run(cfg, fake.clone(), async move {
            let _ = stopped.await;
        }));
        Self { fake, stop, task }
    }

    fn holds_vip(&self) -> bool {
        self.fake.is_attached(VIP)
    }

    /// A clean stop: the node says goodbye and detaches.
    async fn stop(self) {
        let _ = self.stop.send(());
        self.task.await.unwrap().unwrap();
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

    // A stops cleanly and says goodbye, so B takes over after just its skew.
    node_a.stop().await;
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

/// Forwards UDP that arrives on `listen_ip` to `target`, sending from `source_ip` so the receiver
/// sees the original sender's IP. Drops everything while `cut` is set.
async fn relay(
    listen_ip: Ipv4Addr,
    source_ip: Ipv4Addr,
    target: SocketAddrV4,
    cut: Arc<AtomicBool>,
) -> (SocketAddrV4, JoinHandle<()>) {
    let inbound = UdpSocket::bind((listen_ip, 0)).await.unwrap();
    let outbound = UdpSocket::bind((source_ip, 0)).await.unwrap();
    let std::net::SocketAddr::V4(listen) = inbound.local_addr().unwrap() else { unreachable!() };
    let handle = tokio::spawn(async move {
        let mut buf = [0u8; 256];
        while let Ok((len, _)) = inbound.recv_from(&mut buf).await {
            if !cut.load(Ordering::SeqCst) {
                let _ = outbound.send_to(&buf[..len], target).await;
            }
        }
    });
    (listen, handle)
}

#[tokio::test]
async fn split_brain_forms_and_heals() {
    let (ip_a, ip_b) = (Ipv4Addr::new(127, 0, 0, 21), Ipv4Addr::new(127, 0, 0, 22));
    let addrs = free_addrs(&[ip_a, ip_b]);
    let (a, b) = (addrs[0], addrs[1]);
    let cut = Arc::new(AtomicBool::new(false));
    // A talks to B through one relay, B to A through another.
    let (to_b, relay_ab) = relay(ip_b, ip_a, b, cut.clone()).await;
    let (to_a, relay_ba) = relay(ip_a, ip_b, a, cut.clone()).await;

    let node_a = Node::start(config(a, &[to_b], 150));
    let node_b = Node::start(config(b, &[to_a], 100));
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
