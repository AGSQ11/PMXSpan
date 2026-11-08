<p align="center"><img src="packaging/assets/pmxspan.png" width="96" alt="PMXSpan logo"></p>

<h1 align="center">PMXSpan</h1>

<p align="center"><b>One desktop app across all your Proxmox VE clusters.</b><br>Native on Windows and Linux · nothing to install on your nodes</p>

PMXSpan is a standalone desktop app for managing Proxmox VE clusters, written in Rust ([egui](https://github.com/emilk/egui)). It handles provisioning (auto-placement, create, rebuild, IP management, cloud-init, OS image catalog, bandwidth accounting, tenant isolation, diagnostics) in a native GUI that talks directly to the Proxmox VE API. It doesn't need a web server, a database or agents on the nodes.

<p align="center">
  <img src="screenshots/dashboard.png" alt="Dashboard in the Graphite (dark) theme" width="49%">
  <img src="screenshots/dashboard-light.png" alt="Dashboard in the Daylight (light) theme" width="49%">
</p>
<p align="center"><sub>Graphite (dark) and Daylight (light). Every screenshot in this README was taken against the bundled mock cluster. More in <a href="#screenshots">Screenshots</a>.</sub></p>

## Why PMXSpan

I built PMXSpan to automate three jobs I did by hand on Proxmox VE:

1. **Reinstall VMs and containers** without losing the VMID, NICs or IPs.
2. **Deploy from current OS images.** PMXSpan gets the latest official
   cloud images directly from the vendors. No old templates.
3. **Assign IPs automatically** from local IPv4/IPv6 pools (IPAM),
   and show drift against the cluster.

One desktop app for all your clusters. No web server, no database,
no agents on the nodes.

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
| **Create wizard** | Builds VMs from a cloud-init template (full clone, then migrated if the template's disks are node-local), from an **OS catalog image**, or from an ISO, and containers from a `vztmpl` on storage or from the catalog. Auto-placement, IPAM, cloud-init user/password/SSH keys, tags and pools are all set from the wizard. New guests get the `PMXSpan` tag (configurable in Settings) so you can find them in Proxmox. |
| **Auto-placement** | Nodes without enough free RAM are skipped. Each remaining node's storage is filtered by content type and free space. The survivors are ordered by your RAM/CPU/Disk priority and picked *dense*, *sparse* or *random*. |
| **IPAM** | A local IPv4/IPv6 pool database with ranges, gateways, DNS, VLAN, bridge/server scoping and reservations. A *preferred* IP is assigned exactly or the operation fails; it's never silently replaced. Addresses are released when a guest is destroyed or provisioning fails. Every change is kept in a **history** (assigned, adopted, released, made main, reserved), and a **drift** view compares IPAM with the cluster: addresses kept for guests that are gone, static addresses IPAM does not know, main addresses that differ, and app-created guests without a record, each with a one-click fix. |
| **IP assignment** | Per guest, per interface: assign one or more IPv4/IPv6 addresses from a pool (or an exact one), release them, pick the **main** address of each family, add an interface together with its addresses, remove one (its addresses are released), and adopt addresses already in a guest's config. The main addresses go into the guest config (LXC `netN`, VM cloud-init `ipconfigN`, the pool's VLAN on the NIC); additional ones are configured inside the guest. The **anti-spoofing IP filter** keeps an `ipfilter-netN` ipset per interface in sync with the assigned addresses. |
| **OS image catalog** | Images for creating and reinstalling guests from a URL instead of a template: VM disk images (qcow2/raw/vmdk/ova, imported with `import-from`) and container templates. Catalogs import and export as JSON (`{"entries": [...]}` or an array with `id`, `label`, `virt`, `url`, `checksum`, `checksum_algorithm`, `cloud_user`, `ostype`, `nodes`, `config`/`tpl_data`, `image_format`). Images are downloaded to the per-server main or fallback storage with stall detection; permanent errors (404/403) fail at once, transient ones are retried, and a full or broken storage moves on to the next one. The downloaded file is always removed afterwards. |
| **Rebuild / reinstall** | Replaces a guest's disks with a fresh template **or catalog image** while keeping its VMID, name, resources, NICs (**MAC addresses**), IP addresses, firewall rules/ipsets and pool. A catalog image is downloaded and the firewall config read *before* anything is deleted, so a failed download leaves the guest untouched. If the firewall can't be fully restored, the guest is left stopped instead of starting unprotected. You confirm by typing the VMID. Optionally (and by default) the guest is **backed up first**. |
| **Storage** | Browse content, upload ISOs/templates with progress, download from a URL (with optional checksum), browse and download appliance templates, delete volumes. |
| **Datacenter** | Scheduled backup jobs (create/toggle/run now/delete), storage replication, HA resources and HA status. |
| **Access** | Users, API tokens (the secret is shown once), pools, ACLs, and a **tenant wizard** that creates a pool and a new `@pve` (or directory-realm) user with a role on `/pool/<pool>` only, so customers see nothing outside their pool. Existing accounts are never modified. |
| **Jobs** | Every multi-step operation runs in the background and logs each step and the exact error it stopped on, including the tail of the Proxmox task log. No silent failures. |
| **Backups** | Back up now, restore (also another guest's backup over this one), protect and delete. For **Proxmox Backup Server** snapshots, browse the files inside and download single files or whole folders (zip / tar.zst). |
| **Automation** | Per-guest **snapshot schedules** with retention (every N hours, or daily / on chosen weekdays at a time) and **timed power actions** (start, shutdown, reboot, stop). Runs while PMXSpan is open, or unattended from `pmxspan --run-schedules` (see below). **Usage alerts** fire when CPU, memory, network or disk I/O of a guest stays above a threshold. |
| **Tenant limits** | Per tenant pool: maximum guests, vCPUs, memory, disk and IP addresses, enforced when PMXSpan creates a guest in the pool, assigns addresses to one of its guests or moves a guest into it. Usage is shown next to each pool. |
| **Placement controls** | A **VMID range** per server (new guests and clones get the lowest free ID in it) and **maintenance mode** for nodes, which auto-placement skips. |
| **Create extras** | **Custom cloud-init** files (`cicustom` user/vendor/network snippets from the nodes, with a default per catalog image), **HA** on create (default per catalog image, optional group), and a generated **Ed25519 key pair** (private key saved locally, optionally passphrase-protected; the public key goes into the guest). |
| **Recovery copies** | Export the config, firewall, HA settings, snapshot names and IPAM records of one guest or of every listed guest to a JSON file. |
| **Consoles** | noVNC/xterm.js open in your browser, and SPICE opens in virt-viewer. |
| **SSHSpan integration** | With [SSHSpan](#working-with-sshspan) installed, **Connect with SSHSpan** on a guest, a guest row or a node opens an SSH session there after you confirm the address, user and port, and **Send to SSHSpan…** hands many guests (and nodes) over at once after a review. SSHSpan can send back "open this guest" links and public keys, which always land in a confirmation card. Only names, addresses, user names and public keys cross; never a password or private key. Without SSHSpan the buttons are disabled with a hint, or hidden in Settings. |
| **SSH host keys** | PMXSpan reads each VM's SSH host keys through the guest agent (Refresh on the Summary, and automatically after its own create and rebuild) and shows their SHA-256 fingerprints with a copy button, so you can check the first connection with any SSH client. A rebuild that changes the keys is recorded as a rotation. |
| **Public keys from this computer** | **Add from this computer…** in the create wizard, the Cloud-Init tab and Settings lists the `~/.ssh/*.pub` files (type, comment, fingerprint), skips keys already in the field and marks files it cannot read. |

### Not included yet

**Reverse DNS** (PTR) management through external DNS backends.

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

### Automation, IPAM and backups

<table>
  <tr>
    <td width="50%" valign="top"><img src="screenshots/automation-schedules.png" alt="Schedules"><br><sub><b>Schedules</b>: snapshots with retention and timed power actions, with the last result of each.</sub></td>
    <td width="50%" valign="top"><img src="screenshots/automation-alerts.png" alt="Usage alerts"><br><sub><b>Usage alerts</b>: rules for CPU, memory, network and disk I/O, and what is firing now.</sub></td>
  </tr>
  <tr>
    <td width="50%" valign="top"><img src="screenshots/ipam-drift.png" alt="IPAM drift"><br><sub><b>IPAM drift</b>: where IPAM and the cluster disagree, with Adopt / Apply / Release.</sub></td>
    <td width="50%" valign="top"><img src="screenshots/ipam-history.png" alt="IP history"><br><sub><b>IP history</b>: every change to every address.</sub></td>
  </tr>
  <tr>
    <td width="50%" valign="top"><img src="screenshots/pbs-file-restore.png" alt="Single-file restore"><br><sub><b>Single-file restore</b>: browse a Proxmox Backup Server snapshot and download files or folders.</sub></td>
    <td width="50%" valign="top"><img src="screenshots/pool-limits.png" alt="Tenant pool limits"><br><sub><b>Tenant limits</b>: guests, vCPUs, memory, disk and IPs per pool, with current usage.</sub></td>
  </tr>
  <tr>
    <td width="50%" valign="top"><img src="screenshots/create-access-options.png" alt="Create options"><br><sub><b>Create</b>: custom cloud-init snippets, a generated SSH key pair and HA.</sub></td>
    <td width="50%" valign="top"><img src="screenshots/vm-manage.png" alt="Manage tab"><br><sub><b>Manage</b>: config export, clone into the VMID range, rebuild with a backup first.</sub></td>
  </tr>
  <tr>
    <td width="50%" valign="top"><img src="screenshots/node-maintenance.png" alt="Node maintenance"><br><sub><b>Maintenance</b>: a node auto-placement skips, marked in the sidebar.</sub></td>
    <td width="50%" valign="top"></td>
  </tr>
</table>

### Setup

<table>
  <tr>
    <td width="50%" valign="top"><img src="screenshots/os-catalog.png" alt="OS image catalog"><br><sub><b>OS image catalog</b>: cloud images and container templates by URL, JSON import and export.</sub></td>
    <td width="50%" valign="top"><img src="screenshots/servers.png" alt="Server profile"><br><sub><b>Servers</b>: API token or password login with TOTP, certificate pinning, catalog storages, VMID range, nodes in maintenance.</sub></td>
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
| **Any Linux (x86_64)** | `PMXSpan-<ver>-x86_64.AppImage` | `chmod +x PMXSpan-<ver>-x86_64.AppImage` and run it. Needs FUSE 2 (`libfuse2`); without it, run it with `--appimage-extract-and-run`. |
| Other Linux | `pmxspan-<ver>-linux-x86_64.tar.gz` | Extract and run `./pmxspan`. |
| **macOS 11+ (Apple silicon and Intel)** | `PMXSpan-<ver>-macos-universal.dmg` | Open it and drag *PMXSpan* to *Applications*. The app isn't notarized: the first time, right-click it and choose *Open*. |

The Linux builds need glibc 2.31 or newer (Debian 11+, Ubuntu 20.04+, RHEL 9+, Fedora 32+). Uninstalling keeps your data (servers, encrypted vault, IPAM database). See *Data locations* below to remove it too.

Releases are built by `.github/workflows/release.yml` when a `v*` tag is pushed, and attached to the GitHub release with a `SHA256SUMS.txt`. To build the Linux and Windows packages yourself: `packaging/build-installers.sh` (Linux host; the Windows setup is cross-compiled and needs `mingw-w64`, `nsis` and `rustup target add x86_64-pc-windows-gnu`; with `zig` and `cargo-zigbuild` installed the Linux builds target glibc 2.31). The macOS app is built on a Mac with `packaging/macos/build-app.sh`.

## Build from source

You need Rust 1.88 or newer (`rustup` recommended).

```bash
cargo build --release
```

The binary is `target/release/pmxspan` (`pmxspan.exe` on Windows). It is a single self-contained executable. On macOS it runs as-is; `packaging/macos/build-app.sh` wraps it into an `.app` and `.dmg`.

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
| Config, vault, OS catalog, schedules and alert rules | `~/.config/pmxspan/` | `%APPDATA%\PMXSpan\PMXSpan\config\` |
| IPAM database, SSH host keys (`hostkeys.json`), logs | `~/.local/share/pmxspan/` | `%APPDATA%\PMXSpan\PMXSpan\data\` |
| SSHSpan handoff files (deleted after 24 h) | `$XDG_RUNTIME_DIR/pmxspan/handoff/` (else `~/.local/share/pmxspan/handoff/`) | `%LOCALAPPDATA%\PMXSpan\handoff\` |
| Single-instance socket | `$XDG_RUNTIME_DIR/pmxspan/ipc.sock` (else in the data folder) | named pipe `\\.\pipe\pmxspan-<hash>` |

**Portable mode:** put an empty file named `portable` next to the executable and everything is stored in `./data` beside it (including the single-instance socket).

On macOS the handoff files go to `~/Library/Application Support/pmxspan/handoff/`.

Logs rotate daily and are kept for 14 days. Set `PMXSPAN_LOG=debug` for verbose logs.

## Setting up the OS image catalog

1. Open **OS image catalog**. Add images one by one, or use **Import JSON…** to paste a whole catalog. Before anything is saved, the import shows every entry: its URL, whether it replaces an existing one, missing checksums and its config keys.
2. In **Servers → Edit**, set the catalog download storages: a storage with the `import` content type for VM images and one with `vztmpl` for container templates. A fallback storage is optional. (In Proxmox, enable the content type under Datacenter → Storage → Edit → Content. `import` needs Proxmox VE 8.2 or newer.)
3. Download timeouts are in **Settings**: a download whose task log stops growing for the *stall* time is aborted and retried elsewhere.

VMs created from a catalog image get a cloud-init drive and a serial console (`serial0: socket`), and their boot disk is grown to the size you choose. A catalog entry's `config` keys (for example `bios: ovmf`) are applied on create and reinstall. Catalogs get shared, so only harmless keys are accepted: CPU, memory, firmware, machine, display, agent, boot order, tags and startup for VMs; CPU, memory, swap, console, timezone, tags, startup and the `nesting`/`keyctl`/`fuse` features for containers. Anything touching disks, networks, cloud-init, host access (`args`, `hookscript`, passthrough, bind mounts) or container privileges is rejected.

## Running schedules unattended

Proxmox has no scheduler for snapshots or power actions and PMXSpan installs nothing on the nodes, so schedules run while the app is open and connected to the guest's server. To run them without the app, start `pmxspan --run-schedules` every few minutes on an always-on machine. It runs whatever is due once, prints what it did, and exits (status 1 if something failed). It reads the same config and vault as the app, so log in with an **API token** for those servers (a TOTP login can't run unattended), and give it the master password through a file only you can read (`chmod 600`) or the `PMXSPAN_MASTER_PASSWORD` variable. Keep that file outside `~/.config/pmxspan/`, so a backup or sync of the config folder never holds the vault and its password together. Overlapping runs are skipped, and the app leaves due schedules to a run that is in progress:

```bash
crontab -e
```

```
*/5 * * * * /usr/bin/pmxspan --run-schedules --password-file /home/me/.secrets/pmxspan-master >> /home/me/.local/share/pmxspan/schedules.log 2>&1
```

On Windows, create a Task Scheduler task that runs every 5 minutes with the program `C:\Program Files\PMXSpan\pmxspan.exe` and the arguments `--run-schedules --password-file C:\Users\me\pmxspan-master.txt`. Missed runs are caught up for a while (snapshots up to 6 hours late, power actions up to 30 minutes), never repeated. Alerts are only checked while the app is open.

## Custom cloud-init files

Proxmox's API cannot upload snippets, so put your cloud-init files on the nodes yourself, in a storage with the `snippets` content type (for `local` that is `/var/lib/vz/snippets/`). PMXSpan lists them in the create wizard, the Cloud-Init tab and per catalog image (*OS image catalog → Custom cloud-init*, stored on this computer only and never exported or imported), and sets the VM's `cicustom`. Rebuilds keep it.

## Working with SSHSpan

[SSHSpan](https://github.com/AGSQ11/SSHSpan) is an SSH client and key manager. PMXSpan works the same without it; with it, guests and nodes open in SSH in one click and their host keys are checked against the ones PMXSpan read.

**What crosses, and what never does.** PMXSpan hands SSHSpan host names and addresses, ports, user names, display names, the server profile's id and name, pool and node names, guest tags and the public SSH host keys it read. SSHSpan hands PMXSpan "open this guest/node" requests and public keys. Passwords, API tokens, private keys and vault contents never cross, and nothing received is applied on its own.

**Finding SSHSpan.** PMXSpan looks for SSHSpan when you first see one of its buttons: the path set in **Settings → SSHSpan** (absolute, an existing file, `.exe` on Windows), then on Windows the installed-programs registry, `%ProgramFiles%\SSHSpan\sshspan.exe`, `%LOCALAPPDATA%\Programs\SSHSpan\sshspan.exe` and `PATH`, and on Linux/macOS `PATH`, `/usr/bin`, `/usr/local/bin`, `~/.local/bin`, `/opt/SSHSpan/sshspan`, `~/Applications/SSHSpan*.AppImage` and `/Applications/SSHSpan.app`. The first time it starts a program it found on its own, it shows you the exact path and asks; it asks again if that path changes. Without an executable it uses the `sshspan://` link handler registered with the system, if there is one. Otherwise the SSHSpan buttons are disabled with the hint *"SSHSpan was not found on this computer. Install it or set its path in Settings."* Untick **SSHSpan actions** in Settings to hide them all.

**Connect with SSHSpan** (guest header, guest list, node page). The address is the guest's IPAM main IPv4, else its IPAM main IPv6, else an address its guest agent reports (no loopback or link-local, IPv4 first), else a container's static `ip=`. The user is the VM's cloud-init user, else the catalog image's default user, else `root`; containers and nodes use `root`, and nodes use their cluster address from `/cluster/status`. A small card shows host, user and port (you can change them; PMXSpan remembers your values for that guest until it closes) and the host-key status, then SSHSpan's Quick connect dialog opens prefilled.

**Host keys.** PMXSpan sends the host keys it read as *proposed* keys: SSHSpan accepts a server silently only when its live key matches one, refuses with both fingerprints when it differs, and after a PMXSpan rebuild (the link then says when the keys rotated) offers to replace the key it saved. Keys that changed without a rebuild are shown with a warning on the Summary and are never presented as a rotation. Containers and nodes have no host-key source in the Proxmox API; SSHSpan then asks you to trust the key on first connect, as usual.

**Send to SSHSpan…** (Guests toolbar). A review lists every guest of the connected server, the ones in your current list filter ticked, with the address, user, the SSHSpan category it will go to (*Proxmox › server › pool*, or *no pool*), the host-key status, and why a row cannot be sent (no address, an invalid user name). Nodes can be added with a tick. **Send** writes the list to a private handoff file (mode 0600 in a 0700 folder, see Data locations) and opens SSHSpan's own import review, which checks the file's SHA-256 and age before reading it. Handoff files left behind are deleted when PMXSpan next starts.

**Links from SSHSpan.** SSHSpan opens `pmxspan://guest?v=1&cluster=<profile id>&vmid=<id>` and `pmxspan://node?v=1&cluster=<profile id>&node=<name>` ("Manage in PMXSpan") and `pmxspan://add-key?v=1&key=<public key line>&name=<key name>`. Every link is validated (version 1 only, at most 8192 bytes, every field checked) and a bad one is refused with a message. A locked vault is unlocked first (the link waits up to 10 minutes); connecting to a different server asks first; then the guest or node page opens and the window comes to the front. An offered key is shown with its type, SHA-256 fingerprint and comment, and is added only where you choose: Settings' default SSH keys, the open Create wizard, or the open Cloud-Init tab (saved with that form). Keys already there are reported, not added twice.

**One window.** Starting PMXSpan while it is already running brings the running window to the front, and a `pmxspan://` link given to the second start is handed over to it (through a socket only your user can open, at most 64 KiB per message). `pmxspan --run-schedules` is not affected. The Debian package, AppImage and Windows installer register PMXSpan for `pmxspan://` links.

## Trying it without a cluster

`tools/mock_pve.py` is an in-memory imitation of the Proxmox API with two nodes, VMs, containers, templates, storages, tasks, snapshots, firewall rules and ipsets, backups (including a Proxmox Backup Server storage with browsable files), cloud-init snippets, URL downloads (any URL containing `404` fails like a missing image), IPAM-friendly networking and a guest agent (which also returns SSH host keys for VMs). Start it with:

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
                diagnostics, bandwidth, tenant, pool limits, IPAM drift,
                config export, schedules
  automation.rs schedules, due-time logic and usage alerts
  headless.rs   `pmxspan --run-schedules`
  integrations/ SSHSpan: discovery and launch, sshspan:// / pmxspan:// links, handoff bundle,
                address resolution, single-instance IPC
  hostkeys.rs   guest SSH host keys (hostkeys.json)
  sshkey.rs     SSH public key lines and SHA-256 fingerprints
  pubkeys.rs    ~/.ssh/*.pub listing for the key picker
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

Run the unit tests with `cargo test`. They cover IPAM allocation rules (main addresses, atomic multi-allocation), catalog parsing/validation, IP config rewriting, placement ordering, NIC/cloud-init property strings, SSH-key encoding, bandwidth integration, UPID parsing, the vault crypto (round trip, entry binding, upgrading older vaults), log redaction, private file writes, the catalog config allowlist, IPAM history and drift, pool limits, schedule due times, alert firing and resolving, SSH key generation, fingerprint parsing and moving data to new folder locations, and for the SSHSpan integration: link building and percent-encoding (IPv6, unicode, the 8192-byte cap), `pmxspan://` parsing and every refusal, the handoff bundle (JSON shape, SHA-256, private file mode), host-key parsing, fingerprints against known `ssh-keygen -l` values and rotation rules, address/user resolution order, SSHSpan discovery against a fake `PATH`, the single-instance socket and its framing, and the public-key picker.

## Credits

Bundled fonts: [Inter](https://rsms.me/inter/) and [JetBrains Mono](https://www.jetbrains.com/lp/mono/) (SIL Open Font License 1.1), and icons from [Phosphor](https://phosphoricons.com/) (MIT). Their licenses are in `assets/fonts/`.

---

## Repository statistics

<p align="center">
  <a href="https://github.com/AGSQ11/PMXSpan/stargazers"><img alt="GitHub stars" src="https://img.shields.io/github/stars/AGSQ11/PMXSpan?style=flat-square&logo=github&label=Stars"></a>
  <a href="https://github.com/AGSQ11/PMXSpan/forks"><img alt="GitHub forks" src="https://img.shields.io/github/forks/AGSQ11/PMXSpan?style=flat-square&logo=github&label=Forks"></a>
  <a href="https://github.com/AGSQ11/PMXSpan/issues"><img alt="Open GitHub issues" src="https://img.shields.io/github/issues/AGSQ11/PMXSpan?style=flat-square&logo=github&label=Open%20issues"></a>
  <a href="https://github.com/AGSQ11/PMXSpan/graphs/contributors"><img alt="GitHub contributors" src="https://img.shields.io/github/contributors/AGSQ11/PMXSpan?style=flat-square&logo=github&label=Contributors"></a>
</p>

<p align="center">
  <a href="https://github.com/AGSQ11/PMXSpan/releases"><img alt="Total GitHub release downloads" src="https://img.shields.io/github/downloads/AGSQ11/PMXSpan/total?style=flat-square&logo=github&label=Release%20downloads"></a>
  <a href="https://github.com/AGSQ11/PMXSpan/releases/latest"><img alt="Latest GitHub release" src="https://img.shields.io/github/v/release/AGSQ11/PMXSpan?style=flat-square&logo=github&label=Latest%20release"></a>
  <a href="https://github.com/AGSQ11/PMXSpan/commits/main"><img alt="Last commit" src="https://img.shields.io/github/last-commit/AGSQ11/PMXSpan?style=flat-square&logo=github&label=Last%20commit"></a>
</p>

<p align="center"><sub>Live repository metrics. Download counts reflect GitHub release assets only, not clones or installs.</sub></p>
