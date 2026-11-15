//! Integration with SSHSpan, the SSH client and key manager (the shared
//! contract is v1 of the PMXSpan <-> SSHSpan URL schemes).
//!
//! PMXSpan works the same without SSHSpan: nothing here runs at startup or
//! in `--run-schedules`, and SSHSpan actions are disabled with a hint when it
//! is not installed. No secret ever crosses: only host names, user names,
//! public keys, fingerprints and display names.

pub mod bundle;
pub mod ipc;
pub mod link;
pub mod resolve;
pub mod sshspan;
