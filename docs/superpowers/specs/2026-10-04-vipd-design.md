# vipd — cross-platform virtual IP failover daemon: design

- **Date:** 2026-10-04
- **Status:** approved in brainstorming, awaiting spec review
- **Working name:** `vipd` (rename freely)

## 1. Goal

A small Rust daemon that keeps one virtual IP (VIP) alive across two or more machines, the way keepalived does, but running on both **Linux and Windows**.

- The election logic (heartbeats, priorities, timers, preemption, health-check weights) follows VRRP's rules and is identical on every OS.
- Only attaching and detaching the VIP is OS-specific, and it is done with each OS's own commands.

## 2. Decisions

| Topic | Decision |
|---|---|
| Protocol | Our own protocol over UDP. It follows VRRP rules but is not wire-compatible with VRRP or keepalived. |
| Transport | Unicast to a configured peer list. No multicast. |
| Target OSes (v1) | Linux and Windows |
| VIP groups | One group per node. A group can hold one or more VIPs, which always move together. The packet carries a `group_id` so more groups can be added later. |
| Health checks | Check commands with rise/fall and weights. Weight 0 means FAULT. |
| Run mode | Linux: a systemd unit. Windows: a native Windows service. Both can also run in the foreground. |
| Code structure | A pure election core ("events in, actions out", no I/O) inside a tokio runtime shell. One VIP backend per OS. |
| Authentication | HMAC-SHA256 with a shared key, plus replay protection |
| Address family | IPv4 only in v1, for both heartbeats and VIPs |

## 3. Not in v1

- IPv6
- Multiple VIP groups per node
- Multicast discovery
- Interface link tracking
- macOS
- Config reload without restart
- Detecting a VIP removed by hand while MASTER
- A status or metrics endpoint
- A native gratuitous-ARP announcer on Windows (see §15)

## 4. Architecture

```
  UDP heartbeats ─▶ proto: decode + verify ──────────┐
  check results  ─▶ checks: rise/fall, weights ──────┤  events
  timer deadline ────────────────────────────────────┼─────────▶ core::Machine
  stop signal (SIGTERM / Ctrl+C / service stop) ─────┘               │ actions
                                                                     ▼
     runtime: send heartbeats · VIP worker (attach / detach / announce) · hooks
```

| Module | Responsibility | Depends on |
|---|---|---|
| `core` | The election state machine. It has no sockets, no real clock and no processes. | std only |
| `proto` | Packet encode and decode, the HMAC, and replay tracking | `hmac`, `sha2` |
| `checks` | Pure rise/fall state and health aggregation, plus an async runner that executes the check commands | tokio |
| `vip` | The `VipBackend` trait, a Linux backend (`ip` plus gratuitous ARP via `libc`), a Windows backend (`netsh`), a fake backend for tests, and the command-template tokenizer | tokio, `libc` (Linux only) |
| `runtime` | The event loop, the VIP worker and hook execution | everything above |
| `service` | The Windows service: install, uninstall and the SCM entry point | `windows-service` (Windows only) |
| `config` | Loading and validating the TOML | `serde`, `toml` |
| `cli` / `main` | Command-line parsing and process exit codes | `clap` |

- **Crate layout.** The project is a library crate (`src/lib.rs`) plus a binary (`src/main.rs`), so the integration tests in `tests/` can use the modules directly.
- **OS-specific code** lives only in `vip/linux*.rs`, `vip/windows.rs` and `service/windows.rs`, selected with `#[cfg(target_os = ...)]`.
- **Other crates:**
  - `tracing`, `tracing-subscriber` and `tracing-appender` (daily-rotated file logs for the Windows service)
  - `anyhow` and `thiserror`
  - `rand` is not needed, because `boot_id` is time-based
- **Runtime:** tokio's `current_thread` runtime.

### Project layout

```
Cargo.toml
src/
  lib.rs  main.rs  cli.rs  config.rs
  core/      mod.rs  machine.rs  timers.rs
  proto/     mod.rs  packet.rs  replay.rs
  checks/    mod.rs  state.rs  runner.rs
  vip/       mod.rs  template.rs  linux.rs  linux_garp.rs  windows.rs  fake.rs
  runtime/   mod.rs  vip_worker.rs  hooks.rs
  service/   mod.rs  windows.rs
tests/       multi_node.rs
packaging/   vipd.service
examples/    vipd.toml
docs/superpowers/specs/2026-10-04-vipd-design.md
```

## 5. Election engine (`core`)

### 5.1 States and priorities

- **States:**
  - `Backup`: listening.
  - `Master`: holds the VIPs and sends heartbeats.
  - `Fault`: holds no VIPs, sends nothing, and ignores heartbeats.
- Only the master sends heartbeats.
- **Configured base priority:** 1–254. Priority **0** exists only on the wire, where it means *goodbye*.
- **Effective priority** = `clamp(base + Σ weights, 1, 254)`:
  - A check with a **negative** weight contributes that weight while it is failing.
  - A check with a **positive** weight contributes that weight while it is passing.
  - A check with weight **0** contributes nothing. Instead, while it is failing the node is in Fault.

### 5.2 Timers

| Timer | Running in | Duration |
|---|---|---|
| advert | Master | `interval` |
| down | Backup | `3 × interval + skew`, where `skew = (256 − effective_priority) / 256 × interval` |
| reannounce | Master | Fires once, 5 s after becoming master |
| hold-down | Fault, when caused by an attach failure | 10 s |

- **Which `interval` the down timer uses.** It uses the `interval_ms` from the last accepted heartbeat. If none has been accepted yet, it uses the node's own `advert_interval_ms`.
- **On a goodbye** (priority 0), the down timer is set to `skew` alone.
- **Example.** With `interval` 1 s and priorities 150/100, the backup takes over about 3.61 s after the master's last heartbeat, or about 0.61 s after a goodbye.

### 5.3 Tie-break

"Higher" means a higher effective priority. At equal priority, the higher IPv4 address wins: the receiver compares the packet's source IP with its own `bind` IP.

### 5.4 API

```rust
pub enum State { Backup, Master, Fault }
pub enum HookKind { Master, Backup, Fault, Stop }

pub enum Event {
    Started { health: Health },                      // after start-up cleanup + first check round
    Heartbeat { from: Ipv4Addr, priority: u8, interval: Duration },
    TimerFired,
    HealthChanged(Health),
    AttachFailed,
    Shutdown,
}
pub struct Health { pub effective_priority: u8, pub fault: bool }

pub enum Action {
    SendHeartbeat { priority: u8 },   // to every peer; 0 = goodbye
    AttachVips,                       // the VIP worker attaches, then announces
    Announce,                         // re-announce only
    DetachVips,
    RunHook(HookKind),
}

impl Machine {
    pub fn new(cfg: MachineConfig) -> Self;                       // MachineConfig: preempt, advert interval, own IP,
                                                                  // hold-down (10 s), reannounce delay (5 s)
    pub fn handle(&mut self, ev: Event, now: Instant) -> Vec<Action>;
    pub fn next_deadline(&self) -> Option<Instant>;               // earliest active timer
    pub fn state(&self) -> State;
}
```

- `now` is always passed in; `core` never reads a clock.
- Tests use synthetic `Instant`s.
- The runtime sends `TimerFired` when `next_deadline()` passes, and the machine handles every timer that has expired.

### 5.5 Transitions

Rows are checked from top to bottom, and the first row that matches wins. "Mine" means this node's current effective priority.

| State | Event | Actions, then the new state |
|---|---|---|
| (new) | `Started`, health OK | Arm down → **Backup** |
| (new) | `Started`, health fault | `RunHook(Fault)` → **Fault** |
| Backup | Heartbeat, priority 0 | down = `skew` |
| Backup | Heartbeat, priority ≥ mine, or `preempt = false` | Learn the interval and reset down |
| Backup | Heartbeat, priority < mine, with `preempt = true` | Ignore it; down is not reset |
| Backup | down expires | `SendHeartbeat(mine)`, `AttachVips`, `RunHook(Master)`, arm advert and reannounce → **Master** |
| Master | advert expires | `SendHeartbeat(mine)` and re-arm advert |
| Master | reannounce expires | `Announce` |
| Master | Heartbeat, priority 0 | `SendHeartbeat(mine)` and re-arm advert |
| Master | Heartbeat from a higher node (§5.3) | `DetachVips`, `RunHook(Backup)`, arm down → **Backup** |
| Master | Heartbeat from a lower node (split brain) | `SendHeartbeat(mine)`, `Announce`, re-arm advert |
| Master | `AttachFailed` | `SendHeartbeat(0)`, `DetachVips`, `RunHook(Fault)`, arm hold-down → **Fault** |
| Backup or Fault | `AttachFailed` | Ignore it. It is a late report for a role this node has already left. |
| Backup or Master | `HealthChanged`, fault | If Master: `SendHeartbeat(0)` and `DetachVips`. Then `RunHook(Fault)` → **Fault** |
| Backup or Master | `HealthChanged`, no fault | Store the new effective priority. It is used from the next heartbeat or down reset. |
| Fault | `HealthChanged`, no fault, hold-down not running | `RunHook(Backup)`, arm down → **Backup** |
| Fault | `HealthChanged`, any other case | Store the new health and stay in **Fault** |
| Fault | hold-down expires and health is OK | `RunHook(Backup)`, arm down → **Backup** |
| Fault | hold-down expires and health is in fault | Stay in **Fault** until a `HealthChanged` with no fault arrives |
| Fault | Heartbeat | Ignore it |
| any | `Shutdown` | If Master: `SendHeartbeat(0)` and `DetachVips`. Then `RunHook(Stop)`. |

## 6. Wire protocol (`proto`)

### 6.1 Transport

- UDP over IPv4.
- The default port is **8458**. Each node binds to its configured `bind` address.
- The master sends each heartbeat to every address in `peers`.

### 6.2 Packet

The packet is 64 bytes, big-endian.

| Bytes | Field | Notes |
|---|---|---|
| 0–3 | magic `b"VIPD"` | |
| 4 | version = 1 | |
| 5 | kind = 1 | heartbeat |
| 6–7 | `group_id` (u16) | |
| 8 | `priority` (u8) | 0 means goodbye |
| 9 | `flags` (u8) | always 0 in v1, ignored on receive |
| 10–11 | `interval_ms` (u16) | |
| 12–15 | `vip_fingerprint` (u32) | First 4 bytes of SHA-256 over the sorted `"ip/prefix"` strings joined with `,`. Interface names are excluded. |
| 16–23 | `boot_id` (u64) | Unix time in milliseconds when the process started |
| 24–31 | `seq` (u64) | Starts at 1 and goes up by 1 with every packet sent, goodbyes included |
| 32–63 | HMAC-SHA256 | Keyed with `auth_key` (UTF-8 bytes), computed over bytes 0–31 |

### 6.3 Receive validation

The checks run in this order, and a packet that fails any of them is dropped.

1. The length is 64, and the magic and version match.
2. The source IP is one of the `peers` IPs. The source port is ignored.
3. The HMAC is valid. Verification is constant-time.
4. `group_id` equals ours.
5. The replay check passes (§6.4).
6. If `interval_ms` or `vip_fingerprint` differs from ours, the packet is **still accepted**, but a warning is logged.

- A dropped packet counts as silence, exactly as in VRRP.
- Warnings are rate-limited to one per (peer, reason) per 60 s.

### 6.4 Replay protection

For each peer, keyed by source IP, the receiver remembers the `(boot_id, seq)` of the last packet it accepted and when it accepted it. A new packet is accepted if any of these holds:

1. Nothing has been accepted from this peer yet.
2. The `boot_id` matches and `seq` is greater than the last one.
3. The `boot_id` is greater than the last one, meaning the peer restarted.
4. The peer has been silent for at least `3 × its last interval_ms`. In that case any `boot_id` is accepted, which covers a restart after the clock moved backwards.

Known limit: a node that has just started accepts the first packet it sees from each peer, so an attacker who captured traffic can replay one old heartbeat to it. That is acceptable for v1.

## 7. VIP backends (`vip`)

### 7.1 Trait

```rust
pub struct Vip { pub ip: Ipv4Addr, pub prefix: u8, pub interface: String }

pub trait VipBackend {
    /// Some("ip/prefix") as currently configured on the interface, or None.
    async fn find(&self, vip: &Vip) -> anyhow::Result<Option<String>>;
    async fn attach(&self, vip: &Vip) -> anyhow::Result<()>;    // no-op if find() is Some
    async fn detach(&self, vip: &Vip) -> anyhow::Result<()>;    // no-op if find() is None
    async fn announce(&self, vip: &Vip) -> anyhow::Result<()>;  // no-op where the OS announces
}
```

- The trait uses native async functions in traits; there is no `async_trait`.
- The runtime is generic over `B: VipBackend`, so tests can use `vip::fake::FakeBackend`, which records every call and can be told to fail.
- Every OS command runs through `tokio::process::Command` with a **10 s timeout**. A command that times out is killed and counted as a failure.

### 7.2 Linux backend

| Operation | Command |
|---|---|
| find | `ip -o -4 addr show dev {iface}`. Look for the token after `inet` whose address part equals `{ip}`, and return the whole `ip/prefix` token. |
| attach | `ip addr add {ip}/{prefix} dev {iface}` |
| detach | `ip addr del {found} dev {iface}`, using the prefix that `find` actually returned |
| announce | Native gratuitous ARP (`linux_garp.rs`), described below |

How the gratuitous ARP is sent:
- A `socket(AF_PACKET, SOCK_RAW, htons(ETH_P_ARP))`, with the interface index from `if_nametoindex`.
- The interface's MAC address comes from `/sys/class/net/{iface}/address`.
- The frame:
  - Ethernet: destination `ff:ff:ff:ff:ff:ff`, source our MAC, ethertype `0x0806`.
  - ARP: request, sender = our MAC and the VIP, target MAC zero, target IP = the VIP.
- Five frames are sent back-to-back.
- It needs root, or `CAP_NET_ADMIN` plus `CAP_NET_RAW`.

### 7.3 Windows backend

| Operation | Command |
|---|---|
| find | `netsh interface ipv4 show ipaddresses "{iface}"`. Look for a whitespace-separated token equal to `{ip}`, which works in any language. |
| attach | `netsh interface ipv4 add address "{iface}" {ip} {mask} store=active skipassource=true`, then the duplicate-address check below |
| detach | `netsh interface ipv4 delete address "{iface}" {ip} store=active` |
| announce | No-op. Windows announces a new address itself; this is to be verified (§15). |

- **`store=active`** means the VIP disappears when the machine reboots.
- **`skipassource=true`** keeps outgoing traffic on the node's own IP.
- **Duplicate-address check after attach.** Run one PowerShell process: `powershell -NoProfile -NonInteractive -Command <script>`.
  - The script polls `(Get-NetIPAddress -IPAddress {ip}).AddressState` every 250 ms for up to 3 s, until the state is no longer `Tentative`, and prints the final state. The state names are enum values, so they don't depend on the Windows language.
  - `Duplicate`: detach, wait 1 s and retry the attach, up to 3 attempts. After that, return an error, which the runtime turns into `AttachFailed`.
- **Error hint.** When attach fails, the log adds: "if the adapter uses DHCP, enable `dhcpstaticipcoexistence` or use a static IP".

### 7.4 Command overrides and templates

`[vip_commands]` can replace `attach` and/or `detach`. `find` and `announce` stay built in.

- **Tokenizer** (`template.rs`), used for overrides, checks and hooks:
  - Arguments are split on whitespace.
  - A `"double-quoted"` segment is one argument, with the quotes removed. There are no escape sequences.
  - The first token is the program.
  - There is no shell.
- **Placeholders** are `{ip}`, `{prefix}`, `{mask}` and `{iface}`. They are substituted inside each token *after* splitting, so a value containing a space (such as `Ethernet 2`) stays a single argument.
- **Overrides stay idempotent.** `find` still runs first, so an override runs only when a change is actually needed. For an override, exit code 0 means success.
- **The Windows duplicate-address check (§7.3) runs only after the built-in attach.** An override may do something completely different, such as calling a cloud API.

## 8. Health checks (`checks`)

| Key | Default | Range |
|---|---|---|
| `name` | required, unique | |
| `command` | required (§7.4 tokenizer) | |
| `interval_ms` | 1000 | 100–3,600,000 |
| `timeout_ms` | = `interval_ms` | 100–3,600,000 |
| `fall` | 1 | ≥ 1 |
| `rise` | 1 | ≥ 1 |
| `weight` | 0 | −253…253 |

- **Pass and fail.** Exit code 0 is a pass. Any other exit code is a fail, and so is being killed when `timeout_ms` is reached.
- **Overlapping runs.** If the previous run of a check is still going, the new run is skipped.
- **Shell features** are available by writing `sh -c '…'` or `cmd /C …` as the command.
- **State** (pure, in `state.rs`):
  - Each check is `Ok` or `Failing`.
  - `fall` consecutive failures turn `Ok` into `Failing`, and `rise` consecutive passes turn `Failing` into `Ok`. A result in the other direction resets the count.
  - Before the first result, a check has no state. In the first round, the single first result decides it (as in keepalived).
- **Aggregation.** `aggregate(base, &checks) -> Health` computes §5.1.
  - The runtime sends `HealthChanged` only when `Health` actually changes.
- **Start-up.** Every check runs once, concurrently, and each run is bounded by its own timeout. The machine starts only after all of them finish (§11.1).

## 9. Hooks

- **Configuration.** `[hooks]` has optional `on_master`, `on_backup`, `on_fault` and `on_stop` commands, parsed with the §7.4 tokenizer.
- **Environment.** Each hook receives `VIPD_STATE` (`MASTER`, `BACKUP`, `FAULT` or `STOP`), `VIPD_PRIORITY` (the effective priority) and `VIPD_GROUP`.
- **Execution.**
  - Hooks run in the background and are killed after 60 s.
  - Failures are only logged and never affect the election.
  - On shutdown, the runtime waits up to 5 s for `on_stop` to finish.

## 10. Configuration (`config`)

The default path is `/etc/vipd/vipd.toml` on Linux and `C:\ProgramData\vipd\vipd.toml` on Windows.

| Key | Default | Rule |
|---|---|---|
| `node_name` | required | non-empty; used in logs |
| `group_id` | required | 1–65535; same on all nodes |
| `priority` | required | 1–254 |
| `preempt` | `true` | |
| `advert_interval_ms` | 1000 | 50–60000; should match on all nodes (a mismatch is warned about) |
| `auth_key` | required | ≥ 16 characters; same on all nodes |
| `bind` | required | `ip:port`. The IPv4 address must not be `0.0.0.0`, because it is needed for tie-breaks. |
| `peers` | required | at least one `ip:port`; unique; must not contain the `bind` IP |
| `log_level` | `"info"` | `RUST_LOG` overrides it |
| `log_dir` | `C:\ProgramData\vipd` | used by the Windows service only |
| `[[vip]]` | at least one | `ip` (IPv4, required), `prefix` (1–32, default 32), `interface` (required, must exist) |
| `[[check]]` | none | see §8 |
| `[hooks]` | none | see §9 |
| `[vip_commands]` | none | see §7.4 |

How "the interface exists" is checked:
- **Linux:** `/sys/class/net/{iface}` exists.
- **Windows:** `netsh interface ipv4 show interfaces "{iface}"` exits with code 0.

Example (`examples/vipd.toml`):

```toml
node_name = "web-a"
group_id = 51
priority = 150
preempt = true
advert_interval_ms = 1000
auth_key = "a-long-random-shared-secret"

bind = "192.168.1.13:8458"
peers = ["192.168.1.14:8458"]

[[vip]]
ip = "192.168.1.200"
prefix = 24
interface = "eth0"            # Windows: the adapter name, e.g. "Ethernet"

[[check]]
name = "nginx"
command = "curl -sf http://127.0.0.1/"
fall = 2
rise = 2
weight = -60                  # 150 → 90 while failing; a 100 node takes over

[hooks]
on_master = "/usr/local/bin/vip-alert.sh master"
```

## 11. Runtime (`runtime`)

### 11.1 Start-up sequence

1. Load and validate the config. On error, exit with code **2**.
2. Bind the UDP socket.
3. Run `detach` for every configured VIP, cleaning up after a crash. If this fails, exit with code 1, because the node is not safe to run.
4. Run the first round of checks and compute `Health`.
5. Create the `Machine` and feed it `Started { health }`.
6. Enter the event loop.

### 11.2 Event loop

A single `tokio::select!` loop over:
- UDP receive, which is decoded, validated and sent to the machine as `Heartbeat`;
- `sleep_until(next_deadline)`, which sends `TimerFired`;
- the check-result channel, which updates check state and sends `HealthChanged` on a change;
- the VIP worker's error channel, which sends `AttachFailed`;
- the stop signal, which sends `Shutdown`.

The loop carries out the resulting actions as follows.

- **`SendHeartbeat`** is built and sent immediately with `send_to` to each peer, inside the loop. It never waits on a command.
- **`AttachVips`, `DetachVips` and `Announce`** are handed to the VIP worker. The loop never waits for an OS command, so heartbeats keep flowing while slow commands run (for example Windows' 3 s duplicate-address check).
- **`RunHook`** is spawned as a background task.

### 11.3 VIP worker (`vip_worker.rs`)

A single task that owns the backend and processes requests in order.

- It holds a **desired state**: attached or detached.
- **`AttachVips`** sets the desired state to *attached*, then runs `attach` and `announce` for each VIP. If any attach fails, it reports `AttachFailed`; the machine then emits `DetachVips`.
  - On Linux, `announce` sends the first burst of gratuitous ARPs.
- **`DetachVips`** sets the desired state to *detached* and runs `detach` for each VIP.
  - If detach fails, it retries every 2 s, logging an error each time, until it succeeds or the desired state changes.
- **`Announce`** runs `announce` for each VIP only if the desired state is *attached*.

### 11.4 Shutdown

1. Feed `Shutdown` to the machine and carry out the resulting actions.
2. Wait up to 15 s for the VIP worker to finish its queue.
3. Wait up to 5 s for `on_stop`.
4. Exit with code 0.

## 12. CLI

| Command | Purpose |
|---|---|
| `vipd run [--config PATH]` | Run in the foreground. SIGTERM, SIGINT and Ctrl+C trigger a graceful stop. |
| `vipd check-config [--config PATH]` | Validate the config and exit with 0 (valid) or 2 (invalid) |
| `vipd service install [--config PATH]` | Windows only. Registers the service `vipd`: automatic start, LocalSystem, restart after 5 s on failure. |
| `vipd service uninstall` | Windows only |
| `vipd service run --config PATH` | Windows only. The entry point the Service Control Manager calls; users don't run it. |

On Linux, the `service` subcommands print an error that points to `packaging/vipd.service`.

## 13. Service integration and logging

- **Linux** (`packaging/vipd.service`):
  - Runs `ExecStart=/usr/local/bin/vipd run --config /etc/vipd/vipd.toml` as root.
  - `Restart=on-failure`, `RestartSec=2`.
  - `After=network-online.target` and `Wants=network-online.target`.
  - `WantedBy=multi-user.target`.
  - Logs go to stdout, where journald collects them.
- **Windows:**
  - A service stop triggers the same graceful shutdown (§11.4).
  - Logs go to `{log_dir}/vipd.log` through `tracing-appender`, rotated daily.
  - In a console, logs go to stdout.

## 14. Error handling

| Situation | Behaviour |
|---|---|
| A packet is malformed, forged, from the wrong group or replayed | Dropped, with a rate-limited warning. Never a crash. |
| A VIP command hangs | Killed after 10 s and counted as a failure |
| Attach fails | The machine goes to Fault with a 10 s hold-down (§5.5) |
| Detach fails | Retried every 2 s, with loud errors, until it succeeds (§11.3) |
| The process panics or crashes | systemd or the SCM restarts it, and the start-up cleanup (§11.1 step 3) removes any leftover VIP |
| The config is invalid | Refuses to start and names the bad setting. Exit code 2. |
| The wall clock jumps | Timers use a monotonic clock. Only `boot_id` uses wall-clock time, and §6.4 rule 4 covers a backwards jump. |

## 15. To verify on real Windows

1. Windows sends a gratuitous ARP when `netsh … add address` runs. Capture it with Wireshark on another machine.
   - If it doesn't, v1.1 adds an announcer.
2. `delete address … store=active` removes an address that was added with `store=active`.
3. Positional `"{iface}"` arguments work for adapter names that contain spaces.
4. The `Duplicate` retry path works during preemption.
5. On a DHCP adapter, attach works once `dhcpstaticipcoexistence=enabled` is set, and the error hint appears when it isn't.
6. Service install, start, stop and uninstall all work, and restart-on-failure works.

## 16. Testing

1. **`core`** (unit tests with synthetic `Instant`s):
   - the first election;
   - takeover after the master goes silent, at `3 × interval + skew`;
   - takeover after a goodbye, at `skew`;
   - preemption with `preempt` on and off;
   - the equal-priority IP tie-break;
   - split-brain healing;
   - weights and Fault, with entry and exit;
   - the attach-failure hold-down;
   - shutdown from each state.
2. **`proto`:**
   - an encode/decode round trip;
   - tampered bytes, a wrong key and a wrong group are all rejected;
   - all four replay rules;
   - the fingerprint is the same regardless of VIP order.
3. **`checks`:**
   - the rise/fall state machine;
   - aggregation and clamping;
   - the runner kills a check that times out (using `sleep`) and counts it as a fail.
4. **`vip`:**
   - the template tokenizer;
   - command construction and `find` parsing for **both** OSes, as pure functions that run on any OS;
   - an opt-in (`#[ignore]`) Linux test that attaches and detaches a VIP on a dummy interface inside a network namespace, and needs root.
5. **`tests/multi_node.rs`.** Three in-process nodes on `127.0.0.1` with different ports, `FakeBackend` and 50 ms intervals. The tests:
   - stop the master, and exactly one new master appears;
   - restart the old master, and preemption happens;
   - drop traffic between nodes, and split brain forms and then heals.

   Each node needs a distinct tie-break IP, so `127.0.0.2`, `127.0.0.3` and so on are used. These loopback addresses work on Linux.
6. **Windows build.** `cargo check --target x86_64-pc-windows-gnu`. This needs the Windows target's standard library installed through rustup, so ask the user before installing it. After that, run the §15 checklist on a real machine.
