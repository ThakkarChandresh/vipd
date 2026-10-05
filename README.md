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
- **Replace the example `auth_key`** with your own random secret, for example from `openssl rand -base64 32`. Anyone who knows it can forge heartbeats.
- **Firewall:** allow UDP port 8458 (or your `bind` port) between the nodes.
- **VIP prefix:** keep the default `/32`. On Linux, a VIP with the same prefix length as the interface's own address (say `/24`) becomes a secondary address, and the kernel deletes it whenever the primary address is removed, for example by a DHCP change. If you need the subnet prefix, also set `net.ipv4.conf.<interface>.promote_secondaries=1`.
- **Windows paths:** write them in single quotes, which TOML reads literally: `log_dir = 'D:\vipd\logs'`. In double quotes, `\t`, `\n` and the like silently become control characters. The same goes for check and hook commands.
- **Config changes** take effect when vipd restarts (`sudo systemctl restart vipd`, or `Restart-Service vipd` on Windows); there is no reload. SIGHUP stops vipd cleanly, like SIGTERM.

To check a config without starting anything (it has to read the file, hence `sudo`):

```bash
sudo vipd check-config --config /etc/vipd/vipd.toml
```

It lists the problems it finds and exits with 0 if the config is valid, or 2 if not.

### Checks and hooks

- A check is any command, run without a shell, and exit code 0 passes. Only double quotes group words. For pipes, `&&` or variables, call a shell and write the command as a TOML literal string: `command = 'sh -c "pgrep -x nginx && curl -sf http://127.0.0.1/"'` (on Windows, `'cmd /C "…"'`). Hooks follow the same rules.
- A check's `weight` (default 0) adjusts the node's priority: a negative weight applies while the check fails, a positive one while it passes, and `weight = 0` takes the node out of the election while the check fails. If a weight-0 check fails on every node, no node holds the VIP.
- To see why a check fails, set `log_level = "debug"` (or run with `RUST_LOG=vipd=debug`).
- Hooks get `VIPD_STATE` (`MASTER`, `BACKUP`, `FAULT` or `STOP`), `VIPD_PRIORITY` (the effective priority) and `VIPD_GROUP`. They run in the background and are killed after 60 s; at shutdown vipd waits at most 5 s for `on_stop`. Hooks are not serialized, and one that starts first can finish last: a failed attach starts `on_master` and `on_fault` milliseconds apart. So a hook that changes something should check the current state, such as whether the VIP is on the interface, rather than assume it ran last.

## Run on Linux

vipd needs root to add the VIP and send gratuitous ARPs, and it uses the `ip` command from iproute2. To run it as another user, grant `CAP_NET_ADMIN` and `CAP_NET_RAW` as ambient capabilities (systemd's `AmbientCapabilities=`), so the `ip` commands it runs get them too. That user must also be able to read the config, for example after `sudo chown root:vipd /etc/vipd/vipd.toml` and `sudo chmod 640 /etc/vipd/vipd.toml`.

```bash
sudo install -m 755 target/release/vipd /usr/local/bin/vipd
sudo install -D -m 600 examples/vipd.toml /etc/vipd/vipd.toml   # then edit it
sudo install -m 644 packaging/vipd.service /etc/systemd/system/vipd.service
sudo systemctl daemon-reload
sudo systemctl enable --now vipd
journalctl -u vipd -f
```

The unit restarts vipd 2 s after a runtime failure, but not after an invalid config (exit code 2): fix the config, then run `sudo systemctl restart vipd`.

## Run on Windows

In an Administrator PowerShell, from the repository folder:

```powershell
New-Item -ItemType Directory -Force 'C:\Program Files\vipd', C:\ProgramData\vipd
icacls C:\ProgramData\vipd /inheritance:r /grant:r '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F'
Copy-Item target\release\vipd.exe 'C:\Program Files\vipd\'
Copy-Item examples\vipd.toml C:\ProgramData\vipd\vipd.toml   # then edit it from this shell, e.g. with notepad
& 'C:\Program Files\vipd\vipd.exe' service install --config C:\ProgramData\vipd\vipd.toml
sc.exe start vipd
```

The service runs the `vipd.exe` it was installed from, so keep it in a folder only administrators can change, such as `C:\Program Files\vipd`. The `icacls` line lets only SYSTEM and Administrators read the config, which holds `auth_key`, and write the logs.

- **Logs:** `C:\ProgramData\vipd\logs\vipd.YYYY-MM-DD.log`, one file per day (UTC). vipd keeps the newest 14 `vipd*.log` files in that directory and deletes older ones, so keep other files out of it. Set `log_dir` to move it, preferably to a local disk.
- **Restarts:** after a failure, including an invalid config, Windows restarts vipd after 5 s, then 5 s, then every minute. To stop a restart loop, run `sc.exe config vipd start= disabled`; after fixing the cause, run `sc.exe config vipd start= auto` and `sc.exe start vipd`.
- **Errors:** a failed vipd leaves a Service Control Manager event (ID 7024) in the System log with its exit code as a "service-specific error". Event Viewer words that code as an unrelated Windows message: "Incorrect function." means 1, a runtime failure, and "The system cannot find the file specified." means 2, an invalid config. The vipd log has the details.
- **Shutdown:** when Windows shuts down, vipd hands the VIP over before other services stop.

If the adapter gets its IP from DHCP, either give it a static IP or run this once:

```powershell
netsh interface ipv4 set interface interface="Ethernet" dhcpstaticipcoexistence=enabled
```

To remove the service, run `& 'C:\Program Files\vipd\vipd.exe' service uninstall`.

## Good to know

- **Why did a failover happen?** Every `state changed` log line includes its cause, and `health changed` lines show the node's effective priority.
- **A node that cannot attach the VIP** (missing privileges, or a DHCP adapter on Windows) gives it up, and stops preempting until it next becomes master on its own. After fixing the cause, restart vipd on that node.
- **On Windows,** the warning "cannot check the VIP for a duplicate address; keeping it" means the PowerShell check could not run. Windows still runs its own duplicate detection.
- **Moving a VIP to another interface:** remove it from the old interface yourself. vipd only cleans up the interface named in its config.
- **Outgoing traffic** from the master keeps using the node's own address, unless an application binds to the VIP.
- **Exit codes:** 0 for a clean stop, 1 for a runtime failure, 2 for an invalid config. After an error exit the Windows service restarts vipd, and so does systemd unless the config is invalid; start-up then removes any VIP left behind. In a terminal, start vipd again yourself.
- **If a stop logs "the VIPs were still not removed after 15 s",** the VIP may still be on that node, and since the stop was requested, nothing restarts vipd to clean up. Remove it yourself, for example with `sudo ip addr del 192.168.1.200/32 dev eth0` or `netsh interface ipv4 delete address "Ethernet" 192.168.1.200 store=active`.

## Security and limits

- Heartbeats are signed with `auth_key`, so they cannot be forged without the key. They can be replayed, though: someone who records a node's heartbeats and can send from its IP address can play them back while that node is down, and keep the other nodes from taking over for as long as the recording lasts. Keep the heartbeat network trusted, for example on a dedicated VLAN.
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
