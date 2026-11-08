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
        if let Some(pd) = directories::ProjectDirs::from("com", "PMXSpan", "PMXSpan") {
            if let Some(old) = directories::ProjectDirs::from("com", "ProxmoxPanel", "ProxmoxPanel") {
                migrate_dir(old.config_dir(), pd.config_dir());
                migrate_dir(old.data_dir(), pd.data_dir());
            }
            let config_dir = pd.config_dir().to_path_buf();
            let data_dir = pd.data_dir().to_path_buf();
            let log_dir = data_dir.join("logs");
            for d in [&config_dir, &data_dir, &log_dir] {
                private_dir(d);
            }
            return Self { config_dir, data_dir, log_dir };
        }
        migrate_dir(Path::new(".proxmoxpanel"), Path::new(".pmxspan"));
        Self::from_base(PathBuf::from(".pmxspan"))
    }

    fn from_base(base: PathBuf) -> Self {
        let s = Self { config_dir: base.clone(), data_dir: base.clone(), log_dir: base.join("logs") };
        private_dir(&s.config_dir);
        private_dir(&s.log_dir);
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
    pub fn automation_file(&self) -> PathBuf {
        self.config_dir.join("automation.json")
    }
    /// SSH host keys read from guests (next to the IPAM database).
    pub fn hostkeys_file(&self) -> PathBuf {
        self.data_dir.join("hostkeys.json")
    }
    /// Whether everything is kept beside the executable (`portable` marker).
    #[cfg_attr(not(unix), allow(dead_code))]
    pub fn is_portable(&self) -> bool {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .is_some_and(|dir| dir.join("portable").exists() && self.data_dir == dir.join("data"))
    }
}

/// Moves data left by the app's earlier name (ProxmoxPanel) to the new
/// location, once: only when the old directory exists and the new one does not.
fn migrate_dir(old: &Path, new: &Path) {
    if new.exists() || !old.is_dir() {
        return;
    }
    if let Some(parent) = new.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::rename(old, new).is_err() {
        // Different volume or locked: copy instead and leave the old data in place.
        let _ = copy_dir(old, new);
    }
    // Remove the old app folders once empty (Windows: %APPDATA%\ProxmoxPanel\ProxmoxPanel).
    for dir in old.ancestors().skip(1).take(2) {
        if dir.file_name().is_some_and(|n| n == "ProxmoxPanel") {
            let _ = std::fs::remove_dir(dir);
        }
    }
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
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
    /// New VMIDs come from this range (0 = no lower / upper bound).
    pub vmid_min: u64,
    pub vmid_max: u64,
    /// Nodes auto-placement skips (kept locally; Proxmox is not told).
    pub maintenance_nodes: Vec<String>,
    /// Resource limits per tenant pool, enforced by PMXSpan.
    pub pool_limits: std::collections::BTreeMap<String, PoolLimits>,
}

/// Limits for one tenant pool; 0 = unlimited.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PoolLimits {
    pub guests: u64,
    pub cores: u64,
    pub memory_mb: u64,
    pub disk_gb: u64,
    pub ips: u64,
}

impl PoolLimits {
    pub fn is_unlimited(&self) -> bool {
        *self == Self::default()
    }
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
            vmid_min: 0,
            vmid_max: 0,
            maintenance_nodes: Vec::new(),
            pool_limits: Default::default(),
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

    /// The configured VMID range, if any bound is set.
    pub fn vmid_range(&self) -> Option<(u64, u64)> {
        if self.vmid_min == 0 && self.vmid_max == 0 {
            return None;
        }
        Some((self.vmid_min.max(100), if self.vmid_max == 0 { 999_999_999 } else { self.vmid_max }))
    }

    pub fn in_maintenance(&self, node: &str) -> bool {
        self.maintenance_nodes.iter().any(|n| n == node)
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

/// Tag added to guests created by the app.
pub const DEFAULT_MANAGED_TAG: &str = "PMXSpan";

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
    /// Lock a password-protected vault after this many idle minutes (0 = never).
    pub auto_lock_mins: u64,
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
    /// Offer a backup before every rebuild (checked by default in the dialog).
    pub backup_before_rebuild: bool,
    /// Preferred storage for those backups (empty = first backup storage).
    pub rebuild_backup_storage: String,
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
            auto_lock_mins: 30,
            theme: ThemePref::Dark,
            ui_scale: 1.0,
            task_timeout_secs: 3600,
            tag_managed_guests: true,
            managed_tag: DEFAULT_MANAGED_TAG.into(),
            download_stall_secs: 180,
            download_max_wait_secs: 1800,
            download_min_bytes: 0,
            backup_before_rebuild: true,
            rebuild_backup_storage: String::new(),
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
        let mut c: Self = match std::fs::read_to_string(path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
                tracing::error!("config file is invalid, starting with defaults: {e}");
                let _ = std::fs::copy(path, path.with_extension("json.bak"));
                Self::default()
            }),
            Err(_) => Self::default(),
        };
        c.migrate();
        c
    }

    /// Earlier versions saved their default tag (`proxmoxpanel`, then
    /// `pmxspan`); move those to the current one. A custom tag is kept.
    fn migrate(&mut self) {
        let tag = &mut self.settings.managed_tag;
        if ["proxmoxpanel", "pmxspan"].iter().any(|old| tag.trim().eq_ignore_ascii_case(old)) {
            *tag = DEFAULT_MANAGED_TAG.into();
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

/// Creates `dir` (and its parents) and makes it private to the current user
/// on unix (0700), including when it already existed with looser permissions.
pub fn private_dir(dir: &Path) {
    let _ = std::fs::create_dir_all(dir);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
}

/// Creates a new file that only the current user can read (0600 on unix).
/// Fails if the path already exists, so a planted file or symlink is never
/// written through.
pub fn create_private(path: &Path) -> std::io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o.open(path)
}

/// Write to a temp file then rename, so a crash never leaves half a config.
/// The temp file is private from the moment it exists and has a random name.
pub fn write_atomic(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let tmp = path.with_extension(format!("{ext}.{}.tmp", &new_id()[..12]));
    let written = create_private(&tmp).and_then(|mut f| {
        f.write_all(data)?;
        f.sync_all()
    });
    if let Err(e) = written.and_then(|()| std::fs::rename(&tmp, path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::migrate_dir;

    #[test]
    fn migrates_old_data_once() {
        let root = std::env::temp_dir().join(format!("pmxspan-migrate-{}", std::process::id()));
        let old = root.join("ProxmoxPanel").join("ProxmoxPanel").join("config");
        let new = root.join("PMXSpan").join("PMXSpan").join("config");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("vault.json"), b"secret").unwrap();

        migrate_dir(&old, &new);
        assert_eq!(std::fs::read(new.join("vault.json")).unwrap(), b"secret");
        assert!(!old.exists());
        // Empty old app folders are cleaned up.
        assert!(!root.join("ProxmoxPanel").exists());

        // Existing new data is never overwritten.
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("vault.json"), b"stale").unwrap();
        migrate_dir(&old, &new);
        assert_eq!(std::fs::read(new.join("vault.json")).unwrap(), b"secret");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn old_default_tags_are_migrated() {
        let dir = std::env::temp_dir().join(format!("pmxspan-cfg-{}", super::new_id()));
        let path = dir.join("config.json");
        std::fs::create_dir_all(&dir).unwrap();
        for (saved, want) in [("proxmoxpanel", "PMXSpan"), ("pmxspan", "PMXSpan"), ("customer-x", "customer-x")] {
            std::fs::write(&path, format!(r#"{{"settings": {{"managed_tag": "{saved}"}}}}"#)).unwrap();
            assert_eq!(super::AppConfig::load(&path).settings.managed_tag, want);
        }
        assert_eq!(super::AppConfig::load(&dir.join("missing.json")).settings.managed_tag, "PMXSpan");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_writes_are_private() {
        let dir = std::env::temp_dir().join(format!("pmxspan-atomic-{}", super::new_id()));
        let path = dir.join("vault.json");
        super::write_atomic(&path, b"one").unwrap();
        super::write_atomic(&path, b"two").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        // No temp files are left behind.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        // An existing path is never written through.
        assert!(super::create_private(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
