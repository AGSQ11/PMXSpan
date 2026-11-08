//! End to end: a real server on a free loopback port, reading a cluster
//! served by `tools/mock_pve.py`, driven over HTTP by scripted clients. One
//! client walks the generic MCP flow; the other repeats SSHSpan's exact
//! header set and request sequence (src-tauri/src/assistant/mcp.rs there).
//!
//! Skipped (with a note on stderr) when `python3` or `openssl` is missing.

use super::server::RunningServer;
use super::{AppView, Connected, ProfileInfo, ViewState};
use crate::api::{LoginOutcome, PveClient};
use crate::config::{AuthMode, ServerProfile, TlsMode};
use crate::ipam::{Assignment, IpamDb, Pool};
use parking_lot::Mutex;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

const TOKEN: &str = "integration-token-abcdefghijklmnopqrstuvwxyz0123";

struct Mock {
    child: Child,
    port: u16,
    dir: std::path::PathBuf,
}

impl Drop for Mock {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn have(cmd: &str, arg: &str) -> bool {
    Command::new(cmd).arg(arg).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok()
}

fn start_mock() -> Option<Mock> {
    if !have("python3", "--version") || !have("openssl", "version") {
        eprintln!("skipping MCP integration test: python3 and openssl are needed for tools/mock_pve.py");
        return None;
    }
    let port = std::net::TcpListener::bind("127.0.0.1:0").ok()?.local_addr().ok()?.port();
    let dir = std::env::temp_dir().join(format!("pmxspan-mcp-mock-{}", crate::config::new_id()));
    std::fs::create_dir_all(&dir).ok()?;
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/tools/mock_pve.py");
    let child = Command::new("python3")
        .args([script, "--port", &port.to_string(), "--certdir", &dir.to_string_lossy()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mock = Mock { child, port, dir };
    let started = Instant::now();
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        if started.elapsed() > Duration::from_secs(20) {
            panic!("tools/mock_pve.py did not start listening on port {port}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Some(mock)
}

fn profile(port: u16) -> ServerProfile {
    ServerProfile {
        id: "lab-id".into(),
        name: "mock-lab".into(),
        host: "127.0.0.1".into(),
        port,
        auth: AuthMode::ApiToken,
        username: "root".into(),
        realm: "pam".into(),
        token_id: "pmxspan".into(),
        tls: TlsMode::Insecure,
        ..Default::default()
    }
}

/// A view connected to the mock, with an IPAM pool holding db01's address.
async fn connected_view(mock: &Mock) -> (Arc<AppView>, std::path::PathBuf) {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let p = profile(mock.port);
    let client = match PveClient::login(&p, "mocktoken", None).await.expect("login to the mock") {
        LoginOutcome::Ok(c) => c,
        LoginOutcome::NeedTotp => panic!("unexpected TOTP"),
    };
    let dir = mock.dir.join("data");
    std::fs::create_dir_all(&dir).unwrap();
    let mut db = IpamDb::load(&dir.join("ipam.json"));
    let pool = Pool { name: "lan".into(), cidr: "10.20.0.0/24".into(), gateway: "10.20.0.1".into(), ..Default::default() };
    db.assignments.push(Assignment {
        ip: "10.20.0.21".into(),
        pool_id: pool.id.clone(),
        server_id: p.id.clone(),
        vmid: Some(101),
        hostname: "db01".into(),
        iface: Some(0),
        main: true,
        ..Default::default()
    });
    db.pools.push(pool);
    let view = Arc::new(AppView::new(dir.join(super::AUDIT_FILE)));
    view.set_state(ViewState {
        profiles: vec![
            ProfileInfo { id: p.id.clone(), name: p.display_name(), host: p.host.clone(), port: p.port },
            ProfileInfo { id: "other".into(), name: "prod".into(), host: "10.9.9.9".into(), port: 8006 },
        ],
        connected: Some(Connected { client, profile: p }),
        ipam: Some(Arc::new(Mutex::new(db))),
        managed_tag: "PMXSpan".into(),
    });
    (view, dir)
}

fn http() -> reqwest::Client {
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).no_proxy().build().unwrap()
}

struct Client {
    http: reqwest::Client,
    url: String,
    session: Option<String>,
    next: u64,
}

impl Client {
    fn new(port: u16) -> Self {
        Self { http: http(), url: format!("http://127.0.0.1:{port}/mcp"), session: None, next: 1 }
    }

    async fn post(&self, body: &Value) -> reqwest::Response {
        let mut r = self
            .http
            .post(&self.url)
            .header("Authorization", format!("Bearer {TOKEN}"))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .body(body.to_string());
        if let Some(s) = &self.session {
            r = r.header("Mcp-Session-Id", s).header("MCP-Protocol-Version", "2025-06-18");
        }
        r.send().await.unwrap()
    }

    async fn rpc(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let resp = self.post(&json!({"jsonrpc":"2.0","id": self.next,"method": method,"params": params})).await;
        assert_eq!(resp.status(), 200);
        let v: Value = resp.json().await.unwrap();
        assert_eq!(v["id"], self.next);
        v
    }

    /// Calls a tool and returns (text, isError).
    async fn tool(&mut self, name: &str, args: Value) -> (String, bool) {
        let v = self.rpc("tools/call", json!({"name": name, "arguments": args})).await;
        let r = &v["result"];
        assert!(r.is_object(), "{name}: {v}");
        (r["content"][0]["text"].as_str().unwrap_or_default().to_string(), r["isError"].as_bool().unwrap())
    }

    async fn ok(&mut self, name: &str, args: Value) -> String {
        let (text, err) = self.tool(name, args.clone()).await;
        assert!(!err, "{name} {args} failed: {text}");
        text
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generic_client_against_mock_cluster() {
    let Some(mock) = start_mock() else { return };
    let (view, dir) = connected_view(&mock).await;
    let server = RunningServer::start(view.clone(), 0, TOKEN).unwrap();
    let mut c = Client::new(server.port());

    let resp = c
        .post(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"scripted","version":"0"}}}))
        .await;
    assert_eq!(resp.status(), 200);
    c.session = Some(resp.headers()["mcp-session-id"].to_str().unwrap().to_string());
    let init: Value = resp.json().await.unwrap();
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    let resp = c.post(&json!({"jsonrpc":"2.0","method":"notifications/initialized"})).await;
    assert_eq!(resp.status(), 202);

    let tools = c.rpc("tools/list", json!({})).await;
    assert_eq!(tools["result"]["tools"].as_array().unwrap().len(), super::tools::READ_TOOLS.len());

    let servers = c.ok("list_servers", json!({})).await;
    assert!(servers.contains("mock-lab") && servers.contains("connected") && servers.contains("prod"), "{servers}");

    let guests = c.ok("list_guests", json!({})).await;
    for want in ["web01", "db01", "dns01", "staging", "10.20.0.21"] {
        assert!(guests.contains(want), "{want} missing:\n{guests}");
    }
    assert!(!guests.contains("debian-12-cloud"), "templates are hidden by default");
    let prod = c.ok("list_guests", json!({"tag": "prod", "status": "running"})).await;
    assert!(prod.contains("web01") && prod.contains("db01") && !prod.contains("dns01"), "{prod}");

    // find_by_ip: IPAM, static config, guest agent, node, nothing.
    let hit = c.ok("find_by_ip", json!({"ip": "10.20.0.21"})).await;
    assert!(hit.contains("VM 101 'db01'") && hit.contains("via IPAM"), "{hit}");
    let hit = c.ok("find_by_ip", json!({"ip": "10.20.0.53/24"})).await;
    assert!(hit.contains("CT 102 'dns01'") && hit.contains("static network config"), "{hit}");
    let hit = c.ok("find_by_ip", json!({"ip": "10.0.0.100"})).await;
    assert!(hit.contains("VM 100 'web01'") && hit.contains("guest agent"), "{hit}");
    let hit = c.ok("find_by_ip", json!({"ip": "10.0.0.11"})).await;
    assert!(hit.contains("node pve1"), "{hit}");
    let miss = c.ok("find_by_ip", json!({"ip": "192.0.2.77"})).await;
    assert!(miss.starts_with("No guest or node"), "{miss}");
    // The agent addresses found on the way now show in list_guests.
    let guests = c.ok("list_guests", json!({"name": "web"})).await;
    assert!(guests.contains("10.0.0.100"), "{guests}");

    let g = c.ok("get_guest", json!({"vmid": 100})).await;
    assert!(g.contains("before_upgrade") && g.contains("web01") && g.contains("customer-acme") && g.contains("scsi0="), "{g}");
    let (e, err) = c.tool("get_guest", json!({"vmid": 4242})).await;
    assert!(err && e.contains("list_guests"), "{e}");
    let (e, err) = c.tool("list_nodes", json!({"server": "prod"})).await;
    assert!(err && e.contains("not connected"), "{e}");

    assert!(c.ok("list_nodes", json!({"server": "mock-lab"})).await.contains("pve2"));
    assert!(c.ok("get_node_status", json!({"node": "pve1"})).await.contains("AMD EPYC"));
    assert!(c.ok("list_snapshots", json!({"vmid": 100})).await.contains("before_upgrade"));
    let backups = c.ok("list_backups", json!({})).await;
    assert!(backups.contains("pbs:backup/vm/100") && backups.contains("vzdump-qemu-100"), "{backups}");
    let pools = c.ok("ipam_lookup", json!({})).await;
    assert!(pools.contains("lan | 10.20.0.0/24"), "{pools}");
    assert!(c.ok("ipam_lookup", json!({"address": "10.20.0.21"})).await.contains("VMID 101"));
    assert!(c.ok("ipam_lookup", json!({"address": "10.20.0.99"})).await.contains("free in pool lan"));
    let drift = c.ok("list_ipam_drift", json!({})).await;
    assert!(drift.contains("10.20.0.53"), "dns01's static address is not in IPAM:\n{drift}");

    // Actions: refused while off, listed and working once allowed.
    let (t, err) = c.tool("power", json!({"vmid": 103, "action": "start"})).await;
    assert!(err && t.contains("Allow actions over MCP"), "{t}");
    view.set_allow_actions(true);
    assert_eq!(c.rpc("tools/list", json!({})).await["result"]["tools"].as_array().unwrap().len(), 16);
    let t = c.ok("power", json!({"vmid": 103, "action": "start"})).await;
    assert!(t.contains("done"), "{t}");
    let upid = t.lines().find_map(|l| l.strip_prefix("UPID: ")).expect("UPID in the result").to_string();
    let log = c.ok("get_task_log", json!({"upid": upid})).await;
    assert!(log.contains("TASK OK") && log.contains("exit status OK"), "{log}");
    let tasks = c.ok("list_tasks", json!({"vmid": 103})).await;
    assert!(tasks.contains(&upid) && tasks.contains("Start"), "{tasks}");
    assert!(c.ok("snapshot_create", json!({"vmid": 100, "name": "mcp_test", "description": "from MCP"})).await.contains("done"));
    assert!(c.ok("list_snapshots", json!({"vmid": 100})).await.contains("mcp_test"));
    let out = c.ok("agent_exec", json!({"vmid": 100, "argv": ["uname", "-a"]})).await;
    assert!(out.contains("exited with code 0") && out.contains("hello from the mock guest agent"), "{out}");
    let (t, err) = c.tool("agent_exec", json!({"vmid": 102, "argv": ["id"]})).await;
    assert!(err && t.contains("container"), "{t}");
    let (t, err) = c.tool("agent_exec", json!({"vmid": 103, "argv": ["id"]})).await;
    assert!(err, "{t}");
    assert!(c.ok("set_tags", json!({"vmid": 103, "tags": ["test", "mcp"]})).await.contains("→ test,mcp"));
    assert!(c.ok("get_guest", json!({"vmid": 103})).await.contains("tags: test,mcp"));
    let (t, err) = c.tool("power", json!({"vmid": 103, "action": "reset"})).await;
    assert!(err && t.starts_with("Refused"), "{t}");
    let (t, err) = c.tool("destroy", json!({"vmid": 103})).await;
    assert!(err && t.starts_with("Refused"), "{t}");

    let audit = std::fs::read_to_string(dir.join(super::AUDIT_FILE)).unwrap();
    let outcomes: Vec<&str> = audit.lines().map(|l| l.rsplit("outcome=").next().unwrap()).collect();
    assert_eq!(outcomes, ["refused", "ok", "ok", "ok", "error", "error", "ok", "error", "refused"], "{audit}");

    let resp = c
        .http
        .delete(&c.url)
        .header("Authorization", format!("Bearer {TOKEN}"))
        .header("Mcp-Session-Id", c.session.clone().unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
    let resp = c.post(&json!({"jsonrpc":"2.0","id":99,"method":"ping"})).await;
    assert_eq!(resp.status(), 404);
    tokio::task::spawn_blocking(move || server.stop_and_wait(Duration::from_secs(3))).await.unwrap();
}

/// SSHSpan's pin: SHA-256 over canonical JSON of these five fields
/// (`tool_definition_hash` in SSHSpan's mcp.rs).
fn sshspan_pin(def: &Value) -> String {
    fn canonical(v: &Value) -> String {
        match v {
            Value::Object(m) => {
                let mut keys: Vec<&String> = m.keys().collect();
                keys.sort();
                let parts: Vec<String> =
                    keys.iter().map(|k| format!("{}:{}", Value::String((*k).clone()), canonical(&m[k.as_str()]))).collect();
                format!("{{{}}}", parts.join(","))
            }
            Value::Array(a) => format!("[{}]", a.iter().map(canonical).collect::<Vec<_>>().join(",")),
            other => other.to_string(),
        }
    }
    let mut m = serde_json::Map::new();
    for f in ["name", "title", "description", "inputSchema", "annotations"] {
        m.insert(f.into(), def.get(f).cloned().unwrap_or(Value::Null));
    }
    hex::encode(Sha256::digest(canonical(&Value::Object(m)).as_bytes()))
}

/// The requests SSHSpan's client sends, header for header: `post_jsonrpc`
/// (Accept, Content-Type, auth header, Mcp-Session-Id and
/// MCP-Protocol-Version once known), `initialize_unchecked` (uuid string ids,
/// empty capabilities, protocolVersion 2025-11-25, clientInfo SSHSpan),
/// `send_initialized_unchecked`, `fetch_tools_once` (params {} and
/// nextCursor pagination), the tools/call body, and `delete_session`.
async fn sshspan_session(port: u16, find_ip: &str) -> (Vec<String>, String) {
    let http = http();
    let url = format!("http://127.0.0.1:{port}/mcp");
    let auth = format!("Bearer {TOKEN}");
    let post = |body: Value, session: Option<String>, pv: Option<String>| {
        let mut req = http
            .post(&url)
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json")
            .json(&body)
            .header("Authorization", auth.as_str());
        if let Some(sid) = session {
            req = req.header("Mcp-Session-Id", sid);
        }
        if let Some(pv) = pv {
            req = req.header("MCP-Protocol-Version", pv);
        }
        req.send()
    };
    let uuid = || crate::config::new_id();

    // initialize: no session, no version header.
    let id = uuid();
    let resp = post(
        json!({"jsonrpc":"2.0","id":id,"method":"initialize","params":{
            "capabilities":{},"protocolVersion":"2025-11-25","clientInfo":{"name":"SSHSpan","version":"1.9.1"}}}),
        None,
        None,
    )
    .await
    .unwrap();
    assert!(!resp.status().is_redirection());
    assert_eq!(resp.status(), 200);
    let sid = resp.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let sid = sid.expect("SSHSpan stores the Mcp-Session-Id of the initialize reply");
    let ctype = resp.headers()["content-type"].to_str().unwrap().to_lowercase();
    assert!(!ctype.starts_with("text/event-stream"), "read as a plain JSON body");
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["id"], json!(id), "SSHSpan rejects an answer to another id");
    let negotiated = v["result"]["protocolVersion"].as_str().unwrap().to_string();
    assert!(["2025-11-25", "2025-06-18", "2025-03-26"].contains(&negotiated.as_str()));
    assert_eq!(negotiated, "2025-11-25");

    // notifications/initialized: any 2xx (202 expected).
    let resp =
        post(json!({"jsonrpc":"2.0","method":"notifications/initialized"}), Some(sid.clone()), Some(negotiated.clone())).await.unwrap();
    assert_eq!(resp.status(), 202);

    // tools/list, paginated until no nextCursor.
    let mut tools = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let id = uuid();
        let mut params = json!({});
        if let Some(c) = &cursor {
            params["cursor"] = json!(c);
        }
        let resp =
            post(json!({"jsonrpc":"2.0","id":id,"method":"tools/list","params":params}), Some(sid.clone()), Some(negotiated.clone()))
                .await
                .unwrap();
        assert_eq!(resp.status(), 200);
        let v: Value = resp.json().await.unwrap();
        assert_eq!(v["id"], json!(id));
        assert!(v.get("error").is_none(), "{v}");
        tools.extend(v["result"]["tools"].as_array().unwrap().iter().cloned());
        cursor = v["result"].get("nextCursor").and_then(Value::as_str).filter(|c| !c.is_empty()).map(String::from);
        if cursor.is_none() {
            break;
        }
    }
    // SSHSpan's budget: 64 tools minus its 5 built-ins.
    assert!(tools.len() <= 59);
    let pins: Vec<String> = tools.iter().map(sshspan_pin).collect();

    // tools/call.
    let id = uuid();
    let resp = post(
        json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"find_by_ip","arguments":{"ip":find_ip}}}),
        Some(sid.clone()),
        Some(negotiated.clone()),
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), 200);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["id"], json!(id));
    assert_eq!(v["result"]["isError"], false, "{v}");
    let text = v["result"]["content"][0]["text"].as_str().unwrap().to_string();
    assert!(v["result"].get("structuredContent").is_none(), "SSHSpan would show it twice");

    // delete_session: 2xx (or 405) accepted.
    let resp = http
        .delete(&url)
        .header("Mcp-Session-Id", &sid)
        .header("MCP-Protocol-Version", &negotiated)
        .header("Accept", "application/json, text/event-stream")
        .header("Authorization", auth.as_str())
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "{}", resp.status());
    // Spec + SSHSpan: a 404 on the old session means "initialize again".
    let resp = post(json!({"jsonrpc":"2.0","id":uuid(),"method":"ping"}), Some(sid), Some(negotiated)).await.unwrap();
    assert_eq!(resp.status(), 404);
    (pins, text)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sshspan_client_sequence() {
    let Some(mock) = start_mock() else { return };
    let (view, _dir) = connected_view(&mock).await;

    let server = RunningServer::start(view.clone(), 0, TOKEN).unwrap();
    let (pins, text) = sshspan_session(server.port(), "10.20.0.21").await;
    assert!(text.contains("db01"), "{text}");
    tokio::task::spawn_blocking(move || server.stop_and_wait(Duration::from_secs(3))).await.unwrap();

    // A restart (new port, new process state) offers byte-identical
    // definitions, so SSHSpan's approvals survive it.
    let server = RunningServer::start(view.clone(), 0, TOKEN).unwrap();
    let (again, _) = sshspan_session(server.port(), "10.20.0.53").await;
    assert_eq!(pins, again);
    tokio::task::spawn_blocking(move || server.stop_and_wait(Duration::from_secs(3))).await.unwrap();

    // Stopped means stopped: nothing listens any more.
    let port = {
        let s = RunningServer::start(view, 0, TOKEN).unwrap();
        let p = s.port();
        tokio::task::spawn_blocking(move || s.stop_and_wait(Duration::from_secs(3))).await.unwrap();
        p
    };
    assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_err(), "port {port} still accepts after stop");
}
