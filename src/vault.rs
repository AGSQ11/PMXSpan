//! Encrypted storage for API token secrets and passwords.
//!
//! Secrets are sealed with XChaCha20-Poly1305 using a key derived from a
//! master password with Argon2id. A vault may also be created without a
//! master password; it is then only obfuscated (the key is derived from an
//! empty password, so anyone with the file can decrypt it) and the UI warns about it.
//!
//! Format version 2 stores the Argon2 parameters in the file and binds every
//! sealed value to its entry name (AEAD associated data), so values cannot be
//! swapped between servers. Version 1 vaults are upgraded on unlock.

use crate::config::write_atomic;
use anyhow::{Context, anyhow, bail};
use argon2::{Algorithm, Argon2, Params, Version};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

// Fixed format identifier from the app's first release (under its earlier
// name); changing it would make every existing vault unreadable.
const VERIFIER_PLAINTEXT: &[u8] = b"proxmoxpanel-vault-v1";
/// Associated data of the verifier in version 2; entries use their name.
const VERIFIER_AAD: &[u8] = b"verifier";
const VERSION: u32 = 2;

/// Argon2id cost parameters, stored in the file so they can be raised later.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
struct Kdf {
    /// Memory in KiB.
    m_cost: u32,
    t_cost: u32,
    p_cost: u32,
}

impl Kdf {
    /// Parameters for new and upgraded vaults: 64 MiB, 3 passes.
    fn current() -> Self {
        if cfg!(test) { Self { m_cost: 8 * 1024, t_cost: 1, p_cost: 1 } } else { Self { m_cost: 64 * 1024, t_cost: 3, p_cost: 1 } }
    }

    /// What version 1 used: the argon2 crate's defaults (19 MiB, 2 passes).
    fn legacy() -> Self {
        Self { m_cost: Params::DEFAULT_M_COST, t_cost: Params::DEFAULT_T_COST, p_cost: Params::DEFAULT_P_COST }
    }

    /// Rejects values a tampered file could use to stall or exhaust memory.
    fn check(self) -> anyhow::Result<Self> {
        if !(8 * 1024..=1024 * 1024).contains(&self.m_cost) || !(1..=10).contains(&self.t_cost) || !(1..=8).contains(&self.p_cost) {
            bail!("corrupt vault: unsupported key derivation parameters");
        }
        Ok(self)
    }
}

#[derive(Serialize, Deserialize, Default)]
struct VaultFile {
    version: u32,
    salt: String,
    protected: bool,
    /// Missing in version 1 (which used [`Kdf::legacy`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    kdf: Option<Kdf>,
    /// Encrypted known plaintext, used to check the master password.
    verifier: String,
    entries: BTreeMap<String, String>,
}

pub struct Vault {
    path: PathBuf,
    file: VaultFile,
    key: Option<Zeroizing<[u8; 32]>>,
}

impl Vault {
    pub fn exists(path: &Path) -> bool {
        path.exists()
    }

    /// Opens an existing vault file without unlocking it.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let s = std::fs::read_to_string(path).context("reading vault")?;
        let file: VaultFile = serde_json::from_str(&s).context("parsing vault")?;
        if file.version > VERSION {
            bail!("the vault was written by a newer PMXSpan (format {}); update the app", file.version);
        }
        Ok(Self { path: path.to_path_buf(), file, key: None })
    }

    pub fn create(path: &Path, master: &str) -> anyhow::Result<Self> {
        let mut v = Self { path: path.to_path_buf(), file: VaultFile::default(), key: None };
        v.rekey(master, BTreeMap::new())?;
        Ok(v)
    }

    pub fn is_protected(&self) -> bool {
        self.file.protected
    }

    pub fn is_unlocked(&self) -> bool {
        self.key.is_some()
    }

    /// Forgets the key; secrets need the master password again.
    pub fn lock(&mut self) {
        self.key = None;
    }

    pub fn unlock(&mut self, master: &str) -> anyhow::Result<()> {
        let kdf = match self.file.kdf {
            Some(k) if self.file.version >= 2 => k.check()?,
            _ => Kdf::legacy(),
        };
        let salt = B64.decode(&self.file.salt).context("corrupt vault salt")?;
        let key = derive_key(master, &salt, kdf)?;
        let plain = open(&key, &self.file.verifier, self.aad(VERIFIER_AAD)).map_err(|_| anyhow!("wrong master password"))?;
        if plain.as_slice() != VERIFIER_PLAINTEXT {
            bail!("wrong master password");
        }
        self.key = Some(key);
        if self.file.version < VERSION || kdf != Kdf::current() {
            // Older format or weaker parameters: re-seal under the current ones.
            let entries = self.decrypt_all()?;
            if let Err(e) = self.rekey(master, entries) {
                tracing::warn!("could not upgrade the vault format: {e:#}");
            }
        }
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<String> {
        let key = self.key.as_ref()?;
        let sealed = self.file.entries.get(name)?;
        open(key, sealed, self.aad(name.as_bytes())).ok().and_then(|b| String::from_utf8(b.to_vec()).ok())
    }

    pub fn set(&mut self, name: &str, secret: &str) -> anyhow::Result<()> {
        let key = self.key.as_ref().ok_or_else(|| anyhow!("vault is locked"))?;
        if secret.is_empty() {
            self.file.entries.remove(name);
        } else {
            let sealed = seal(key, secret.as_bytes(), self.aad(name.as_bytes()))?;
            self.file.entries.insert(name.to_string(), sealed);
        }
        self.save()
    }

    pub fn remove(&mut self, name: &str) -> anyhow::Result<()> {
        self.file.entries.remove(name);
        self.save()
    }

    /// Re-encrypts every entry under a new master password.
    pub fn change_master(&mut self, new_master: &str) -> anyhow::Result<()> {
        let entries = self.decrypt_all()?;
        self.rekey(new_master, entries)
    }

    /// Associated data for a sealed value: none in version 1.
    fn aad<'a>(&self, aad: &'a [u8]) -> &'a [u8] {
        if self.file.version >= 2 { aad } else { b"" }
    }

    fn decrypt_all(&self) -> anyhow::Result<BTreeMap<String, Zeroizing<Vec<u8>>>> {
        let key = self.key.as_ref().ok_or_else(|| anyhow!("vault is locked"))?;
        self.file
            .entries
            .iter()
            .map(|(k, v)| Ok((k.clone(), open(key, v, self.aad(k.as_bytes())).with_context(|| format!("entry {k}"))?)))
            .collect()
    }

    /// Writes the vault in the current format: new salt, current Argon2
    /// parameters, every entry sealed with its name as associated data.
    fn rekey(&mut self, master: &str, entries: BTreeMap<String, Zeroizing<Vec<u8>>>) -> anyhow::Result<()> {
        let mut salt = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut salt);
        let kdf = Kdf::current();
        let key = derive_key(master, &salt, kdf)?;
        let mut sealed = BTreeMap::new();
        for (k, plain) in &entries {
            sealed.insert(k.clone(), seal(&key, plain, k.as_bytes())?);
        }
        self.file = VaultFile {
            version: VERSION,
            salt: B64.encode(salt),
            protected: !master.is_empty(),
            kdf: Some(kdf),
            verifier: seal(&key, VERIFIER_PLAINTEXT, VERIFIER_AAD)?,
            entries: sealed,
        };
        self.key = Some(key);
        self.save()
    }

    fn save(&self) -> anyhow::Result<()> {
        write_atomic(&self.path, serde_json::to_string_pretty(&self.file)?.as_bytes())
    }
}

fn derive_key(master: &str, salt: &[u8], kdf: Kdf) -> anyhow::Result<Zeroizing<[u8; 32]>> {
    let params = Params::new(kdf.m_cost, kdf.t_cost, kdf.p_cost, Some(32)).map_err(|e| anyhow!("key derivation parameters: {e}"))?;
    let mut key = Zeroizing::new([0u8; 32]);
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(master.as_bytes(), salt, key.as_mut())
        .map_err(|e| anyhow!("key derivation failed: {e}"))?;
    Ok(key)
}

fn seal(key: &[u8; 32], plain: &[u8], aad: &[u8]) -> anyhow::Result<String> {
    let cipher = XChaCha20Poly1305::new(key.into());
    let mut nonce = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut nonce);
    let ct = cipher.encrypt(XNonce::from_slice(&nonce), Payload { msg: plain, aad }).map_err(|_| anyhow!("encryption failed"))?;
    let mut out = nonce.to_vec();
    out.extend_from_slice(&ct);
    Ok(B64.encode(out))
}

fn open(key: &[u8; 32], sealed: &str, aad: &[u8]) -> anyhow::Result<Zeroizing<Vec<u8>>> {
    let raw = B64.decode(sealed)?;
    if raw.len() < 24 {
        bail!("sealed value too short");
    }
    let (nonce, ct) = raw.split_at(24);
    let cipher = XChaCha20Poly1305::new(key.into());
    cipher.decrypt(XNonce::from_slice(nonce), Payload { msg: ct, aad }).map(Zeroizing::new).map_err(|_| anyhow!("decryption failed"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path() -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("pxp-vault-{}", crate::config::new_id()));
        (dir.join("vault.json"), dir)
    }

    #[test]
    fn roundtrip_and_wrong_password() {
        let (path, dir) = temp_path();
        let mut v = Vault::create(&path, "hunter22").unwrap();
        v.set("a", "secret-value").unwrap();
        let mut v2 = Vault::open(&path).unwrap();
        assert!(v2.unlock("nope").is_err());
        v2.unlock("hunter22").unwrap();
        assert_eq!(v2.get("a").as_deref(), Some("secret-value"));
        v2.lock();
        assert_eq!(v2.get("a"), None);
        v2.unlock("hunter22").unwrap();
        v2.change_master("").unwrap();
        let mut v3 = Vault::open(&path).unwrap();
        assert!(!v3.is_protected());
        v3.unlock("").unwrap();
        assert_eq!(v3.get("a").as_deref(), Some("secret-value"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn entries_are_bound_to_their_name() {
        let (path, dir) = temp_path();
        let mut v = Vault::create(&path, "hunter22").unwrap();
        v.set("server:a", "secret-a").unwrap();
        v.set("server:b", "secret-b").unwrap();
        // Swap the sealed values on disk: neither decrypts any more.
        let mut f: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let e = f["entries"].as_object_mut().unwrap();
        let a = e["server:a"].clone();
        e["server:a"] = e["server:b"].clone();
        e["server:b"] = a;
        std::fs::write(&path, f.to_string()).unwrap();
        let mut v = Vault::open(&path).unwrap();
        v.unlock("hunter22").unwrap();
        assert_eq!(v.get("server:a"), None);
        assert_eq!(v.get("server:b"), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn upgrades_version_1() {
        // A vault as written by version 1: default Argon2, no associated data.
        let (path, dir) = temp_path();
        let salt = [7u8; 16];
        let key = derive_key("hunter22", &salt, Kdf::legacy()).unwrap();
        let file = VaultFile {
            version: 1,
            salt: B64.encode(salt),
            protected: true,
            kdf: None,
            verifier: seal(&key, VERIFIER_PLAINTEXT, b"").unwrap(),
            entries: [("server:a".to_string(), seal(&key, b"secret-a", b"").unwrap())].into(),
        };
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, serde_json::to_string(&file).unwrap()).unwrap();

        let mut v = Vault::open(&path).unwrap();
        assert!(v.unlock("wrong").is_err());
        v.unlock("hunter22").unwrap();
        assert_eq!(v.get("server:a").as_deref(), Some("secret-a"));
        let on_disk: VaultFile = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(on_disk.version, VERSION);
        assert_eq!(on_disk.kdf, Some(Kdf::current()));
        let mut v = Vault::open(&path).unwrap();
        v.unlock("hunter22").unwrap();
        assert_eq!(v.get("server:a").as_deref(), Some("secret-a"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
