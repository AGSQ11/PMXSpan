//! Small helpers shared across the app: lenient JSON access, Proxmox
//! property-string parsing, formatting, and secret generation.

use rand::Rng;
use serde_json::Value;

/// Lenient accessors for Proxmox JSON, which mixes numbers, numeric strings
/// and booleans encoded as 0/1 depending on endpoint and version.
pub trait ValueExt {
    fn s(&self, key: &str) -> String;
    fn opt_s(&self, key: &str) -> Option<String>;
    fn f(&self, key: &str) -> f64;
    fn u(&self, key: &str) -> u64;
    fn b(&self, key: &str) -> bool;
    fn arr(&self) -> Vec<Value>;
}

impl ValueExt for Value {
    fn s(&self, key: &str) -> String {
        self.opt_s(key).unwrap_or_default()
    }

    fn opt_s(&self, key: &str) -> Option<String> {
        match self.get(key)? {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            Value::Bool(b) => Some(if *b { "1".into() } else { "0".into() }),
            Value::Null => None,
            other => Some(other.to_string()),
        }
    }

    fn f(&self, key: &str) -> f64 {
        match self.get(key) {
            Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
            Some(Value::String(s)) => s.trim().parse().unwrap_or(0.0),
            Some(Value::Bool(b)) => f64::from(u8::from(*b)),
            _ => 0.0,
        }
    }

    fn u(&self, key: &str) -> u64 {
        let v = self.f(key);
        if v.is_finite() && v > 0.0 { v as u64 } else { 0 }
    }

    fn b(&self, key: &str) -> bool {
        match self.get(key) {
            Some(Value::Bool(b)) => *b,
            Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0) != 0.0,
            Some(Value::String(s)) => matches!(s.as_str(), "1" | "true" | "yes" | "on"),
            _ => false,
        }
    }

    fn arr(&self) -> Vec<Value> {
        match self {
            Value::Array(a) => a.clone(),
            _ => Vec::new(),
        }
    }
}

/// A Proxmox property string such as `virtio=BC:24:11:00:00:01,bridge=vmbr0,tag=10`
/// or `local-lvm:vm-100-disk-0,size=32G`. Order is preserved so rewriting a
/// value does not reshuffle the user's config.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PropString {
    pub parts: Vec<(String, String)>,
}

impl PropString {
    pub fn parse(s: &str) -> Self {
        let parts = s
            .split(',')
            .filter(|p| !p.is_empty())
            .map(|p| match p.split_once('=') {
                Some((k, v)) => (k.trim().to_string(), v.trim().to_string()),
                None => (String::new(), p.trim().to_string()),
            })
            .collect();
        Self { parts }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.parts.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    /// The leading positional value (e.g. the volume id of a disk).
    pub fn positional(&self) -> Option<&str> {
        self.get("")
    }

    pub fn set(&mut self, key: &str, value: impl Into<String>) {
        let value = value.into();
        if let Some(slot) = self.parts.iter_mut().find(|(k, _)| k == key) {
            slot.1 = value;
        } else {
            self.parts.push((key.to_string(), value));
        }
    }

    pub fn remove(&mut self, key: &str) {
        self.parts.retain(|(k, _)| k != key);
    }

    pub fn set_opt(&mut self, key: &str, value: Option<String>) {
        match value.filter(|v| !v.is_empty()) {
            Some(v) => self.set(key, v),
            None => self.remove(key),
        }
    }

    /// For QEMU NICs the model is the key holding the MAC (`virtio=AA:..`).
    pub fn qemu_nic_model(&self) -> Option<(&str, &str)> {
        const MODELS: [&str; 6] = ["virtio", "e1000", "e1000e", "rtl8139", "vmxnet3", "i82551"];
        self.parts.iter().find(|(k, _)| MODELS.contains(&k.as_str())).map(|(k, v)| (k.as_str(), v.as_str()))
    }
}

impl std::fmt::Display for PropString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut first = true;
        for (k, v) in &self.parts {
            if !first {
                f.write_str(",")?;
            }
            first = false;
            if k.is_empty() {
                f.write_str(v)?;
            } else {
                write!(f, "{k}={v}")?;
            }
        }
        Ok(())
    }
}

/// Parses Proxmox size strings (`32G`, `512M`, `1T`, `2048` = bytes) into bytes.
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num, unit) = match s.char_indices().find(|(_, c)| c.is_ascii_alphabetic()) {
        Some((i, _)) => (&s[..i], s[i..].to_ascii_uppercase()),
        None => (s, String::new()),
    };
    let n: f64 = num.trim().parse().ok()?;
    let mult: f64 = match unit.trim_end_matches('B').trim_end_matches('I') {
        "" => 1.0,
        "K" => 1024.0,
        "M" => 1024.0 * 1024.0,
        "G" => 1024.0 * 1024.0 * 1024.0,
        "T" => 1024.0_f64.powi(4),
        "P" => 1024.0_f64.powi(5),
        _ => return None,
    };
    Some((n * mult) as u64)
}

pub fn fmt_bytes(b: f64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    if !b.is_finite() || b <= 0.0 {
        return "0 B".into();
    }
    let mut v = b;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{v:.0} {}", UNITS[i]) } else { format!("{v:.2} {}", UNITS[i]) }
}

/// Decimal units, used for traffic accounting (1 GB = 10^9 bytes).
pub fn fmt_bytes_si(b: f64) -> String {
    const UNITS: [&str; 6] = ["B", "kB", "MB", "GB", "TB", "PB"];
    if !b.is_finite() || b <= 0.0 {
        return "0 B".into();
    }
    let mut v = b;
    let mut i = 0;
    while v >= 1000.0 && i < UNITS.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    format!("{v:.2} {}", UNITS[i])
}

pub fn fmt_duration(secs: u64) -> String {
    if secs == 0 {
        return "-".into();
    }
    let d = secs / 86400;
    let h = (secs % 86400) / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if d > 0 {
        format!("{d}d {h}h {m}m")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

pub fn fmt_epoch(ts: i64) -> String {
    use chrono::TimeZone;
    if ts <= 0 {
        return "-".into();
    }
    match chrono::Local.timestamp_opt(ts, 0) {
        chrono::LocalResult::Single(t) => t.format("%Y-%m-%d %H:%M:%S").to_string(),
        _ => ts.to_string(),
    }
}

pub fn pct(used: f64, total: f64) -> f32 {
    if total <= 0.0 { 0.0 } else { (used / total).clamp(0.0, 1.0) as f32 }
}

/// A strong random password that satisfies Proxmox (>= 8 chars) and common
/// guest password policies (upper, lower, digit, symbol).
pub fn generate_password(len: usize) -> String {
    const LOWER: &[u8] = b"abcdefghijkmnopqrstuvwxyz";
    const UPPER: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ";
    const DIGIT: &[u8] = b"23456789";
    const SYMBOL: &[u8] = b"!@#%^*-_=+";
    let len = len.max(12);
    let mut rng = rand::thread_rng();
    let all: Vec<u8> = [LOWER, UPPER, DIGIT, SYMBOL].concat();
    let mut out: Vec<u8> = vec![
        LOWER[rng.gen_range(0..LOWER.len())],
        UPPER[rng.gen_range(0..UPPER.len())],
        DIGIT[rng.gen_range(0..DIGIT.len())],
        SYMBOL[rng.gen_range(0..SYMBOL.len())],
    ];
    while out.len() < len {
        out.push(all[rng.gen_range(0..all.len())]);
    }
    // Fisher-Yates so the guaranteed classes are not always first.
    for i in (1..out.len()).rev() {
        let j = rng.gen_range(0..=i);
        out.swap(i, j);
    }
    String::from_utf8(out).expect("ascii")
}

/// Random locally-administered unicast MAC with an optional fixed prefix
/// (e.g. `BC:24:11`, the Proxmox OUI).
pub fn generate_mac(prefix: &str) -> String {
    let mut rng = rand::thread_rng();
    let mut bytes: Vec<u8> =
        prefix.split([':', '-']).filter(|p| !p.is_empty()).filter_map(|p| u8::from_str_radix(p, 16).ok()).take(5).collect();
    if bytes.is_empty() {
        // Locally administered, unicast.
        bytes.push((rng.r#gen::<u8>() & 0xfc) | 0x02);
    }
    while bytes.len() < 6 {
        bytes.push(rng.r#gen());
    }
    bytes.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":")
}

pub fn validate_mac(mac: &str) -> bool {
    let parts: Vec<&str> = mac.split(':').collect();
    parts.len() == 6 && parts.iter().all(|p| p.len() == 2 && u8::from_str_radix(p, 16).is_ok())
}

/// Hostname sanitization: keeps ASCII letters, digits, dashes and dots,
/// trims leading/trailing dashes and dots, and lowercases.
pub fn sanitize_hostname(h: &str) -> String {
    let cleaned: String = h.trim().chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '.').collect();
    cleaned.trim_matches(|c| c == '-' || c == '.').to_ascii_lowercase()
}

/// Redacts credentials from free text before it reaches logs.
pub fn redact(s: &str) -> String {
    let mut out = s.to_string();
    for marker in ["PVEAPIToken=", "PVEAuthCookie=", "CSRFPreventionToken:", "password=", "cipassword=", "ticket="] {
        let mut search_from = 0;
        while let Some(pos) = out[search_from..].find(marker) {
            let start = search_from + pos + marker.len();
            let end = out[start..]
                .find(|c: char| c.is_whitespace() || c == '&' || c == ';' || c == '"' || c == ',')
                .map(|e| start + e)
                .unwrap_or(out.len());
            out.replace_range(start..end, "[redacted]");
            search_from = start + "[redacted]".len();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prop_string_roundtrip() {
        let mut p = PropString::parse("virtio=BC:24:11:AA:BB:CC,bridge=vmbr0,firewall=1");
        assert_eq!(p.qemu_nic_model(), Some(("virtio", "BC:24:11:AA:BB:CC")));
        p.set("tag", "10");
        p.remove("firewall");
        assert_eq!(p.to_string(), "virtio=BC:24:11:AA:BB:CC,bridge=vmbr0,tag=10");
        let d = PropString::parse("local-lvm:vm-100-disk-0,size=32G");
        assert_eq!(d.positional(), Some("local-lvm:vm-100-disk-0"));
        assert_eq!(d.get("size").and_then(parse_size), Some(32 * 1024 * 1024 * 1024));
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("512M"), Some(512 * 1024 * 1024));
        assert_eq!(parse_size("1T"), Some(1024_u64.pow(4)));
        assert_eq!(parse_size("100"), Some(100));
        assert_eq!(parse_size("x"), None);
    }

    #[test]
    fn passwords_and_macs() {
        let p = generate_password(16);
        assert_eq!(p.len(), 16);
        assert!(p.chars().any(|c| c.is_ascii_uppercase()));
        assert!(p.chars().any(|c| c.is_ascii_digit()));
        let m = generate_mac("BC:24:11");
        assert!(m.starts_with("BC:24:11:"));
        assert!(validate_mac(&m));
        let m = generate_mac("");
        let first = u8::from_str_radix(&m[..2], 16).unwrap();
        assert_eq!(first & 0x03, 0x02);
    }

    #[test]
    fn redaction() {
        let r = redact("Authorization: PVEAPIToken=root@pam!x=secret-uuid other");
        assert!(!r.contains("secret-uuid"));
        assert!(r.contains("other"));
        assert_eq!(sanitize_hostname(" -Web_01.Example.com. "), "web01.example.com");
    }
}
