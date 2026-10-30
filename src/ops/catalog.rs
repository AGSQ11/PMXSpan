//! OS image catalog.
//!
//! A catalog entry points at a cloud image (QEMU: qcow2/raw/vmdk/ova) or a
//! container template (LXC: tar.zst/xz/gz). Creating or reinstalling a guest
//! from an entry makes the *node* download it with `download-url` into a
//! download storage (`import` content for QEMU, `vztmpl` for LXC), then:
//! - QEMU: the boot disk is created with `import-from=<artifact>`;
//! - LXC: the container is created with `ostemplate=<artifact>`.
//!
//! The downloaded artifact is always removed afterwards. Downloads try the
//! configured main storage, then the fallback; a stalled or failing storage
//! moves on to the next candidate, while a broken URL fails immediately.
//! Catalogs are plain JSON (an array of entries, or `{"entries": [...]}`), so
//! they can be shared and pasted in unchanged.

use crate::api::types::{NodeStorage, Upid};
use crate::api::{Params, PveClient};
use crate::config::{ServerProfile, Settings, write_atomic};
use crate::jobs::JobCtx;
use crate::util::ValueExt;
use anyhow::{Context, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const GIB: u64 = 1024 * 1024 * 1024;

/// Guest config keys a catalog entry may set on QEMU guests. Catalogs are
/// shared as JSON, so they are not trusted: anything that reaches the host
/// (raw `args`, hookscripts, PCI/USB passthrough), touches identity, disks,
/// network or cloud-init, or weakens isolation stays off this list.
const QEMU_CONFIG_KEYS: &[&str] = &[
    "acpi",
    "agent",
    "balloon",
    "bios",
    "boot",
    "cores",
    "cpu",
    "cpulimit",
    "cpuunits",
    "hotplug",
    "keyboard",
    "localtime",
    "machine",
    "memory",
    "numa",
    "onboot",
    "ostype",
    "scsihw",
    "serial0",
    "shares",
    "sockets",
    "startup",
    "tablet",
    "tags",
    "vcpus",
    "vga",
];

/// Same for containers; `features` is further limited to [`LXC_FEATURES`].
const LXC_CONFIG_KEYS: &[&str] = &[
    "arch", "cmode", "console", "cores", "cpulimit", "cpuunits", "features", "memory", "onboot", "ostype", "startup", "swap", "tags",
    "timezone", "tty",
];

/// Container features that stay inside an unprivileged container.
const LXC_FEATURES: &[&str] = &["nesting", "keyctl", "fuse"];

/// Checks one `config` override against the allowlist for its guest type.
pub fn check_config_key(virt: &str, key: &str, value: &str) -> anyhow::Result<()> {
    let allowed = if virt == "lxc" { LXC_CONFIG_KEYS } else { QEMU_CONFIG_KEYS };
    if !allowed.contains(&key) {
        bail!("config key `{key}` is not allowed in a catalog (allowed: {})", allowed.join(", "));
    }
    if value.chars().any(char::is_control) {
        bail!("config value of `{key}` contains control characters");
    }
    if key == "features" {
        for (k, v) in crate::util::PropString::parse(value).parts {
            if !LXC_FEATURES.contains(&k.as_str()) || !matches!(v.as_str(), "0" | "1") {
                bail!("container feature `{k}` is not allowed in a catalog (allowed: {})", LXC_FEATURES.join(", "));
            }
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CatalogEntry {
    pub id: String,
    pub label: String,
    /// `qemu` or `lxc`.
    pub virt: String,
    pub url: String,
    pub checksum: String,
    pub checksum_algorithm: String,
    /// Default cloud-init user of the image (QEMU only), e.g. `debian`.
    pub cloud_user: String,
    pub ostype: String,
    /// Restrict to these nodes (empty = any).
    pub nodes: Vec<String>,
    /// Extra guest config applied on create/reinstall (e.g. `bios: ovmf`).
    pub config: BTreeMap<String, String>,
    /// QEMU image format: qcow2, raw, vmdk or ova (guessed from the URL if empty).
    pub image_format: String,
    pub notes: String,
}

pub const QEMU_OSTYPES: [&str; 11] = ["l26", "l24", "win11", "win10", "win8", "win7", "wvista", "wxp", "w2k", "solaris", "other"];

/// Lowercase, `[a-z0-9._-]`, dashes for anything else.
pub fn normalize_id(v: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in v.trim().to_lowercase().chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            out.push(c);
            dash = false;
        } else if !dash {
            out.push('-');
            dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

/// QEMU ostype from free text, falling back to `win10` for `win*` and `l26` otherwise.
pub fn normalize_ostype(virt: &str, ostype: &str) -> String {
    let o = ostype.trim().to_lowercase();
    if virt == "lxc" {
        return o;
    }
    if QEMU_OSTYPES.contains(&o.as_str()) {
        return o;
    }
    if o.starts_with("win") || o.starts_with("microsoft") {
        return "win10".into();
    }
    if o.contains("solaris") {
        return "solaris".into();
    }
    if o == "other" {
        return "other".into();
    }
    "l26".into()
}

fn list_value(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Array(a)) => a.iter().filter_map(|x| x.as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)).collect(),
        Some(Value::String(s)) => s.split([',', ' ', ';', '\n']).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect(),
        _ => Vec::new(),
    }
}

impl CatalogEntry {
    pub fn is_lxc(&self) -> bool {
        self.virt == "lxc"
    }

    /// Validates and normalizes one entry.
    pub fn from_value(v: &Value, index: usize) -> anyhow::Result<Self> {
        let n = index + 1;
        let obj = v.as_object().ok_or_else(|| anyhow!("catalog entry #{n} must be an object"))?;
        let s = |k: &str| {
            obj.get(k).map(|x| match x {
                Value::String(s) => s.trim().to_string(),
                Value::Null => String::new(),
                other => other.to_string(),
            })
        };
        let id = normalize_id(&s("id").unwrap_or_default());
        if id.is_empty() {
            bail!("catalog entry #{n} is missing a valid id");
        }
        let label = s("label").unwrap_or_default();
        if label.is_empty() {
            bail!("catalog entry `{id}` is missing label");
        }
        let virt = s("virt").unwrap_or_default().to_lowercase();
        if virt != "qemu" && virt != "lxc" {
            bail!("catalog entry `{id}` must use virt `qemu` or `lxc`");
        }
        let url = s("url").unwrap_or_default();
        let lower = url.to_lowercase();
        let host_ok = lower.split_once("://").map(|(_, rest)| !rest.split('/').next().unwrap_or("").is_empty()).unwrap_or(false);
        if !(lower.starts_with("http://") || lower.starts_with("https://")) || !host_ok {
            bail!("catalog entry `{id}` must use an HTTP or HTTPS url");
        }
        let checksum = s("checksum").unwrap_or_default();
        let checksum_algorithm = s("checksum_algorithm").unwrap_or_default().to_lowercase();
        if !checksum_algorithm.is_empty() && checksum.is_empty() {
            bail!("catalog entry `{id}` defines checksum_algorithm without checksum");
        }
        if !checksum_algorithm.is_empty() && !["md5", "sha1", "sha224", "sha256", "sha384", "sha512"].contains(&checksum_algorithm.as_str())
        {
            bail!("catalog entry `{id}` uses unsupported checksum_algorithm `{checksum_algorithm}`");
        }
        let cloud_user = s("cloud_user").unwrap_or_default();
        if virt != "qemu" && !cloud_user.is_empty() {
            bail!("catalog entry `{id}` cannot define cloud_user for LXC");
        }
        let mut image_format = s("image_format").or_else(|| s("format")).unwrap_or_default().to_lowercase();
        if virt == "qemu" {
            if !image_format.is_empty() && !["qcow2", "raw", "vmdk", "ova"].contains(&image_format.as_str()) {
                bail!("catalog entry `{id}` uses unsupported image_format `{image_format}`");
            }
        } else {
            image_format.clear();
        }
        let mut config = BTreeMap::new();
        if let Some(Value::Object(c)) = obj.get("config").or_else(|| obj.get("tpl_data")) {
            for (k, v) in c {
                let k = k.trim();
                if k.is_empty() {
                    continue;
                }
                let v = match v {
                    Value::String(s) => s.trim().to_string(),
                    Value::Null => String::new(),
                    Value::Array(_) | Value::Object(_) => continue,
                    other => other.to_string(),
                };
                check_config_key(&virt, k, &v).with_context(|| format!("catalog entry `{id}`"))?;
                config.insert(k.to_string(), v);
            }
        }
        Ok(Self {
            ostype: normalize_ostype(&virt, &s("ostype").unwrap_or_default()),
            nodes: list_value(obj.get("nodes")),
            notes: s("notes").unwrap_or_default(),
            id,
            label,
            virt,
            url,
            checksum,
            checksum_algorithm,
            cloud_user,
            config,
            image_format,
        })
    }

    pub fn validate(&self) -> anyhow::Result<Self> {
        Self::from_value(&serde_json::to_value(self)?, 0)
    }

    /// Extension of the downloaded file.
    fn extension(&self) -> &'static str {
        if self.is_lxc() {
            let p = url_path(&self.url).to_lowercase();
            for ext in [".tar.zst", ".tar.xz", ".tar.gz", ".tgz"] {
                if p.ends_with(ext) {
                    return ext;
                }
            }
            return ".tar.zst";
        }
        let fmt = if self.image_format.is_empty() {
            let p = url_path(&self.url).to_lowercase();
            ["qcow2", "raw", "vmdk", "ova"].into_iter().find(|f| p.ends_with(&format!(".{f}"))).unwrap_or("qcow2")
        } else {
            self.image_format.as_str()
        };
        match fmt {
            "raw" => ".raw",
            "vmdk" => ".vmdk",
            "ova" => ".ova",
            _ => ".qcow2",
        }
    }

    /// Unique artifact filename, so concurrent jobs never clobber each other.
    pub fn artifact_filename(&self, vmid: u64, token: &str) -> String {
        let ext = self.extension();
        let base = url_path(&self.url).rsplit('/').next().unwrap_or("").to_string();
        let mut stem: String =
            base.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '-' }).collect();
        for e in [".tar.zst", ".tar.xz", ".tar.gz", ".tgz", ".qcow2", ".raw", ".vmdk", ".ova", ".img"] {
            if stem.to_lowercase().ends_with(e) {
                stem.truncate(stem.len() - e.len());
                break;
            }
        }
        let stem = stem.trim_matches(|c| c == '.' || c == '-' || c == '_').to_string();
        let stem = if stem.is_empty() { if self.is_lxc() { "template".to_string() } else { "disk".to_string() } } else { stem };
        let token: String = token.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
        let prefix = format!("{}-vm{vmid}-{token}", self.id);
        let max_stem = 240usize.saturating_sub(prefix.len() + 1 + ext.len()).max(1);
        format!("{prefix}-{}{ext}", &stem[..stem.len().min(max_stem)])
    }

    pub fn content(&self) -> &'static str {
        if self.is_lxc() { "vztmpl" } else { "import" }
    }
}

fn url_path(url: &str) -> &str {
    let after = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let path = after.find('/').map(|i| &after[i..]).unwrap_or("");
    path.split(['?', '#']).next().unwrap_or("")
}

/// Parses a catalog: a JSON array, or `{"entries": [...]}`.
pub fn parse_catalog(text: &str) -> anyhow::Result<Vec<CatalogEntry>> {
    let v: Value = serde_json::from_str(text.trim()).context("the catalog must be JSON")?;
    let list = match &v {
        Value::Array(a) => a.clone(),
        Value::Object(o) if o.get("entries").map(Value::is_array).unwrap_or(false) => o["entries"].as_array().cloned().unwrap_or_default(),
        Value::Object(_) => vec![v.clone()],
        _ => bail!("the catalog must be a JSON array of entries"),
    };
    let mut out: Vec<CatalogEntry> = Vec::new();
    for (i, e) in list.iter().enumerate() {
        let e = CatalogEntry::from_value(e, i)?;
        if out.iter().any(|x| x.id == e.id) {
            bail!("catalog contains duplicate id `{}`", e.id);
        }
        out.push(e);
    }
    Ok(out)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CatalogStore {
    pub entries: Vec<CatalogEntry>,
    #[serde(skip)]
    path: PathBuf,
}

impl CatalogStore {
    pub fn load(path: &Path) -> Self {
        let mut s: CatalogStore = std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        s.path = path.to_path_buf();
        s
    }

    pub fn save(&self) -> anyhow::Result<()> {
        write_atomic(&self.path, serde_json::to_string_pretty(self)?.as_bytes())
    }

    pub fn for_virt(&self, lxc: bool) -> Vec<CatalogEntry> {
        self.entries.iter().filter(|e| e.is_lxc() == lxc).cloned().collect()
    }

    /// Adds or replaces an entry (matched by its original id).
    pub fn upsert(&mut self, original_id: Option<&str>, entry: CatalogEntry) -> anyhow::Result<()> {
        let entry = entry.validate()?;
        if self.entries.iter().any(|e| e.id == entry.id && Some(e.id.as_str()) != original_id) {
            bail!("an entry with id `{}` already exists", entry.id);
        }
        match original_id.and_then(|id| self.entries.iter_mut().find(|e| e.id == id)) {
            Some(slot) => *slot = entry,
            None => self.entries.push(entry),
        }
        self.save()
    }

    pub fn remove(&mut self, id: &str) -> anyhow::Result<()> {
        self.entries.retain(|e| e.id != id);
        self.save()
    }

    /// The stored entry an imported one would replace, if any.
    pub fn existing(&self, id: &str) -> Option<&CatalogEntry> {
        self.entries.iter().find(|e| e.id == id)
    }

    /// Merges an imported catalog; entries with the same id are replaced.
    pub fn import(&mut self, entries: Vec<CatalogEntry>) -> anyhow::Result<(usize, usize)> {
        let (mut added, mut replaced) = (0, 0);
        for e in entries {
            match self.entries.iter_mut().find(|x| x.id == e.id) {
                Some(slot) => {
                    *slot = e;
                    replaced += 1;
                }
                None => {
                    self.entries.push(e);
                    added += 1;
                }
            }
        }
        self.save()?;
        Ok((added, replaced))
    }

    pub fn export_json(&self) -> String {
        serde_json::to_string_pretty(&self.entries).unwrap_or_else(|_| "[]".into())
    }
}

/// Best-effort size of the image (HEAD Content-Length), never below a floor:
/// 3 GiB for VM images, 1 GiB for container templates, or the configured minimum.
pub async fn estimate_size(entry: &CatalogEntry, settings: &Settings) -> u64 {
    let floor = if settings.download_min_bytes > 0 {
        settings.download_min_bytes
    } else if entry.is_lxc() {
        GIB
    } else {
        3 * GIB
    };
    let len = async {
        let client = reqwest::Client::builder().timeout(Duration::from_secs(6)).connect_timeout(Duration::from_secs(4)).build().ok()?;
        let r = client.head(&entry.url).send().await.ok()?;
        r.headers().get(reqwest::header::CONTENT_LENGTH)?.to_str().ok()?.parse::<u64>().ok()
    }
    .await
    .unwrap_or(0);
    len.max(floor)
}

/// Checks that the image URL answers, for the catalog editor's "Test" button.
pub async fn probe_url(url: &str) -> anyhow::Result<String> {
    let client = reqwest::Client::builder().timeout(Duration::from_secs(10)).build()?;
    let r = client.head(url).send().await.map_err(|e| anyhow!("{e}"))?;
    let status = r.status();
    let len = r.headers().get(reqwest::header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<f64>().ok());
    if !status.is_success() {
        bail!("HTTP {status}");
    }
    Ok(format!("HTTP {status}{}", len.map(|l| format!(", {}", crate::util::fmt_bytes(l))).unwrap_or_default()))
}

/// Configured download storages for a server profile: main, then fallback.
pub fn configured_storages(profile: &ServerProfile, lxc: bool) -> Vec<String> {
    let (a, b) = if lxc {
        (&profile.catalog_lxc_storage, &profile.catalog_lxc_storage_fallback)
    } else {
        (&profile.catalog_qemu_storage, &profile.catalog_qemu_storage_fallback)
    };
    let mut v: Vec<String> = [a, b].iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    v.dedup();
    v
}

/// Usable download storages on one node, in try order. With storages
/// configured on the server profile only those are used (main, fallback);
/// otherwise any active storage carrying the needed
/// content, most free space first. Each must hold the image with 20% headroom
/// (a nearly-full `local` on the root FS must never be picked).
pub fn pick_candidates(storages: &[NodeStorage], configured: &[String], content: &str, size: u64) -> Vec<NodeStorage> {
    let need = (size as f64 * 1.2) as u64;
    let usable = |s: &&NodeStorage| s.usable() && s.has_content(content) && (s.avail <= 0.0 || s.avail as u64 >= need);
    if !configured.is_empty() {
        return configured.iter().filter_map(|name| storages.iter().filter(usable).find(|s| &s.storage == name).cloned()).collect();
    }
    let mut v: Vec<NodeStorage> = storages.iter().filter(usable).cloned().collect();
    v.sort_by(|a, b| b.avail.total_cmp(&a.avail));
    v
}

pub async fn node_candidates(
    client: &PveClient,
    node: &str,
    entry: &CatalogEntry,
    profile: &ServerProfile,
    size: u64,
) -> anyhow::Result<Vec<NodeStorage>> {
    let storages: Vec<NodeStorage> =
        client.get_list(&format!("/nodes/{node}/storage")).await?.iter().map(NodeStorage::from_value).collect();
    Ok(pick_candidates(&storages, &configured_storages(profile, entry.is_lxc()), entry.content(), size))
}

/// Explains why a node has no download storage (for error messages).
pub fn describe_missing(profile: &ServerProfile, entry: &CatalogEntry) -> String {
    let conf = configured_storages(profile, entry.is_lxc());
    if conf.is_empty() {
        format!(
            "no active storage with content `{}` and enough free space (enable `{}` on a storage, or set catalog download storages on the server)",
            entry.content(),
            entry.content()
        )
    } else {
        format!(
            "configured download storage(s) {} are offline, disabled, missing content `{}` or too full",
            conf.join(", "),
            entry.content()
        )
    }
}

/// A downloaded image on a node's storage.
#[derive(Clone, Debug)]
pub struct Artifact {
    pub node: String,
    pub storage: String,
    pub volid: String,
}

#[derive(Debug)]
enum Failure {
    /// The URL itself is broken (404/403/...): no storage or retry can help.
    Permanent(String),
    /// The mirror is briefly unreachable: retry the same URL later.
    Transient(String),
    /// This storage failed (stall, write error, ...): try the next candidate.
    Storage(String),
}

/// Classifies a failed download from its exit status and task log.
fn classify(text: &str) -> &'static str {
    let hay = text.to_lowercase();
    for n in ["404 not found", "404:", "410 gone", "403 forbidden", "unauthorized", "401"] {
        if hay.contains(n) {
            return "permanent";
        }
    }
    for n in [
        "unable to resolve host",
        "name or service not known",
        "temporary failure in name resolution",
        "connection refused",
        "no route to host",
        "connection timed out",
        "operation timed out",
        "ssl",
        "certificate",
        "handshake",
        "500 internal",
        "502 bad gateway",
        "503 service",
        "504 gateway",
        "429 too many",
        "http error 5",
        "could not connect",
    ] {
        if hay.contains(n) {
            return "transient";
        }
    }
    "storage"
}

async fn task_log(client: &PveClient, node: &str, upid: &str) -> Vec<String> {
    client
        .get_q(&format!("/nodes/{node}/tasks/{}/log", urlencoding::encode(upid)), &Params::new().set("start", 0).set("limit", 5000))
        .await
        .map(|v| v.arr().iter().map(|l| l.s("t")).collect())
        .unwrap_or_default()
}

async fn stop_task(client: &PveClient, node: &str, upid: &str) {
    let _ = client.delete(&format!("/nodes/{node}/tasks/{}", urlencoding::encode(upid)), &Params::new()).await;
}

async fn artifact_visible(client: &PveClient, a: &Artifact) -> bool {
    client
        .get_q(&format!("/nodes/{}/storage/{}/content", a.node, a.storage), &Params::new())
        .await
        .map(|v| v.arr().iter().any(|i| i.s("volid") == a.volid))
        .unwrap_or(false)
}

/// Removes a downloaded artifact. Failures are logged, never fatal.
pub async fn cleanup(client: &PveClient, ctx: &JobCtx, a: &Artifact) {
    let path = format!("/nodes/{}/storage/{}/content/{}", a.node, a.storage, urlencoding::encode(&a.volid));
    match client.delete(&path, &Params::new()).await {
        Ok(v) => {
            if let Some(upid) = v.as_str() {
                let _ = client.wait_task(upid, Duration::from_secs(300), |_| {}).await;
            }
            ctx.info(format!("removed downloaded image {}", a.volid));
        }
        Err(e) => {
            let msg = format!("{e:#}").to_lowercase();
            if !(msg.contains("no such") || msg.contains("not found") || msg.contains("does not exist")) {
                ctx.warn(format!("could not remove downloaded image {}: {e:#} (delete it from Storage manually)", a.volid));
            }
        }
    }
}

async fn download_to(
    client: &PveClient,
    ctx: &JobCtx,
    node: &str,
    storage: &str,
    entry: &CatalogEntry,
    vmid: u64,
    settings: &Settings,
) -> Result<Artifact, Failure> {
    let token = format!("{:x}", rand::random::<u32>());
    let filename = entry.artifact_filename(vmid, &token);
    let a = Artifact { node: node.to_string(), storage: storage.to_string(), volid: format!("{storage}:{}/{filename}", entry.content()) };
    let p = Params::new()
        .set("content", entry.content())
        .set("filename", &filename)
        .set("url", &entry.url)
        .opt("checksum", Some(&entry.checksum))
        .opt("checksum-algorithm", Some(&entry.checksum_algorithm));
    ctx.info(format!("downloading {} to {storage} on {node}", entry.url));
    let upid = match client.post(&format!("/nodes/{node}/storage/{storage}/download-url"), &p).await {
        Ok(v) => v.as_str().map(str::to_string).ok_or_else(|| Failure::Storage("download-url returned no task".into()))?,
        Err(e) => {
            let api_status = e.downcast_ref::<crate::api::ApiError>().map(|a| a.status);
            // 4xx on the POST itself (bad params, missing privilege, content type off): the storage can't take it.
            return Err(match api_status {
                Some(s) if (400..500).contains(&s) && s != 401 => Failure::Storage(format!("{e:#}")),
                Some(_) => Failure::Storage(format!("{e:#}")),
                None => Failure::Storage(format!("node unreachable: {e:#}")),
            });
        }
    };
    let node_of = Upid::parse(&upid).map(|u| u.node).unwrap_or_else(|| node.to_string());
    // Stall detection driven by the task's own log, never by polling the
    // (possibly hung) storage.
    let stall = Duration::from_secs(settings.download_stall_secs.max(30));
    let max_wait = Duration::from_secs(settings.download_max_wait_secs.max(60));
    let started = Instant::now();
    let mut last_len = 0usize;
    let mut last_progress = Instant::now();
    let mut misses = 0;
    loop {
        match client.get(&format!("/nodes/{node_of}/tasks/{}/status", urlencoding::encode(&upid))).await {
            Ok(st) => {
                misses = 0;
                if st.s("status") == "stopped" {
                    let exit = st.s("exitstatus");
                    if crate::api::types::task_exit_ok(&exit) {
                        break;
                    }
                    let log = task_log(client, &node_of, &upid).await;
                    let tail = log.iter().rev().take(8).rev().cloned().collect::<Vec<_>>().join("\n");
                    let text = format!("{exit}\n{}", log.join("\n"));
                    cleanup(client, ctx, &a).await;
                    let msg = format!("{exit}{}", if tail.is_empty() { String::new() } else { format!("\n{tail}") });
                    return Err(match classify(&text) {
                        "permanent" => Failure::Permanent(msg),
                        "transient" => Failure::Transient(msg),
                        _ => Failure::Storage(msg),
                    });
                }
            }
            Err(e) => {
                misses += 1;
                if misses > 6 {
                    stop_task(client, &node_of, &upid).await;
                    cleanup(client, ctx, &a).await;
                    return Err(Failure::Storage(format!("lost contact with the download task: {e:#}")));
                }
            }
        }
        let len = task_log(client, &node_of, &upid).await.len();
        if len > last_len {
            last_len = len;
            last_progress = Instant::now();
            ctx.progress((started.elapsed().as_secs_f32() / max_wait.as_secs_f32()).min(0.3));
        } else if last_progress.elapsed() >= stall {
            stop_task(client, &node_of, &upid).await;
            cleanup(client, ctx, &a).await;
            return Err(Failure::Storage(format!("no download progress for {}s", last_progress.elapsed().as_secs())));
        }
        if started.elapsed() >= max_wait {
            stop_task(client, &node_of, &upid).await;
            cleanup(client, ctx, &a).await;
            return Err(Failure::Storage(format!("download exceeded the maximum wait of {}s", max_wait.as_secs())));
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    // Storage content can lag behind task completion.
    for _ in 0..15 {
        if artifact_visible(client, &a).await {
            ctx.info(format!("downloaded {}", a.volid));
            return Ok(a);
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    cleanup(client, ctx, &a).await;
    Err(Failure::Storage(format!("download finished but {} never appeared on the storage", a.volid)))
}

/// Downloads an entry's image to the first working candidate storage.
/// A transient mirror error is retried (same URL) a couple of times; a
/// permanent URL error fails at once without trying other storages.
pub async fn download(
    client: &PveClient,
    ctx: &JobCtx,
    node: &str,
    entry: &CatalogEntry,
    vmid: u64,
    candidates: &[NodeStorage],
    settings: &Settings,
) -> anyhow::Result<Artifact> {
    if candidates.is_empty() {
        bail!("no usable download storage on {node}");
    }
    let mut tried = Vec::new();
    let mut last = String::new();
    for c in candidates {
        let mut attempts = 0;
        loop {
            attempts += 1;
            match download_to(client, ctx, node, &c.storage, entry, vmid, settings).await {
                Ok(a) => return Ok(a),
                Err(Failure::Permanent(m)) => {
                    bail!("the image URL is broken (no retry or other storage can fix this - check the catalog entry):\n{m}")
                }
                Err(Failure::Transient(m)) if attempts < 3 => {
                    ctx.warn(format!("mirror temporarily unreachable, retrying in 20s: {}", m.lines().next().unwrap_or("")));
                    tokio::time::sleep(Duration::from_secs(20)).await;
                }
                Err(Failure::Transient(m)) => bail!("the image source is unreachable (tried {attempts} times); try again later:\n{m}"),
                Err(Failure::Storage(m)) => {
                    ctx.warn(format!("download to `{}` failed, trying the next storage: {}", c.storage, m.lines().next().unwrap_or("")));
                    tried.push(c.storage.clone());
                    last = m;
                    break;
                }
            }
        }
    }
    bail!("all candidate download storages failed ({}): {last}", tried.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn module_catalog_compat() {
        let text = r#"{"entries": [
            {"id": "Debian 12 Cloud", "label": "Debian 12", "virt": "qemu",
             "url": "https://cloud.debian.org/images/cloud/bookworm/latest/debian-12-genericcloud-amd64.qcow2",
             "cloud_user": "debian", "ostype": "Linux", "nodes": "pve1 pve2", "tpl_data": {"bios": "ovmf", "cores": 2}},
            {"id": "alpine", "label": "Alpine 3.20", "virt": "lxc",
             "url": "http://download.proxmox.com/images/system/alpine-3.20-default_20240908_amd64.tar.xz", "ostype": "alpine"}
        ]}"#;
        let c = parse_catalog(text).unwrap();
        assert_eq!(c[0].id, "debian-12-cloud");
        assert_eq!(c[0].ostype, "l26");
        assert_eq!(c[0].nodes, vec!["pve1", "pve2"]);
        assert_eq!(c[0].config.get("bios").map(String::as_str), Some("ovmf"));
        assert_eq!(c[0].config.get("cores").map(String::as_str), Some("2"));
        assert_eq!(c[1].content(), "vztmpl");
        let f = c[0].artifact_filename(105, "ab-12");
        assert_eq!(f, "debian-12-cloud-vm105-ab12-debian-12-genericcloud-amd64.qcow2");
        assert!(c[1].artifact_filename(7, "x").ends_with(".tar.xz"));
        assert_eq!(normalize_ostype("qemu", "Windows Server"), "win10");
    }

    #[test]
    fn validation_messages() {
        let bad = |v: Value| format!("{:#}", CatalogEntry::from_value(&v, 0).unwrap_err());
        assert!(bad(json!({"id": "", "label": "x"})).contains("missing a valid id"));
        assert!(bad(json!({"id": "a", "label": "A", "virt": "kvm"})).contains("qemu` or `lxc"));
        assert!(bad(json!({"id": "a", "label": "A", "virt": "qemu", "url": "ftp://x/y"})).contains("HTTP or HTTPS"));
        assert!(
            bad(json!({"id": "a", "label": "A", "virt": "lxc", "url": "https://x/y.tar.zst", "cloud_user": "root"})).contains("cloud_user")
        );
        assert!(
            bad(json!({"id": "a", "label": "A", "virt": "qemu", "url": "https://x/y", "checksum_algorithm": "sha256"}))
                .contains("without checksum")
        );
        let cfg = |virt: &str, k: &str, v: &str| {
            bad(json!({"id": "a", "label": "A", "virt": virt, "url": "https://x/y.tar.zst", "config": {k: v}}))
        };
        assert!(cfg("lxc", "unprivileged", "0").contains("not allowed"));
        assert!(cfg("lxc", "mp0", "/,mp=/host").contains("not allowed"));
        assert!(cfg("lxc", "features", "nesting=1,mount=nfs").contains("feature `mount`"));
        assert!(cfg("qemu", "args", "-chardev x").contains("not allowed"));
        assert!(cfg("qemu", "hookscript", "local:snippets/x.sh").contains("not allowed"));
        assert!(cfg("qemu", "cicustom", "user=local:snippets/u.yml").contains("not allowed"));
        assert!(cfg("qemu", "net0", "virtio,bridge=vmbr1,firewall=0").contains("not allowed"));
        assert!(
            CatalogEntry::from_value(
                &json!({"id": "a", "label": "A", "virt": "lxc", "url": "https://x/y.tar.zst",
            "config": {"features": "nesting=1,keyctl=1", "cores": 2}}),
                0
            )
            .is_ok()
        );
        assert!(parse_catalog(r#"[{"id":"a","label":"A","virt":"lxc","url":"https://h/a.tar.zst"},{"id":"A","label":"B","virt":"lxc","url":"https://h/b.tar.zst"}]"#)
            .unwrap_err()
            .to_string()
            .contains("duplicate"));
    }

    #[test]
    fn candidates_and_classification() {
        let st = |name: &str, content: &str, avail_gb: u64, active: bool| NodeStorage {
            storage: name.into(),
            content: content.into(),
            active,
            enabled: true,
            avail: (avail_gb * GIB) as f64,
            ..Default::default()
        };
        let list = vec![st("local", "iso,vztmpl,import", 50, true), st("nfs", "import", 500, false), st("big", "import,images", 800, true)];
        // Configured: main is offline, fallback used.
        assert_eq!(
            pick_candidates(&list, &["nfs".into(), "local".into()], "import", 3 * GIB)
                .iter()
                .map(|s| s.storage.clone())
                .collect::<Vec<_>>(),
            vec!["local"]
        );
        // Auto: most free space first, offline skipped.
        assert_eq!(
            pick_candidates(&list, &[], "import", 3 * GIB).iter().map(|s| s.storage.clone()).collect::<Vec<_>>(),
            vec!["big", "local"]
        );
        // 20% headroom: a 45 GiB image needs 54 GiB.
        assert_eq!(pick_candidates(&list, &[], "import", 45 * GIB).len(), 1);
        assert_eq!(classify("ERROR: 404 Not Found"), "permanent");
        assert_eq!(classify("could not connect to server: connection refused"), "transient");
        assert_eq!(classify("write error: no space left on device"), "storage");
    }
}
