//! Encrypted storage for API token secrets and passwords.
//!
//! Secrets are sealed with XChaCha20-Poly1305 using a key derived from a
//! master password with Argon2id. A vault may also be created without a
//! master password; it is then only obfuscated (the key is derived from an
//! empty password, so anyone with the file can decrypt it) and the UI warns about it.

use crate::config::write_atomic;
use anyhow::{Context, anyhow, bail};
use argon2::Argon2;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

// Fixed format identifier from the app's first release (under its earlier
// name); changing it would make every existing vault unreadable.
const VERIFIER_PLAINTEXT: &[u8] = b"proxmoxpanel-vault-v1";

#[derive(Serialize, Deserialize, Default)]
struct VaultFile {
    version: u32,
    salt: String,
    protected: bool,
    /// Encrypted known plaintext, used to check the master password.
    verifier: String,
    entries: BTreeMap<String, String>,
}

pub struct Vault {
    path: PathBuf,
    file: VaultFile,
    key: Option<[u8; 32]>,
}

impl Vault {
    pub fn exists(path: &Path) -> bool {
        path.exists()
    }

    /// Opens an existing vault file without unlocking it.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let s = std::fs::read_to_string(path).context("reading vault")?;
        let file: VaultFile = serde_json::from_str(&s).context("parsing vault")?;
        Ok(Self { path: path.to_path_buf(), file, key: None })
    }

    pub fn create(path: &Path, master: &str) -> anyhow::Result<Self> {
        let mut salt = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut salt);
        let key = derive_key(master, &salt)?;
        let verifier = seal(&key, VERIFIER_PLAINTEXT)?;
        let file = VaultFile { version: 1, salt: B64.encode(salt), protected: !master.is_empty(), verifier, entries: BTreeMap::new() };
        let v = Self { path: path.to_path_buf(), file, key: Some(key) };
        v.save()?;
        Ok(v)
    }

    pub fn is_protected(&self) -> bool {
        self.file.protected
    }

    pub fn is_unlocked(&self) -> bool {
        self.key.is_some()
    }

    pub fn unlock(&mut self, master: &str) -> anyhow::Result<()> {
        let salt = B64.decode(&self.file.salt).context("corrupt vault salt")?;
        let key = derive_key(master, &salt)?;
        let plain = open(&key, &self.file.verifier).map_err(|_| anyhow!("wrong master password"))?;
        if plain != VERIFIER_PLAINTEXT {
            bail!("wrong master password");
        }
        self.key = Some(key);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<String> {
        let key = self.key.as_ref()?;
        let sealed = self.file.entries.get(name)?;
        open(key, sealed).ok().and_then(|b| String::from_utf8(b).ok())
    }

    pub fn set(&mut self, name: &str, secret: &str) -> anyhow::Result<()> {
        let key = self.key.ok_or_else(|| anyhow!("vault is locked"))?;
        if secret.is_empty() {
            self.file.entries.remove(name);
        } else {
            self.file.entries.insert(name.to_string(), seal(&key, secret.as_bytes())?);
        }
        self.save()
    }

    pub fn remove(&mut self, name: &str) -> anyhow::Result<()> {
        self.file.entries.remove(name);
        self.save()
    }

    /// Re-encrypts every entry under a new master password.
    pub fn change_master(&mut self, new_master: &str) -> anyhow::Result<()> {
        let old_key = self.key.ok_or_else(|| anyhow!("vault is locked"))?;
        let mut salt = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut salt);
        let new_key = derive_key(new_master, &salt)?;
        let mut entries = BTreeMap::new();
        for (k, v) in &self.file.entries {
            let plain = open(&old_key, v)?;
            entries.insert(k.clone(), seal(&new_key, &plain)?);
        }
        self.file.entries = entries;
        self.file.salt = B64.encode(salt);
        self.file.verifier = seal(&new_key, VERIFIER_PLAINTEXT)?;
        self.file.protected = !new_master.is_empty();
        self.key = Some(new_key);
        self.save()
    }

    fn save(&self) -> anyhow::Result<()> {
        write_atomic(&self.path, serde_json::to_string_pretty(&self.file)?.as_bytes())
    }
}

fn derive_key(master: &str, salt: &[u8]) -> anyhow::Result<[u8; 32]> {
    let mut key = [0u8; 32];
    Argon2::default().hash_password_into(master.as_bytes(), salt, &mut key).map_err(|e| anyhow!("key derivation failed: {e}"))?;
    Ok(key)
}

fn seal(key: &[u8; 32], plain: &[u8]) -> anyhow::Result<String> {
    let cipher = XChaCha20Poly1305::new(key.into());
    let mut nonce = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut nonce);
    let ct = cipher.encrypt(XNonce::from_slice(&nonce), plain).map_err(|_| anyhow!("encryption failed"))?;
    let mut out = nonce.to_vec();
    out.extend_from_slice(&ct);
    Ok(B64.encode(out))
}

fn open(key: &[u8; 32], sealed: &str) -> anyhow::Result<Vec<u8>> {
    let raw = B64.decode(sealed)?;
    if raw.len() < 24 {
        bail!("sealed value too short");
    }
    let (nonce, ct) = raw.split_at(24);
    let cipher = XChaCha20Poly1305::new(key.into());
    cipher.decrypt(XNonce::from_slice(nonce), ct).map_err(|_| anyhow!("decryption failed"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_wrong_password() {
        let dir = std::env::temp_dir().join(format!("pxp-vault-{}", crate::config::new_id()));
        let path = dir.join("vault.json");
        let mut v = Vault::create(&path, "hunter22").unwrap();
        v.set("a", "secret-value").unwrap();
        let mut v2 = Vault::open(&path).unwrap();
        assert!(v2.unlock("nope").is_err());
        v2.unlock("hunter22").unwrap();
        assert_eq!(v2.get("a").as_deref(), Some("secret-value"));
        v2.change_master("").unwrap();
        let mut v3 = Vault::open(&path).unwrap();
        assert!(!v3.is_protected());
        v3.unlock("").unwrap();
        assert_eq!(v3.get("a").as_deref(), Some("secret-value"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
