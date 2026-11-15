//! The bulk handoff to SSHSpan (contract section 2.2): a JSON bundle written
//! to a private per-user directory, announced with `sshspan://import`.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const FORMAT: &str = "pmxspan-sshspan-handoff";
pub const VERSION: u32 = 1;
/// Bundles left behind longer than this are removed at startup.
pub const MAX_AGE: Duration = Duration::from_secs(24 * 3600);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Bundle {
    pub format: String,
    pub version: u32,
    pub created_at: String,
    pub pmxspan_version: String,
    pub cluster: BundleCluster,
    pub hosts: Vec<BundleHost>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BundleCluster {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BundleHost {
    /// `vm`, `ct` or `node`.
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub vmid: Option<u64>,
    pub node: String,
    pub name: String,
    pub pool: Option<String>,
    pub tags: Vec<String>,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub host_keys: Vec<BundleHostKey>,
    pub host_key_rotated_at: Option<String>,
    pub category: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BundleHostKey {
    #[serde(rename = "type")]
    pub key_type: String,
    pub key: String,
    pub fetched_at: String,
}

/// `2026-11-08T12:00:00Z`.
pub fn rfc3339(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0).unwrap_or_default().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

impl Bundle {
    pub fn new(cluster: BundleCluster, hosts: Vec<BundleHost>) -> Self {
        Self {
            format: FORMAT.into(),
            version: VERSION,
            created_at: rfc3339(chrono::Utc::now().timestamp()),
            pmxspan_version: env!("CARGO_PKG_VERSION").into(),
            cluster,
            hosts,
        }
    }
}

/// The private handoff directory for this platform.
pub fn handoff_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from)?;
        return Some(base.join("PMXSpan").join("handoff"));
    }
    if cfg!(target_os = "macos") {
        let home = directories::UserDirs::new()?.home_dir().to_path_buf();
        return Some(home.join("Library").join("Application Support").join("pmxspan").join("handoff"));
    }
    if let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|p| p.is_absolute() && p.is_dir()) {
        return Some(rt.join("pmxspan").join("handoff"));
    }
    let home = directories::UserDirs::new()?.home_dir().to_path_buf();
    Some(home.join(".local").join("share").join("pmxspan").join("handoff"))
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// Writes the bundle as a new private file (0600, never through an existing
/// path) named `<random 16 hex>.json` in `dir` (created 0700). Returns the
/// absolute path and the SHA-256 of the bytes written.
pub fn write(dir: &Path, bundle: &Bundle) -> anyhow::Result<(PathBuf, String)> {
    use std::io::Write;
    crate::config::private_dir(dir);
    let dir = std::fs::canonicalize(dir).map_err(|e| anyhow::anyhow!("handoff folder {}: {e}", dir.display()))?;
    let data = serde_json::to_vec_pretty(bundle)?;
    let name = format!("{}.json", &crate::config::new_id()[..16]);
    let path = dir.join(name);
    let mut f = crate::config::create_private(&path).map_err(|e| anyhow::anyhow!("creating {}: {e}", path.display()))?;
    f.write_all(&data)?;
    f.sync_all()?;
    Ok((path, sha256_hex(&data)))
}

/// Removes `*.json` bundles older than `max_age` from `dir`; returns how many.
pub fn cleanup(dir: &Path, max_age: Duration) -> usize {
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    let mut n = 0;
    for e in rd.flatten() {
        let p = e.path();
        let Ok(meta) = std::fs::symlink_metadata(&p) else { continue };
        let old = meta.modified().ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age >= max_age);
        if meta.is_file() && old && p.extension().is_some_and(|x| x == "json") && std::fs::remove_file(&p).is_ok() {
            n += 1;
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Bundle {
        let mut b = Bundle::new(
            BundleCluster { id: "abc".into(), name: "lab".into(), host: "pve.example".into(), port: 8006 },
            vec![
                BundleHost {
                    kind: "vm".into(),
                    vmid: Some(100),
                    node: "pve1".into(),
                    name: "web01".into(),
                    pool: Some("customers".into()),
                    tags: vec!["PMXSpan".into(), "web".into()],
                    host: "10.0.1.5".into(),
                    port: 22,
                    user: "debian".into(),
                    host_keys: vec![BundleHostKey {
                        key_type: "ssh-ed25519".into(),
                        key: crate::sshkey::tests::GH_ED25519.into(),
                        fetched_at: rfc3339(1_762_603_140),
                    }],
                    host_key_rotated_at: None,
                    category: vec!["Proxmox".into(), "lab".into(), "customers".into()],
                },
                BundleHost {
                    kind: "node".into(),
                    vmid: None,
                    node: "pve1".into(),
                    name: "pve1".into(),
                    pool: None,
                    tags: vec![],
                    host: "192.168.1.10".into(),
                    port: 22,
                    user: "root".into(),
                    host_keys: vec![],
                    host_key_rotated_at: None,
                    category: vec!["Proxmox".into(), "lab".into(), "Nodes".into()],
                },
            ],
        );
        b.created_at = "2026-11-08T12:00:00Z".into();
        b
    }

    #[test]
    fn serializes_the_v1_shape() {
        let v = serde_json::to_value(sample()).unwrap();
        assert_eq!(v["format"], "pmxspan-sshspan-handoff");
        assert_eq!(v["version"], 1);
        assert_eq!(v["pmxspan_version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(v["cluster"]["port"], 8006);
        let h = &v["hosts"][0];
        assert_eq!(h["vmid"], 100);
        assert_eq!(h["host_keys"][0]["type"], "ssh-ed25519");
        assert_eq!(h["host_keys"][0]["fetched_at"], "2025-11-08T11:59:00Z");
        assert!(h["host_key_rotated_at"].is_null());
        // Nodes have no vmid member at all.
        assert!(v["hosts"][1].get("vmid").is_none());
        assert!(v["hosts"][1]["pool"].is_null());
        // Round trip, tolerant of unknown members.
        let mut extra = v.clone();
        extra["future"] = serde_json::json!(true);
        let back: Bundle = serde_json::from_value(extra).unwrap();
        assert_eq!(back, sample());
    }

    #[test]
    fn writes_private_files_with_matching_sha256() {
        let dir = std::env::temp_dir().join(format!("pmxspan-handoff-{}", crate::config::new_id())).join("handoff");
        let (path, sum) = write(&dir, &sample()).unwrap();
        assert!(path.is_absolute());
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(sum, sha256_hex(&bytes));
        assert_eq!(sum.len(), 64);
        let name = path.file_stem().unwrap().to_str().unwrap();
        assert!(name.len() == 16 && name.bytes().all(|b| b.is_ascii_hexdigit()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        }
        let url = crate::integrations::link::import_url(&path, &sum).unwrap();
        assert!(url.ends_with(&format!("sha256={sum}")));
        // Fresh bundles survive the cleanup, old ones do not.
        assert_eq!(cleanup(&dir, MAX_AGE), 0);
        assert_eq!(cleanup(&dir, Duration::ZERO), 1);
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }
}
