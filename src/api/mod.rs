//! Proxmox VE API client.
//!
//! Supports API-token auth (stateless, no CSRF) and ticket auth (password,
//! optional TOTP second factor, CSRF header on writes, automatic renewal).

pub mod tls;
pub mod types;

use crate::config::{AuthMode, ServerProfile, TlsMode};
use crate::util::ValueExt;
use anyhow::{Context, anyhow, bail};
use parking_lot::RwLock;
use serde_json::Value;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Tickets are valid for two hours; renew well before that.
const TICKET_RENEW_AFTER: Duration = Duration::from_secs(60 * 60);

#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub message: String,
    pub path: String,
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({}): {}", self.path, self.status, self.message)
    }
}

impl std::error::Error for ApiError {}

/// Ordered form parameters. Proxmox takes form-encoded bodies for writes.
#[derive(Clone, Debug, Default)]
pub struct Params(pub Vec<(String, String)>);

impl Params {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn set(mut self, k: &str, v: impl ToString) -> Self {
        self.push(k, v);
        self
    }
    pub fn push(&mut self, k: &str, v: impl ToString) {
        self.0.retain(|(key, _)| key != k);
        self.0.push((k.to_string(), v.to_string()));
    }
    pub fn opt(mut self, k: &str, v: Option<impl ToString>) -> Self {
        if let Some(v) = v {
            let s = v.to_string();
            if !s.is_empty() {
                self.push(k, s);
            }
        }
        self
    }
    pub fn flag(self, k: &str, on: bool) -> Self {
        self.set(k, if on { "1" } else { "0" })
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn get(&self, k: &str) -> Option<&str> {
        self.0.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str())
    }
}

#[derive(Clone)]
struct Ticket {
    ticket: String,
    csrf: String,
    obtained: Instant,
}

enum Credentials {
    Token { header: String },
    Password { userid: String, password: String },
}

struct Inner {
    base: String,
    host: String,
    port: u16,
    userid: String,
    http: reqwest::Client,
    creds: Credentials,
    ticket: RwLock<Option<Ticket>>,
}

/// Cheap to clone; all clones share the HTTP pool and the auth ticket.
#[derive(Clone)]
pub struct PveClient {
    inner: Arc<Inner>,
}

pub enum LoginOutcome {
    Ok(PveClient),
    /// The account has TOTP enabled and no code was supplied.
    NeedTotp,
}

impl PveClient {
    fn build_http(profile: &ServerProfile) -> anyhow::Result<reqwest::Client> {
        let mut b = reqwest::Client::builder()
            .http1_only()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(profile.request_timeout_secs.max(5)))
            .user_agent(concat!("PMXSpan/", env!("CARGO_PKG_VERSION")))
            .no_proxy();
        match profile.tls {
            TlsMode::System => {}
            TlsMode::Pinned => {
                let fp = tls::parse_fingerprint(&profile.fingerprint).ok_or_else(|| {
                    anyhow!(
                        "TLS mode is 'pinned' but no valid SHA-256 fingerprint is configured; use 'Fetch fingerprint' in the server editor"
                    )
                })?;
                let v = Arc::new(tls::FingerprintVerifier::new(Some(fp)));
                b = b.use_preconfigured_tls(tls::client_config(v)?);
            }
            TlsMode::Insecure => {
                let v = Arc::new(tls::FingerprintVerifier::new(None));
                b = b.use_preconfigured_tls(tls::client_config(v)?);
            }
        }
        Ok(b.build()?)
    }

    /// Connects and authenticates. With password auth this performs the ticket
    /// login immediately so bad credentials fail here, not on the first call.
    pub async fn login(profile: &ServerProfile, secret: &str, otp: Option<&str>) -> anyhow::Result<LoginOutcome> {
        let host = profile.host.trim().to_string();
        if host.is_empty() {
            bail!("server host is empty");
        }
        if secret.is_empty() {
            bail!("no secret stored for this server (token secret or password)");
        }
        let http = Self::build_http(profile)?;
        let base = format!("https://{}:{}/api2/json", bracket_ipv6(&host), profile.port);
        let userid = profile.userid();
        let creds = match profile.auth {
            AuthMode::ApiToken => {
                if profile.token_id.trim().is_empty() {
                    bail!("API token auth selected but token id is empty");
                }
                Credentials::Token { header: format!("PVEAPIToken={}!{}={}", userid, profile.token_id.trim(), secret) }
            }
            AuthMode::Password => Credentials::Password { userid: userid.clone(), password: secret.to_string() },
        };
        let client =
            PveClient { inner: Arc::new(Inner { base, host, port: profile.port, userid, http, creds, ticket: RwLock::new(None) }) };
        if let Credentials::Password { .. } = client.inner.creds {
            if !client.ticket_login(otp).await? {
                return Ok(LoginOutcome::NeedTotp);
            }
        } else {
            // Tokens are stateless; validate them with a cheap authenticated call.
            client.get("/version").await.context("API token rejected")?;
        }
        Ok(LoginOutcome::Ok(client))
    }

    /// Returns `false` when a TOTP code is required but was not given.
    async fn ticket_login(&self, otp: Option<&str>) -> anyhow::Result<bool> {
        let Credentials::Password { userid, password } = &self.inner.creds else { return Ok(true) };
        let url = format!("{}/access/ticket", self.inner.base);
        let resp = self
            .inner
            .http
            .post(&url)
            .form(&[("username", userid.as_str()), ("password", password.as_str())])
            .send()
            .await
            .map_err(|e| anyhow!("connection failed: {}", error_chain(&e)))?;
        let data = Self::decode(resp, "/access/ticket").await?;
        let mut ticket = data.s("ticket");
        let mut csrf = data.s("CSRFPreventionToken");
        if data.b("NeedTFA") || ticket.starts_with("PVE:!tfa!") || ticket.contains("!tfa!") {
            let Some(code) = otp.filter(|c| !c.trim().is_empty()) else { return Ok(false) };
            let resp = self
                .inner
                .http
                .post(&url)
                .form(&[("username", userid.as_str()), ("tfa-challenge", ticket.as_str()), ("password", &format!("totp:{}", code.trim()))])
                .send()
                .await
                .map_err(|e| anyhow!("connection failed: {}", error_chain(&e)))?;
            let data = Self::decode(resp, "/access/ticket").await.context("second factor rejected")?;
            ticket = data.s("ticket");
            csrf = data.s("CSRFPreventionToken");
        }
        if ticket.is_empty() {
            bail!("login succeeded but no ticket was returned");
        }
        *self.inner.ticket.write() = Some(Ticket { ticket, csrf, obtained: Instant::now() });
        Ok(true)
    }

    async fn ensure_ticket(&self) -> anyhow::Result<()> {
        if let Credentials::Password { .. } = self.inner.creds {
            let stale = self.inner.ticket.read().as_ref().map(|t| t.obtained.elapsed() > TICKET_RENEW_AFTER).unwrap_or(true);
            if stale {
                // Renew using the current ticket as password (Proxmox supports this),
                // falling back to a full login.
                let current = self.inner.ticket.read().clone();
                if let Some(t) = current {
                    let url = format!("{}/access/ticket", self.inner.base);
                    let r = self
                        .inner
                        .http
                        .post(&url)
                        .form(&[("username", self.inner.userid.as_str()), ("password", t.ticket.as_str())])
                        .send()
                        .await;
                    if let Ok(resp) = r
                        && let Ok(data) = Self::decode(resp, "/access/ticket").await
                    {
                        let ticket = data.s("ticket");
                        if !ticket.is_empty() {
                            *self.inner.ticket.write() =
                                Some(Ticket { ticket, csrf: data.s("CSRFPreventionToken"), obtained: Instant::now() });
                            return Ok(());
                        }
                    }
                }
                if !self.ticket_login(None).await? {
                    bail!("session expired and a TOTP code is required; reconnect to this server");
                }
            }
        }
        Ok(())
    }

    fn authorize(&self, req: reqwest::RequestBuilder, write: bool) -> reqwest::RequestBuilder {
        match &self.inner.creds {
            Credentials::Token { header } => req.header("Authorization", header),
            Credentials::Password { .. } => {
                let t = self.inner.ticket.read().clone();
                match t {
                    Some(t) => {
                        let req = req.header("Cookie", format!("PVEAuthCookie={}", t.ticket));
                        if write { req.header("CSRFPreventionToken", t.csrf) } else { req }
                    }
                    None => req,
                }
            }
        }
    }

    pub fn host(&self) -> &str {
        &self.inner.host
    }
    pub fn port(&self) -> u16 {
        self.inner.port
    }
    pub fn userid(&self) -> &str {
        &self.inner.userid
    }
    pub fn is_root(&self) -> bool {
        self.inner.userid == "root@pam" && matches!(self.inner.creds, Credentials::Password { .. })
    }
    pub fn uses_token(&self) -> bool {
        matches!(self.inner.creds, Credentials::Token { .. })
    }

    fn url(&self, path: &str) -> String {
        let path = path.trim_start_matches('/');
        format!("{}/{}", self.inner.base, path)
    }

    async fn decode(resp: reqwest::Response, path: &str) -> anyhow::Result<Value> {
        let status = resp.status();
        // Proxmox puts the real error in the HTTP reason phrase.
        let reason = resp
            .extensions()
            .get::<hyper::ext::ReasonPhrase>()
            .map(|r| String::from_utf8_lossy(r.as_bytes()).to_string())
            .or_else(|| status.canonical_reason().map(str::to_string))
            .unwrap_or_default();
        let text = resp.text().await.unwrap_or_default();
        let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if status.is_success() {
            return Ok(body.get("data").cloned().unwrap_or(Value::Null));
        }
        let mut message = reason.trim().to_string();
        if let Some(m) = body.get("message").and_then(Value::as_str) {
            let m = m.trim();
            if !m.is_empty() && !message.contains(m) {
                message = if message.is_empty() { m.to_string() } else { format!("{message}: {m}") };
            }
        }
        if let Some(Value::Object(errs)) = body.get("errors") {
            let detail: Vec<String> = errs.iter().map(|(k, v)| format!("{k}: {}", v.as_str().unwrap_or(&v.to_string()).trim())).collect();
            if !detail.is_empty() {
                message = format!("{message} [{}]", detail.join("; "));
            }
        }
        if message.is_empty() {
            message = text.chars().take(300).collect();
        }
        Err(ApiError { status: status.as_u16(), message, path: path.to_string() }.into())
    }

    async fn send(&self, method: reqwest::Method, path: &str, params: &Params) -> anyhow::Result<Value> {
        self.ensure_ticket().await?;
        let write = method != reqwest::Method::GET;
        let url = self.url(path);
        let mut req = self.inner.http.request(method.clone(), &url);
        req = match method {
            reqwest::Method::GET | reqwest::Method::DELETE => req.query(&params.0),
            _ => req.form(&params.0),
        };
        req = self.authorize(req, write);
        tracing::debug!("{} {}", method, path);
        let resp = req.send().await.map_err(|e| anyhow!("{} {}: {}", method, path, error_chain(&e)))?;
        let result = Self::decode(resp, path).await;
        if let Err(e) = &result {
            tracing::warn!("{} {} failed: {:#}", method, path, e);
        }
        result
    }

    pub async fn get(&self, path: &str) -> anyhow::Result<Value> {
        self.send(reqwest::Method::GET, path, &Params::new()).await
    }
    pub async fn get_q(&self, path: &str, params: &Params) -> anyhow::Result<Value> {
        self.send(reqwest::Method::GET, path, params).await
    }
    pub async fn post(&self, path: &str, params: &Params) -> anyhow::Result<Value> {
        self.send(reqwest::Method::POST, path, params).await
    }
    pub async fn put(&self, path: &str, params: &Params) -> anyhow::Result<Value> {
        self.send(reqwest::Method::PUT, path, params).await
    }
    pub async fn delete(&self, path: &str, params: &Params) -> anyhow::Result<Value> {
        self.send(reqwest::Method::DELETE, path, params).await
    }

    pub async fn get_list(&self, path: &str) -> anyhow::Result<Vec<Value>> {
        Ok(self.get(path).await?.arr())
    }

    /// Uploads an ISO / container template / snippet to a storage.
    /// `progress` is called with (bytes sent, total).
    pub async fn upload(
        &self,
        node: &str,
        storage: &str,
        content: &str,
        file: &std::path::Path,
        progress: impl Fn(u64, u64) + Send + Sync + 'static,
    ) -> anyhow::Result<String> {
        use futures_util::StreamExt;
        self.ensure_ticket().await?;
        let name = file.file_name().and_then(|n| n.to_str()).ok_or_else(|| anyhow!("invalid file name"))?.to_string();
        let f = tokio::fs::File::open(file).await.with_context(|| format!("opening {}", file.display()))?;
        let total = f.metadata().await?.len();
        let progress = Arc::new(progress);
        let sent = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let stream = tokio_util_stream(f).map({
            let progress = progress.clone();
            let sent = sent.clone();
            move |chunk: std::io::Result<bytes_crate::Bytes>| {
                if let Ok(c) = &chunk {
                    let n = sent.fetch_add(c.len() as u64, std::sync::atomic::Ordering::Relaxed) + c.len() as u64;
                    progress(n, total);
                }
                chunk
            }
        });
        let body = reqwest::Body::wrap_stream(stream);
        let part = reqwest::multipart::Part::stream_with_length(body, total).file_name(name).mime_str("application/octet-stream")?;
        let form = reqwest::multipart::Form::new().text("content", content.to_string()).part("filename", part);
        let path = format!("/nodes/{node}/storage/{storage}/upload");
        let req = self.inner.http.post(self.url(&path)).multipart(form).timeout(Duration::from_secs(6 * 3600));
        let req = self.authorize(req, true);
        let resp = req.send().await.map_err(|e| anyhow!("upload failed: {}", error_chain(&e)))?;
        let data = Self::decode(resp, &path).await?;
        Ok(data.as_str().unwrap_or_default().to_string())
    }

    /// Polls a task until it stops. Returns the final status object, or an
    /// error that includes the tail of the task log when it failed - failures
    /// are never silent.
    pub async fn wait_task(&self, upid: &str, timeout: Duration, mut on_tick: impl FnMut(&Value) + Send) -> anyhow::Result<Value> {
        let node = types::Upid::parse(upid).map(|u| u.node).ok_or_else(|| anyhow!("malformed UPID: {upid}"))?;
        let started = Instant::now();
        let path = format!("/nodes/{node}/tasks/{}/status", urlencoding::encode(upid));
        let mut misses = 0;
        loop {
            match self.get(&path).await {
                Ok(st) => {
                    misses = 0;
                    on_tick(&st);
                    if st.s("status") == "stopped" {
                        let exit = st.s("exitstatus");
                        if types::task_exit_ok(&exit) {
                            return Ok(st);
                        }
                        let tail = self.task_log_tail(&node, upid, 25).await.unwrap_or_default();
                        bail!("task failed: {exit}{}", if tail.is_empty() { String::new() } else { format!("\n--- task log ---\n{tail}") });
                    }
                }
                Err(e) => {
                    // Task status can briefly vanish right after submission.
                    misses += 1;
                    if misses > 10 {
                        return Err(e.context("lost track of task"));
                    }
                }
            }
            if started.elapsed() > timeout {
                bail!("timed out waiting for task {upid} (it may still be running on the server)");
            }
            tokio::time::sleep(Duration::from_millis(1000)).await;
        }
    }

    pub async fn task_log_tail(&self, node: &str, upid: &str, lines: u64) -> anyhow::Result<String> {
        let path = format!("/nodes/{node}/tasks/{}/log", urlencoding::encode(upid));
        // Ask for a large window then keep the tail; `total` is not always reliable.
        let data = self.get_q(&path, &Params::new().set("start", 0).set("limit", 5000)).await?;
        let all: Vec<String> = data.arr().iter().map(|l| l.s("t")).collect();
        let skip = all.len().saturating_sub(lines as usize);
        Ok(all[skip..].join("\n"))
    }
}

/// Connects without credentials and returns the presented certificate's
/// SHA-256 fingerprint, for trust-on-first-use pinning.
pub async fn fetch_fingerprint(host: &str, port: u16) -> anyhow::Result<String> {
    let v = Arc::new(tls::FingerprintVerifier::new(None));
    let captured = v.captured();
    let http = reqwest::Client::builder()
        .http1_only()
        .no_proxy()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(15))
        .use_preconfigured_tls(tls::client_config(v)?)
        .build()?;
    let url = format!("https://{}:{}/api2/json/version", bracket_ipv6(host.trim()), port);
    // 401 is expected; we only need the handshake to happen.
    let _ = http.get(&url).send().await.map_err(|e| anyhow!("connection failed: {}", error_chain(&e)))?;
    let fp = captured.lock().clone().ok_or_else(|| anyhow!("server did not present a certificate"))?;
    Ok(tls::format_fingerprint(&fp))
}

fn bracket_ipv6(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') { format!("[{host}]") } else { host.to_string() }
}

fn error_chain(e: &dyn std::error::Error) -> String {
    let mut parts = vec![e.to_string()];
    let mut src = e.source();
    while let Some(s) = src {
        let msg = s.to_string();
        if !parts.iter().any(|p| p.contains(&msg)) {
            parts.push(msg);
        }
        src = s.source();
    }
    parts.join(": ")
}

/// Turns a file into a stream of byte chunks without pulling in tokio-util.
fn tokio_util_stream(file: tokio::fs::File) -> impl futures_util::Stream<Item = std::io::Result<bytes_crate::Bytes>> + Send + 'static {
    use tokio::io::AsyncReadExt;
    futures_util::stream::unfold(file, |mut f| async move {
        let mut buf = vec![0u8; 256 * 1024];
        match f.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok(bytes_crate::Bytes::from(buf)), f))
            }
            Err(e) => Some((Err(e), f)),
        }
    })
}
