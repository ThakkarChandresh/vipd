# vipd

vipd keeps a virtual IP (VIP) on one healthy machine of a group, on **Linux and Windows**.

It works like keepalived. The nodes elect a master using VRRP's rules: priorities, timers, preemption and a priority-0 goodbye. Only the master holds the VIP. Two things differ from keepalived:

- The heartbeats are signed UDP packets sent directly between the nodes.
- The VIP is attached with each OS's own commands: `ip addr` on Linux, `netsh` on Windows.

The design is in [docs/superpowers/specs/2026-10-04-vipd-design.md](docs/superpowers/specs/2026-10-04-vipd-design.md).

## Build

```bash
cargo build --release
```

The binary is `target/release/vipd`, or `vipd.exe` on Windows.

## Configure

Copy [examples/vipd.toml](examples/vipd.toml) to every node.

- **Change on each node:** `node_name`, `priority`, `bind`, `peers` and the VIP's `interface`.
- **Keep identical on all nodes:** `group_id`, `auth_key`, `advert_interval_ms` and the VIP list.
- **Firewall:** allow UDP port 8458 (or your `bind` port) between the nodes.
- **VIP prefix:** keep the default `/32`. On Linux, a VIP with the same prefix length as the interface's own address (say `/24`) becomes a secondary address, and the kernel deletes it whenever the primary address is removed, for example by a DHCP change. If you need the subnet prefix, also set `net.ipv4.conf.<interface>.promote_secondaries=1`.

To check a config without starting anything:

```bash
vipd check-config --config /etc/vipd/vipd.toml
```

It lists the problems it finds and exits with 0 if the config is valid, or 2 if not.

### Checks and hooks

- A check is any command, and exit code 0 passes. Its `weight` adjusts the node's priority: a negative weight applies while the check fails, a positive one while it passes, and `weight = 0` takes the node out of the election while the check fails.
- To see why a check fails, set `log_level = "debug"` (or run with `RUST_LOG=vipd=debug`).
- Hooks get `VIPD_STATE` (`MASTER`, `BACKUP`, `FAULT` or `STOP`), `VIPD_PRIORITY` and `VIPD_GROUP`. They run in the background and are not serialized, so after quick changes two can run at once: act on `VIPD_STATE`, not on the order they arrive in.

## Run on Linux

vipd needs root to add the VIP and send gratuitous ARPs, and it uses the `ip` command from iproute2. To run it as another user, grant `CAP_NET_ADMIN` and `CAP_NET_RAW` as ambient capabilities (systemd's `AmbientCapabilities=`), so the `ip` commands it runs get them too.

```bash
sudo install -m 755 target/release/vipd /usr/local/bin/vipd
sudo install -D -m 600 examples/vipd.toml /etc/vipd/vipd.toml   # then edit it
sudo install -m 644 packaging/vipd.service /etc/systemd/system/vipd.service
sudo systemctl daemon-reload
sudo systemctl enable --now vipd
journalctl -u vipd -f
```

## Run on Windows

In an Administrator PowerShell:

```powershell
New-Item -ItemType Directory -Force C:\ProgramData\vipd
Copy-Item examples\vipd.toml C:\ProgramData\vipd\vipd.toml   # then edit it
.\vipd.exe service install --config C:\ProgramData\vipd\vipd.toml
sc.exe start vipd
```

- **Logs:** `C:\ProgramData\vipd\logs\vipd.YYYY-MM-DD.log`, one file per day (UTC). vipd keeps the newest 14 `vipd*.log` files in that directory and deletes older ones, so keep other files out of it. Set `log_dir` to move it, preferably to a local disk.
- **Restarts:** after a failure, Windows restarts vipd after 5 s, then 5 s, then every minute. To stop a restart loop, run `sc.exe config vipd start= disabled`.
- **Errors:** a failed vipd leaves a Service Control Manager event (ID 7024) in the System log with its exit code as a "service-specific error". Event Viewer words that code as an unrelated Windows message: "Incorrect function." means 1, a runtime failure, and "The system cannot find the file specified." means 2, an invalid config. The vipd log has the details.
- **Shutdown:** when Windows shuts down, vipd hands the VIP over before other services stop.

If the adapter gets its IP from DHCP, either give it a static IP or run this once:

```powershell
netsh interface ipv4 set interface interface="Ethernet" dhcpstaticipcoexistence=enabled
```

To remove the service, run `vipd.exe service uninstall`.

## Good to know

- **Why did a failover happen?** Every `state changed` log line includes its cause, and `health changed` lines show the node's effective priority.
- **A node that cannot attach the VIP** (missing privileges, or a DHCP adapter on Windows) gives it up, and stops preempting until it next becomes master on its own. After fixing the cause, restart vipd on that node.
- **On Windows,** the warning "cannot check the VIP for a duplicate address; keeping it" means the PowerShell check could not run. Windows still runs its own duplicate detection.
- **Moving a VIP to another interface:** remove it from the old interface yourself. vipd only cleans up the interface named in its config.
- **Outgoing traffic** from the master keeps using the node's own address, unless an application binds to the VIP.
- **Exit codes:** 0 for a clean stop, 1 for a runtime failure (including VIPs that could not be removed at shutdown), 2 for an invalid config. systemd and the Windows service restart vipd after an error exit, and start-up removes any VIP left behind. In a terminal, start vipd again yourself.

## Security and limits

- Heartbeats are signed with `auth_key`, so they cannot be forged. They can be replayed, though: someone who records a node's heartbeats and can send from its IP address can play them back while that node is down, and keep the other nodes from taking over for as long as the recording lasts. Keep the heartbeat network trusted, for example on a dedicated VLAN.
- After restoring a node's VM from a snapshot, restart vipd on it. Until then the other nodes ignore its heartbeats as old.

## Test

```bash
cargo test
```

One test needs root, because it adds an address to a temporary dummy interface, so it is skipped by default. To run it, build the tests, then run the test binary that `--no-run` prints:

```bash
cargo test --lib --no-run
sudo target/debug/deps/vipd-<hash> vip::linux --ignored
```
