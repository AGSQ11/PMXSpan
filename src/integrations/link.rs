//! The URL contract shared with SSHSpan (v1): field validation, percent
//! encoding, building `sshspan://` links and parsing `pmxspan://` links.
//!
//! Every value is percent-encoded UTF-8 (spaces are `%20`, a `+` is a plus),
//! `v=1` is mandatory, unknown parameters are ignored and no link may exceed
//! 8192 bytes.

use std::net::IpAddr;

pub const LINK_VERSION: &str = "1";
pub const MAX_URL: usize = 8192;
pub const MAX_NAME: usize = 128;

// ---- field validation -------------------------------------------------------

/// A DNS name or an IP literal (IPv6 without brackets).
pub fn valid_host(h: &str) -> bool {
    if h.is_empty() || h.len() > 253 {
        return false;
    }
    if h.parse::<IpAddr>().is_ok() {
        return true;
    }
    if h.contains(':') {
        return false;
    }
    let name = h.strip_suffix('.').unwrap_or(h);
    !name.is_empty()
        && name.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
        // A name made of digits and dots only would be a malformed IPv4 address.
        && !name.bytes().all(|b| b.is_ascii_digit() || b == b'.')
}

/// POSIX-ish user names: `[A-Za-z0-9._@-]{1,64}`.
pub fn valid_user(u: &str) -> bool {
    (1..=64).contains(&u.len()) && u.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'@' | b'-'))
}

/// Display names: at most 128 characters, no control characters.
pub fn valid_name(n: &str) -> bool {
    n.chars().count() <= MAX_NAME && !n.chars().any(char::is_control)
}

pub fn valid_vmid(v: u64) -> bool {
    (100..=999_999_999).contains(&v)
}

/// Server profile ids (PMXSpan writes 32 hex digits).
pub fn valid_profile_id(id: &str) -> bool {
    (1..=128).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// Proxmox node names.
pub fn valid_node(n: &str) -> bool {
    (1..=63).contains(&n.len()) && n.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_'))
}

// ---- encoding ---------------------------------------------------------------

/// Percent-encodes everything but the RFC 3986 unreserved characters.
pub fn enc(s: &str) -> String {
    urlencoding::encode(s).into_owned()
}

/// Strict percent-decoding: a stray `%` or invalid UTF-8 is an error and `+`
/// stays a plus.
pub fn dec(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

// ---- errors -----------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkError {
    TooLong(usize),
    /// Not a `<scheme>://` link of the expected scheme.
    WrongScheme,
    UnknownAction(String),
    MissingVersion,
    /// A newer version than this PMXSpan understands.
    NewerVersion(String),
    BadVersion(String),
    Missing(&'static str),
    Invalid(&'static str, String),
    Duplicate(String),
    BadEncoding,
}

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LinkError::TooLong(n) => write!(f, "the link is {n} bytes long; at most {MAX_URL} are accepted"),
            LinkError::WrongScheme => write!(f, "this is not a pmxspan:// link"),
            LinkError::UnknownAction(a) => write!(f, "pmxspan://{} is not a link PMXSpan knows", short(a)),
            LinkError::MissingVersion => write!(f, "the link has no version (v=1)"),
            LinkError::NewerVersion(v) => {
                write!(f, "This link was made by a newer SSHSpan (link version {}); update PMXSpan", short(v))
            }
            LinkError::BadVersion(v) => write!(f, "link version '{}' is not supported", short(v)),
            LinkError::Missing(p) => write!(f, "the link has no '{p}'"),
            LinkError::Invalid(p, why) => write!(f, "'{p}' in the link is not valid: {why}"),
            LinkError::Duplicate(p) => write!(f, "'{}' appears more than once in the link", short(p)),
            LinkError::BadEncoding => write!(f, "the link is not correctly percent-encoded UTF-8"),
        }
    }
}

impl std::error::Error for LinkError {}

fn short(s: &str) -> String {
    let clean: String = s.chars().filter(|c| !c.is_control()).take(40).collect();
    if s.chars().count() > 40 { format!("{clean}…") } else { clean }
}

/// Splits `scheme://action?query` into (action, decoded parameters).
fn split(url: &str, scheme: &str) -> Result<(String, Vec<(String, String)>), LinkError> {
    if url.len() > MAX_URL {
        return Err(LinkError::TooLong(url.len()));
    }
    let prefix = format!("{scheme}://");
    let rest = url.get(..prefix.len()).filter(|p| p.eq_ignore_ascii_case(&prefix)).map(|_| &url[prefix.len()..]);
    let rest = rest.ok_or(LinkError::WrongScheme)?;
    let rest = rest.split('#').next().unwrap_or("");
    let (action, query) = rest.split_once('?').unwrap_or((rest, ""));
    let action = action.trim_end_matches('/').to_ascii_lowercase();
    let mut params = Vec::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        params.push((dec(k).ok_or(LinkError::BadEncoding)?, dec(v).ok_or(LinkError::BadEncoding)?));
    }
    Ok((action, params))
}

/// Parameter access with the v1 rules.
struct Query(Vec<(String, String)>);

impl Query {
    /// A single-valued parameter; given twice is refused.
    fn one(&self, key: &'static str) -> Result<Option<&str>, LinkError> {
        let mut it = self.0.iter().filter(|(k, _)| k == key);
        let first = it.next().map(|(_, v)| v.as_str());
        if it.next().is_some() {
            return Err(LinkError::Duplicate(key.to_string()));
        }
        Ok(first)
    }
    fn req(&self, key: &'static str) -> Result<&str, LinkError> {
        self.one(key)?.filter(|v| !v.is_empty()).ok_or(LinkError::Missing(key))
    }
    fn version(&self) -> Result<(), LinkError> {
        match self.one("v")? {
            None | Some("") => Err(LinkError::MissingVersion),
            Some(LINK_VERSION) => Ok(()),
            Some(v) => match v.parse::<u64>() {
                Ok(n) if n > 1 => Err(LinkError::NewerVersion(v.to_string())),
                _ => Err(LinkError::BadVersion(v.to_string())),
            },
        }
    }
}

// ---- pmxspan:// (received) --------------------------------------------------

/// A validated `pmxspan://` request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Incoming {
    /// Open a guest page.
    Guest { cluster: String, vmid: u64 },
    /// Open a node page.
    Node { cluster: String, node: String },
    /// Offer a public key (shown in a confirmation card, never applied).
    AddKey { key: crate::sshkey::PublicKey, name: String, source: String },
}

impl Incoming {
    pub fn cluster(&self) -> Option<&str> {
        match self {
            Incoming::Guest { cluster, .. } | Incoming::Node { cluster, .. } => Some(cluster),
            Incoming::AddKey { .. } => None,
        }
    }
}

fn cluster_param(q: &Query) -> Result<String, LinkError> {
    let c = q.req("cluster")?;
    if !valid_profile_id(c) {
        return Err(LinkError::Invalid("cluster", "not a server profile id".into()));
    }
    Ok(c.to_string())
}

/// Parses and validates a `pmxspan://` link exactly per the v1 contract.
pub fn parse_incoming(url: &str) -> Result<Incoming, LinkError> {
    let (action, params) = split(url.trim(), "pmxspan")?;
    let q = Query(params);
    // Known action first, so a typo is reported as such.
    if !matches!(action.as_str(), "guest" | "node" | "add-key") {
        return Err(LinkError::UnknownAction(action));
    }
    q.version()?;
    match action.as_str() {
        "guest" => {
            let cluster = cluster_param(&q)?;
            let raw = q.req("vmid")?;
            let vmid = raw
                .parse::<u64>()
                .ok()
                .filter(|v| valid_vmid(*v))
                .ok_or_else(|| LinkError::Invalid("vmid", "must be a number from 100 to 999999999".into()))?;
            Ok(Incoming::Guest { cluster, vmid })
        }
        "node" => {
            let cluster = cluster_param(&q)?;
            let node = q.req("node")?;
            if !valid_node(node) {
                return Err(LinkError::Invalid("node", "not a Proxmox node name".into()));
            }
            Ok(Incoming::Node { cluster, node: node.to_string() })
        }
        _ => {
            let line = q.req("key")?;
            let key = crate::sshkey::parse_line(line).map_err(|e| LinkError::Invalid("key", e.to_string()))?;
            let name = q.one("name")?.unwrap_or("").trim().to_string();
            if !valid_name(&name) {
                return Err(LinkError::Invalid("name", "longer than 128 characters or contains control characters".into()));
            }
            let source = q.one("source")?.unwrap_or("").to_string();
            if source.len() > 64 || source.chars().any(char::is_control) {
                return Err(LinkError::Invalid("source", "too long or contains control characters".into()));
            }
            Ok(Incoming::AddKey { key, name, source })
        }
    }
}

// ---- sshspan:// (sent) ------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetKind {
    Vm,
    Ct,
    Node,
}

impl TargetKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TargetKind::Vm => "vm",
            TargetKind::Ct => "ct",
            TargetKind::Node => "node",
        }
    }
}

/// Everything `sshspan://connect` carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectTarget {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub name: String,
    pub cluster_id: String,
    pub cluster_name: String,
    pub kind: TargetKind,
    pub vmid: Option<u64>,
    pub node: String,
    pub pool: String,
    /// Base64 wire-format host keys.
    pub host_keys: Vec<String>,
    /// Unix seconds, only when a rebuild changed the keys.
    pub host_key_rotated: Option<i64>,
}

/// Why a target cannot be sent; shown next to the fields.
pub fn check_target(host: &str, port: u16, user: &str, name: &str) -> Result<(), String> {
    if host.trim().is_empty() {
        return Err("no address: give the host name or IP address".into());
    }
    if !valid_host(host) {
        return Err(format!("'{host}' is not a host name or IP address"));
    }
    if port == 0 {
        return Err("the port must be 1 to 65535".into());
    }
    if user.is_empty() {
        return Err("no user name".into());
    }
    if !valid_user(user) {
        return Err(format!("user '{user}' may only use letters, digits and . _ @ - (at most 64)"));
    }
    if !valid_name(name) {
        return Err("the name is longer than 128 characters or contains control characters".into());
    }
    Ok(())
}

/// Builds `sshspan://connect?v=1&...`; refused when a field is invalid or the
/// link would exceed 8192 bytes.
pub fn connect_url(t: &ConnectTarget) -> Result<String, String> {
    check_target(&t.host, t.port, &t.user, &t.name)?;
    let mut parts: Vec<(&str, String)> =
        vec![("v", LINK_VERSION.into()), ("host", t.host.clone()), ("port", t.port.to_string()), ("user", t.user.clone())];
    if !t.name.is_empty() {
        parts.push(("name", t.name.clone()));
    }
    parts.push(("source", "pmxspan".into()));
    parts.push(("cluster", t.cluster_id.clone()));
    parts.push(("clustername", t.cluster_name.chars().filter(|c| !c.is_control()).take(MAX_NAME).collect()));
    parts.push(("kind", t.kind.as_str().into()));
    if let Some(v) = t.vmid {
        parts.push(("vmid", v.to_string()));
    }
    if !t.node.is_empty() {
        parts.push(("node", t.node.clone()));
    }
    if !t.pool.is_empty() {
        parts.push(("pool", t.pool.clone()));
    }
    for k in &t.host_keys {
        parts.push(("hostkey", k.clone()));
    }
    if let Some(r) = t.host_key_rotated {
        parts.push(("hostkey_rotated", r.to_string()));
    }
    let query: Vec<String> = parts.iter().map(|(k, v)| format!("{k}={}", enc(v))).collect();
    let url = format!("sshspan://connect?{}", query.join("&"));
    if url.len() > MAX_URL {
        return Err(format!("the link would be {} bytes long; SSHSpan accepts at most {MAX_URL}", url.len()));
    }
    Ok(url)
}

/// Builds `sshspan://import?v=1&source=pmxspan&bundle=<path>&sha256=<hex>`.
pub fn import_url(bundle: &std::path::Path, sha256_hex: &str) -> Result<String, String> {
    if !bundle.is_absolute() {
        return Err("the bundle path is not absolute".into());
    }
    let path = bundle.to_str().ok_or("the bundle path is not valid UTF-8")?;
    let url = format!("sshspan://import?v=1&source=pmxspan&bundle={}&sha256={}", enc(path), enc(sha256_hex));
    if url.len() > MAX_URL {
        return Err("the bundle path is too long for a link".into());
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sshkey::tests::{GH_ECDSA, GH_ED25519};

    fn target() -> ConnectTarget {
        ConnectTarget {
            host: "10.0.1.5".into(),
            port: 22,
            user: "debian".into(),
            name: "web01".into(),
            cluster_id: "abc123".into(),
            cluster_name: "lab".into(),
            kind: TargetKind::Vm,
            vmid: Some(100),
            node: "pve1".into(),
            pool: "customers".into(),
            host_keys: vec![],
            host_key_rotated: None,
        }
    }

    /// Minimal parser of what we build, to check round trips.
    fn params(url: &str) -> Vec<(String, String)> {
        split(url, "sshspan").unwrap().1
    }

    #[test]
    fn builds_connect_links() {
        let url = connect_url(&target()).unwrap();
        assert_eq!(
            url,
            "sshspan://connect?v=1&host=10.0.1.5&port=22&user=debian&name=web01&source=pmxspan&cluster=abc123\
             &clustername=lab&kind=vm&vmid=100&node=pve1&pool=customers"
        );
        // IPv6 hosts go without brackets, colons percent-encoded.
        let mut t = target();
        t.host = "2001:db8::5".into();
        t.name = "web 01 · ünïcode".into();
        t.cluster_name = "my lab+test".into();
        t.host_keys = vec![GH_ED25519.into(), GH_ECDSA.into()];
        t.host_key_rotated = Some(1_762_603_200);
        let url = connect_url(&t).unwrap();
        assert!(url.contains("host=2001%3Adb8%3A%3A5"), "{url}");
        assert!(url.contains("name=web%2001%20%C2%B7%20%C3%BCn%C3%AFcode"), "{url}");
        assert!(url.contains("clustername=my%20lab%2Btest"), "{url}");
        assert!(!url.contains('+') && !url.contains(' '));
        let p = params(&url);
        let get = |k: &str| p.iter().filter(|(x, _)| x == k).map(|(_, v)| v.clone()).collect::<Vec<_>>();
        assert_eq!(get("host"), ["2001:db8::5"]);
        assert_eq!(get("name"), ["web 01 · ünïcode"]);
        assert_eq!(get("hostkey"), [GH_ED25519, GH_ECDSA]);
        assert_eq!(get("hostkey_rotated"), ["1762603200"]);
        // Nodes carry no vmid.
        let mut n = target();
        n.kind = TargetKind::Node;
        n.vmid = None;
        n.pool.clear();
        let url = connect_url(&n).unwrap();
        assert!(url.contains("kind=node") && !url.contains("vmid") && !url.contains("pool"));
    }

    #[test]
    fn refuses_invalid_targets_and_long_links() {
        for (host, user, port) in [
            ("", "root", 22),
            ("[2001:db8::1]", "root", 22),
            ("bad host", "root", 22),
            ("-x.example", "root", 22),
            ("10.0.0.1", "", 22),
            ("10.0.0.1", "ro ot", 22),
            ("10.0.0.1", "root", 0),
        ] {
            let mut t = target();
            t.host = host.into();
            t.user = user.into();
            t.port = port;
            assert!(connect_url(&t).is_err(), "{host} {user} {port}");
        }
        let mut t = target();
        t.name = "x".repeat(129);
        assert!(connect_url(&t).is_err());
        // 8192 cap: enough host keys push it over.
        let mut t = target();
        t.host_keys = vec![GH_ECDSA.to_string(); 60];
        let e = connect_url(&t).unwrap_err();
        assert!(e.contains("8192"), "{e}");
    }

    #[test]
    fn builds_import_links() {
        let path = if cfg!(windows) {
            r"C:\Users\me\AppData\Local\PMXSpan\handoff\ab cd.json"
        } else {
            "/run/user/1000/pmxspan/handoff/ab cd.json"
        };
        let url = import_url(std::path::Path::new(path), &"a".repeat(64)).unwrap();
        assert!(url.starts_with("sshspan://import?v=1&source=pmxspan&bundle="));
        let p = params(&url);
        assert_eq!(p[2], ("bundle".to_string(), path.to_string()));
        assert!(import_url(std::path::Path::new("relative.json"), "00").is_err());
    }

    #[test]
    fn validates_hosts_users_names() {
        for h in ["web01", "web01.example.com", "web01.example.com.", "10.0.0.1", "2001:db8::1", "::1", "xn--bcher-kva.example"] {
            assert!(valid_host(h), "{h}");
        }
        for h in ["", "a..b", "web_01", "10.0.0", "999.1.1.1", "[::1]", "fe80::1%eth0", "a b", &"a".repeat(254)] {
            assert!(!valid_host(h), "{h}");
        }
        assert!(valid_user("debian") && valid_user("first.last@corp") && !valid_user(&"u".repeat(65)) && !valid_user("ro/ot"));
        assert!(valid_name("web 01 ünïcode") && !valid_name("a\nb"));
    }

    #[test]
    fn parses_guest_and_node_links() {
        assert_eq!(
            parse_incoming("pmxspan://guest?v=1&cluster=0123abcd&vmid=100&future=yes").unwrap(),
            Incoming::Guest { cluster: "0123abcd".into(), vmid: 100 }
        );
        assert_eq!(
            parse_incoming("PMXSpan://node/?node=pve1&v=1&cluster=0123abcd").unwrap(),
            Incoming::Node { cluster: "0123abcd".into(), node: "pve1".into() }
        );
    }

    #[test]
    fn parses_add_key_links() {
        let line = format!("ssh-ed25519 {GH_ED25519} me@laptop");
        let url = format!("pmxspan://add-key?v=1&key={}&name={}&source=sshspan", enc(&line), enc("Work key"));
        match parse_incoming(&url).unwrap() {
            Incoming::AddKey { key, name, source } => {
                assert_eq!(key.comment, "me@laptop");
                assert_eq!(key.fingerprint, crate::sshkey::tests::GH_ED25519_FP);
                assert_eq!(name, "Work key");
                assert_eq!(source, "sshspan");
            }
            other => panic!("{other:?}"),
        }
        // A '+' in base64 that was not encoded stays a plus.
        let raw = format!("pmxspan://add-key?v=1&key=ecdsa-sha2-nistp256%20{GH_ECDSA}");
        assert!(parse_incoming(&raw).is_ok());
    }

    #[test]
    fn refuses_every_bad_link() {
        let line = format!("ssh-ed25519 {GH_ED25519}");
        let long = format!("pmxspan://guest?v=1&cluster=a&vmid=100&x={}", "y".repeat(MAX_URL));
        let cases: Vec<(String, LinkError)> = vec![
            (long.clone(), LinkError::TooLong(long.len())),
            ("sshspan://guest?v=1".into(), LinkError::WrongScheme),
            ("https://guest?v=1".into(), LinkError::WrongScheme),
            ("pmxspan://delete?v=1".into(), LinkError::UnknownAction("delete".into())),
            ("pmxspan://guest?cluster=a&vmid=100".into(), LinkError::MissingVersion),
            ("pmxspan://guest?v=2&cluster=a&vmid=100".into(), LinkError::NewerVersion("2".into())),
            ("pmxspan://guest?v=one&cluster=a&vmid=100".into(), LinkError::BadVersion("one".into())),
            ("pmxspan://guest?v=1&v=1&cluster=a&vmid=100".into(), LinkError::Duplicate("v".into())),
            ("pmxspan://guest?v=1&vmid=100".into(), LinkError::Missing("cluster")),
            ("pmxspan://guest?v=1&cluster=a".into(), LinkError::Missing("vmid")),
            ("pmxspan://guest?v=1&cluster=a%2Fb&vmid=100".into(), LinkError::Invalid("cluster", "not a server profile id".into())),
            ("pmxspan://guest?v=1&cluster=a&vmid=99".into(), LinkError::Invalid("vmid", "must be a number from 100 to 999999999".into())),
            (
                "pmxspan://guest?v=1&cluster=a&vmid=1000000000".into(),
                LinkError::Invalid("vmid", "must be a number from 100 to 999999999".into()),
            ),
            ("pmxspan://guest?v=1&cluster=a&vmid=1%2".into(), LinkError::BadEncoding),
            ("pmxspan://guest?v=1&cluster=%FF&vmid=100".into(), LinkError::BadEncoding),
            ("pmxspan://node?v=1&cluster=a".into(), LinkError::Missing("node")),
            ("pmxspan://node?v=1&cluster=a&node=pve%201".into(), LinkError::Invalid("node", "not a Proxmox node name".into())),
            ("pmxspan://add-key?v=1".into(), LinkError::Missing("key")),
            (
                format!("pmxspan://add-key?v=1&key={}&name={}", enc(&line), enc("a\u{1b}b")),
                LinkError::Invalid("name", "longer than 128 characters or contains control characters".into()),
            ),
        ];
        for (url, want) in cases {
            assert_eq!(parse_incoming(&url), Err(want), "{url}");
        }
        // Key validation failures carry the key error.
        for bad in ["ssh-dss AAAA", "ssh-ed25519 !!!", &format!("ssh-ed25519 {GH_ECDSA}")] {
            match parse_incoming(&format!("pmxspan://add-key?v=1&key={}", enc(bad))) {
                Err(LinkError::Invalid("key", _)) => {}
                other => panic!("{bad}: {other:?}"),
            }
        }
        assert!(LinkError::NewerVersion("2".into()).to_string().contains("update PMXSpan"));
    }
}
