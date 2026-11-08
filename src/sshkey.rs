//! OpenSSH public key lines (`type base64 [comment]`): parsing, wire-format
//! checks and SHA-256 fingerprints as `ssh-keygen -l` prints them.
//!
//! Used for guest host keys, the public-key picker and keys offered by
//! SSHSpan. Only public material is ever handled here.

use base64::Engine;
use sha2::{Digest, Sha256};

/// Key types PMXSpan accepts, for user keys and host keys alike.
pub const ALLOWED_TYPES: [&str; 5] = ["ssh-ed25519", "ssh-rsa", "ecdsa-sha2-nistp256", "ecdsa-sha2-nistp384", "ecdsa-sha2-nistp521"];

/// Longest line accepted (a 16384-bit RSA key is about 2.8 KiB).
pub const MAX_LINE: usize = 16 * 1024;

/// A validated public key line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicKey {
    pub key_type: String,
    /// Base64 of the wire-format key (the second field of the line).
    pub base64: String,
    pub comment: String,
    /// `SHA256:<base64 without padding>`.
    pub fingerprint: String,
}

impl PublicKey {
    /// The line as it goes into `authorized_keys` / cloud-init.
    pub fn line(&self) -> String {
        if self.comment.is_empty() {
            format!("{} {}", self.key_type, self.base64)
        } else {
            format!("{} {} {}", self.key_type, self.base64, self.comment)
        }
    }

    /// Short type name for display (`ed25519`, `rsa`, `ecdsa-p256`).
    pub fn short_type(&self) -> &str {
        short_type(&self.key_type)
    }
}

pub fn short_type(t: &str) -> &str {
    match t {
        "ssh-ed25519" => "ed25519",
        "ssh-rsa" => "rsa",
        "ecdsa-sha2-nistp256" => "ecdsa-p256",
        "ecdsa-sha2-nistp384" => "ecdsa-p384",
        "ecdsa-sha2-nistp521" => "ecdsa-p521",
        other => other,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyError {
    Empty,
    TooLong,
    UnsupportedType(String),
    MissingKey,
    BadBase64,
    /// The type inside the decoded key differs from the line's prefix.
    TypeMismatch {
        line: String,
        inner: String,
    },
    Malformed,
    ControlChars,
}

impl std::fmt::Display for KeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeyError::Empty => write!(f, "the key line is empty"),
            KeyError::TooLong => write!(f, "the key line is longer than {} KiB", MAX_LINE / 1024),
            KeyError::UnsupportedType(t) => {
                write!(f, "key type '{}' is not supported (use {})", truncate(t, 40), ALLOWED_TYPES.join(", "))
            }
            KeyError::MissingKey => write!(f, "the key data is missing after the type"),
            KeyError::BadBase64 => write!(f, "the key data is not valid base64"),
            KeyError::TypeMismatch { line, inner } => {
                write!(f, "the line says {line} but the key inside is {}", truncate(inner, 40))
            }
            KeyError::Malformed => write!(f, "the key data is not an SSH public key"),
            KeyError::ControlChars => write!(f, "the key line contains control characters"),
        }
    }
}

impl std::error::Error for KeyError {}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { format!("{}…", s.chars().take(n).collect::<String>()) }
}

/// `SHA256:<base64 without padding>` of a wire-format key.
pub fn fingerprint(blob: &[u8]) -> String {
    let digest = Sha256::digest(blob);
    format!("SHA256:{}", base64::engine::general_purpose::STANDARD_NO_PAD.encode(digest))
}

fn decode_b64(s: &str) -> Option<Vec<u8>> {
    use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
    STANDARD.decode(s).or_else(|_| STANDARD_NO_PAD.decode(s)).ok()
}

/// Reads one SSH `string` (u32 length + bytes) at `pos`.
fn read_string<'a>(blob: &'a [u8], pos: &mut usize) -> Option<&'a [u8]> {
    let len_bytes = blob.get(*pos..*pos + 4)?;
    let len = u32::from_be_bytes([len_bytes[0], len_bytes[1], len_bytes[2], len_bytes[3]]) as usize;
    let start = *pos + 4;
    let s = blob.get(start..start.checked_add(len)?)?;
    *pos = start + len;
    Some(s)
}

/// Checks a decoded wire-format key and returns the type string inside it.
fn inner_type(blob: &[u8]) -> Result<String, KeyError> {
    let mut pos = 0;
    let t = read_string(blob, &mut pos).ok_or(KeyError::Malformed)?;
    let t = std::str::from_utf8(t).map_err(|_| KeyError::Malformed)?.to_string();
    // Every supported type carries at least one more field.
    let first = read_string(blob, &mut pos).ok_or(KeyError::Malformed)?;
    if t == "ssh-ed25519" && (first.len() != 32 || pos != blob.len()) {
        return Err(KeyError::Malformed);
    }
    Ok(t)
}

/// Parses a base64 wire key with a known type (as received in a bundle or URL).
pub fn parse_parts(key_type: &str, b64: &str, comment: &str) -> Result<PublicKey, KeyError> {
    if !ALLOWED_TYPES.contains(&key_type) {
        return Err(KeyError::UnsupportedType(key_type.to_string()));
    }
    let blob = decode_b64(b64).ok_or(KeyError::BadBase64)?;
    let inner = inner_type(&blob)?;
    if inner != key_type {
        return Err(KeyError::TypeMismatch { line: key_type.to_string(), inner });
    }
    Ok(PublicKey { key_type: key_type.to_string(), base64: b64.to_string(), comment: comment.to_string(), fingerprint: fingerprint(&blob) })
}

/// Parses `type base64 [comment]`. Leading/trailing whitespace is ignored;
/// the comment keeps its inner spacing.
pub fn parse_line(line: &str) -> Result<PublicKey, KeyError> {
    let line = line.trim();
    if line.is_empty() {
        return Err(KeyError::Empty);
    }
    if line.len() > MAX_LINE {
        return Err(KeyError::TooLong);
    }
    if line.chars().any(|c| c.is_control() && c != '\t') {
        return Err(KeyError::ControlChars);
    }
    let (key_type, rest) = line.split_once([' ', '\t']).unwrap_or((line, ""));
    if !ALLOWED_TYPES.contains(&key_type) {
        return Err(KeyError::UnsupportedType(key_type.to_string()));
    }
    let rest = rest.trim_start();
    if rest.is_empty() {
        return Err(KeyError::MissingKey);
    }
    let (b64, comment) = rest.split_once([' ', '\t']).unwrap_or((rest, ""));
    parse_parts(key_type, b64, comment.trim())
}

/// The wire keys (base64 field) of every parseable line in a multi-line
/// field, for de-duplication. Unparseable lines are skipped.
pub fn wire_keys(text: &str) -> std::collections::HashSet<String> {
    text.lines().filter_map(|l| parse_line(l).ok()).map(|k| k.base64).collect()
}

/// Appends `line` to a newline-separated key field unless the same wire key
/// is already in it. Returns whether it was added.
pub fn append_unique(field: &mut String, key: &PublicKey) -> bool {
    if wire_keys(field).contains(&key.base64) {
        return false;
    }
    if !field.trim().is_empty() && !field.ends_with('\n') {
        field.push('\n');
    }
    field.push_str(&key.line());
    true
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// GitHub's published host keys and fingerprints (docs.github.com).
    pub const GH_ED25519: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl";
    pub const GH_ED25519_FP: &str = "SHA256:+DiY3wvvV6TuJJhbpZisF/zLDA0zPMSvHdkr4UvCOqU";
    pub const GH_ECDSA: &str = "AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBEmKSENjQEezOmxkZMy7opKgwFB9nkt5YRrYMjNuG5N87uRgg6CLrbo5wAdT/y6v0mKV0U2w0WZ2YB/++Tpockg=";
    pub const GH_ECDSA_FP: &str = "SHA256:p2QAMXNIC1TJYWeIOttrVc98/R1BUFWu3/LiyKgUfQM";

    #[test]
    fn parses_good_lines_and_fingerprints_like_ssh_keygen() {
        let k = parse_line(&format!("ssh-ed25519 {GH_ED25519} root@web01")).unwrap();
        assert_eq!(k.key_type, "ssh-ed25519");
        assert_eq!(k.comment, "root@web01");
        assert_eq!(k.fingerprint, GH_ED25519_FP);
        assert_eq!(k.short_type(), "ed25519");
        let k = parse_line(&format!("  ecdsa-sha2-nistp256\t{GH_ECDSA}  my laptop key \n")).unwrap();
        assert_eq!(k.fingerprint, GH_ECDSA_FP);
        assert_eq!(k.comment, "my laptop key");
        assert_eq!(k.line(), format!("ecdsa-sha2-nistp256 {GH_ECDSA} my laptop key"));
        // No comment.
        assert_eq!(parse_line(&format!("ssh-ed25519 {GH_ED25519}")).unwrap().comment, "");
    }

    #[test]
    fn refuses_bad_lines() {
        assert_eq!(parse_line("   "), Err(KeyError::Empty));
        assert_eq!(parse_line("ssh-dss AAAA"), Err(KeyError::UnsupportedType("ssh-dss".into())));
        assert_eq!(parse_line("ssh-ed25519"), Err(KeyError::MissingKey));
        assert_eq!(parse_line("ssh-ed25519 not*base64!"), Err(KeyError::BadBase64));
        // The ECDSA blob presented as ed25519: the inner type does not match.
        assert_eq!(
            parse_line(&format!("ssh-ed25519 {GH_ECDSA}")),
            Err(KeyError::TypeMismatch { line: "ssh-ed25519".into(), inner: "ecdsa-sha2-nistp256".into() })
        );
        // Valid base64, not a key.
        assert_eq!(parse_line("ssh-rsa aGVsbG8gd29ybGQ="), Err(KeyError::Malformed));
        assert_eq!(parse_line(&format!("ssh-ed25519 {GH_ED25519} a\u{7}b")), Err(KeyError::ControlChars));
        assert_eq!(parse_line(&format!("ssh-ed25519 {}", "A".repeat(MAX_LINE))), Err(KeyError::TooLong));
        // Truncated ed25519 key.
        assert!(parse_line(&format!("ssh-ed25519 {}", &GH_ED25519[..40])).is_err());
    }

    #[test]
    fn dedupes_on_the_wire_key() {
        let mut field = format!("ssh-ed25519 {GH_ED25519} old comment");
        let same = parse_line(&format!("ssh-ed25519 {GH_ED25519} other comment")).unwrap();
        assert!(!append_unique(&mut field, &same));
        let other = parse_line(&format!("ecdsa-sha2-nistp256 {GH_ECDSA}")).unwrap();
        assert!(append_unique(&mut field, &other));
        assert_eq!(field.lines().count(), 2);
        let mut empty = String::new();
        assert!(append_unique(&mut empty, &other));
        assert!(!empty.starts_with('\n'));
    }
}
