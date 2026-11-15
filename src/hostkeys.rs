//! Guest SSH host keys (`hostkeys.json` in the data directory).
//!
//! Keys are read from VMs through the guest agent (`cat` of
//! `/etc/ssh/ssh_host_*_key.pub`), checked (type prefix = type inside the
//! key) and stored per (server profile, VMID) with their SHA-256
//! fingerprints. After a PMXSpan rebuild a different key set is recorded as a
//! rotation (old set kept in `previous`). Containers and nodes have no key
//! source in the Proxmox API.
//!
//! The store is useful with any SSH client: the Summary shows fingerprints to
//! compare on first connect.

use crate::api::PveClient;
use crate::api::types::{GuestKind, GuestRef};
use crate::config::write_atomic;
use crate::sshkey;
use crate::util::ValueExt;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HostKey {
    #[serde(rename = "type")]
    pub key_type: String,
    /// Base64 wire-format key.
    pub key: String,
    pub fingerprint_sha256: String,
    /// Unix seconds.
    pub fetched_at: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HostKeyRecord {
    pub server_id: String,
    pub vmid: u64,
    pub keys: Vec<HostKey>,
    /// The set before the current one.
    pub previous: Vec<HostKey>,
    /// Set when a PMXSpan rebuild produced a different set.
    pub rotated_at: Option<i64>,
    /// Set when the keys changed without a PMXSpan rebuild (worth a look).
    pub changed_at: Option<i64>,
    /// Catalog image the guest was created or rebuilt from (for its default user).
    pub catalog_id: String,
}

impl HostKeyRecord {
    /// Unix seconds of the last successful read.
    pub fn fetched_at(&self) -> i64 {
        self.keys.iter().map(|k| k.fetched_at).max().unwrap_or(0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    New,
    Unchanged,
    /// Changed after a PMXSpan rebuild.
    Rotated,
    /// Changed with no rebuild to explain it.
    Changed,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HostKeyStore {
    pub version: u32,
    pub records: Vec<HostKeyRecord>,
    #[serde(skip)]
    path: PathBuf,
}

fn same_set(a: &[HostKey], b: &[HostKey]) -> bool {
    let set = |v: &[HostKey]| v.iter().map(|k| (k.key_type.clone(), k.key.clone())).collect::<std::collections::BTreeSet<_>>();
    set(a) == set(b)
}

impl HostKeyStore {
    pub fn load(path: &Path) -> Self {
        let mut s: Self = match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                tracing::error!("{} is invalid, starting empty: {e}", path.display());
                let _ = std::fs::copy(path, path.with_extension("json.bak"));
                Self::default()
            }),
            Err(_) => Self::default(),
        };
        s.version = 1;
        s.path = path.to_path_buf();
        s
    }

    pub fn save(&self) -> anyhow::Result<()> {
        write_atomic(&self.path, serde_json::to_string_pretty(self)?.as_bytes())
    }

    pub fn get(&self, server_id: &str, vmid: u64) -> Option<&HostKeyRecord> {
        self.records.iter().find(|r| r.server_id == server_id && r.vmid == vmid)
    }

    fn entry(&mut self, server_id: &str, vmid: u64) -> &mut HostKeyRecord {
        if let Some(i) = self.records.iter().position(|r| r.server_id == server_id && r.vmid == vmid) {
            return &mut self.records[i];
        }
        self.records.push(HostKeyRecord { server_id: server_id.into(), vmid, ..Default::default() });
        self.records.last_mut().expect("just pushed")
    }

    /// Records a freshly read key set.
    pub fn update(&mut self, server_id: &str, vmid: u64, keys: Vec<HostKey>, after_rebuild: bool, now: i64) -> Outcome {
        let r = self.entry(server_id, vmid);
        if r.keys.is_empty() {
            r.keys = keys;
            return Outcome::New;
        }
        if same_set(&r.keys, &keys) {
            for k in r.keys.iter_mut() {
                k.fetched_at = now;
            }
            return Outcome::Unchanged;
        }
        r.previous = std::mem::replace(&mut r.keys, keys);
        if after_rebuild {
            r.rotated_at = Some(now);
            r.changed_at = None;
            Outcome::Rotated
        } else {
            // Not vouched for by a rebuild: no rotation claim is made.
            r.rotated_at = None;
            r.changed_at = Some(now);
            Outcome::Changed
        }
    }

    pub fn set_catalog(&mut self, server_id: &str, vmid: u64, catalog_id: &str) {
        self.entry(server_id, vmid).catalog_id = catalog_id.to_string();
    }

    /// Forgets a guest (destroyed).
    pub fn forget(&mut self, server_id: &str, vmid: u64) -> bool {
        let n = self.records.len();
        self.records.retain(|r| !(r.server_id == server_id && r.vmid == vmid));
        n != self.records.len()
    }
}

// ---- process-wide store (the window app only) --------------------------------

static STORE: OnceLock<Mutex<HostKeyStore>> = OnceLock::new();

/// Loads the store; called once by the window app (never by `--run-schedules`).
pub fn init(path: &Path) {
    let _ = STORE.set(Mutex::new(HostKeyStore::load(path)));
}

/// The store, when the window app initialised it.
pub fn global() -> Option<&'static Mutex<HostKeyStore>> {
    STORE.get()
}

/// Drops a destroyed guest's record.
pub fn forget(server_id: &str, vmid: u64) {
    if let Some(s) = global() {
        let mut st = s.lock();
        if st.forget(server_id, vmid)
            && let Err(e) = st.save()
        {
            tracing::warn!("saving host keys failed: {e:#}");
        }
    }
}

// ---- reading keys from a guest --------------------------------------------------

/// Host key files tried one by one when the glob read gives nothing.
const KEY_FILES: [&str; 3] = ["/etc/ssh/ssh_host_ed25519_key.pub", "/etc/ssh/ssh_host_ecdsa_key.pub", "/etc/ssh/ssh_host_rsa_key.pub"];

/// Parses `cat` output; returns the valid keys (one per type) and the reasons
/// lines were refused.
pub fn parse_output(out: &str, now: i64) -> (Vec<HostKey>, Vec<String>) {
    let mut keys: Vec<HostKey> = Vec::new();
    let mut refused = Vec::new();
    for line in out.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        match sshkey::parse_line(line) {
            Ok(k) => {
                if !keys.iter().any(|x| x.key_type == k.key_type) {
                    keys.push(HostKey { key_type: k.key_type, key: k.base64, fingerprint_sha256: k.fingerprint, fetched_at: now });
                }
            }
            Err(e) => refused.push(e.to_string()),
        }
    }
    (keys, refused)
}

async fn exec_out(client: &PveClient, g: &GuestRef, argv: &[&str]) -> anyhow::Result<String> {
    let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
    let st = crate::ops::guest::agent_exec(client, g, &argv, None).await?;
    Ok(st.s("out-data"))
}

/// Reads the guest's host keys through its agent.
pub async fn fetch(client: &PveClient, g: &GuestRef) -> anyhow::Result<Vec<HostKey>> {
    if g.kind != GuestKind::Qemu {
        anyhow::bail!("host keys can only be read from VMs with a guest agent");
    }
    let now = chrono::Utc::now().timestamp();
    let out = exec_out(client, g, &["sh", "-c", "cat /etc/ssh/ssh_host_*_key.pub 2>/dev/null"]).await.unwrap_or_default();
    let (mut keys, mut refused) = parse_output(&out, now);
    if keys.is_empty() {
        let mut all = String::new();
        for f in KEY_FILES {
            if let Ok(o) = exec_out(client, g, &["cat", f]).await {
                all.push_str(&o);
                all.push('\n');
            }
        }
        (keys, refused) = parse_output(&all, now);
    }
    for r in &refused {
        tracing::warn!("host key of {} {} refused: {r}", g.kind.label(), g.vmid);
    }
    if keys.is_empty() {
        anyhow::bail!(match refused.first() {
            Some(r) => format!("no usable SSH host key found ({r})"),
            None => "no SSH host keys found (is the SSH server installed?)".into(),
        });
    }
    Ok(keys)
}

/// Reads and stores a guest's keys now (the Summary's Refresh button).
pub async fn refresh(client: &PveClient, server_id: &str, g: &GuestRef) -> anyhow::Result<Outcome> {
    let keys = fetch(client, g).await?;
    let store = global().ok_or_else(|| anyhow::anyhow!("the host key store is not available"))?;
    let mut st = store.lock();
    let outcome = st.update(server_id, g.vmid, keys, false, chrono::Utc::now().timestamp());
    st.save()?;
    Ok(outcome)
}

const POLL_FOR: Duration = Duration::from_secs(180);
const POLL_EVERY: Duration = Duration::from_secs(10);

/// After PMXSpan created or rebuilt a VM: waits up to 3 minutes for the guest
/// agent in the background, then records the keys. Gives up quietly.
pub fn after_provision(client: &PveClient, ctx: &crate::jobs::JobCtx, server_id: &str, g: &GuestRef, catalog_id: &str, rebuild: bool) {
    let Some(store) = global() else { return };
    if g.kind != GuestKind::Qemu {
        return;
    }
    if !catalog_id.is_empty() {
        let mut st = store.lock();
        st.set_catalog(server_id, g.vmid, catalog_id);
        let _ = st.save();
    }
    ctx.info("SSH host keys are read in the background once the guest agent answers (up to 3 minutes)");
    let (client, server_id, g) = (client.clone(), server_id.to_string(), g.clone());
    crate::rt::spawn(async move {
        let deadline = tokio::time::Instant::now() + POLL_FOR;
        loop {
            if crate::ops::guest::agent_ping(&client, &g).await.is_ok()
                && let Ok(keys) = fetch(&client, &g).await
            {
                let outcome = {
                    let mut st = store.lock();
                    let o = st.update(&server_id, g.vmid, keys, rebuild, chrono::Utc::now().timestamp());
                    if let Err(e) = st.save() {
                        tracing::warn!("saving host keys failed: {e:#}");
                    }
                    o
                };
                tracing::info!("SSH host keys of VM {} recorded ({outcome:?})", g.vmid);
                crate::rt::bump_generation();
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                tracing::info!("SSH host keys of VM {}: guest agent did not answer within 3 minutes; not recorded", g.vmid);
                return;
            }
            tokio::time::sleep(POLL_EVERY).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sshkey::tests::{GH_ECDSA, GH_ECDSA_FP, GH_ED25519, GH_ED25519_FP};

    fn keys(list: &[(&str, &str)], at: i64) -> Vec<HostKey> {
        list.iter()
            .map(|(t, k)| HostKey { key_type: t.to_string(), key: k.to_string(), fingerprint_sha256: String::new(), fetched_at: at })
            .collect()
    }

    #[test]
    fn parses_cat_output() {
        let out = format!(
            "ssh-ed25519 {GH_ED25519} root@web01\n\necdsa-sha2-nistp256 {GH_ECDSA} root@web01\nssh-ed25519 {GH_ECDSA} forged\n\
             ssh-rsa !!!notbase64 x\nssh-ed25519 {GH_ED25519} duplicate\n"
        );
        let (k, refused) = parse_output(&out, 42);
        assert_eq!(k.len(), 2);
        assert_eq!(k[0].fingerprint_sha256, GH_ED25519_FP);
        assert_eq!(k[1].fingerprint_sha256, GH_ECDSA_FP);
        assert_eq!(k[1].fetched_at, 42);
        assert_eq!(refused.len(), 2);
        assert!(refused[0].contains("the key inside is ecdsa-sha2-nistp256"), "{refused:?}");
        assert!(refused[1].contains("base64"));
    }

    #[test]
    fn detects_rotation_only_after_a_rebuild() {
        let dir = std::env::temp_dir().join(format!("pmxspan-hk-{}", crate::config::new_id()));
        let path = dir.join("hostkeys.json");
        let mut st = HostKeyStore::load(&path);
        let a = keys(&[("ssh-ed25519", GH_ED25519)], 10);
        let b = keys(&[("ecdsa-sha2-nistp256", GH_ECDSA)], 20);
        assert_eq!(st.update("s", 100, a.clone(), false, 10), Outcome::New);
        assert_eq!(st.update("s", 100, a.clone(), true, 15), Outcome::Unchanged);
        assert_eq!(st.get("s", 100).unwrap().fetched_at(), 15);
        assert_eq!(st.get("s", 100).unwrap().rotated_at, None);

        assert_eq!(st.update("s", 100, b.clone(), true, 30), Outcome::Rotated);
        let r = st.get("s", 100).unwrap();
        assert_eq!(r.rotated_at, Some(30));
        assert_eq!(r.previous[0].key, GH_ED25519);
        assert_eq!(r.keys[0].key, GH_ECDSA);

        // A change nobody rebuilt for is not a rotation.
        assert_eq!(st.update("s", 100, a.clone(), false, 40), Outcome::Changed);
        let r = st.get("s", 100).unwrap();
        assert_eq!((r.rotated_at, r.changed_at), (None, Some(40)));

        // Per server profile.
        assert_eq!(st.update("other", 100, b, false, 50), Outcome::New);
        st.set_catalog("s", 100, "debian-12");
        st.save().unwrap();
        let back = HostKeyStore::load(&path);
        assert_eq!(back.records.len(), 2);
        assert_eq!(back.get("s", 100).unwrap().catalog_id, "debian-12");
        let mut back = back;
        assert!(back.forget("s", 100));
        assert!(back.get("s", 100).is_none() && back.get("other", 100).is_some());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
