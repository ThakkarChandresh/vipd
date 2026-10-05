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

To check a config without starting anything:

```bash
vipd check-config --config /etc/vipd/vipd.toml
```

## Run on Linux

vipd needs root, or `CAP_NET_ADMIN` plus `CAP_NET_RAW`, to add the VIP and send gratuitous ARPs.

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

Logs go to `C:\ProgramData\vipd\vipd.log`.

If the adapter gets its IP from DHCP, either give it a static IP or run this once:

```powershell
netsh interface ipv4 set interface interface="Ethernet" dhcpstaticipcoexistence=enabled
```

To remove the service, run `vipd.exe service uninstall`.

## Test

```bash
cargo test
```
