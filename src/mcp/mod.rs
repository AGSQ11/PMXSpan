//! Optional, local-only MCP (Model Context Protocol) server.
//!
//! When "MCP server" is enabled in the settings and the vault is unlocked,
//! PMXSpan listens on `127.0.0.1:<port>/mcp` (and `[::1]` when available) and
//! speaks MCP over Streamable HTTP so AI assistants (SSHSpan's, or any other
//! MCP client) can read the connected cluster and, when allowed, run a small
//! set of non-destructive actions. Every request needs the bearer token kept
//! in the vault. The server stops when the vault locks, when it is disabled
//! and when the app quits; it is never started by `--run-schedules`.
//!
//! - [`protocol`]: JSON-RPC / MCP lifecycle pieces without HTTP.
//! - [`server`]: the HTTP endpoint, its checks and the sessions.
//! - [`tools`]: tool definitions (stable: clients pin them) and handlers.
//! - [`audit`]: the append-only `mcp-audit.log`.

pub mod audit;
pub mod protocol;
pub mod server;
pub mod tools;

#[cfg(test)]
mod integration_tests;

use crate::api::PveClient;
use crate::config::{AppPaths, ServerProfile, Settings};
use crate::ipam::IpamDb;
use crate::vault::Vault;
use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

pub const DEFAULT_PORT: u16 = 48731;
/// Vault entry holding the bearer token.
pub const TOKEN_KEY: &str = "mcp:bearer-token";
pub const AUDIT_FILE: &str = "mcp-audit.log";
/// Agent-reported addresses are reused this long by `list_guests`.
const AGENT_CACHE_TTL: Duration = Duration::from_secs(600);

// ---- state the tools read -------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct ProfileInfo {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
}

#[derive(Clone)]
pub struct Connected {
    pub client: PveClient,
    pub profile: ServerProfile,
}

/// A copy of what the UI knows, refreshed every frame while the server runs.
#[derive(Clone, Default)]
pub struct ViewState {
    pub profiles: Vec<ProfileInfo>,
    pub connected: Option<Connected>,
    pub ipam: Option<Arc<Mutex<IpamDb>>>,
    pub managed_tag: String,
}

/// (server id, vmid) -> (when seen, addresses).
type AgentCache = HashMap<(String, u64), (Instant, Vec<String>)>;

/// Shared between the UI (writer) and the server's tool handlers (readers).
pub struct AppView {
    state: RwLock<ViewState>,
    allow_actions: AtomicBool,
    pub audit: audit::AuditLog,
    agent_ips: Mutex<AgentCache>,
}

impl AppView {
    pub fn new(audit_path: std::path::PathBuf) -> Self {
        Self {
            state: RwLock::new(ViewState::default()),
            allow_actions: AtomicBool::new(false),
            audit: audit::AuditLog::new(audit_path),
            agent_ips: Mutex::new(HashMap::new()),
        }
    }

    pub fn snapshot(&self) -> ViewState {
        self.state.read().clone()
    }

    pub fn set_state(&self, state: ViewState) {
        *self.state.write() = state;
    }

    /// Forgets the connection (and the credentials its client holds).
    pub fn clear(&self) {
        *self.state.write() = ViewState::default();
        self.agent_ips.lock().clear();
    }

    pub fn allow_actions(&self) -> bool {
        self.allow_actions.load(Ordering::Relaxed)
    }

    pub fn set_allow_actions(&self, on: bool) {
        self.allow_actions.store(on, Ordering::Relaxed);
    }

    pub fn cache_agent_ips(&self, server_id: &str, vmid: u64, ips: Vec<String>) {
        let mut c = self.agent_ips.lock();
        c.retain(|_, (at, _)| at.elapsed() < AGENT_CACHE_TTL);
        c.insert((server_id.to_string(), vmid), (Instant::now(), ips));
    }

    pub fn cached_agent_ips(&self, server_id: &str, vmid: u64) -> Option<Vec<String>> {
        let c = self.agent_ips.lock();
        c.get(&(server_id.to_string(), vmid)).filter(|(at, _)| at.elapsed() < AGENT_CACHE_TTL).map(|(_, ips)| ips.clone())
    }

    /// Every cached (vmid, address) of a server.
    pub fn cached_agent_all(&self, server_id: &str) -> Vec<(u64, String)> {
        let c = self.agent_ips.lock();
        c.iter()
            .filter(|((sid, _), (at, _))| sid == server_id && at.elapsed() < AGENT_CACHE_TTL)
            .flat_map(|((_, vmid), (_, ips))| ips.iter().map(move |ip| (*vmid, ip.clone())))
            .collect()
    }
}

// ---- status shown in the settings ------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub enum McpStatus {
    Stopped,
    Listening(Vec<SocketAddr>),
    Error(String),
}

static STATUS: std::sync::Mutex<Option<McpStatus>> = std::sync::Mutex::new(None);
/// Number of live client sessions (for the status line).
pub(crate) static SESSIONS: AtomicUsize = AtomicUsize::new(0);
/// Bumped whenever the token in the vault changes.
static TOKEN_EPOCH: AtomicU64 = AtomicU64::new(0);
/// A newly generated token waiting to be shown once in the settings.
static REVEAL: std::sync::Mutex<Option<Zeroizing<String>>> = std::sync::Mutex::new(None);

pub fn status() -> McpStatus {
    STATUS.lock().map(|s| s.clone().unwrap_or(McpStatus::Stopped)).unwrap_or(McpStatus::Stopped)
}

fn set_status(s: McpStatus) {
    let changed = match STATUS.lock() {
        Ok(mut g) if g.as_ref() != Some(&s) => {
            *g = Some(s);
            true
        }
        _ => false,
    };
    if changed {
        crate::rt::repaint();
    }
}

pub fn session_count() -> usize {
    SESSIONS.load(Ordering::Relaxed)
}

/// The token generated last, if it has not been hidden yet.
pub fn revealed_token() -> Option<Zeroizing<String>> {
    REVEAL.lock().ok().and_then(|g| g.clone())
}

pub fn hide_token() {
    if let Ok(mut g) = REVEAL.lock() {
        *g = None;
    }
}

fn reveal(token: &Zeroizing<String>) {
    if let Ok(mut g) = REVEAL.lock() {
        *g = Some(token.clone());
    }
}

// ---- token -----------------------------------------------------------------

/// 32 random bytes, base64url without padding (43 characters).
pub fn generate_token() -> Zeroizing<String> {
    use base64::Engine;
    use rand::RngCore;
    let mut bytes = Zeroizing::new([0u8; 32]);
    rand::rngs::OsRng.fill_bytes(bytes.as_mut());
    Zeroizing::new(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes.as_ref()))
}

/// The stored token, generating (and revealing) one on first use.
pub fn ensure_token(vault: &mut Vault) -> anyhow::Result<Zeroizing<String>> {
    if let Some(t) = vault.get(TOKEN_KEY).filter(|t| !t.is_empty()) {
        return Ok(Zeroizing::new(t));
    }
    regenerate_token(vault)
}

/// Replaces the token; the old one stops working immediately.
pub fn regenerate_token(vault: &mut Vault) -> anyhow::Result<Zeroizing<String>> {
    let t = generate_token();
    vault.set(TOKEN_KEY, &t)?;
    TOKEN_EPOCH.fetch_add(1, Ordering::Relaxed);
    reveal(&t);
    tracing::info!("MCP bearer token generated");
    Ok(t)
}

pub fn has_token(vault: &Vault) -> bool {
    vault.get(TOKEN_KEY).is_some_and(|t| !t.is_empty())
}

// ---- lifecycle -------------------------------------------------------------

/// Owned by the UI: starts and stops the server to match the settings and
/// the vault state. Costs nothing while disabled.
pub struct McpHost {
    view: Arc<AppView>,
    running: Option<server::RunningServer>,
    /// Port whose start failed; not retried until the settings change.
    failed: Option<u16>,
    token_epoch: u64,
}

impl McpHost {
    pub fn new(paths: &AppPaths) -> Self {
        Self {
            view: Arc::new(AppView::new(paths.data_dir.join(AUDIT_FILE))),
            running: None,
            failed: None,
            token_epoch: TOKEN_EPOCH.load(Ordering::Relaxed),
        }
    }

    /// Called every frame while the vault is unlocked. Returns an error to
    /// show as a toast (once per failure).
    pub fn tick(
        &mut self,
        settings: &Settings,
        profiles: &[ServerProfile],
        vault: Option<&mut Vault>,
        connected: Option<(&PveClient, &ServerProfile)>,
        ipam: &Arc<Mutex<IpamDb>>,
    ) -> Option<String> {
        let Some(vault) = vault.filter(|v| v.is_unlocked() && settings.mcp_enabled) else {
            self.failed = None;
            if self.running.is_some() || !matches!(status(), McpStatus::Stopped) {
                self.stop();
            }
            return None;
        };
        let port = settings.mcp_port;
        if self.running.as_ref().is_some_and(|r| r.port() != port) {
            self.stop();
        }
        self.view.set_allow_actions(settings.mcp_allow_actions);
        self.view.set_state(ViewState {
            profiles: profiles
                .iter()
                .map(|p| ProfileInfo { id: p.id.clone(), name: p.display_name(), host: p.host.clone(), port: p.port })
                .collect(),
            connected: connected.map(|(c, p)| Connected { client: c.clone(), profile: p.clone() }),
            ipam: Some(ipam.clone()),
            managed_tag: if settings.tag_managed_guests { settings.managed_tag.clone() } else { String::new() },
        });
        let epoch = TOKEN_EPOCH.load(Ordering::Relaxed);
        if epoch != self.token_epoch {
            self.token_epoch = epoch;
            if let Some(r) = &self.running
                && let Some(t) = vault.get(TOKEN_KEY)
            {
                r.state().set_token(&t);
            }
        }
        if self.running.is_some() || self.failed == Some(port) {
            return None;
        }
        let token = match ensure_token(vault) {
            Ok(t) => t,
            Err(e) => return self.fail(port, format!("MCP server: storing its token in the vault failed: {e:#}")),
        };
        self.token_epoch = TOKEN_EPOCH.load(Ordering::Relaxed);
        match server::RunningServer::start(self.view.clone(), port, &token) {
            Ok(r) => {
                tracing::info!("MCP server listening on {}", fmt_addrs(r.addrs()));
                set_status(McpStatus::Listening(r.addrs().to_vec()));
                self.failed = None;
                self.running = Some(r);
                None
            }
            Err(e) => self.fail(port, format!("MCP server could not listen on 127.0.0.1:{port}: {e:#}")),
        }
    }

    fn fail(&mut self, port: u16, msg: String) -> Option<String> {
        tracing::error!("{msg}");
        set_status(McpStatus::Error(msg.clone()));
        self.failed = Some(port);
        Some(msg)
    }

    /// Stops the server (connections drain for up to 2 s in the background)
    /// and forgets the connection it was reading.
    pub fn stop(&mut self) {
        if let Some(r) = self.running.take() {
            r.signal_stop();
            tracing::info!("MCP server stopped");
        }
        self.view.clear();
        set_status(McpStatus::Stopped);
    }

    /// The vault was locked: stop and forget the token shown once.
    pub fn on_lock(&mut self) {
        self.failed = None;
        self.stop();
        hide_token();
    }

    /// App exit: stop and wait for the drain (at most ~2 s).
    pub fn shutdown(&mut self) {
        if let Some(r) = self.running.take() {
            r.stop_and_wait(Duration::from_millis(2500));
        }
        self.view.clear();
        hide_token();
    }
}

pub fn fmt_addrs(addrs: &[SocketAddr]) -> String {
    addrs.iter().map(SocketAddr::to_string).collect::<Vec<_>>().join(", ")
}

/// The endpoint URL clients use.
pub fn endpoint_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/mcp")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_random_base64url() {
        let a = generate_token();
        let b = generate_token();
        assert_eq!(a.len(), 43);
        assert_ne!(*a, *b);
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn token_lives_in_the_vault() {
        let dir = std::env::temp_dir().join(format!("pmxspan-mcp-vault-{}", crate::config::new_id()));
        let path = dir.join("vault.json");
        let mut v = Vault::create(&path, "correct horse battery").unwrap();
        assert!(!has_token(&v));
        let t = ensure_token(&mut v).unwrap();
        assert_eq!(*ensure_token(&mut v).unwrap(), *t, "an existing token is kept");
        assert_eq!(revealed_token().as_deref().map(String::as_str), Some(t.as_str()));
        let t2 = regenerate_token(&mut v).unwrap();
        assert_ne!(*t, *t2);
        // Sealed on disk, not in clear.
        assert!(!std::fs::read_to_string(&path).unwrap().contains(t2.as_str()));
        let mut reopened = Vault::open(&path).unwrap();
        reopened.unlock("correct horse battery").unwrap();
        assert_eq!(reopened.get(TOKEN_KEY).as_deref(), Some(t2.as_str()));
        hide_token();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disabled_host_does_nothing() {
        let dir = std::env::temp_dir().join(format!("pmxspan-mcp-host-{}", crate::config::new_id()));
        let paths = AppPaths { config_dir: dir.clone(), data_dir: dir.clone(), log_dir: dir.join("logs") };
        let mut host = McpHost::new(&paths);
        let mut v = Vault::create(&dir.join("vault.json"), "").unwrap();
        let ipam = Arc::new(Mutex::new(IpamDb::default()));
        let settings = Settings::default();
        assert!(!settings.mcp_enabled && !settings.mcp_allow_actions);
        assert_eq!(settings.mcp_port, DEFAULT_PORT);
        assert!(host.tick(&settings, &[], Some(&mut v), None, &ipam).is_none());
        assert!(host.running.is_none());
        assert!(!has_token(&v), "no token is generated while disabled");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
