# Changelog

All notable changes to PMXSpan are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html). Release tags are the bare
version (`0.7.0`).

## [Unreleased]

## [0.7.0] - 2026-11-17

### Added

- **In-app consoles.** **Console** on a guest opens its console in a PMXSpan window: a VNC viewer
  for VM displays and a terminal for containers, VM serial ports (*Serial console (serialN)*) and
  node shells (**Shell** on a node page). They run over the same authenticated connection as the
  rest of the app, so there is no browser login and VNC works with API tokens. The VNC viewer has
  Ctrl+Alt+Del, fit-to-window or 1:1, a clipboard panel (type text as key presses, or send it as an
  RFB clipboard), a full refresh and Reconnect. The terminal is a VT100/xterm emulator with 16, 256
  and true colours, scrollback, selection and paste (a multi-line paste asks first). Several
  consoles can be open at once; locking or disconnecting closes them. The Proxmox web console and
  SPICE stay in the console menu. Settings → Preferences → *Consoles* sets the font size,
  scrollback, default scaling and the paste confirmation.
- **SSHSpan integration.** With [SSHSpan](https://github.com/AGSQ11/SSHSpan) installed,
  **Connect with SSHSpan** on a guest, a guest row or a node opens an SSH session there after you
  confirm the address, user and port, and **Send to SSHSpan…** hands many guests and nodes over at
  once after a review. SSHSpan can send `pmxspan://` links back ("open this guest", public keys);
  each lands in a confirmation card. Only names, addresses, user names and public keys cross, never
  a password or private key. PMXSpan works the same without SSHSpan. PMXSpan now runs as a single
  instance, so a second start or a link hands over to the running one.
- **SSH host keys.** PMXSpan reads each VM's SSH host keys through the guest agent (Refresh on the
  Summary, and automatically after its own create and rebuild), shows their SHA-256 fingerprints
  with a copy button and records a rebuild that changes them as a rotation (`hostkeys.json`).
- **Public keys from this computer.** **Add from this computer…** in the create wizard, the
  Cloud-Init tab and Settings lists the `~/.ssh/*.pub` files with type, comment and fingerprint.
- **MCP server** (off by default). A local [MCP](https://modelcontextprotocol.io/) endpoint for AI
  assistants on this computer, such as SSHSpan's: read-only tools for guests, nodes, IPs, tasks,
  backups, snapshots and IPAM, and, only when **Allow actions over MCP** is on, a few
  non-destructive actions (power, snapshot create, guest agent commands). Loopback only, a bearer
  token kept in the vault, every action written to `mcp-audit.log`. Destructive operations are
  always refused.

### Changed

- **Catalog entries no longer set a guest's size.** `memory`, `cores`, `sockets` and `vcpus` in a
  catalog entry's `config` are ignored, with a warning: you size the guest in the wizard, and tenant
  limits check that size. A rebuild keeps the guest's own sizing.
- **Catalog serial ports:** a catalog entry's `config` may only set `serial0: socket`.
- **Schedules follow the guest they were made for.** Destroying a guest in PMXSpan removes its
  snapshot schedules and timed power actions. A schedule whose VMID now belongs to a guest with
  another name is refused and switched off instead of acting on that guest, so renaming a guest also
  switches its schedules off. Rebuilds keep the name and are unaffected.
- **Releasing a drift orphan asks first.** The IPAM drift view no longer shows orphans when the
  cluster's resource list is empty or not loaded, and releasing an orphan asks for confirmation and
  checks with the cluster that the VMID is really free.
- **An unreadable IPAM database or catalog is kept.** A file that cannot be read or parsed is
  renamed to `<name>.json.unreadable-<date>-<time>` and an error is logged, instead of being treated
  as empty and overwritten by the next save. If it cannot be moved aside, PMXSpan refuses to save
  over it.
- **An unopenable vault shows the unlock screen** with its error, not the first-run "create a vault"
  screen, and creating a vault never overwrites an existing file.
- **Log size is bounded.** A single log event is cut at 16 KiB, the in-memory log at 4 MiB, and a
  day's log file stops growing at 32 MiB. MCP calls are logged as a short one-line summary.
- **MCP:** `snapshot_create` refuses names starting with `auto-`, which belong to snapshot schedules
  and their retention. `find_by_ip` checks the static guest configuration before addresses a guest
  reports about itself; a guest-reported address is used only when nothing configured owns it, and
  is marked unverified.

### Security

Fixes for the 14 confirmed findings of a full security audit (Cloudflare security-audit skill), one
commit and regression test each:

- Bracketed paste strips every ESC byte, so a pasted text cannot close the paste early and type
  commands into a console.
- Schedules are revoked when their guest is destroyed and bound to the guest's identity, so a reused
  VMID does not inherit another guest's snapshots, retention or power actions.
- Node names in MCP read tools are checked as single safe path segments.
- Catalog sizing overrides can no longer bypass tenant pool limits (see *Changed*).
- Importing a catalog drops a local custom cloud-init binding of an entry the import replaces.
- Unreadable IPAM and catalog files are kept instead of overwritten (see *Changed*).
- Log retention is bounded in bytes and MCP call text is clipped (see *Changed*).
- MCP connections get a 15-second response write deadline, so a client that stops reading cannot
  hold a connection slot.
- `find_by_ip` ranks static configuration above guest-reported addresses (see *Changed*).
- Drift orphans from an incomplete resource list cannot be released in one click (see *Changed*).
- The `auto-` snapshot namespace is reserved for schedules (see *Changed*).
- The idle auto-lock is also enforced while the window is hidden.
- An unopenable vault is no longer presented as a first run that would overwrite it (see *Changed*).
- Secret redaction in logs runs in linear time on hostile input.

### Fixed

- Builds with the clippy of Rust 1.100 (`chunks_exact_to_as_chunks`, `needless_range_loop`).

## [0.6.0] - 2026-11-01

First public release: a native desktop manager for Proxmox VE clusters.
Server profiles with API-token or ticket login (TOTP), certificate pinning and an encrypted vault;
dashboard, guest list and guest detail; the create wizard with auto-placement, IPAM, cloud-init
and the OS image catalog; rebuild/reinstall that keeps the VMID, NICs and IPs; IPAM with history
and drift; storage, datacenter, access and tenant management with tenant limits; backups including
Proxmox Backup Server file browsing; snapshot schedules, timed power actions and usage alerts,
also unattended through `pmxspan --run-schedules`.
