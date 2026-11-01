# Security policy

PMXSpan is a personal project, maintained in my spare time and offered as is (see [LICENSE](LICENSE)). I take security reports seriously, but there is no support contract, no response-time promise, and no bug bounty.

## Supported versions

Only the latest release gets security fixes. Older versions are not patched; update to the newest release instead.

## Reporting a vulnerability

**Please do not open a public issue for a security problem.**

Report it privately through GitHub: on the repository page, open the **Security** tab and click **Report a vulnerability**. Include:

- the PMXSpan version and operating system;
- what an attacker can do, and what they need (access, credentials, user interaction);
- steps or a proof of concept that reproduce it.

I'll reply when I can and, if the report holds up, fix it in a new release. You're welcome to be credited in the release notes; say if you'd rather stay anonymous. Please give me a reasonable chance to release a fix before disclosing details publicly.

## How PMXSpan handles sensitive data

PMXSpan is a desktop app. It talks directly to the Proxmox VE API and installs nothing on your nodes.

- **Credentials** (API token secrets and passwords) are stored in a local vault encrypted with XChaCha20-Poly1305. The key is derived from your master password with Argon2id (64 MiB). The vault locks itself after 30 idle minutes by default.
- **No master password:** you can create the vault without one, but then it is only obfuscated, and anyone who gets the file can read your credentials. The app warns about this.
- **Local files:** the config, vault and logs are written readable by your user only (folders `0700`, files `0600` on Linux and macOS). Credentials are redacted from logs before anything is written.
- **TLS:** each server profile verifies the Proxmox certificate one of three ways:
  - against the system trust store;
  - by pinning its SHA-256 fingerprint (the default);
  - or not at all. The **Insecure** setting is meant for labs only.
- **OS image catalogs** imported as JSON are treated as untrusted. Only an allowlist of guest config keys is accepted, and settings that would reach the host (raw QEMU args, hookscripts, device passthrough, host serial devices) are rejected.

## Recommendations

- Use an **API token** with only the privileges you need rather than `root@pam` with a password.
- Keep TLS on **Pinned** or **System**; don't use **Insecure** outside a lab.
- Set a master password for the vault.
- For unattended schedules (`--run-schedules`), keep the password file **outside** the PMXSpan config folder and anything synced or backed up with it, readable only by you (`chmod 600`). Or use the `PMXSPAN_MASTER_PASSWORD` environment variable.
- Only import catalogs from sources you trust, and review the import preview before saving.
- Download releases only from this repository's Releases page and check them against `SHA256SUMS.txt`. The Windows installer and macOS app are not code-signed.

## Out of scope

- An attacker who already controls your computer or user account. They can read the unlocked vault from memory or capture your master password.
- Vulnerabilities in Proxmox VE, Proxmox Backup Server or the guest operating systems themselves. Report those to Proxmox or the respective vendor.
- Risks you opt into: turning off TLS verification, a vault without a master password, or giving PMXSpan broader Proxmox privileges than it needs.
- Missing hardening that isn't an exploitable vulnerability, such as "should use X instead of Y" or best-practice and compliance checklists.
