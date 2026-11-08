//! The HTTP side of the MCP server: Streamable HTTP on `/mcp`, loopback
//! only, bearer-token auth, sessions.
//!
//! Every request goes through the same checks, in this order: loopback peer,
//! endpoint path, loopback `Host`, loopback `Origin` (when sent), bearer
//! token. Then `POST` carries JSON-RPC (one message or a batch) and is
//! answered with `application/json` (202 with no body when it held only
//! notifications), `DELETE` ends a session, and anything else is 405.

use super::protocol::{self, Body, Message, RpcError};
use super::{AppView, SESSIONS, tools};
use bytes_crate::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::header::{self, HeaderMap, HeaderValue};
use hyper::{Method, Request, Response, StatusCode};
use parking_lot::{Mutex, RwLock};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use subtle::ConstantTimeEq;
use tokio::net::{TcpListener, TcpSocket};

pub const ENDPOINT: &str = "/mcp";
pub const MAX_BODY_BYTES: usize = 1024 * 1024;
pub const MAX_CONNECTIONS: usize = 32;
pub const SESSION_IDLE: Duration = Duration::from_secs(30 * 60);
const MAX_SESSIONS: usize = 128;
const DRAIN: Duration = Duration::from_secs(2);

struct Session {
    last_seen: Instant,
    version: String,
    initialized: bool,
}

/// Shared by every connection of one running server.
pub struct ServerState {
    /// SHA-256 of the bearer token; the token itself is not kept.
    token_digest: RwLock<[u8; 32]>,
    sessions: Mutex<HashMap<String, Session>>,
    pub view: Arc<AppView>,
}

fn digest(s: &str) -> [u8; 32] {
    Sha256::digest(s.as_bytes()).into()
}

impl ServerState {
    pub fn new(view: Arc<AppView>, token: &str) -> Self {
        Self { token_digest: RwLock::new(digest(token)), sessions: Mutex::new(HashMap::new()), view }
    }

    /// Switches to a new token and ends every session.
    pub fn set_token(&self, token: &str) {
        *self.token_digest.write() = digest(token);
        self.sessions.lock().clear();
        SESSIONS.store(0, Ordering::Relaxed);
    }

    /// Constant time in the token's content and length: both sides are
    /// hashed to 32 bytes and compared with `subtle`.
    pub fn token_matches(&self, presented: &str) -> bool {
        let want = *self.token_digest.read();
        bool::from(digest(presented).ct_eq(&want))
    }

    fn new_session(&self, version: &str) -> String {
        use rand::RngCore;
        let mut raw = [0u8; 24];
        rand::rngs::OsRng.fill_bytes(&mut raw);
        let id = hex::encode(raw);
        let mut s = self.sessions.lock();
        s.retain(|_, v| v.last_seen.elapsed() < SESSION_IDLE);
        if s.len() >= MAX_SESSIONS
            && let Some(oldest) = s.iter().min_by_key(|(_, v)| v.last_seen).map(|(k, _)| k.clone())
        {
            s.remove(&oldest);
        }
        s.insert(id.clone(), Session { last_seen: Instant::now(), version: version.to_string(), initialized: false });
        SESSIONS.store(s.len(), Ordering::Relaxed);
        id
    }

    /// Marks a session used; `None` when unknown or idle for too long.
    fn touch(&self, id: &str) -> Option<String> {
        let mut s = self.sessions.lock();
        let alive = s.get(id).is_some_and(|v| v.last_seen.elapsed() < SESSION_IDLE);
        if !alive {
            if s.remove(id).is_some() {
                SESSIONS.store(s.len(), Ordering::Relaxed);
            }
            return None;
        }
        let v = s.get_mut(id)?;
        v.last_seen = Instant::now();
        Some(v.version.clone())
    }

    fn mark_initialized(&self, id: &str) {
        if let Some(v) = self.sessions.lock().get_mut(id) {
            v.initialized = true;
        }
    }

    fn end_session(&self, id: &str) -> bool {
        let mut s = self.sessions.lock();
        let had = s.remove(id).is_some();
        SESSIONS.store(s.len(), Ordering::Relaxed);
        had
    }

    pub fn sweep(&self) {
        let mut s = self.sessions.lock();
        s.retain(|_, v| v.last_seen.elapsed() < SESSION_IDLE);
        SESSIONS.store(s.len(), Ordering::Relaxed);
    }

    #[cfg(test)]
    fn age_session(&self, id: &str, by: Duration) {
        if let Some(v) = self.sessions.lock().get_mut(id) {
            v.last_seen = Instant::now().checked_sub(by).unwrap_or(v.last_seen);
        }
    }

    #[cfg(test)]
    fn is_initialized(&self, id: &str) -> bool {
        self.sessions.lock().get(id).is_some_and(|v| v.initialized)
    }
}

// ---- checks ----------------------------------------------------------------

pub fn peer_allowed(ip: IpAddr) -> bool {
    ip.to_canonical().is_loopback()
}

fn loopback_host(host: &str) -> bool {
    let h = host.trim().trim_start_matches('[').trim_end_matches(']');
    h.eq_ignore_ascii_case("localhost") || h.parse::<IpAddr>().is_ok_and(|ip| ip.to_canonical().is_loopback())
}

/// Splits `host[:port]` / `[v6][:port]` and checks the host part.
pub fn host_header_allowed(value: &str) -> bool {
    let v = value.trim();
    let host = if let Some(rest) = v.strip_prefix('[') {
        match rest.split_once(']') {
            Some((h, tail)) if tail.is_empty() || tail.starts_with(':') => h,
            _ => return false,
        }
    } else {
        match v.rsplit_once(':') {
            Some((h, port)) if port.chars().all(|c| c.is_ascii_digit()) => h,
            Some(_) => return false,
            None => v,
        }
    };
    !host.is_empty() && loopback_host(host)
}

/// An `Origin` header is fine only when it names a loopback http(s) origin.
pub fn origin_allowed(origin: &str) -> bool {
    let o = origin.trim();
    let Some(rest) = o.strip_prefix("http://").or_else(|| o.strip_prefix("https://")) else { return false };
    let authority = rest.split('/').next().unwrap_or("");
    !authority.contains('@') && host_header_allowed(authority)
}

/// The token of an `Authorization: Bearer <token>` header.
pub fn bearer(headers: &HeaderMap) -> Option<&str> {
    let v = headers.get(header::AUTHORIZATION)?.to_str().ok()?.trim();
    let (scheme, token) = v.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| token.trim()).filter(|t| !t.is_empty())
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok()).map(str::trim)
}

// ---- responses -------------------------------------------------------------

type Resp = Response<Full<Bytes>>;

fn json_response(status: StatusCode, body: &Value) -> Resp {
    let mut r = Response::new(Full::new(Bytes::from(body.to_string())));
    *r.status_mut() = status;
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

/// An HTTP refusal whose body is a JSON-RPC error with a null id.
fn refusal(status: StatusCode, code: i64, message: &str) -> Resp {
    json_response(status, &protocol::error_response(&Value::Null, &RpcError::new(code, message)))
}

fn empty(status: StatusCode) -> Resp {
    let mut r = Response::new(Full::new(Bytes::new()));
    *r.status_mut() = status;
    r
}

fn unauthorized(wrong: bool) -> Resp {
    let (msg, challenge) = if wrong {
        (
            "Wrong bearer token. Copy the current token from PMXSpan (Settings → MCP server) into your MCP client; regenerating it there invalidates the old one.",
            "Bearer realm=\"pmxspan\", error=\"invalid_token\"",
        )
    } else {
        (
            "Missing bearer token. Send 'Authorization: Bearer <token>' with the token shown in PMXSpan (Settings → MCP server).",
            "Bearer realm=\"pmxspan\"",
        )
    };
    let mut r = refusal(StatusCode::UNAUTHORIZED, protocol::UNAUTHORIZED, msg);
    r.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static(challenge));
    r
}

fn method_not_allowed() -> Resp {
    let mut r = refusal(
        StatusCode::METHOD_NOT_ALLOWED,
        protocol::BAD_TRANSPORT,
        "Method not allowed: this MCP endpoint takes POST (JSON-RPC) and DELETE (end session); it offers no server-sent event stream on GET.",
    );
    r.headers_mut().insert(header::ALLOW, HeaderValue::from_static("POST, DELETE"));
    r
}

// ---- request handling ------------------------------------------------------

/// Handles one HTTP request. Generic over the body so tests can call it
/// without a socket.
pub async fn handle<B>(state: &ServerState, peer: SocketAddr, req: Request<B>) -> Resp
where
    B: hyper::body::Body<Data = Bytes>,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    if !peer_allowed(peer.ip()) {
        return refusal(StatusCode::FORBIDDEN, protocol::FORBIDDEN, "Forbidden: the PMXSpan MCP server only accepts loopback connections.");
    }
    if req.uri().path() != ENDPOINT {
        return refusal(StatusCode::NOT_FOUND, protocol::BAD_TRANSPORT, "Not found: the MCP endpoint is /mcp.");
    }
    let headers = req.headers();
    if let Some(host) = header_str(headers, "host")
        && !host_header_allowed(host)
    {
        return refusal(
            StatusCode::FORBIDDEN,
            protocol::FORBIDDEN,
            "Forbidden: the Host header must name a loopback address (127.0.0.1, [::1] or localhost).",
        );
    }
    if let Some(origin) = headers.get(header::ORIGIN)
        && !origin.to_str().is_ok_and(origin_allowed)
    {
        return refusal(
            StatusCode::FORBIDDEN,
            protocol::FORBIDDEN,
            "Forbidden: requests from web pages are refused; only a loopback Origin (or none) is accepted.",
        );
    }
    match bearer(headers) {
        None => return unauthorized(false),
        Some(t) if !state.token_matches(t) => return unauthorized(true),
        Some(_) => {}
    }
    match *req.method() {
        Method::POST => post(state, req).await,
        Method::DELETE => delete(state, req.headers()),
        _ => method_not_allowed(),
    }
}

fn protocol_header_problem(headers: &HeaderMap) -> Option<Resp> {
    let v = header_str(headers, "mcp-protocol-version")?;
    if protocol::is_supported(v) {
        return None;
    }
    Some(refusal(
        StatusCode::BAD_REQUEST,
        protocol::BAD_TRANSPORT,
        &format!(
            "Unsupported MCP-Protocol-Version '{v}'. This server speaks {}; initialize again and use the version it returns.",
            protocol::SUPPORTED_VERSIONS.join(", ")
        ),
    ))
}

fn delete(state: &ServerState, headers: &HeaderMap) -> Resp {
    if let Some(r) = protocol_header_problem(headers) {
        return r;
    }
    let Some(id) = header_str(headers, "mcp-session-id").filter(|s| !s.is_empty()) else {
        return refusal(StatusCode::BAD_REQUEST, protocol::SESSION_REQUIRED, "Missing Mcp-Session-Id header: name the session to end.");
    };
    if state.end_session(id) {
        empty(StatusCode::NO_CONTENT)
    } else {
        refusal(StatusCode::NOT_FOUND, protocol::SESSION_NOT_FOUND, "Unknown or expired session; there is nothing to end.")
    }
}

fn accepts_json(headers: &HeaderMap) -> bool {
    let Some(accept) = header_str(headers, "accept") else { return true };
    accept
        .split(',')
        .map(|p| p.split(';').next().unwrap_or("").trim().to_ascii_lowercase())
        .any(|m| m == "application/json" || m == "application/*" || m == "*/*" || m.is_empty())
}

async fn post<B>(state: &ServerState, req: Request<B>) -> Resp
where
    B: hyper::body::Body<Data = Bytes>,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let headers = req.headers().clone();
    let ctype = header_str(&headers, "content-type").unwrap_or("").to_ascii_lowercase();
    if !ctype.starts_with("application/json") {
        return refusal(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            protocol::BAD_TRANSPORT,
            "Unsupported media type: POST a JSON-RPC message with 'Content-Type: application/json'.",
        );
    }
    if !accepts_json(&headers) {
        return refusal(
            StatusCode::NOT_ACCEPTABLE,
            protocol::BAD_TRANSPORT,
            "Not acceptable: this server answers with application/json; include it in the Accept header.",
        );
    }
    let too_big =
        || refusal(StatusCode::PAYLOAD_TOO_LARGE, protocol::INVALID_REQUEST, "Request body too large: the limit is 1 MiB per request.");
    if header_str(&headers, "content-length").and_then(|v| v.parse::<u64>().ok()).is_some_and(|n| n > MAX_BODY_BYTES as u64) {
        return too_big();
    }
    let bytes = match Limited::new(req.into_body(), MAX_BODY_BYTES).collect().await {
        Ok(c) => c.to_bytes(),
        Err(e) => {
            if e.downcast_ref::<http_body_util::LengthLimitError>().is_some() {
                return too_big();
            }
            return refusal(StatusCode::BAD_REQUEST, protocol::INVALID_REQUEST, "Could not read the request body.");
        }
    };
    let body = match protocol::parse_body(&bytes) {
        Ok(b) => b,
        Err(e) => return json_response(StatusCode::BAD_REQUEST, &protocol::error_response(&Value::Null, &e)),
    };

    let is_init = |v: &Value| v.get("method").and_then(Value::as_str) == Some("initialize");
    match &body {
        Body::Single(v) if is_init(v) => return initialize(state, v),
        Body::Batch(items) if items.iter().any(is_init) => {
            let err = RpcError::new(protocol::INVALID_REQUEST, "Invalid request: initialize must be sent on its own, not inside a batch.");
            return json_response(StatusCode::BAD_REQUEST, &protocol::error_response(&Value::Null, &err));
        }
        _ => {}
    }

    if let Some(r) = protocol_header_problem(&headers) {
        return r;
    }
    let Some(session) = header_str(&headers, "mcp-session-id").filter(|s| !s.is_empty()).map(str::to_string) else {
        return refusal(
            StatusCode::BAD_REQUEST,
            protocol::SESSION_REQUIRED,
            "Missing Mcp-Session-Id header: send initialize first and repeat the Mcp-Session-Id it returns on every later request.",
        );
    };
    if state.touch(&session).is_none() {
        return refusal(
            StatusCode::NOT_FOUND,
            protocol::SESSION_NOT_FOUND,
            "Unknown or expired session (sessions end after 30 idle minutes, on DELETE, when the token changes and when PMXSpan stops the server). Send initialize again without a session id.",
        );
    }

    match body {
        Body::Single(v) => match process(state, &session, &v).await {
            Some(resp) => json_response(StatusCode::OK, &resp),
            None => empty(StatusCode::ACCEPTED),
        },
        Body::Batch(items) => {
            let mut out = Vec::new();
            for v in &items {
                if let Some(r) = process(state, &session, v).await {
                    out.push(r);
                }
            }
            if out.is_empty() { empty(StatusCode::ACCEPTED) } else { json_response(StatusCode::OK, &Value::Array(out)) }
        }
    }
}

fn initialize(state: &ServerState, v: &Value) -> Resp {
    let (id, params) = match protocol::classify(v) {
        Message::Request { id, params, .. } => (id, params),
        Message::Invalid { id, error } => return json_response(StatusCode::BAD_REQUEST, &protocol::error_response(&id, &error)),
        _ => {
            let err = RpcError::new(protocol::INVALID_REQUEST, "Invalid request: initialize must be a request with an id.");
            return json_response(StatusCode::BAD_REQUEST, &protocol::error_response(&Value::Null, &err));
        }
    };
    let requested = params.get("protocolVersion").and_then(Value::as_str);
    let version = protocol::negotiate(requested);
    let client = params.pointer("/clientInfo/name").and_then(Value::as_str).unwrap_or("unknown client");
    let session = state.new_session(version);
    tracing::info!(
        "MCP session {} opened by {} (protocol {version})",
        &session[..8],
        client.chars().filter(|c| !c.is_control()).take(64).collect::<String>()
    );
    let mut r = json_response(StatusCode::OK, &protocol::result_response(&id, protocol::initialize_result(version)));
    if let Ok(h) = HeaderValue::from_str(&session) {
        r.headers_mut().insert("mcp-session-id", h);
    }
    r
}

/// Answers one message of a session; `None` for notifications and responses.
async fn process(state: &ServerState, session: &str, v: &Value) -> Option<Value> {
    match protocol::classify(v) {
        Message::Notification { method } => {
            if method == "notifications/initialized" {
                state.mark_initialized(session);
            }
            None
        }
        Message::Response => None,
        Message::Invalid { id, error } => Some(protocol::error_response(&id, &error)),
        Message::Request { id, method, params } => {
            let r = match method.as_str() {
                "ping" => Ok(json!({})),
                "tools/list" => tools_list(state, &params),
                "tools/call" => tools_call(state, session, &params).await,
                "initialize" => Err(RpcError::new(
                    protocol::INVALID_REQUEST,
                    "This session is already initialized; send initialize without an Mcp-Session-Id to start a new one.",
                )),
                other => Err(RpcError::new(
                    protocol::METHOD_NOT_FOUND,
                    format!("Method not found: '{other}'. This server offers ping, tools/list and tools/call."),
                )),
            };
            Some(match r {
                Ok(result) => protocol::result_response(&id, result),
                Err(e) => protocol::error_response(&id, &e),
            })
        }
    }
}

fn tools_list(state: &ServerState, params: &Value) -> Result<Value, RpcError> {
    if params.get("cursor").is_some_and(|c| !c.is_null()) {
        return Err(RpcError::new(
            protocol::INVALID_PARAMS,
            "Unknown cursor: this server returns every tool in one page; call tools/list without a cursor.",
        ));
    }
    Ok(json!({ "tools": tools::definitions(state.view.allow_actions()) }))
}

async fn tools_call(state: &ServerState, session: &str, params: &Value) -> Result<Value, RpcError> {
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return Err(RpcError::new(protocol::INVALID_PARAMS, "Invalid params: tools/call needs \"name\" (a tool from tools/list)."));
    };
    let args = match params.get("arguments") {
        None | Some(Value::Null) => json!({}),
        Some(a @ Value::Object(_)) => a.clone(),
        Some(_) => return Err(RpcError::new(protocol::INVALID_PARAMS, "Invalid params: \"arguments\" must be an object.")),
    };
    let out = tools::call(&state.view, session, name, &args).await?;
    Ok(json!({ "content": [{ "type": "text", "text": out.text }], "isError": out.is_error }))
}

// ---- lifecycle -------------------------------------------------------------

pub struct RunningServer {
    port: u16,
    addrs: Vec<SocketAddr>,
    state: Arc<ServerState>,
    stop: tokio::sync::watch::Sender<bool>,
    done: std::sync::mpsc::Receiver<()>,
}

fn bind(addr: SocketAddr) -> std::io::Result<TcpListener> {
    let s = if addr.is_ipv4() { TcpSocket::new_v4()? } else { TcpSocket::new_v6()? };
    // Lets a restart on the same port succeed while old connections drain.
    // (Not on Windows, where SO_REUSEADDR would allow port hijacking.)
    #[cfg(unix)]
    s.set_reuseaddr(true)?;
    s.bind(addr)?;
    s.listen(64)
}

impl RunningServer {
    /// Binds `127.0.0.1:<port>` (required) and `[::1]:<port>` (when
    /// available) and starts serving on the app runtime. Port 0 picks a
    /// free port (tests).
    pub fn start(view: Arc<AppView>, port: u16, token: &str) -> anyhow::Result<Self> {
        let _rt = crate::rt::runtime().enter();
        let v4 = bind(SocketAddr::from(([127, 0, 0, 1], port)))?;
        let port = v4.local_addr()?.port();
        let mut listeners = vec![v4];
        match bind(SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port))) {
            Ok(l) => listeners.push(l),
            Err(e) => tracing::debug!("MCP server: [::1]:{port} not available: {e}"),
        }
        let addrs = listeners.iter().filter_map(|l| l.local_addr().ok()).collect();
        let state = Arc::new(ServerState::new(view, token));
        let (stop, stop_rx) = tokio::sync::watch::channel(false);
        let (done_tx, done) = std::sync::mpsc::channel();
        crate::rt::spawn(serve(listeners, state.clone(), stop_rx, done_tx));
        Ok(Self { port, addrs, state, stop, done })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn addrs(&self) -> &[SocketAddr] {
        &self.addrs
    }

    pub fn state(&self) -> &Arc<ServerState> {
        &self.state
    }

    /// Stops accepting at once; open connections get up to 2 s to finish.
    pub fn signal_stop(self) {
        let _ = self.stop.send(true);
    }

    pub fn stop_and_wait(self, max: Duration) {
        let _ = self.stop.send(true);
        let _ = self.done.recv_timeout(max);
    }
}

async fn serve(
    listeners: Vec<TcpListener>,
    state: Arc<ServerState>,
    mut stop: tokio::sync::watch::Receiver<bool>,
    done: std::sync::mpsc::Sender<()>,
) {
    use hyper::server::conn::http1;
    use hyper_util::rt::{TokioIo, TokioTimer};
    use hyper_util::server::graceful::GracefulShutdown;

    let graceful = GracefulShutdown::new();
    let slots = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    let (conn_tx, mut conn_rx) = tokio::sync::mpsc::channel::<(tokio::net::TcpStream, SocketAddr)>(16);
    let mut acceptors = Vec::new();
    for l in listeners {
        let tx = conn_tx.clone();
        acceptors.push(tokio::spawn(async move {
            loop {
                match l.accept().await {
                    Ok(c) => {
                        if tx.send(c).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::debug!("MCP server accept failed: {e}");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
        }));
    }
    drop(conn_tx);
    let mut sweep = tokio::time::interval(Duration::from_secs(60));
    loop {
        tokio::select! {
            _ = stop.changed() => break,
            _ = sweep.tick() => state.sweep(),
            c = conn_rx.recv() => {
                let Some((stream, peer)) = c else { break };
                if !peer_allowed(peer.ip()) {
                    tracing::warn!("MCP server refused a connection from non-loopback {peer}");
                    continue;
                }
                let Ok(slot) = slots.clone().try_acquire_owned() else {
                    tracing::warn!("MCP server: more than {MAX_CONNECTIONS} connections, refusing {peer}");
                    continue;
                };
                let st = state.clone();
                let svc = hyper::service::service_fn(move |req| {
                    let st = st.clone();
                    async move { Ok::<_, Infallible>(handle(&st, peer, req).await) }
                });
                let conn = http1::Builder::new()
                    .timer(TokioTimer::new())
                    .header_read_timeout(Duration::from_secs(15))
                    .serve_connection(TokioIo::new(stream), svc);
                let fut = graceful.watch(conn);
                tokio::spawn(async move {
                    let _ = fut.await;
                    drop(slot);
                });
            }
        }
    }
    for a in &acceptors {
        a.abort();
    }
    drop(conn_rx);
    tokio::select! {
        _ = graceful.shutdown() => {}
        _ = tokio::time::sleep(DRAIN) => tracing::debug!("MCP server: connections still open after the drain, closing"),
    }
    state.sessions.lock().clear();
    SESSIONS.store(0, Ordering::Relaxed);
    let _ = done.send(());
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub const TOKEN: &str = "test-token-0123456789abcdefghijklmnopqrstu";

    pub fn state() -> ServerState {
        let dir = std::env::temp_dir().join(format!("pmxspan-mcp-srv-{}", crate::config::new_id()));
        ServerState::new(Arc::new(AppView::new(dir.join("mcp-audit.log"))), TOKEN)
    }

    fn local() -> SocketAddr {
        "127.0.0.1:50000".parse().unwrap()
    }

    pub struct Req {
        method: Method,
        headers: Vec<(String, String)>,
        body: String,
    }

    impl Req {
        pub fn post(body: Value) -> Self {
            Self {
                method: Method::POST,
                headers: vec![
                    ("host".into(), "127.0.0.1:48731".into()),
                    ("authorization".into(), format!("Bearer {TOKEN}")),
                    ("content-type".into(), "application/json".into()),
                    ("accept".into(), "application/json, text/event-stream".into()),
                ],
                body: body.to_string(),
            }
        }
        pub fn method(mut self, m: Method) -> Self {
            self.method = m;
            self
        }
        pub fn header(mut self, k: &str, v: &str) -> Self {
            self.headers.retain(|(n, _)| !n.eq_ignore_ascii_case(k));
            self.headers.push((k.to_ascii_lowercase(), v.into()));
            self
        }
        pub fn without(mut self, k: &str) -> Self {
            self.headers.retain(|(n, _)| !n.eq_ignore_ascii_case(k));
            self
        }
        pub fn raw_body(mut self, b: &str) -> Self {
            self.body = b.into();
            self
        }
        pub async fn send_from(self, st: &ServerState, peer: SocketAddr) -> (StatusCode, HeaderMap, Value) {
            let mut b = Request::builder().method(self.method).uri("/mcp");
            for (k, v) in &self.headers {
                b = b.header(k.as_str(), v.as_str());
            }
            let req = b.body(Full::new(Bytes::from(self.body))).unwrap();
            let resp = handle(st, peer, req).await;
            let status = resp.status();
            let headers = resp.headers().clone();
            let bytes = resp.into_body().collect().await.unwrap().to_bytes();
            let v = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap() };
            (status, headers, v)
        }
        pub async fn send(self, st: &ServerState) -> (StatusCode, HeaderMap, Value) {
            self.send_from(st, local()).await
        }
    }

    fn init_body() -> Value {
        json!({"jsonrpc":"2.0","id":"init-1","method":"initialize","params":{
            "capabilities":{}, "protocolVersion":"2025-11-25", "clientInfo":{"name":"test","version":"1"}}})
    }

    async fn open(st: &ServerState) -> String {
        let (status, h, v) = Req::post(init_body()).send(st).await;
        assert_eq!(status, StatusCode::OK, "{v}");
        h.get("mcp-session-id").unwrap().to_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn auth_is_required_and_checked() {
        let st = state();
        let (s, h, v) = Req::post(init_body()).without("authorization").send(&st).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        assert_eq!(h.get("www-authenticate").unwrap(), "Bearer realm=\"pmxspan\"");
        assert_eq!(v["error"]["code"], protocol::UNAUTHORIZED);
        assert!(v["id"].is_null());
        assert!(v["error"]["message"].as_str().unwrap().contains("Settings → MCP server"));

        let (s, h, _) = Req::post(init_body()).header("authorization", "Bearer nope").send(&st).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        assert!(h.get("www-authenticate").unwrap().to_str().unwrap().contains("invalid_token"));
        let (s, _, _) = Req::post(init_body()).header("authorization", &format!("Basic {TOKEN}")).send(&st).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        // A prefix or an extension of the token is wrong too.
        for t in [&TOKEN[..10], &format!("{TOKEN}x")] {
            let (s, _, _) = Req::post(init_body()).header("authorization", &format!("Bearer {t}")).send(&st).await;
            assert_eq!(s, StatusCode::UNAUTHORIZED);
        }
        let (s, _, _) = Req::post(init_body()).header("authorization", &format!("bearer {TOKEN}")).send(&st).await;
        assert_eq!(s, StatusCode::OK, "the scheme is case-insensitive");
    }

    #[test]
    fn token_comparison_is_digest_based() {
        let st = state();
        assert!(st.token_matches(TOKEN));
        assert!(!st.token_matches(""));
        assert!(!st.token_matches(&TOKEN[..TOKEN.len() - 1]));
        // Inputs of any length compare as two fixed 32-byte digests, so the
        // comparison does no work proportional to a matching prefix.
        assert_eq!(digest("a").len(), digest(&"a".repeat(10_000)).len());
        st.set_token("other");
        assert!(!st.token_matches(TOKEN));
        assert!(st.token_matches("other"));
    }

    #[tokio::test]
    async fn non_loopback_peers_and_origins_are_refused() {
        let st = state();
        let (s, _, v) = Req::post(init_body()).send_from(&st, "192.168.1.5:4000".parse().unwrap()).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        assert_eq!(v["error"]["code"], protocol::FORBIDDEN);
        let (s, _, _) = Req::post(init_body()).send_from(&st, "[::1]:4000".parse().unwrap()).await;
        assert_eq!(s, StatusCode::OK);
        let (s, _, _) = Req::post(init_body()).send_from(&st, "[::ffff:127.0.0.1]:4000".parse().unwrap()).await;
        assert_eq!(s, StatusCode::OK);

        for bad in ["https://evil.example", "http://127.0.0.1.evil.example", "null", "http://user@127.0.0.1", "file://"] {
            let (s, _, v) = Req::post(init_body()).header("origin", bad).send(&st).await;
            assert_eq!(s, StatusCode::FORBIDDEN, "{bad}");
            assert!(v["error"]["message"].as_str().unwrap().contains("Origin"));
        }
        for ok in ["http://localhost:3000", "http://127.0.0.1", "https://[::1]:8443", "tauri://localhost"] {
            let (s, _, _) = Req::post(init_body()).header("origin", ok).send(&st).await;
            let expect = if ok.starts_with("tauri") { StatusCode::FORBIDDEN } else { StatusCode::OK };
            assert_eq!(s, expect, "{ok}");
        }
        let (s, _, _) = Req::post(init_body()).header("host", "evil.example:48731").send(&st).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "DNS rebinding: a foreign Host is refused");
        for ok in ["localhost:48731", "[::1]:48731", "127.0.0.1"] {
            assert!(host_header_allowed(ok), "{ok}");
        }
        assert!(!host_header_allowed("[::1]x"));
    }

    #[tokio::test]
    async fn session_lifecycle() {
        let st = state();
        let (s, h, v) = Req::post(init_body()).send(&st).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(h.get("content-type").unwrap(), "application/json");
        assert_eq!(v["id"], "init-1");
        assert_eq!(v["result"]["protocolVersion"], "2025-11-25");
        assert_eq!(v["result"]["serverInfo"]["name"], "pmxspan");
        let sid = h.get("mcp-session-id").unwrap().to_str().unwrap().to_string();
        assert!(sid.len() >= 32 && sid.bytes().all(|b| (0x21..=0x7e).contains(&b)));

        let with = |r: Req| r.header("mcp-session-id", &sid).header("mcp-protocol-version", "2025-11-25");
        let (s, _, v) = with(Req::post(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))).send(&st).await;
        assert_eq!(s, StatusCode::ACCEPTED);
        assert!(v.is_null());
        assert!(st.is_initialized(&sid));

        let (s, _, v) = with(Req::post(json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}))).send(&st).await;
        assert_eq!(s, StatusCode::OK);
        let names: Vec<&str> = v["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"list_guests") && names.contains(&"find_by_ip"));
        assert!(!names.contains(&"power"), "actions are hidden while not allowed");

        let (s, _, v) =
            with(Req::post(json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list_servers","arguments":{}}})))
                .send(&st)
                .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["result"]["isError"], false);
        assert_eq!(v["result"]["content"][0]["type"], "text");

        let (s, _, v) =
            with(Req::post(json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"list_guests","arguments":{}}})))
                .send(&st)
                .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["result"]["isError"], true, "no server connected");
        assert!(v["result"]["content"][0]["text"].as_str().unwrap().contains("connect"));

        let (s, _, v) = with(Req::post(json!({"jsonrpc":"2.0","id":5,"method":"ping"}))).send(&st).await;
        assert_eq!((s, &v["result"]), (StatusCode::OK, &json!({})));

        let (s, _, v) = with(Req::post(json!({"jsonrpc":"2.0","id":6,"method":"resources/list"}))).send(&st).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["error"]["code"], protocol::METHOD_NOT_FOUND);

        let (s, _, v) = with(Req::post(json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"nope"}}))).send(&st).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["error"]["code"], protocol::INVALID_PARAMS);

        let (s, _, _) = with(Req::post(json!({})).method(Method::DELETE)).send(&st).await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        let (s, _, v) = with(Req::post(json!({"jsonrpc":"2.0","id":8,"method":"ping"}))).send(&st).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{v}");
        assert_eq!(v["error"]["code"], protocol::SESSION_NOT_FOUND);
        let (s, _, _) = with(Req::post(json!({})).method(Method::DELETE)).send(&st).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn session_rules() {
        let st = state();
        let ping = json!({"jsonrpc":"2.0","id":1,"method":"ping"});
        let (s, _, v) = Req::post(ping.clone()).send(&st).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"]["code"], protocol::SESSION_REQUIRED);
        let (s, _, _) = Req::post(ping.clone()).header("mcp-session-id", "deadbeef").send(&st).await;
        assert_eq!(s, StatusCode::NOT_FOUND);

        let sid = open(&st).await;
        let (s, _, v) = Req::post(ping.clone()).header("mcp-session-id", &sid).header("mcp-protocol-version", "1999-01-01").send(&st).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(v["error"]["message"].as_str().unwrap().contains("MCP-Protocol-Version"));
        // No version header: accepted (older clients).
        let (s, _, _) = Req::post(ping.clone()).header("mcp-session-id", &sid).send(&st).await;
        assert_eq!(s, StatusCode::OK);

        // Idle sessions expire.
        st.age_session(&sid, SESSION_IDLE + Duration::from_secs(1));
        let (s, _, _) = Req::post(ping.clone()).header("mcp-session-id", &sid).send(&st).await;
        assert_eq!(s, StatusCode::NOT_FOUND);

        // A new token ends every session.
        let sid = open(&st).await;
        st.set_token(TOKEN);
        let (s, _, _) = Req::post(ping).header("mcp-session-id", &sid).send(&st).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn version_negotiation_on_initialize() {
        let st = state();
        let mut body = init_body();
        body["params"]["protocolVersion"] = json!("2025-06-18");
        let (_, _, v) = Req::post(body.clone()).send(&st).await;
        assert_eq!(v["result"]["protocolVersion"], "2025-06-18");
        body["params"]["protocolVersion"] = json!("2024-11-05");
        let (_, _, v) = Req::post(body).send(&st).await;
        assert_eq!(v["result"]["protocolVersion"], protocol::LATEST_VERSION);
    }

    #[tokio::test]
    async fn batches() {
        let st = state();
        let sid = open(&st).await;
        let batch = json!([
            {"jsonrpc":"2.0","method":"notifications/initialized"},
            {"jsonrpc":"2.0","id":1,"method":"ping"},
            {"jsonrpc":"2.0","id":2,"method":"bogus"},
            {"jsonrpc":"1.0","id":3,"method":"ping"}
        ]);
        let (s, _, v) = Req::post(batch).header("mcp-session-id", &sid).send(&st).await;
        assert_eq!(s, StatusCode::OK);
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0]["result"], json!({}));
        assert_eq!(arr[1]["error"]["code"], protocol::METHOD_NOT_FOUND);
        assert_eq!(arr[2]["error"]["code"], protocol::INVALID_REQUEST);
        let (s, _, v) =
            Req::post(json!([{"jsonrpc":"2.0","method":"notifications/initialized"}])).header("mcp-session-id", &sid).send(&st).await;
        assert_eq!((s, v), (StatusCode::ACCEPTED, Value::Null));
        let (s, _, v) = Req::post(json!([init_body()])).send(&st).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(v["error"]["message"].as_str().unwrap().contains("batch"));
    }

    #[tokio::test]
    async fn transport_errors_explain_themselves() {
        let st = state();
        let (s, h, v) = Req::post(json!({})).method(Method::GET).send(&st).await;
        assert_eq!(s, StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(h.get("allow").unwrap(), "POST, DELETE");
        assert!(v["error"]["message"].is_string());
        let (s, _, v) = Req::post(json!({})).raw_body("{not json").send(&st).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"]["code"], protocol::PARSE_ERROR);
        let (s, _, _) = Req::post(init_body()).header("content-type", "text/plain").send(&st).await;
        assert_eq!(s, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        let (s, _, _) = Req::post(init_body()).header("accept", "text/event-stream").send(&st).await;
        assert_eq!(s, StatusCode::NOT_ACCEPTABLE);
        let big = format!("{{\"x\":\"{}\"}}", "a".repeat(MAX_BODY_BYTES + 10));
        let (s, _, v) = Req::post(json!({})).raw_body(&big).send(&st).await;
        assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE);
        assert!(v["error"]["message"].as_str().unwrap().contains("1 MiB"));
        let (s, _, _) = Req::post(json!({})).header("content-length", "99999999").raw_body("{}").send(&st).await;
        assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE);
        let (s, _, _) = Req::post(json!({})).method(Method::DELETE).send(&st).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
    }
}
