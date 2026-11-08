//! SSH public keys on this computer (`~/.ssh/*.pub`, `%USERPROFILE%\.ssh` on
//! Windows) for the "Add from this computer…" picker. Keys written there by
//! `ssh-keygen` or by SSHSpan's Deploy show up alike. Only `.pub` files are
//! read; nothing private is ever opened.

use crate::sshkey::{self, PublicKey};
use std::path::{Path, PathBuf};

/// Larger files are not public keys.
pub const MAX_FILE: u64 = 64 * 1024;
/// At most this many files are listed.
const MAX_FILES: usize = 200;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Ok(PublicKey),
    Unreadable(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PubKeyFile {
    pub file_name: String,
    pub path: PathBuf,
    pub status: Status,
}

/// The user's `.ssh` directory.
pub fn ssh_dir() -> Option<PathBuf> {
    if cfg!(windows)
        && let Some(p) = std::env::var_os("USERPROFILE").map(PathBuf::from).filter(|p| p.is_absolute())
    {
        return Some(p.join(".ssh"));
    }
    directories::UserDirs::new().map(|d| d.home_dir().join(".ssh"))
}

/// Reads one `.pub` file: the first non-comment line must be a supported key.
pub fn read_file(path: &Path) -> Status {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) => return Status::Unreadable(format!("cannot read: {e}")),
    };
    if !meta.is_file() {
        return Status::Unreadable("not a file".into());
    }
    if meta.len() > MAX_FILE {
        return Status::Unreadable(format!("larger than {} KiB", MAX_FILE / 1024));
    }
    let text = match std::fs::read(path) {
        Ok(b) => match String::from_utf8(b) {
            Ok(t) => t,
            Err(_) => return Status::Unreadable("not a text file".into()),
        },
        Err(e) => return Status::Unreadable(format!("cannot read: {e}")),
    };
    let Some(line) = text.lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with('#')) else {
        return Status::Unreadable("empty".into());
    };
    match sshkey::parse_line(line) {
        Ok(k) => Status::Ok(k),
        Err(e) => Status::Unreadable(e.to_string()),
    }
}

/// Every `*.pub` file in `dir`, sorted by name. A missing directory is empty.
pub fn scan(dir: &Path) -> Vec<PubKeyFile> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<PubKeyFile> = rd
        .flatten()
        .filter_map(|e| {
            let file_name = e.file_name().to_str()?.to_string();
            file_name.ends_with(".pub").then(|| {
                let path = e.path();
                PubKeyFile { status: read_file(&path), file_name, path }
            })
        })
        .take(MAX_FILES)
        .collect();
    out.sort_by(|a, b| a.file_name.cmp(&b.file_name));
    out
}

/// Appends the chosen keys to a key field, skipping keys already in it.
/// Returns (added, skipped as duplicates).
pub fn append(field: &mut String, keys: &[PublicKey]) -> (usize, usize) {
    let mut added = 0;
    let mut dupes = 0;
    for k in keys {
        if sshkey::append_unique(field, k) {
            added += 1;
        } else {
            dupes += 1;
        }
    }
    (added, dupes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sshkey::tests::{GH_ECDSA, GH_ED25519};

    #[test]
    fn scans_parses_and_marks_unreadable_files() {
        let dir = std::env::temp_dir().join(format!("pmxspan-pub-{}", crate::config::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("id_ed25519.pub"), format!("ssh-ed25519 {GH_ED25519} me@laptop\n")).unwrap();
        std::fs::write(dir.join("work.pub"), format!("# comment\necdsa-sha2-nistp256 {GH_ECDSA}\n")).unwrap();
        std::fs::write(dir.join("broken.pub"), "ssh-ed25519 no!pe").unwrap();
        std::fs::write(dir.join("huge.pub"), vec![b'a'; (MAX_FILE + 1) as usize]).unwrap();
        std::fs::write(dir.join("id_ed25519"), "PRIVATE - never read").unwrap();
        std::fs::write(dir.join("empty.pub"), "\n").unwrap();
        let list = scan(&dir);
        let names: Vec<&str> = list.iter().map(|f| f.file_name.as_str()).collect();
        assert_eq!(names, ["broken.pub", "empty.pub", "huge.pub", "id_ed25519.pub", "work.pub"]);
        assert!(matches!(&list[0].status, Status::Unreadable(r) if r.contains("base64")));
        assert!(matches!(&list[1].status, Status::Unreadable(r) if r == "empty"));
        assert!(matches!(&list[2].status, Status::Unreadable(r) if r.contains("64 KiB")));
        match &list[3].status {
            Status::Ok(k) => assert_eq!((k.short_type(), k.comment.as_str()), ("ed25519", "me@laptop")),
            other => panic!("{other:?}"),
        }
        assert!(matches!(&list[4].status, Status::Ok(k) if k.key_type == "ecdsa-sha2-nistp256"));
        assert!(scan(&dir.join("missing")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn appends_without_duplicates() {
        let a = sshkey::parse_line(&format!("ssh-ed25519 {GH_ED25519} me")).unwrap();
        let b = sshkey::parse_line(&format!("ecdsa-sha2-nistp256 {GH_ECDSA} work")).unwrap();
        let mut field = format!("ssh-ed25519 {GH_ED25519} already here");
        assert_eq!(append(&mut field, &[a.clone(), b.clone(), b]), (1, 2));
        assert_eq!(field, format!("ssh-ed25519 {GH_ED25519} already here\necdsa-sha2-nistp256 {GH_ECDSA} work"));
    }
}
