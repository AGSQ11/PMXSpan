//! Persistent, non-secret application configuration: server profiles and
//! operator preferences. Secrets live separately in the [`crate::vault`].

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct AppPaths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub log_dir: PathBuf,
}

impl AppPaths {
    pub fn resolve() -> Self {
        // A `portable` marker next to the executable keeps everything beside it
        // (handy for USB sticks / jump hosts).
        if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf))
            && dir.join("portable").exists()
        {
            let base = dir.join("data");
            return Self::from_base(base);
        }
        if let Some(pd) = directories::ProjectDirs::from("com", "ProxmoxPanel", "ProxmoxPanel") {
            let config_dir = pd.config_dir().to_path_buf();
            let data_dir = pd.data_dir().to_path_buf();
            let log_dir = data_dir.join("logs");
            for d in [&config_dir, &data_dir, &log_dir] {
                let _ = std::fs::create_dir_all(d);
            }
            return Self { config_dir, data_dir, log_dir };
        }
        Self::from_base(PathBuf::from(".proxmoxpanel"))
    }

    fn from_base(base: PathBuf) -> Self {
        let s = Self { config_dir: base.clone(), data_dir: base.clone(), log_dir: base.join("logs") };
        let _ = std::fs::create_dir_all(&s.log_dir);
        s
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.json")
    }
    pub fn vault_file(&self) -> PathBuf {
        self.config_dir.join("vault.json")
    }
    pub fn ipam_file(&self) -> PathBuf {
        self.data_dir.join("ipam.json")
    }
    pub fn catalog_file(&self) -> PathBuf {
        self.config_dir.join("catalog.json")
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthMode {
    #[default]
    ApiToken,
    Password,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TlsMode {
    /// Verify against the OS trust store (CA-signed certificates).
    System,
    /// Accept only a certificate whose SHA-256 fingerprint matches.
    #[default]
    Pinned,
    /// Accept anything. Only for labs.
    Insecure,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerProfile {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub auth: AuthMode,
    /// `root` in `root@pam`.
    pub username: String,
    /// `pam`, `pve`, or a configured LDAP/AD realm.
    pub realm: String,
    /// Token id (the part after `!`) when using API token auth.
    pub token_id: String,
    pub tls: TlsMode,
    /// Hex SHA-256 fingerprint (colons optional) for [`TlsMode::Pinned`].
    pub fingerprint: String,
    /// Ask for a TOTP code when logging in with a password.
    pub totp: bool,
    pub request_timeout_secs: u64,
    pub notes: String,
    /// OS image catalog download storages (content `import`), main then fallback.
    /// Empty = any active storage with `import` content.
    pub catalog_qemu_storage: String,
    pub catalog_qemu_storage_fallback: String,
    /// Container template download storages (content `vztmpl`), main then fallback.
    pub catalog_lxc_storage: String,
    pub catalog_lxc_storage_fallback: String,
}

impl Default for ServerProfile {
    fn default() -> Self {
        Self {
            id: new_id(),
            name: String::new(),
            host: String::new(),
            port: 8006,
            auth: AuthMode::ApiToken,
            username: "root".into(),
            realm: "pam".into(),
            token_id: String::new(),
            tls: TlsMode::Pinned,
            fingerprint: String::new(),
            totp: false,
            request_timeout_secs: 30,
            notes: String::new(),
            catalog_qemu_storage: String::new(),
            catalog_qemu_storage_fallback: String::new(),
            catalog_lxc_storage: String::new(),
            catalog_lxc_storage_fallback: String::new(),
        }
    }
}

impl ServerProfile {
    pub fn userid(&self) -> String {
        format!("{}@{}", self.username.trim(), self.realm.trim())
    }

    pub fn display_name(&self) -> String {
        if self.name.trim().is_empty() { self.host.clone() } else { self.name.clone() }
    }

    pub fn web_url(&self) -> String {
        format!("https://{}:{}", self.host.trim(), self.port)
    }

    pub fn secret_key(&self) -> String {
        format!("server:{}", self.id)
    }
}

/// How auto-placement picks a node when creating a guest.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlacementStrategy {
    /// Fill the most-loaded node that still fits (consolidate).
    Dense,
    /// Use the least-loaded node (spread).
    #[default]
    Sparse,
    Random,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Resource {
    Ram,
    Cpu,
    Disk,
}

impl Resource {
    pub fn label(self) -> &'static str {
        match self {
            Resource::Ram => "RAM",
            Resource::Cpu => "CPU",
            Resource::Disk => "Disk",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThemePref {
    #[default]
    Dark,
    Light,
    System,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub refresh_secs: u64,
    pub placement: PlacementStrategy,
    pub resource_priority: Vec<Resource>,
    /// Prefix for generated NIC MAC addresses.
    pub mac_prefix: String,
    pub default_bridge: String,
    pub default_dns: String,
    pub default_search_domain: String,
    /// SSH public keys offered by default in the create wizard.
    pub default_ssh_keys: String,
    pub confirm_destructive: bool,
    pub theme: ThemePref,
    pub ui_scale: f32,
    /// Wait this long for a task before giving up watching (it keeps running server-side).
    pub task_timeout_secs: u64,
    /// Fill new guests' description with a managed-by marker.
    pub tag_managed_guests: bool,
    pub managed_tag: String,
    /// Abort a catalog download when its task log shows no progress for this long.
    pub download_stall_secs: u64,
    /// Hard limit for one catalog download.
    pub download_max_wait_secs: u64,
    /// Free-space floor for catalog downloads (0 = 3 GiB VM images / 1 GiB templates).
    pub download_min_bytes: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            refresh_secs: 5,
            placement: PlacementStrategy::Sparse,
            resource_priority: vec![Resource::Ram, Resource::Cpu, Resource::Disk],
            mac_prefix: "BC:24:11".into(),
            default_bridge: "vmbr0".into(),
            default_dns: String::new(),
            default_search_domain: String::new(),
            default_ssh_keys: String::new(),
            confirm_destructive: true,
            theme: ThemePref::Dark,
            ui_scale: 1.0,
            task_timeout_secs: 3600,
            tag_managed_guests: true,
            managed_tag: "proxmoxpanel".into(),
            download_stall_secs: 180,
            download_max_wait_secs: 1800,
            download_min_bytes: 0,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub profiles: Vec<ServerProfile>,
    pub last_profile: Option<String>,
    pub settings: Settings,
}

impl AppConfig {
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
                tracing::error!("config file is invalid, starting with defaults: {e}");
                let _ = std::fs::copy(path, path.with_extension("json.bak"));
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        write_atomic(path, serde_json::to_string_pretty(self)?.as_bytes())
    }

    pub fn profile(&self, id: &str) -> Option<&ServerProfile> {
        self.profiles.iter().find(|p| p.id == id)
    }
}

pub fn new_id() -> String {
    use rand::Rng;
    let v: u128 = rand::thread_rng().r#gen();
    format!("{v:032x}")
}

/// Write to a temp file then rename, so a crash never leaves half a config.
pub fn write_atomic(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}
