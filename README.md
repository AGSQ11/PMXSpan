<p align="center"><img src="packaging/assets/pmxspan.png" width="96" alt="PMXSpan logo"></p>

<h1 align="center">PMXSpan</h1>

<p align="center"><b>One desktop app across all your Proxmox VE clusters.</b><br>Native on Windows and Linux · nothing to install on your nodes</p>

PMXSpan is a standalone desktop app for managing Proxmox VE clusters, written in Rust ([egui](https://github.com/emilk/egui)). It handles provisioning (auto-placement, create, rebuild, IP management, cloud-init, OS image catalog, bandwidth accounting, tenant isolation, diagnostics) in a native GUI that talks directly to the Proxmox VE API. It doesn't need a web server, a database or agents on the nodes.

<p align="center">
  <img src="screenshots/dashboard.png" alt="Dashboard in the Graphite (dark) theme" width="49%">
  <img src="screenshots/dashboard-light.png" alt="Dashboard in the Daylight (light) theme" width="49%">
</p>
<p align="center"><sub>Graphite (dark) and Daylight (light). Every screenshot in this README was taken against the bundled mock cluster. More in <a href="#screenshots">Screenshots</a>.</sub></p>

## Features

| Area | What you get |
|---|---|
| **Connections** | Multiple server profiles. You can log in with an API token or with a password ticket (TOTP 2FA supported, auto-renewed). TLS can verify against the OS trust store, **pin the certificate's SHA-256 fingerprint** (with a "fetch from server" button), or skip verification for labs. |
| **Secrets** | Tokens and passwords are kept in a local vault encrypted with Argon2id (64 MiB) + XChaCha20-Poly1305 under a master password. The vault locks itself after 30 idle minutes (configurable) or from the lock button. Config, vault and logs are private to your user, and logs are redacted before anything is written. |
| **Diagnostics** | Shows the Proxmox version, cluster/quorum, node status, which storages accept each content type (`images`, `rootdir`, `iso`, `vztmpl`, `backup`, `snippets`), and the credential's effective privileges on `/`. |
| **Look & feel** | Two themes, **Graphite** (dark) and **Daylight** (light), switched with the sun/moon button in the top bar or in Settings (or following the system). Sidebar with the node/guest tree, a global search for guests, VMIDs, tags and IP addresses, and cards throughout. Fonts and icons are bundled, so it looks the same on every machine. |
| **Dashboard** | Nodes, running guests, CPU and memory at a glance; a guest table with each guest's main IP; per-node usage, recent tasks and storage usage. |
| **Guests** | Sortable, filterable list with quick power actions, plus a navigation tree per node. |
| **Guest detail** | Summary (live status, agent/LXC IP addresses, traffic for the billing period, RRD graphs). Hardware (CPU/memory/options/tags/notes, disk resize/move/add/detach, ISO insert/eject, boot order, raw config). Network (bridge/VLAN/rate/firewall/MAC, LXC addressing). **IP addresses** (see below). Cloud-Init. Snapshots. Backups (back up/restore/protect/delete). Firewall (options, rules, reorder, log). Guest agent (OS/filesystems, set password, run commands). Task history. Manage (password reset, migrate, clone, **rebuild**, HA, pool, template, clear lock, destroy). |
| **Create wizard** | Builds VMs from a cloud-init template (full clone, then migrated if the template's disks are node-local), from an **OS catalog image**, or from an ISO, and containers from a `vztmpl` on storage or from the catalog. Auto-placement, IPAM, cloud-init user/password/SSH keys, tags and pools are all set from the wizard. |
| **Auto-placement** | Nodes without enough free RAM are skipped. Each remaining node's storage is filtered by content type and free space. The survivors are ordered by your RAM/CPU/Disk priority and picked *dense*, *sparse* or *random*. |
| **IPAM** | A local IPv4/IPv6 pool database with ranges, gateways, DNS, VLAN, bridge/server scoping and reservations. A *preferred* IP is assigned exactly or the operation fails; it's never silently replaced. Addresses are released when a guest is destroyed or provisioning fails. |
| **IP assignment** | Per guest, per interface: assign one or more IPv4/IPv6 addresses from a pool (or an exact one), release them, pick the **main** address of each family, add an interface together with its addresses, remove one (its addresses are released), and adopt addresses already in a guest's config. The main addresses go into the guest config (LXC `netN`, VM cloud-init `ipconfigN`, the pool's VLAN on the NIC); additional ones are configured inside the guest. The **anti-spoofing IP filter** keeps an `ipfilter-netN` ipset per interface in sync with the assigned addresses. |
| **OS image catalog** | Images for creating and reinstalling guests from a URL instead of a template: VM disk images (qcow2/raw/vmdk/ova, imported with `import-from`) and container templates. Catalogs import and export as JSON (`{"entries": [...]}` or an array with `id`, `label`, `virt`, `url`, `checksum`, `checksum_algorithm`, `cloud_user`, `ostype`, `nodes`, `config`/`tpl_data`, `image_format`). Images are downloaded to the per-server main or fallback storage with stall detection; permanent errors (404/403) fail at once, transient ones are retried, and a full or broken storage moves on to the next one. The downloaded file is always removed afterwards. |
| **Rebuild / reinstall** | Replaces a guest's disks with a fresh template **or catalog image** while keeping its VMID, name, resources, NICs (**MAC addresses**), IP addresses, firewall rules/ipsets and pool. A catalog image is downloaded and the firewall config read *before* anything is deleted, so a failed download leaves the guest untouched. If the firewall can't be fully restored, the guest is left stopped instead of starting unprotected. You confirm by typing the VMID. |
| **Storage** | Browse content, upload ISOs/templates with progress, download from a URL (with optional checksum), browse and download appliance templates, delete volumes. |
| **Datacenter** | Scheduled backup jobs (create/toggle/run now/delete), storage replication, HA resources and HA status. |
| **Access** | Users, API tokens (the secret is shown once), pools, ACLs, and a **tenant wizard** that creates a pool and a new `@pve` (or directory-realm) user with a role on `/pool/<pool>` only, so customers see nothing outside their pool. Existing accounts are never modified. |
| **Jobs** | Every multi-step operation runs in the background and logs each step and the exact error it stopped on, including the tail of the Proxmox task log. No silent failures. |
| **Consoles** | noVNC/xterm.js open in your browser, and SPICE opens in virt-viewer. |

### Not included yet

Per-guest **snapshot schedules** and **reverse DNS** (PTR) management through external DNS backends.

Stale create locks are cleared with **Manage → Clear lock**. It uses the API's `skiplock`, which Proxmox only honours for `root@pam`.

## Screenshots

### Overview

<table>
  <tr>
    <td width="50%" valign="top"><img src="screenshots/dashboard.png" alt="Dashboard"><br><sub><b>Dashboard</b>: nodes, running guests, CPU and memory, a guest table with main IPs, node usage and recent tasks.</sub></td>
    <td width="50%" valign="top"><img src="screenshots/search.png" alt="Global search"><br><sub><b>Search</b>: find guests by name, VMID, tag or IP address from anywhere.</sub></td>
  </tr>
  <tr>
    <td width="50%" valign="top"><img src="screenshots/guests.png" alt="Guest list"><br><sub><b>Guests</b>: sortable, filterable list with quick power actions.</sub></td>
    <td width="50%" valign="top"><img src="screenshots/jobs.png" alt="Jobs panel"><br><sub><b>Jobs</b>: every long operation runs in the background with a step-by-step log.</sub></td>
  </tr>
</table>

### Guests

<table>
  <tr>
    <td width="50%" valign="top"><img src="screenshots/vm-summary.png" alt="VM summary"><br><sub><b>Summary</b>: resources, IP addresses, traffic for the billing period and performance graphs.</sub></td>
    <td width="50%" valign="top"><img src="screenshots/vm-ip-addresses.png" alt="IP addresses tab"><br><sub><b>IP addresses</b>: several addresses per interface, main address per family, anti-spoofing IP filter.</sub></td>
  </tr>
  <tr>
    <td width="50%" valign="top"><img src="screenshots/vm-hardware.png" alt="Hardware tab"><br><sub><b>Hardware</b>: CPU, memory, options, disks, CD-ROM and boot order.</sub></td>
    <td width="50%" valign="top"><img src="screenshots/vm-network.png" alt="Network tab"><br><sub><b>Network</b>: interfaces (bridge, VLAN, rate limit, firewall, MAC).</sub></td>
  </tr>
  <tr>
    <td width="50%" valign="top"><img src="screenshots/vm-cloud-init.png" alt="Cloud-Init tab"><br><sub><b>Cloud-Init</b>: user, password, SSH keys, DNS and per-NIC addressing from IPAM.</sub></td>
    <td width="50%" valign="top"><img src="screenshots/vm-snapshots.png" alt="Snapshots tab"><br><sub><b>Snapshots</b>: take (optionally with RAM), roll back and delete.</sub></td>
  </tr>
  <tr>
    <td width="50%" valign="top"><img src="screenshots/vm-backups.png" alt="Backups tab"><br><sub><b>Backups</b>: back up now, restore, protect and delete.</sub></td>
    <td width="50%" valign="top"><img src="screenshots/vm-firewall.png" alt="Firewall tab"><br><sub><b>Firewall</b>: options, ordered rules and the firewall log.</sub></td>
  </tr>
  <tr>
    <td width="50%" valign="top"><img src="screenshots/vm-guest-agent.png" alt="Guest agent tab"><br><sub><b>Guest agent</b>: OS info, filesystems, set a user password, run commands.</sub></td>
    <td width="50%" valign="top"><img src="screenshots/vm-rebuild-from-catalog.png" alt="Manage tab"><br><sub><b>Manage</b>: password reset, migrate, clone, rebuild from a template or the OS catalog, HA and pool.</sub></td>
  </tr>
  <tr>
    <td width="50%" valign="top"><img src="screenshots/container.png" alt="Container summary"><br><sub><b>Containers</b> get the same views, with root disk and swap usage.</sub></td>
    <td width="50%" valign="top"><img src="screenshots/create-from-catalog.png" alt="Create wizard"><br><sub><b>Create</b>: VMs from a template, an ISO or an OS catalog image; containers from a template or the catalog.</sub></td>
  </tr>
</table>

### Datacenter

<table>
  <tr>
    <td width="50%" valign="top"><img src="screenshots/node.png" alt="Node view"><br><sub><b>Node</b>: status, versions, updates, power actions and graphs.</sub></td>
    <td width="50%" valign="top"><img src="screenshots/storage.png" alt="Storage view"><br><sub><b>Storage</b>: browse content, upload, download from a URL, appliance templates.</sub></td>
  </tr>
  <tr>
    <td width="50%" valign="top"><img src="screenshots/tasks.png" alt="Task log"><br><sub><b>Tasks</b>: the cluster task log with filters and each task's log.</sub></td>
    <td width="50%" valign="top"><img src="screenshots/backup-ha.png" alt="Backup jobs"><br><sub><b>Backup · Replication · HA</b>: scheduled backup jobs, replication and HA resources.</sub></td>
  </tr>
  <tr>
    <td width="50%" valign="top"><img src="screenshots/tenant-wizard.png" alt="Tenant wizard"><br><sub><b>Users · Pools · Tenants</b>: a pool and a user limited to it in one step; users, tokens, pools and ACLs.</sub></td>
    <td width="50%" valign="top"><img src="screenshots/ipam.png" alt="IP address management"><br><sub><b>IP address management</b>: pools, ranges, gateways, VLANs, reservations and every assignment.</sub></td>
  </tr>
</table>

### Setup

<table>
  <tr>
    <td width="50%" valign="top"><img src="screenshots/os-catalog.png" alt="OS image catalog"><br><sub><b>OS image catalog</b>: cloud images and container templates by URL, JSON import and export.</sub></td>
    <td width="50%" valign="top"><img src="screenshots/servers.png" alt="Server profile"><br><sub><b>Servers</b>: API token or password login with TOTP, certificate pinning, catalog download storages.</sub></td>
  </tr>
  <tr>
    <td width="50%" valign="top"><img src="screenshots/diagnostics.png" alt="Connection diagnostics"><br><sub><b>Diagnostics</b>: version, quorum, nodes, storage capabilities and effective privileges.</sub></td>
    <td width="50%" valign="top"><img src="screenshots/settings.png" alt="Settings"><br><sub><b>Settings</b>: theme, placement policy, defaults for new guests, download timeouts.</sub></td>
  </tr>
  <tr>
    <td width="50%" valign="top"><img src="screenshots/cluster-switcher-light.png" alt="Cluster switcher"><br><sub><b>Cluster switcher</b>: change server, open the web UI or disconnect (Daylight theme).</sub></td>
    <td width="50%" valign="top"><img src="screenshots/vm-summary-light.png" alt="VM summary in light theme"><br><sub><b>Daylight</b> theme on a VM page.</sub></td>
  </tr>
  <tr>
    <td width="50%" valign="top"><img src="screenshots/unlock.png" alt="Unlock screen"><br><sub><b>Vault</b>: credentials are encrypted with a master password (Argon2id + XChaCha20-Poly1305).</sub></td>
    <td width="50%" valign="top"><img src="screenshots/dashboard-light.png" alt="Dashboard in light theme"><br><sub><b>Daylight</b> theme on the Dashboard.</sub></td>
  </tr>
</table>

## Install

| Platform | File | How |
|---|---|---|
| **Windows 10/11 (x64)** | `PMXSpan-<ver>-Setup.exe` | Run it. It installs to *Program Files*, adds a Start-menu entry (and optionally a desktop shortcut) and registers an uninstaller under *Apps & features*. Windows SmartScreen may warn when the installer isn't code-signed: click *More info → Run anyway*. (Builds are signed when `packaging/build-installers.sh` runs with `SIGN_PFX` set.) |
| Windows, no install | `pmxspan-<ver>-windows-x86_64-portable.zip` | Unzip and run `pmxspan.exe`. |
| **Debian / Ubuntu / Mint** | `pmxspan_<ver>_amd64.deb` | `sudo apt install ./pmxspan_<ver>_amd64.deb` (use `apt`, not `dpkg -i`, so dependencies are pulled in). Then launch *PMXSpan* from the app menu or run `pmxspan`. |
| Other Linux | `pmxspan-<ver>-linux-x86_64.tar.gz` | Extract and run `./pmxspan`. |

Uninstalling keeps your data (servers, encrypted vault, IPAM database). See *Data locations* below to remove it too.

To build all installers yourself: `packaging/build-installers.sh` (Linux host; the Windows setup is cross-compiled and needs `mingw-w64`, `nsis` and `rustup target add x86_64-pc-windows-gnu`). CI also builds them on every push to `main`; download them from the workflow run's artifacts.

## Build from source

You need Rust 1.88 or newer (`rustup` recommended).

```bash
cargo build --release
```

The binary is `target/release/pmxspan` (`pmxspan.exe` on Windows). It is a single self-contained executable.

- **Linux runtime:** needs an X11 or Wayland desktop with OpenGL or Vulkan (on Debian/Ubuntu: `libxkbcommon-x11-0 libgl1`), and `xdg-desktop-portal` for file pickers. SPICE needs `virt-viewer`.
- **Windows:** build natively with the MSVC toolchain, or cross-compile from Linux:

  ```bash
  rustup target add x86_64-pc-windows-gnu
  ```

  ```bash
  sudo apt install gcc-mingw-w64-x86-64
  ```

  ```bash
  cargo build --release --target x86_64-pc-windows-gnu
  ```

CI (`.github/workflows/build.yml`) runs format, clippy and tests, then builds release binaries for both platforms.

## First run

1. Choose a master password. It encrypts stored credentials.
2. **Servers → Add server**. Enter the host and port (8006). For auth, an API token is recommended: create one under *Datacenter → Permissions → API Tokens*. For TLS, keep *Pin certificate* and click **Fetch from server**. Compare the fingerprint with *Node → System → Certificates* or with `pvenode cert info`.
3. **Test connection** to see diagnostics, then **Save & connect**.

Browser consoles need you to be logged in to the Proxmox web UI in that browser. API tokens can't open consoles.

## Data locations

| | Linux | Windows |
|---|---|---|
| Config, vault, OS catalog | `~/.config/pmxspan/` | `%APPDATA%\PMXSpan\PMXSpan\config\` |
| IPAM database, logs | `~/.local/share/pmxspan/` | `%APPDATA%\PMXSpan\PMXSpan\data\` |

**Portable mode:** put an empty file named `portable` next to the executable and everything is stored in `./data` beside it.

Logs rotate daily and are kept for 14 days. Set `PMXSPAN_LOG=debug` for verbose logs.

## Setting up the OS image catalog

1. Open **OS image catalog**. Add images one by one, or use **Import JSON…** to paste a whole catalog. Before anything is saved, the import shows every entry: its URL, whether it replaces an existing one, missing checksums and its config keys.
2. In **Servers → Edit**, set the catalog download storages: a storage with the `import` content type for VM images and one with `vztmpl` for container templates. A fallback storage is optional. (In Proxmox, enable the content type under Datacenter → Storage → Edit → Content. `import` needs Proxmox VE 8.2 or newer.)
3. Download timeouts are in **Settings**: a download whose task log stops growing for the *stall* time is aborted and retried elsewhere.

VMs created from a catalog image get a cloud-init drive and a serial console (`serial0: socket`), and their boot disk is grown to the size you choose. A catalog entry's `config` keys (for example `bios: ovmf`) are applied on create and reinstall. Catalogs get shared, so only harmless keys are accepted: CPU, memory, firmware, machine, display, agent, boot order, tags and startup for VMs; CPU, memory, swap, console, timezone, tags, startup and the `nesting`/`keyctl`/`fuse` features for containers. Anything touching disks, networks, cloud-init, host access (`args`, `hookscript`, passthrough, bind mounts) or container privileges is rejected.

## Trying it without a cluster

`tools/mock_pve.py` is an in-memory imitation of the Proxmox API with two nodes, VMs, containers, templates, storages, tasks, snapshots, firewall rules and ipsets, backups, URL downloads (any URL containing `404` fails like a missing image), IPAM-friendly networking and a guest agent. Start it with:

```bash
python3 tools/mock_pve.py --port 8006
```

Then add a server `127.0.0.1:8006` and log in with:

- user `root@pam`, password `mockpass`, or
- token `root@pam!pmxspan`, secret `mocktoken`, or
- user `totp@pam`, password `mockpass`, TOTP `123456`.

## Project layout

```
src/
  api/          Proxmox API client (auth, TLS pinning, task waiting, uploads) and typed responses
  ops/          provisioning logic:
                create, rebuild, OS catalog, IP assignment, placement, guest ops,
                diagnostics, bandwidth, tenant
  ipam.rs       local IP address management
  vault.rs      encrypted credential storage
  jobs.rs       background jobs with step logs
  rt.rs         UI <-> tokio bridge (Fetch handles)
  ui/           egui views
assets/fonts/   bundled fonts and their licenses
packaging/      app icon (make_icon.py), Debian package, Windows installer
screenshots/    the images in this README
tools/mock_pve.py        mock Proxmox VE API for development/demo
tools/make_icon_font.py  rebuilds the icon font subset
```

Run the unit tests with `cargo test`. They cover IPAM allocation rules (main addresses, atomic multi-allocation), catalog parsing/validation, IP config rewriting, placement ordering, NIC/cloud-init property strings, SSH-key encoding, bandwidth integration, UPID parsing, the vault crypto (round trip, entry binding, upgrading older vaults), log redaction, private file writes, the catalog config allowlist, fingerprint parsing and moving data to new folder locations.

## Credits

Bundled fonts: [Inter](https://rsms.me/inter/) and [JetBrains Mono](https://www.jetbrains.com/lp/mono/) (SIL Open Font License 1.1), and icons from [Phosphor](https://phosphoricons.com/) (MIT). Their licenses are in `assets/fonts/`.
