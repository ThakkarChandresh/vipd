# vipd — cross-platform virtual IP failover daemon: design

- **Date:** 2026-10-04
- **Status:** implemented on branch `feat/vipd-v1`. This document describes the shipped design. The plan's "Deliberate differences from the spec" table (docs/superpowers/plans/2026-10-04-vipd.md) records each change from the original design and why it was made.
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
  timer deadline ────────────────────────────────────┼─────────▶ election::Machine
  stop (SIGTERM / SIGHUP / Ctrl+C / service stop) ───┘               │ actions
                                                                     ▼
     runtime: send heartbeats · VIP worker (attach / detach / announce) · hooks
```

| Module | Responsibility | Depends on |
|---|---|---|
| `election` | The election state machine. It has no sockets, no real clock and no processes. | std only |
| `proto` | Packet encode and decode, the HMAC, and replay tracking | `hmac`, `sha2` |
| `exec` | The command tokenizer (§7.4), and running a command without a shell, with a timeout | tokio, `libc` (Linux only) |
| `checks` | Pure rise/fall state and health aggregation, plus an async runner that executes the check commands | `election` (for `Health`), `exec`, tokio |
| `vip` | The `VipBackend` trait, `VipManager` and `CommandOverrides` (the `[vip_commands]` overrides), a Linux backend (`ip` plus gratuitous ARP via `libc`), a Windows backend (`netsh`) and a fake backend for tests | `exec`, tokio, `libc` (Linux only) |
| `runtime` | The event loop, the VIP worker and hook execution | everything above |
| `service` | The Windows service: install, uninstall and the SCM entry point | `windows-service` (Windows only) |
| `config` | Loading and validating the TOML | `checks`, `exec`, `proto`, `vip`, `serde`, `toml` |
| `logging` | Log output: stdout, or daily files for the Windows service (§13) | `tracing-subscriber`, `tracing-appender` (Windows only) |
| `cli` / `main` | Command-line parsing and process exit codes | `clap` |

- **Crate layout.** The project is a library crate (`src/lib.rs`) plus a binary (`src/main.rs`), so the integration tests in `tests/` can use the modules directly.
- **OS-specific code** lives mostly in `vip/linux.rs`, `vip/garp.rs` (sending only), `vip/windows.rs` and `service/windows.rs`, selected with `#[cfg(...)]`. Smaller pieces include the process groups in `exec.rs`, the Unix signals in `main.rs`, the Linux interface-name check in `config.rs` and the Windows log files in `logging.rs`.
- **Other crates:**
  - `tracing`, `tracing-subscriber` and `tracing-appender` (daily-rotated file logs for the Windows service)
  - `anyhow` and `thiserror`
  - `rand` is not needed: `boot_id` is time-based, and its 16 random bits come from std's `RandomState`
- **Runtime:** tokio's `current_thread` runtime.

### Project layout

```
Cargo.toml
src/
  lib.rs  main.rs  cli.rs  config.rs  exec.rs  logging.rs
  election/  mod.rs  machine.rs  timers.rs
  proto/     mod.rs  packet.rs  replay.rs
  checks/    mod.rs  state.rs  runner.rs
  vip/       mod.rs  linux.rs  garp.rs  windows.rs  fake.rs
  runtime/   mod.rs  vip_worker.rs  hooks.rs  limiter.rs
  service/   mod.rs  windows.rs
tests/       multi_node.rs  cli.rs  example_config.rs
packaging/   vipd.service
examples/    vipd.toml
docs/superpowers/specs/2026-10-04-vipd-design.md
```

## 5. Election engine (`election`)

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
    pub fn health(&self) -> Health;                               // the latest Health
}
```

- `now` is always passed in; `election` never reads a clock.
- Tests use synthetic `Instant`s.
- The runtime sends `TimerFired` when `next_deadline()` passes, and the machine handles every timer that has expired.

### 5.5 Transitions

Rows are checked from top to bottom, and the first row that matches wins. "Mine" means this node's current effective priority. "Preemption on" means `preempt = true` and preemption is not suspended (below).

| State | Event | Actions, then the new state |
|---|---|---|
| (new) | `Started`, health OK | `RunHook(Backup)`, arm down → **Backup** |
| (new) | `Started`, health fault | `RunHook(Fault)` → **Fault** |
| Backup | Heartbeat, priority 0 | down = `skew` |
| Backup | Heartbeat, priority ≥ mine, or preemption off | Learn the interval and reset down |
| Backup | Heartbeat, priority < mine, with preemption on | Ignore it; down is not reset |
| Backup | down expires | `SendHeartbeat(mine)`, `AttachVips`, `RunHook(Master)`, arm advert and reannounce, end any preemption suspension → **Master** |
| Master | advert expires | `SendHeartbeat(mine)` and re-arm advert |
| Master | reannounce expires | `Announce` |
| Master | Heartbeat, priority 0 | `SendHeartbeat(mine)` and re-arm advert |
| Master | Heartbeat from a higher node (§5.3) | `DetachVips`, `RunHook(Backup)`, arm down → **Backup** |
| Master | Heartbeat from a lower node (split brain) | `SendHeartbeat(mine)`, `Announce`, re-arm advert |
| Master | `AttachFailed` | `SendHeartbeat(0)`, `DetachVips`, `RunHook(Fault)`, arm hold-down, suspend preemption → **Fault** |
| Backup or Fault | `AttachFailed` | Ignore it. It is a late report for a role this node has already left. |
| Backup or Master | `HealthChanged`, fault | If Master: `SendHeartbeat(0)` and `DetachVips`. Then `RunHook(Fault)` → **Fault** |
| Backup or Master | `HealthChanged`, no fault | Store the new effective priority. It is used from the next heartbeat or down reset. |
| Fault | `HealthChanged`, no fault, hold-down not running | `RunHook(Backup)`, arm down → **Backup** |
| Fault | `HealthChanged`, any other case | Store the new health and stay in **Fault** |
| Fault | hold-down expires and health is OK | `RunHook(Backup)`, arm down → **Backup** |
| Fault | hold-down expires and health is in fault | Stay in **Fault** until a `HealthChanged` with no fault arrives |
| Fault | Heartbeat | Ignore it |
| any | `Shutdown` | If Master: `SendHeartbeat(0)` and `DetachVips`. Then `RunHook(Stop)`. |

- **Preemption suspension.** After a failed attach, the node stops preempting until it next becomes master on its own (its down timer runs out) or restarts. Until then a lower master's heartbeats keep it a backup, as with `preempt = false`. Otherwise a node that can never attach would take the VIP from a healthy master after every hold-down.

## 6. Wire protocol (`proto`)

### 6.1 Transport

- UDP over IPv4.
- Each node binds to its configured `bind` address and port. There is no default port: `bind` and every entry in `peers` name one, and the example config uses **8458**.
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
| 10–11 | `interval_ms` (u16) | 50–60000 (§6.3) |
| 12–15 | `vip_fingerprint` (u32) | First 4 bytes of SHA-256 over the sorted `"ip/prefix"` strings joined with `,`. Interface names are excluded. |
| 16–23 | `boot_id` (u64) | The Unix time in milliseconds at start-up, shifted left 16 bits, plus 16 random bits. Two starts in the same millisecond (a host without a real-time clock) still get different values. |
| 24–31 | `seq` (u64) | Starts at 1 and goes up by 1 with every packet sent, goodbyes included |
| 32–63 | HMAC-SHA256 | Keyed with `auth_key` (UTF-8 bytes), computed over bytes 0–31 |

### 6.3 Receive validation

The checks run in this order, and a packet that fails any of them is dropped.

1. The source IP is one of the `peers` IPs. The source port is ignored. Checking this first means a stranger's packets are never hashed.
2. The length is 64, and the magic, version and kind match.
3. The HMAC is valid. Verification is constant-time.
4. `interval_ms` is within 50–60000, the range `advert_interval_ms` has in the config. A zero interval would make replay rule 4 accept everything and give the election a zero-length down timer.
5. `group_id` equals ours.
6. The replay check passes (§6.4).
7. If `interval_ms` or `vip_fingerprint` differs from ours, the packet is **still accepted**, but a warning is logged.

- Steps 2–4 are one `Codec::decode` call.
- A dropped packet counts as silence, exactly as in VRRP.
- Warnings are rate-limited to one per (peer, reason) per 60 s. Packets from addresses not in `peers` share one warning slot, so spoofed source addresses can neither grow the limiter nor flood the log.

### 6.4 Replay protection

For each peer, keyed by source IP, the receiver remembers:
- the last packet it accepted: its `boot_id`, `seq` and `interval_ms`, and when it arrived;
- the newest run it ever accepted: the highest `boot_id`, and the highest `seq` accepted with it.

A new packet is accepted if any of these holds:

1. Nothing has been accepted from this peer yet.
2. The `boot_id` is the newest run's and `seq` is greater than that run's highest `seq`, or the `boot_id` is the last packet's and `seq` is greater than that packet's.
3. The `boot_id` is greater than the newest run's, meaning the peer restarted.
4. The `boot_id` is another, older one, and the peer has been silent for at least `3 × its last interval_ms`. This covers a peer that restarted after its clock moved backwards.

So within the newest run, and within the last run accepted, only a higher `seq` is accepted, however long the peer has been silent.

Known limit: once one packet of an older run is accepted, the rest of that run is too, and a node that has just started accepts any run. So whoever captured a stretch of a peer's heartbeats, and can send from its IP, can replay it while that peer is silent, and keep this node from becoming master for as long as the capture lasts. Closing this needs a boot counter that survives restarts. Also, a VM restored from a snapshot is ignored until its `seq` passes the old one, so restart vipd there.

## 7. VIP backends (`vip`)

### 7.1 Trait

```rust
pub struct Vip { pub ip: Ipv4Addr, pub prefix: u8, pub interface: String }

pub trait VipBackend: Send + Sync + 'static {
    async fn interface_exists(&self, iface: &str) -> anyhow::Result<bool>;
    /// Some("ip/prefix") as currently configured on the interface, or None.
    async fn find(&self, vip: &Vip) -> anyhow::Result<Option<String>>;
    async fn attach(&self, vip: &Vip) -> anyhow::Result<()>;               // only called when find() is None
    async fn detach(&self, vip: &Vip, found: &str) -> anyhow::Result<()>;  // found: what find() returned
    async fn announce(&self, vip: &Vip) -> anyhow::Result<()>;             // no-op where the OS announces
}
```

- **`VipManager`** wraps the backend and makes attach and detach idempotent in one place, for every backend and override. It runs `find` first: it attaches only if `find` returned `None`, and detaches only if it returned `Some(found)`, passing `found` on, so Linux deletes exactly the `ip/prefix` it found. It also applies the `[vip_commands]` overrides (§7.4).
- The trait uses native async functions in traits; there is no `async_trait`. Each method is declared as `fn … -> impl Future<Output = …> + Send`, and the backends implement them with `async fn`.
- The runtime is generic over `B: VipBackend`, so tests can use `vip::fake::FakeBackend`, which records every call and can be told to fail.
- Every OS command runs through `tokio::process::Command` (§7.4) with a **10 s timeout**. A command that times out is killed and counted as a failure.

### 7.2 Linux backend

| Operation | Command |
|---|---|
| find | `ip -o -4 addr show dev {iface}`. Look for the token after `inet` whose address part equals `{ip}`, and return the whole `ip/prefix` token. |
| attach | `ip addr add {ip}/{prefix} dev {iface}` |
| detach | `ip addr del {found} dev {iface}`, using the prefix that `find` actually returned |
| announce | Native gratuitous ARP (`garp.rs`), described below |

How the gratuitous ARP is sent:
- A `socket(AF_PACKET, SOCK_RAW | SOCK_CLOEXEC, htons(ETH_P_ARP))`, with the interface index from `if_nametoindex`. Close-on-exec keeps checks and hooks started meanwhile from inheriting it.
- The interface's MAC address comes from `/sys/class/net/{iface}/address`.
- The frame:
  - Ethernet: destination `ff:ff:ff:ff:ff:ff`, source our MAC, ethertype `0x0806`.
  - ARP: request, sender = our MAC and the VIP, target MAC zero, target IP = the VIP.
- Five frames are sent back-to-back.
- Building the frame and parsing the MAC address are portable, and tested on any OS. Only `send` is Linux-only.
- Sending needs root or `CAP_NET_RAW`. The `ip addr` commands need root or `CAP_NET_ADMIN`.

### 7.3 Windows backend

| Operation | Command |
|---|---|
| find | `netsh interface ipv4 show ipaddresses "{iface}"`. Look for a whitespace-separated token equal to `{ip}`, which works in any language. |
| attach | `netsh interface ipv4 add address "{iface}" {ip} {mask} store=active skipassource=true`, then the duplicate-address check below |
| detach | `netsh interface ipv4 delete address "{iface}" {ip} store=active` |
| announce | No-op. Windows announces a new address itself; this is to be verified (§15). |

- **`store=active`** means the VIP disappears when the machine reboots.
- **`skipassource=true`** keeps outgoing traffic on the node's own IP.
- **Duplicate-address check after attach.** Run one PowerShell process by full path, because the service runs as LocalSystem and a PATH lookup could pick up a planted `powershell.exe`: `%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe -NoProfile -NonInteractive -Command <script>`.
  - The script polls the `AddressState` of the VIP on its own adapter every 250 ms for up to 3 s, until the state is no longer `Tentative`, and prints the final state. The state names are enum values, so they don't depend on the Windows language.
  - It runs `Get-NetIPAddress -IPAddress '{ip}'` and keeps only the address whose `InterfaceAlias` equals `$env:VIPD_IFACE` exactly (`-eq`, no wildcards), because another adapter could hold the same IP.
  - The adapter name is not part of the script: it is passed in the `VIPD_IFACE` environment variable and compared with `-eq $env:VIPD_IFACE`, so no character in it can change the script. The script contains no double quotes, so Windows command-line quoting cannot change it either.
  - `Preferred`: done.
  - `Duplicate`: detach, wait 1 s and retry the attach, up to 3 attempts. After that, return an error, which the runtime turns into `AttachFailed`.
  - Nothing printed: netsh is asked again. If it does not list the VIP either, the address is not on the adapter: return an error, which the runtime turns into `AttachFailed` (config validation rejects an adapter index, which `netsh` accepts but `InterfaceAlias` never equals). If netsh does list it, the check failed inside PowerShell: keep the address, with the warning "the duplicate-address check did not see the VIP, but netsh does; keeping it".
  - The check cannot run (PowerShell fails to start, exits with an error or times out): keep the address, with the warning "cannot check the VIP for a duplicate address; keeping it". Windows still runs its own duplicate detection, so a broken PowerShell must not stop a node from ever holding the VIP.
  - Any other state, such as still `Tentative` after 3 s: keep the address, with a warning.
- **Error hint.** When `netsh … add address` fails, the error adds a hint: if the adapter uses DHCP, run `netsh interface ipv4 set interface interface="<adapter>" dhcpstaticipcoexistence=enabled` once, or give it a static IP.

### 7.4 Command overrides and templates

`[vip_commands]` can replace `attach` and/or `detach`. `find` and `announce` stay built in. `config` parses them into `vip::CommandOverrides`, which `VipManager` applies; keeping the type in `vip` avoids a `config` ↔ `vip` dependency cycle.

- **Tokenizer** (`exec.rs`), used for overrides, checks and hooks:
  - Arguments are split on whitespace.
  - A `"double-quoted"` segment is one argument, with the quotes removed. There are no escape sequences.
  - The first token is the program.
  - There is no shell.
- **Running a command** (`exec.rs`). VIP commands, overrides, checks and hooks all run the same way: without a shell, with stdin closed and the output captured, and with vipd's own environment (hooks get their variables on top, §9, and the duplicate-address check gets `VIPD_IFACE`).
  - On Linux every command runs in its own process group, and the whole group is killed on timeout or when the run is cancelled (for example at shutdown), so the children of an `sh -c` wrapper die too. A descendant that moves to a group or session of its own (`setsid`) still escapes.
  - On Windows only the direct process is killed; the rest would need a Job Object.
- **Placeholders** are `{ip}`, `{prefix}`, `{mask}` and `{iface}`. They are substituted inside each token *after* splitting, so a value containing a space (such as `Ethernet 2`) stays a single argument.
- **Overrides stay idempotent.** `find` still runs first, so an override runs only when a change is actually needed. For an override, exit code 0 means success.
- **The Windows duplicate-address check (§7.3) runs only after the built-in attach.** An override may do more than the built-in command, such as also calling a cloud API, but it must still add or remove `{ip}` on `{iface}`: the built-in `find` decides whether an override runs at all, so a detach override never runs for an address `find` cannot see.

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

- **Pass and fail.** Exit code 0 is a pass. Any other exit code is a fail, and so is failing to start, or being killed when `timeout_ms` is reached (on Linux with its whole process group, §7.4).
- **Overlapping runs.** If the previous run of a check is still going, the new run is skipped.
- **Shell features** are available by writing `sh -c "…"` or `cmd /C "…"` as the command. The §7.4 tokenizer groups words only with double quotes, so in TOML write it as a literal string: `command = 'sh -c "…"'`.
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
- **When.** A hook runs whenever the machine emits `RunHook` (§5.5): on entering a state, and on a stop.
  - At start, a node runs `on_backup`, or `on_fault` if its health is in fault. So after a crash as master, the hooks learn that this node is no longer master.
  - `on_master` starts as the node takes over, at the same time as the VIP worker's attach (§11.3), so the VIP may not be on the interface yet. On Windows the attach takes a few seconds, because it waits for the duplicate-address check (§7.3).
  - Hooks are not serialized: after quick state changes two can run at once and finish in either order. A hook should act on its `VIPD_STATE`, not on the order the hooks ran in.
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
| `auth_key` | required | ≥ 16 characters, no leading or trailing whitespace, not the example key from `examples/vipd.toml`; same on all nodes |
| `bind` | required | `ip:port`. The IPv4 address must not be `0.0.0.0`, because it is needed for tie-breaks, and the port must not be 0. |
| `peers` | required | at least one `ip:port`; unique IPs; must not contain the `bind` IP; port not 0 |
| `log_level` | `"info"` | `trace`, `debug`, `info`, `warn` or `error`; `RUST_LOG` overrides it |
| `log_dir` | `C:\ProgramData\vipd\logs` | an absolute path; used by the Windows service only (§13) |
| `[[vip]]` | at least one | `ip` (IPv4, required; a unique unicast address that is not the `bind` or a peer IP), `prefix` (1–32, default 32, the safe choice; see below), `interface` (required, must exist; on Linux also a valid interface name; on Windows the adapter's name, not its index, without surrounding whitespace) |
| `[[check]]` | none | see §8 |
| `[hooks]` | none | see §9 |
| `[vip_commands]` | none | see §7.4 |

How "the interface exists" is checked:
- **Linux:** `/sys/class/net/{iface}` exists.
- **Windows:** `netsh interface ipv4 show interfaces "{iface}"` exits with code 0.
- `vipd check-config` reports a missing interface, or a `bind` address this machine does not have, as an invalid config (exit code 2). `vipd run` checks the interfaces at start-up (§11.1) and exits with code 1.

**VIP prefix.** Keep the default `/32`, which is the safe choice. On Linux, a VIP with the same prefix length as the interface's own address (say `/24`) becomes a secondary address, and the kernel deletes it whenever the primary address is removed, for example by a DHCP change. A subnet prefix needs `net.ipv4.conf.<interface>.promote_secondaries=1` as well.

Example (`examples/vipd.toml`):

```toml
# vipd example configuration (see docs/superpowers/specs/2026-10-04-vipd-design.md §10).
# Linux: /etc/vipd/vipd.toml    Windows: C:\ProgramData\vipd\vipd.toml

node_name = "web-a"                 # appears in the start-up log line
group_id = 51                       # same on every node
priority = 150                      # 1-254, higher wins
preempt = true                      # a returning higher node takes the VIP back
advert_interval_ms = 1000
auth_key = "a-long-random-shared-secret"   # replace with your own random secret; same on every node, 16+ characters

bind = "192.168.1.13:8458"          # this node's IP (not 0.0.0.0) and UDP port
peers = ["192.168.1.14:8458"]       # every other node

[[vip]]
ip = "192.168.1.200"
prefix = 32                         # the default; see "VIP prefix" in the README before using /24
interface = "eth0"                  # on Windows the adapter name, e.g. "Ethernet"

[[check]]
name = "nginx"
command = "curl -sf -o /dev/null http://127.0.0.1/"
fall = 2
rise = 2
weight = -60                        # 150 -> 90 while failing, so a priority-100 node takes over

[hooks]
on_master = "/usr/local/bin/vip-alert.sh master"
```

## 11. Runtime (`runtime`)

### 11.1 Start-up sequence

1. Load and validate the config. On error, exit with code **2**.
2. Check that every VIP's interface exists (§10). If one does not, step 3 still runs, and then vipd exits with code 1 and this error. Otherwise one missing interface would leave the VIPs that a crash left on the other interfaces next to the peer's, while the service manager restarts vipd again and again.
3. Run `detach` for every configured VIP, cleaning up after a crash. A VIP that cannot be detached does not stop the others from being tried. If any fails, exit with code 1, because the node is not safe to run.
4. Bind the UDP socket. If this fails, exit with code 1. Binding only after step 3 means that a socket that cannot be bound, because the port is taken or the `bind` IP is gone, does not skip the cleanup.
5. Run the first round of checks and compute `Health`.
6. Create the `Machine` and feed it `Started { health }`.
7. Enter the event loop.

A stop during start-up never cuts step 3 short, because the peer may already hold the VIP:
- A stop during the interface check skips the rest of that check. Step 3 still runs to the end, and then vipd exits.
- A stop during step 3 lets it finish, and then vipd exits.
- A stop during the first round of checks exits at once; nothing is held yet.

### 11.2 Event loop

A single `tokio::select!` loop over:
- UDP receive, which is decoded, validated and sent to the machine as `Heartbeat`;
- `sleep_until(next_deadline)`, which sends `TimerFired`;
- the check-result channel, which updates check state and sends `HealthChanged` on a change;
- the VIP worker's event channel, which sends `AttachFailed`. If the channel closes, the worker has died (it stops early only if it panics). No VIP can move without it, so `run` returns an error (exit code 1); the service manager restarts vipd, and start-up removes any leftover VIP;
- the stop signal, which sends `Shutdown`.

The loop carries out the resulting actions as follows.

- **`SendHeartbeat`** is built and sent immediately with `send_to` to each peer, inside the loop. It never waits on a command.
- **`AttachVips`, `DetachVips` and `Announce`** are handed to the VIP worker. The loop never waits for an OS command, so heartbeats keep flowing while slow commands run (for example Windows' 3 s duplicate-address check).
- **`RunHook`** is spawned as a background task.

### 11.3 VIP worker (`vip_worker.rs`)

A single task that owns the backend and processes requests in order, skipping those that a later request supersedes.

- It holds a **desired state**: attached or detached.
- **`AttachVips`** sets the desired state to *attached*, then runs `attach` and `announce` for each VIP. If any attach fails, it reports `AttachFailed`; the machine then emits `DetachVips`.
  - Each attach request carries a number, one higher for every `AttachVips`, and `AttachFailed` carries the number of the attach that failed. The runtime passes the failure to the machine only if it belongs to the newest attach. An older one belongs to a master term the node has since left, and the attach queued after it may still succeed, so it must not fault the current term.
  - On Linux, `announce` sends the first burst of gratuitous ARPs.
- **`DetachVips`** sets the desired state to *detached* and runs `detach` for each VIP.
  - If detach fails, it retries every 2 s, logging an error each time, until it succeeds or the desired state changes.
- **`Announce`** runs `announce` for each VIP only if the desired state is *attached*.
- **Coalescing.** Before it runs an attach, detach or announce request, the worker takes every request already waiting. If a later attach or detach is among them, it skips the current one: that later request supersedes an attach or detach, and makes an announce moot. So a backend slower than the election (Windows' 3 s duplicate-address check, or slow overrides) does not replay a backlog of past terms, which would leave the VIP trailing the election by seconds and make a stop wait for the whole backlog. The shutdown flush (§11.4) is never skipped.

### 11.4 Shutdown

1. Stop the check loops, feed `Shutdown` to the machine and carry out the resulting actions.
2. Wait up to 15 s for the VIP worker to finish its queue, including the retries of a failed detach.
3. Wait up to 5 s for `on_stop`. A hook still running when vipd exits is killed.
4. Exit with code 0. If the VIP worker did not finish in time, or had stopped, exit with code 1 instead, because a VIP may still be attached. (The Windows service still reports 0 for a requested stop, §13.)

## 12. CLI

| Command | Purpose |
|---|---|
| `vipd run [--config PATH]` | Run in the foreground. SIGTERM, SIGHUP, SIGINT and Ctrl+C trigger a graceful stop; a SIGHUP that is already ignored at start (`nohup`) stays ignored. |
| `vipd check-config [--config PATH]` | Validate the config, including that every VIP's interface exists and that `bind` is an address of this machine, and exit with 0 (valid) or 2 (invalid) |
| `vipd service install [--config PATH]` | Windows only. Refuses an invalid config. Registers the service `vipd`, which runs this `vipd.exe` with the config's absolute path: automatic start, LocalSystem, a 25 s pre-shutdown timeout (§13), and restart on failure after 5 s, then 5 s, then every 60 s. Error exits count as failures, not only crashes. The failure count resets after a day without failures. |
| `vipd service uninstall` | Windows only. Stops the service, waiting up to 60 s, then deletes it, so an install right after works. |
| `vipd service run --config PATH` | Windows only. The entry point the Service Control Manager calls; users don't run it. |

On Linux, the `service` subcommands print an error that points to `packaging/vipd.service`.

## 13. Service integration and logging

- **Linux** (`packaging/vipd.service`):
  - Runs `ExecStart=/usr/local/bin/vipd run --config /etc/vipd/vipd.toml` as root.
  - `Restart=on-failure`, `RestartSec=2` and `RestartPreventExitStatus=2`, because an invalid config does not fix itself.
  - `After=network-online.target` and `Wants=network-online.target`.
  - `WantedBy=multi-user.target`.
  - Logs go to stdout, where journald collects them.
- **Windows:**
  - A service stop triggers the same graceful shutdown (§11.4). So does a system shutdown or reboot: vipd accepts the PRESHUTDOWN notification, with a 25 s pre-shutdown timeout, so it hands the VIP over before other services stop.
  - Exit codes reported to the SCM: a stop the operator or Windows asked for reports 0, even if VIP cleanup failed, because any other code would make the SCM restart the service it was just told to stop. Otherwise an invalid config reports 2 and any other failure 1, as on the command line.
  - Logs go to `{log_dir}/vipd.YYYY-MM-DD.log` through `tracing-appender`: a new file each day (UTC), the last 14 kept, written synchronously so the last lines are on disk when the SCM ends the process.
  - `log_dir` defaults to `C:\ProgramData\vipd\logs`. It is a directory of its own because the 14-file limit prunes every `vipd*.log` file in it. If the config cannot be loaded, or `log_dir` cannot be used, logs go to the default directory.
  - In a console, logs go to stdout. Ctrl+C stops `vipd.exe run` gracefully, but closing the console window kills it, and the VIP stays on the node until vipd starts again.

## 14. Error handling

| Situation | Behaviour |
|---|---|
| A packet is malformed, forged, from the wrong group or replayed | Dropped, with a rate-limited warning. Never a crash. |
| A VIP command hangs | Killed after 10 s and counted as a failure |
| Attach fails | The machine goes to Fault with a 10 s hold-down, and stops preempting until it next becomes master on its own or restarts (§5.5) |
| Detach fails | Retried every 2 s, with loud errors, until it succeeds (§11.3) |
| The process panics or crashes, or the VIP worker dies (§11.2) | systemd or the SCM restarts it, and the start-up cleanup (§11.1 step 3) removes any leftover VIP |
| A stop cannot remove the VIPs within 15 s | Logged, and exit code 1 (§11.4); the Windows service still reports 0. The VIP may still be attached, and nothing restarts vipd after a requested stop. |
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
7. Failover is graceful when Windows shuts down or reboots (PRESHUTDOWN): the peer takes over at once, and no VIP is left behind.
8. An error exit restarts the service, both right after `service install` and after a reboot.
9. The duplicate-address check runs, with no PowerShell quoting problems, and reports `Preferred` after an attach.
10. `service uninstall` followed at once by `service install` works.
11. `check-config` rejects an adapter index given as `interface`, and accepts the adapter's name.
12. Closing the console window of a foreground `vipd.exe run` leaves the VIP until vipd starts again, and start-up then removes it.
13. The install steps from the README work: `icacls` leaves the config readable only by SYSTEM and Administrators, and the service starts from `C:\Program Files\vipd`.

## 16. Testing

1. **`election`** (unit tests with synthetic `Instant`s):
   - the first election;
   - takeover after the master goes silent, at `3 × interval + skew`;
   - takeover after a goodbye, at `skew`;
   - preemption with `preempt` on and off;
   - the equal-priority IP tie-break;
   - split-brain healing;
   - weights and Fault, with entry and exit;
   - the attach-failure hold-down, and the preemption suspension that follows it;
   - shutdown from each state.
2. **`proto`:**
   - an encode/decode round trip;
   - tampered bytes, a wrong key, a bad length, magic, version or kind, and an `interval_ms` outside 50–60000 are all rejected;
   - all four replay rules;
   - the fingerprint is the same regardless of VIP order.
3. **`checks`:**
   - the rise/fall state machine;
   - aggregation and clamping;
   - the runner kills a check that times out (using `sleep`) and counts it as a fail.
4. **`exec` and `vip`:**
   - the command tokenizer, and a timeout killing the command's whole process group (Linux);
   - command construction and `find` parsing for **both** OSes, as pure functions that run on any OS;
   - an opt-in (`#[ignore]`) Linux test that attaches and detaches a VIP on a temporary dummy interface, and needs root.
5. **`tests/multi_node.rs`.** In-process nodes (up to three) on loopback addresses, with `FakeBackend` and 50 ms intervals. The tests:
   - stop the master, and exactly one new master appears;
   - restart the old master, and preemption happens;
   - drop traffic between nodes, and split brain forms and then heals.

   Each node needs a distinct tie-break IP, so each node gets its own `127.0.0.x` address. These loopback addresses work on Linux.
6. **Windows build.** `cargo check --target x86_64-pc-windows-gnu`. This needs the Windows target's standard library installed through rustup, so ask the user before installing it. After that, run the §15 checklist on a real machine.
