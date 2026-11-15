//! Append-only audit trail of MCP actions (`mcp-audit.log` in the data dir).
//!
//! One line per action attempt, allowed or refused:
//! `2026-11-08T12:00:00Z session=<id> tool=<name> args_sha256=<hex> outcome=<ok|error|refused>`.
//! Arguments are recorded only as a SHA-256 of their canonical JSON (keys
//! sorted), so the file never holds command lines or other payloads; the
//! application log has the (redacted) arguments.

use parking_lot::Mutex;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    Error,
    Refused,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Error => "error",
            Outcome::Refused => "refused",
        }
    }
}

/// SHA-256 (hex) of the arguments' canonical JSON. serde_json's map is
/// ordered by key, so equal arguments always hash the same.
pub fn args_hash(args: &Value) -> String {
    hex::encode(Sha256::digest(canonical(args).as_bytes()))
}

fn canonical(v: &Value) -> String {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            let inner: Vec<String> =
                keys.iter().map(|k| format!("{}:{}", Value::String((*k).clone()), canonical(&m[k.as_str()]))).collect();
            format!("{{{}}}", inner.join(","))
        }
        Value::Array(a) => format!("[{}]", a.iter().map(canonical).collect::<Vec<_>>().join(",")),
        other => other.to_string(),
    }
}

/// Keeps a client-chosen name to a safe, single-token form for the log line.
fn token(s: &str) -> String {
    let t: String = s.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/')).take(64).collect();
    if t.is_empty() { "-".into() } else { t }
}

pub fn format_line(at: chrono::DateTime<chrono::Utc>, session: &str, tool: &str, args_sha256: &str, outcome: Outcome) -> String {
    format!(
        "{} session={} tool={} args_sha256={} outcome={}\n",
        at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        token(session),
        token(tool),
        args_sha256,
        outcome.as_str()
    )
}

pub struct AuditLog {
    path: PathBuf,
    lock: Mutex<()>,
}

impl AuditLog {
    pub fn new(path: PathBuf) -> Self {
        Self { path, lock: Mutex::new(()) }
    }

    #[cfg(test)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends one line; failures are logged, never fatal.
    pub fn record(&self, session: &str, tool: &str, args: &Value, outcome: Outcome) {
        let line = format_line(chrono::Utc::now(), session, tool, &args_hash(args), outcome);
        let _g = self.lock.lock();
        if let Err(e) = append(&self.path, &line) {
            tracing::warn!("writing the MCP audit log {} failed: {e}", self.path.display());
        }
    }
}

fn append(path: &Path, line: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut o = std::fs::OpenOptions::new();
    o.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o.open(path)?.write_all(line.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn line_format_is_stable() {
        let at = chrono::DateTime::parse_from_rfc3339("2026-11-08T12:34:56Z").unwrap().with_timezone(&chrono::Utc);
        let line = format_line(at, "ab12cd", "power", "00ff", Outcome::Ok);
        assert_eq!(line, "2026-11-08T12:34:56Z session=ab12cd tool=power args_sha256=00ff outcome=ok\n");
        // Client-chosen names cannot break the line apart.
        let line = format_line(at, "x y\nz", "destroy guest\n", "00", Outcome::Refused);
        assert_eq!(line, "2026-11-08T12:34:56Z session=xyz tool=destroyguest args_sha256=00 outcome=refused\n");
        assert!(format_line(at, "", "", "00", Outcome::Error).contains("session=- tool=- "));
    }

    #[test]
    fn args_hash_ignores_key_order() {
        let a = args_hash(&json!({"vmid": 100, "action": "start", "x": {"b": 1, "a": [1, 2]}}));
        let b = args_hash(&json!({"x": {"a": [1, 2], "b": 1}, "action": "start", "vmid": 100}));
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
        assert_ne!(a, args_hash(&json!({"vmid": 101, "action": "start"})));
    }

    #[test]
    fn appends_private_lines() {
        let dir = std::env::temp_dir().join(format!("pmxspan-audit-{}", crate::config::new_id()));
        let log = AuditLog::new(dir.join("mcp-audit.log"));
        log.record("s1", "power", &json!({"vmid": 1}), Outcome::Ok);
        log.record("s1", "destroy", &json!({}), Outcome::Refused);
        let text = std::fs::read_to_string(log.path()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains(" session=s1 tool=power args_sha256=") && lines[0].ends_with("outcome=ok"));
        assert!(lines[1].ends_with("outcome=refused"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(log.path()).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
