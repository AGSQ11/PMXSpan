//! MCP tools: their definitions and handlers.
//!
//! The definitions are a contract: clients such as SSHSpan pin a SHA-256 of
//! each one (name, title, description, inputSchema, annotations) and ask the
//! user to approve it again when it changes. They are written out here, in
//! one place, and `tests/mcp_tools.golden.json` holds their exact serialized
//! form; a test fails on any change, which must then be deliberate.
//!
//! Read tools are always listed. The four action tools are listed (and run)
//! only while "Allow actions over MCP" is on. Destructive operations do not
//! exist as tools; asking for one by name gets a refusal that says why.

use super::audit::Outcome;
use super::protocol::{self, RpcError};
use super::{AppView, Connected};
use crate::api::types::{GuestKind, GuestRef, Resource, TaskInfo, Upid, describe_task_type};
use crate::api::{Params, PveClient};
use crate::ipam::IpamDb;
use crate::util::{ValueExt, fmt_bytes, fmt_duration, fmt_epoch};
use parking_lot::Mutex;
use serde_json::{Map, Value, json};
use std::fmt::Write as _;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

pub const READ_TOOLS: [&str; 12] = [
    "list_servers",
    "list_nodes",
    "list_guests",
    "get_guest",
    "find_by_ip",
    "ipam_lookup",
    "list_tasks",
    "get_task_log",
    "get_node_status",
    "list_backups",
    "list_snapshots",
    "list_ipam_drift",
];
pub const ACTION_TOOLS: [&str; 4] = ["power", "snapshot_create", "agent_exec", "set_tags"];

/// How long an action waits for its Proxmox task before reporting it as
/// still running.
const ACTION_WAIT: Duration = Duration::from_secs(30);
/// Cap on any single text result.
const MAX_TEXT: usize = 24 * 1024;
const MAX_EXEC_OUTPUT: usize = 16 * 1024;
const PROBE_TIMEOUT: Duration = Duration::from_secs(4);
const PROBE_PARALLEL: usize = 8;

// ---- definitions -----------------------------------------------------------

fn server_prop() -> Value {
    json!({
        "type": "string",
        "description": "PMXSpan server profile id or name. Default: the server PMXSpan is connected to."
    })
}

fn vmid_prop() -> Value {
    json!({ "type": "integer", "minimum": 1, "description": "Guest VMID, e.g. 100." })
}

fn read_only() -> Value {
    json!({ "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false })
}

fn tool(name: &str, title: &str, description: &str, properties: Value, required: &[&str], annotations: Value) -> Value {
    let mut schema = json!({ "type": "object", "properties": properties });
    if !required.is_empty() {
        schema["required"] = json!(required);
    }
    json!({ "name": name, "title": title, "description": description, "inputSchema": schema, "annotations": annotations })
}

fn read_definitions() -> Vec<Value> {
    vec![
        tool(
            "list_servers",
            "List servers",
            "Lists the Proxmox VE server profiles configured in PMXSpan and shows which one is connected. Other tools answer only for the connected server.",
            json!({}),
            &[],
            read_only(),
        ),
        tool(
            "list_nodes",
            "List nodes",
            "Lists the cluster's nodes with status, CPU, memory, uptime and whether PMXSpan has them in maintenance.",
            json!({ "server": server_prop() }),
            &[],
            read_only(),
        ),
        tool(
            "list_guests",
            "List guests",
            "Lists VMs and containers with VMID, name, kind, node, status, pool, tags and main IP addresses (from PMXSpan's IPAM and, when recently seen, the guest agent). All filters are optional and combine.",
            json!({
                "server": server_prop(),
                "name": { "type": "string", "description": "Substring of the guest name (case-insensitive)." },
                "tag": { "type": "string", "description": "Only guests carrying this tag." },
                "pool": { "type": "string", "description": "Only guests in this resource pool." },
                "status": { "type": "string", "description": "Only guests in this state, e.g. running, stopped, paused." },
                "node": { "type": "string", "description": "Only guests on this node." },
                "include_templates": { "type": "boolean", "description": "Include templates (default false)." },
                "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "description": "Maximum rows (default 200)." }
            }),
            &[],
            read_only(),
        ),
        tool(
            "get_guest",
            "Get guest",
            "Shows one guest: status, CPU/memory/disk, node, pool, tags, HA state, every known IP address (IPAM, static config, guest agent), snapshot names and a short configuration summary. Secrets are never included.",
            json!({ "server": server_prop(), "vmid": vmid_prop() }),
            &["vmid"],
            read_only(),
        ),
        tool(
            "find_by_ip",
            "Find guest by IP",
            "Finds which guest (or node) owns an IP address: checks PMXSpan's IPAM first, then the nodes, static guest network config, and finally the guest agents and container interfaces of running guests. Use it to map an SSH session's host address to a VM or container.",
            json!({
                "server": server_prop(),
                "ip": { "type": "string", "description": "IPv4 or IPv6 address, e.g. 10.0.0.15 (a /prefix is ignored)." }
            }),
            &["ip"],
            read_only(),
        ),
        tool(
            "ipam_lookup",
            "IPAM lookup",
            "Queries PMXSpan's local IP address management: with an address, shows who holds it (or whether it is free and in which pool); without one, lists the pools usable on the server with their used and free counts.",
            json!({
                "server": server_prop(),
                "address": { "type": "string", "description": "IP address to look up." },
                "pool": { "type": "string", "description": "Only this pool (name or id)." }
            }),
            &[],
            read_only(),
        ),
        tool(
            "list_tasks",
            "List tasks",
            "Lists recent Proxmox tasks (newest first) with type, target, user, start time, status and UPID. Pass a UPID to get_task_log for its output.",
            json!({
                "server": server_prop(),
                "vmid": { "type": "integer", "minimum": 1, "description": "Only tasks about this guest." },
                "node": { "type": "string", "description": "Only tasks on this node." },
                "limit": { "type": "integer", "minimum": 1, "maximum": 100, "description": "Maximum rows (default 20)." }
            }),
            &[],
            read_only(),
        ),
        tool(
            "get_task_log",
            "Get task log",
            "Shows a task's status and the last lines of its log.",
            json!({
                "server": server_prop(),
                "upid": { "type": "string", "description": "Task id (UPID:...), from list_tasks or an action's result." },
                "lines": { "type": "integer", "minimum": 1, "maximum": 500, "description": "Lines from the end (default 50)." }
            }),
            &["upid"],
            read_only(),
        ),
        tool(
            "get_node_status",
            "Get node status",
            "Shows one node's CPU, load, memory, swap, root disk, kernel, Proxmox VE version and uptime.",
            json!({ "server": server_prop(), "node": { "type": "string", "description": "Node name." } }),
            &["node"],
            read_only(),
        ),
        tool(
            "list_backups",
            "List backups",
            "Lists backups on the cluster's backup storages (newest first), optionally for one guest or one node.",
            json!({
                "server": server_prop(),
                "vmid": { "type": "integer", "minimum": 1, "description": "Only backups of this guest." },
                "node": { "type": "string", "description": "Only storages seen from this node." },
                "limit": { "type": "integer", "minimum": 1, "maximum": 500, "description": "Maximum rows (default 50)." }
            }),
            &[],
            read_only(),
        ),
        tool(
            "list_snapshots",
            "List snapshots",
            "Lists a guest's snapshots, oldest first, with time, description and whether RAM state is included.",
            json!({ "server": server_prop(), "vmid": vmid_prop() }),
            &["vmid"],
            read_only(),
        ),
        tool(
            "list_ipam_drift",
            "List IPAM drift",
            "Compares PMXSpan's IPAM with the guests' network configuration: addresses kept for deleted guests, static addresses IPAM does not track, main addresses that differ, and PMXSpan-created guests without an IPAM record.",
            json!({ "server": server_prop() }),
            &[],
            read_only(),
        ),
    ]
}

fn action_definitions() -> Vec<Value> {
    let action = |destructive: bool, idempotent: bool| json!({ "readOnlyHint": false, "destructiveHint": destructive, "idempotentHint": idempotent, "openWorldHint": false });
    vec![
        tool(
            "power",
            "Power action",
            "Starts, shuts down, reboots, stops (hard), suspends or resumes a guest. Waits up to 30 seconds for the Proxmox task and returns its UPID and outcome. Shutdown falls back to a hard stop after 3 minutes, like in PMXSpan.",
            json!({
                "server": server_prop(),
                "vmid": vmid_prop(),
                "action": { "type": "string", "enum": ["start", "shutdown", "reboot", "stop", "suspend", "resume"], "description": "What to do." }
            }),
            &["vmid", "action"],
            action(true, false),
        ),
        tool(
            "snapshot_create",
            "Create snapshot",
            "Takes a snapshot of a guest. Waits up to 30 seconds for the task and returns its UPID and outcome.",
            json!({
                "server": server_prop(),
                "vmid": vmid_prop(),
                "name": { "type": "string", "description": "Snapshot name: starts with a letter; letters, digits, '-' and '_'; at most 40 characters." },
                "description": { "type": "string", "description": "Optional description." },
                "vmstate": { "type": "boolean", "description": "Include the VM's RAM (QEMU only, default false)." }
            }),
            &["vmid", "name"],
            action(false, false),
        ),
        tool(
            "agent_exec",
            "Run command via guest agent",
            "Runs a command inside a running QEMU VM through the QEMU guest agent (no shell: pass the program and its arguments) and returns the exit code and output (capped at 16 KiB each). Refused when the agent is not running; containers have no guest agent.",
            json!({
                "server": server_prop(),
                "vmid": vmid_prop(),
                "argv": { "type": "array", "items": { "type": "string" }, "minItems": 1, "maxItems": 64, "description": "Program and arguments, e.g. [\"systemctl\", \"status\", \"nginx\"]." },
                "stdin": { "type": "string", "description": "Optional data for the command's standard input." }
            }),
            &["vmid", "argv"],
            action(true, false),
        ),
        tool(
            "set_tags",
            "Set tags",
            "Replaces a guest's Proxmox tags with the given list (an empty list removes all tags).",
            json!({
                "server": server_prop(),
                "vmid": vmid_prop(),
                "tags": { "type": "array", "items": { "type": "string" }, "maxItems": 32, "description": "Tags: letters, digits, '_', '-', '+' and '.', starting with a letter, digit or '_'." }
            }),
            &["vmid", "tags"],
            action(false, true),
        ),
    ]
}

/// The tools a client sees: the read set, plus the actions when allowed.
pub fn definitions(allow_actions: bool) -> Vec<Value> {
    let mut v = read_definitions();
    if allow_actions {
        v.extend(action_definitions());
    }
    v
}

/// Every definition, read and action, for the golden file.
#[cfg(test)]
pub fn all_definitions() -> Vec<Value> {
    definitions(true)
}

/// Operations that are never offered over MCP (v1). A call naming one is
/// refused with an explanation instead of "unknown tool".
pub fn is_destructive_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    const WORDS: [&str; 16] = [
        "destroy",
        "delete",
        "remove",
        "purge",
        "wipe",
        "erase",
        "rebuild",
        "reinstall",
        "rollback",
        "restore",
        "firewall",
        "acl",
        "permission",
        "user",
        "token",
        "password",
    ];
    WORDS.iter().any(|w| n.contains(w)) || matches!(n.as_str(), "reset" | "migrate" | "set_access" | "access")
}

fn refusal_text(name: &str) -> String {
    format!(
        "Refused: '{name}' is a destructive or security-sensitive operation (destroy, rebuild, rollback, deleting snapshots or backups, firewall and access changes). \
PMXSpan never runs these over MCP, even with actions allowed. Ask the user to do it in PMXSpan itself."
    )
}

fn actions_off_text(name: &str) -> String {
    format!(
        "Refused: '{name}' is an action and actions over MCP are switched off in PMXSpan. \
The user can turn on Settings → MCP server → \"Allow actions over MCP\" in PMXSpan; until then only the read-only tools work."
    )
}

// ---- dispatch ----------------------------------------------------------------

pub struct ToolOutput {
    pub text: String,
    pub is_error: bool,
}

impl ToolOutput {
    fn ok(text: String) -> Self {
        Self { text: cap(text), is_error: false }
    }
    fn err(text: impl Into<String>) -> Self {
        Self { text: text.into(), is_error: true }
    }
}

fn cap(mut s: String) -> String {
    if s.len() > MAX_TEXT {
        let mut cut = MAX_TEXT;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
        s.push_str("\n… (output truncated; narrow the query with filters or a limit)");
    }
    s
}

/// Runs a tool. A protocol error only for an unknown tool; everything else
/// (bad arguments, Proxmox errors, refusals) is a tool result with
/// `isError: true` so the model can read and act on it.
pub async fn call(view: &AppView, session: &str, name: &str, args: &Value) -> Result<ToolOutput, RpcError> {
    if READ_TOOLS.contains(&name) {
        tracing::debug!("MCP tool {name} called");
        return Ok(match run_read(view, name, args).await {
            Ok(t) => ToolOutput::ok(t),
            Err(e) => ToolOutput::err(e),
        });
    }
    let short = session.get(..8).unwrap_or(session);
    if ACTION_TOOLS.contains(&name) {
        if !view.allow_actions() {
            tracing::info!("MCP action {name} refused (actions are off), session {short}, arguments {args}");
            view.audit.record(session, name, args, Outcome::Refused);
            return Ok(ToolOutput::err(actions_off_text(name)));
        }
        tracing::info!("MCP action {name} requested, session {short}, arguments {args}");
        let r = run_action(view, name, args).await;
        let outcome = if r.is_ok() { Outcome::Ok } else { Outcome::Error };
        view.audit.record(session, name, args, outcome);
        return Ok(match r {
            Ok(t) => {
                tracing::info!("MCP action {name} done: {}", t.lines().next().unwrap_or(""));
                ToolOutput::ok(t)
            }
            Err(e) => {
                tracing::warn!("MCP action {name} failed: {}", e.lines().next().unwrap_or(""));
                ToolOutput::err(e)
            }
        });
    }
    if is_destructive_name(name) {
        tracing::warn!("MCP call to destructive operation '{name}' refused, session {short}");
        view.audit.record(session, name, args, Outcome::Refused);
        return Ok(ToolOutput::err(refusal_text(name)));
    }
    Err(RpcError::new(protocol::INVALID_PARAMS, format!("Unknown tool '{name}'. Call tools/list for the tools this server offers.")))
}

type R = Result<String, String>;

async fn run_read(view: &AppView, name: &str, args: &Value) -> R {
    if name == "list_servers" {
        return Ok(list_servers(view));
    }
    let ctx = resolve(view, args)?;
    match name {
        "list_nodes" => list_nodes(&ctx).await,
        "list_guests" => list_guests(view, &ctx, args).await,
        "get_guest" => get_guest(view, &ctx, args).await,
        "find_by_ip" => find_by_ip(view, &ctx, args).await,
        "ipam_lookup" => ipam_lookup(&ctx, args),
        "list_tasks" => list_tasks(&ctx, args).await,
        "get_task_log" => get_task_log(&ctx, args).await,
        "get_node_status" => get_node_status(&ctx, args).await,
        "list_backups" => list_backups(&ctx, args).await,
        "list_snapshots" => list_snapshots(&ctx, args).await,
        "list_ipam_drift" => list_ipam_drift(&ctx).await,
        _ => Err(format!("internal: no handler for {name}")),
    }
}

async fn run_action(view: &AppView, name: &str, args: &Value) -> R {
    let ctx = resolve(view, args)?;
    match name {
        "power" => power(&ctx, args).await,
        "snapshot_create" => snapshot_create(&ctx, args).await,
        "agent_exec" => agent_exec(&ctx, args).await,
        "set_tags" => set_tags(&ctx, args).await,
        _ => Err(format!("internal: no handler for {name}")),
    }
}

// ---- context and arguments ---------------------------------------------------

struct Ctx {
    client: PveClient,
    server_id: String,
    label: String,
    maintenance: Vec<String>,
    ipam: Option<Arc<Mutex<IpamDb>>>,
    managed_tag: String,
}

fn resolve(view: &AppView, args: &Value) -> Result<Ctx, String> {
    let st = view.snapshot();
    let wanted = arg_str(args, "server");
    let Some(Connected { client, profile }) = st.connected else {
        return Err(match wanted {
            Some(w) => format!(
                "PMXSpan is not connected to any server, so '{w}' cannot be queried. Ask the user to connect to it in PMXSpan (Servers view); list_servers shows the profiles."
            ),
            None => "PMXSpan is not connected to any server. Ask the user to connect to one in PMXSpan (Servers view); list_servers shows the profiles.".into(),
        });
    };
    if let Some(w) = wanted {
        let matches = |id: &str, name: &str| id == w || name.eq_ignore_ascii_case(&w);
        if !matches(&profile.id, &profile.display_name()) {
            return Err(match st.profiles.iter().find(|p| matches(&p.id, &p.name)) {
                Some(p) => format!(
                    "Server '{}' is configured in PMXSpan but not connected; only the connected server ('{}') can be queried. Ask the user to connect to '{}' in PMXSpan, or omit 'server'.",
                    p.name,
                    profile.display_name(),
                    p.name
                ),
                None => format!(
                    "No server profile '{w}' in PMXSpan. Use list_servers to see the profiles (by id or name), or omit 'server' for the connected one ('{}').",
                    profile.display_name()
                ),
            });
        }
    }
    Ok(Ctx {
        client,
        server_id: profile.id.clone(),
        label: profile.display_name(),
        maintenance: profile.maintenance_nodes.clone(),
        ipam: st.ipam,
        managed_tag: st.managed_tag,
    })
}

fn arg_str(args: &Value, key: &str) -> Option<String> {
    match args.get(key)? {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn arg_u64(args: &Value, key: &str) -> Result<Option<u64>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) if n.as_u64().is_some_and(|v| v > 0) => Ok(n.as_u64()),
        Some(Value::String(s)) if s.trim().parse::<u64>().is_ok_and(|v| v > 0) => Ok(s.trim().parse().ok()),
        Some(other) => Err(format!("Invalid arguments: '{key}' must be a positive integer, got {other}.")),
    }
}

fn need_vmid(args: &Value) -> Result<u64, String> {
    arg_u64(args, "vmid")?.ok_or_else(|| "Invalid arguments: 'vmid' is required (the guest's VMID, e.g. 100).".to_string())
}

fn arg_bool(args: &Value, key: &str) -> Result<bool, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(other) => Err(format!("Invalid arguments: '{key}' must be true or false, got {other}.")),
    }
}

fn arg_limit(args: &Value, key: &str, default: u64, max: u64) -> Result<usize, String> {
    Ok(arg_u64(args, key)?.unwrap_or(default).min(max) as usize)
}

fn arg_strings(args: &Value, key: &str) -> Result<Option<Vec<String>>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(a)) => a
            .iter()
            .map(|v| v.as_str().map(str::to_string).ok_or_else(|| format!("Invalid arguments: every item of '{key}' must be a string.")))
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        Some(_) => Err(format!("Invalid arguments: '{key}' must be an array of strings.")),
    }
}

fn api_err(what: &str, e: anyhow::Error) -> String {
    crate::util::redact(&format!("{what} failed: {e:#}"))
}

async fn resources(ctx: &Ctx) -> Result<Vec<Resource>, String> {
    let list = ctx.client.get_list("/cluster/resources").await.map_err(|e| api_err("Reading the cluster resources", e))?;
    Ok(list.iter().map(Resource::from_value).collect())
}

fn find_guest(ctx: &Ctx, res: &[Resource], vmid: u64) -> Result<Resource, String> {
    res.iter()
        .find(|r| r.kind().is_some() && r.vmid == vmid)
        .cloned()
        .ok_or_else(|| format!("No guest with VMID {vmid} on {}. Use list_guests to see the VMIDs.", ctx.label))
}

fn guest_ref(r: &Resource) -> GuestRef {
    GuestRef { node: r.node.clone(), kind: r.kind().unwrap_or(GuestKind::Qemu), vmid: r.vmid }
}

fn or_dash(s: &str) -> &str {
    if s.is_empty() { "-" } else { s }
}

fn tags_of(r: &Resource) -> String {
    let t = r.tag_list();
    if t.is_empty() { "-".into() } else { t.join(",") }
}

fn ipam_ips(ctx: &Ctx, vmid: u64) -> Vec<String> {
    let Some(ipam) = &ctx.ipam else { return Vec::new() };
    let db = ipam.lock();
    let mut v: Vec<(bool, String)> = db.guest_ips(&ctx.server_id, vmid).iter().map(|a| (!a.main, a.ip.clone())).collect();
    v.sort();
    v.into_iter().map(|(_, ip)| ip).collect()
}

fn same_ip(a: &str, b: &IpAddr) -> bool {
    a.split('/').next().and_then(|s| s.trim().parse::<IpAddr>().ok()).is_some_and(|x| x == *b)
}

// ---- read tools --------------------------------------------------------------

fn list_servers(view: &AppView) -> String {
    let st = view.snapshot();
    let connected = st.connected.as_ref().map(|c| c.profile.id.clone());
    if st.profiles.is_empty() {
        return "No server profiles are configured in PMXSpan.".into();
    }
    let mut out = format!("{} server profile(s); id | name | address | state\n", st.profiles.len());
    for p in &st.profiles {
        let state = if connected.as_deref() == Some(p.id.as_str()) { "connected" } else { "not connected" };
        let _ = writeln!(out, "{} | {} | {}:{} | {state}", p.id, p.name, p.host, p.port);
    }
    if connected.is_none() {
        out.push_str("PMXSpan is not connected to any server; the other tools need a connection made in PMXSpan.\n");
    }
    out
}

async fn list_nodes(ctx: &Ctx) -> R {
    let nodes = ctx.client.get_list("/nodes").await.map_err(|e| api_err("Listing nodes", e))?;
    let mut rows: Vec<&Value> = nodes.iter().collect();
    rows.sort_by_key(|n| n.s("node"));
    let mut out = format!("{}: {} node(s); node | status | cpu | memory | uptime | maintenance\n", ctx.label, rows.len());
    for n in rows {
        let name = n.s("node");
        let _ = writeln!(
            out,
            "{name} | {} | {:.0}% of {} cores | {} / {} | {} | {}",
            n.s("status"),
            n.f("cpu") * 100.0,
            n.u("maxcpu"),
            fmt_bytes(n.f("mem")),
            fmt_bytes(n.f("maxmem")),
            if n.u("uptime") > 0 { fmt_duration(n.u("uptime")) } else { "-".into() },
            if ctx.maintenance.contains(&name) { "yes (auto-placement skips it)" } else { "no" }
        );
    }
    Ok(out)
}

async fn list_guests(view: &AppView, ctx: &Ctx, args: &Value) -> R {
    let name = arg_str(args, "name").map(|s| s.to_lowercase());
    let tag = arg_str(args, "tag");
    let pool = arg_str(args, "pool");
    let status = arg_str(args, "status");
    let node = arg_str(args, "node");
    let templates = arg_bool(args, "include_templates")?;
    let limit = arg_limit(args, "limit", 200, 1000)?;
    let res = resources(ctx).await?;
    let mut guests: Vec<&Resource> = res
        .iter()
        .filter(|r| r.kind().is_some())
        .filter(|r| templates || !r.template)
        .filter(|r| name.as_ref().is_none_or(|n| r.name.to_lowercase().contains(n)))
        .filter(|r| tag.as_ref().is_none_or(|t| r.tag_list().iter().any(|x| x.eq_ignore_ascii_case(t))))
        .filter(|r| pool.as_ref().is_none_or(|p| r.pool.eq_ignore_ascii_case(p)))
        .filter(|r| status.as_ref().is_none_or(|s| r.status.eq_ignore_ascii_case(s)))
        .filter(|r| node.as_ref().is_none_or(|n| r.node.eq_ignore_ascii_case(n)))
        .collect();
    guests.sort_by_key(|r| r.vmid);
    let total = guests.len();
    let mut out = format!(
        "{}: {total} guest(s){}; vmid | kind | name | node | status | pool | tags | ips\n",
        ctx.label,
        if total > limit { format!(", first {limit} shown") } else { String::new() }
    );
    for r in guests.into_iter().take(limit) {
        let mut ips = ipam_ips(ctx, r.vmid);
        for ip in view.cached_agent_ips(&ctx.server_id, r.vmid).unwrap_or_default() {
            if !ips.contains(&ip) {
                ips.push(ip);
            }
        }
        let _ = writeln!(
            out,
            "{} | {}{} | {} | {} | {} | {} | {} | {}",
            r.vmid,
            r.kind().map(GuestKind::label).unwrap_or("?"),
            if r.template { " template" } else { "" },
            or_dash(&r.name),
            r.node,
            r.status,
            or_dash(&r.pool),
            tags_of(r),
            if ips.is_empty() { "-".into() } else { ips.join(",") }
        );
    }
    Ok(out)
}

/// Live addresses from the guest agent (QEMU) or the container's
/// interfaces (LXC), without loopback and link-local; bounded in time.
async fn live_ips(client: &PveClient, g: &GuestRef) -> Option<Vec<String>> {
    let fut = async {
        let mut out = Vec::new();
        match g.kind {
            GuestKind::Qemu => {
                let r = crate::ops::guest::agent_get(client, g, "network-get-interfaces").await.ok()?;
                for iface in r.arr() {
                    for a in iface.get("ip-addresses").map(|a| a.arr()).unwrap_or_default() {
                        out.push(a.s("ip-address"));
                    }
                }
            }
            GuestKind::Lxc => {
                for iface in client.get_list(&format!("{}/interfaces", g.path())).await.ok()? {
                    for k in ["inet", "inet6"] {
                        out.push(iface.s(k).split('/').next().unwrap_or("").to_string());
                    }
                }
            }
        }
        out.retain(|ip| {
            ip.parse::<IpAddr>().is_ok_and(|a| {
                !a.is_loopback() && !a.is_unspecified() && !matches!(a, IpAddr::V6(v6) if (v6.segments()[0] & 0xffc0) == 0xfe80)
            })
        });
        out.dedup();
        Some(out)
    };
    tokio::time::timeout(PROBE_TIMEOUT, fut).await.ok().flatten()
}

/// Config keys worth showing; everything else (passwords, keys, cloud-init
/// user data, descriptions) is left out.
fn config_summary(kind: GuestKind, cfg: &Map<String, Value>) -> Vec<String> {
    let s = |k: &str| cfg.get(k).map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()));
    let mut out = Vec::new();
    for k in [
        "ostype",
        "cores",
        "sockets",
        "cpu",
        "memory",
        "balloon",
        "swap",
        "machine",
        "bios",
        "onboot",
        "agent",
        "boot",
        "unprivileged",
        "features",
    ] {
        if let Some(v) = s(k) {
            out.push(format!("{k}={v}"));
        }
    }
    for k in crate::ops::guest::disk_keys(kind, cfg) {
        if let Some(v) = s(&k) {
            let p = crate::util::PropString::parse(&v);
            let vol = p.positional().unwrap_or("?").to_string();
            let size = p.get("size").map(|x| format!(" size={x}")).unwrap_or_default();
            out.push(format!("{k}={vol}{size}"));
        }
    }
    let mut nets: Vec<&String> = cfg.keys().filter(|k| crate::api::types::is_net_key(k) || k.starts_with("ipconfig")).collect();
    nets.sort();
    for k in nets {
        if let Some(v) = s(k) {
            let p = crate::util::PropString::parse(&v);
            let keep: Vec<String> =
                ["name", "bridge", "tag", "ip", "gw", "ip6", "gw6", "firewall", "hwaddr", "virtio", "e1000", "rtl8139", "vmxnet3"]
                    .iter()
                    .filter_map(|f| p.get(f).map(|x| format!("{f}={x}")))
                    .collect();
            out.push(format!("{k}: {}", keep.join(",")));
        }
    }
    out
}

async fn get_guest(view: &AppView, ctx: &Ctx, args: &Value) -> R {
    let vmid = need_vmid(args)?;
    let res = resources(ctx).await?;
    let r = find_guest(ctx, &res, vmid)?;
    let g = guest_ref(&r);
    let c = &ctx.client;
    let (status, cfg, snaps) =
        tokio::join!(crate::ops::guest::status(c, &g), crate::ops::guest::config(c, &g), crate::ops::guest::snapshots(c, &g));
    let st = status.unwrap_or(Value::Null);
    let cfg = cfg.unwrap_or_default();
    let mut out = format!("{} {} '{}' on {} (node {})\n", g.kind.label(), vmid, or_dash(&r.name), ctx.label, r.node);
    let _ = writeln!(out, "status: {}{}", r.status, if r.template { " (template)" } else { "" });
    if !r.lock.is_empty() {
        let _ = writeln!(out, "lock: {}", r.lock);
    }
    if r.status == "running" {
        let _ = writeln!(out, "uptime: {}", fmt_duration(r.uptime));
    }
    let _ = writeln!(
        out,
        "cpu: {:.0}% of {} | memory: {} / {} | disk: {}",
        r.cpu * 100.0,
        r.maxcpu,
        fmt_bytes(r.mem),
        fmt_bytes(r.maxmem),
        fmt_bytes(r.maxdisk)
    );
    let _ = writeln!(out, "pool: {} | tags: {} | HA: {}", or_dash(&r.pool), tags_of(&r), or_dash(&r.hastate));

    let mut ip_lines = Vec::new();
    if let Some(ipam) = &ctx.ipam {
        for a in ipam.lock().guest_ips(&ctx.server_id, vmid) {
            ip_lines.push(format!("{} (IPAM, net{}{})", a.ip, a.iface(), if a.main { ", main" } else { "" }));
        }
    }
    for (n, ip) in crate::ops::ipassign::static_ips(g.kind, &cfg) {
        if !ip_lines.iter().any(|l| l.starts_with(&format!("{ip} "))) {
            ip_lines.push(format!("{ip} (static config, net{n})"));
        }
    }
    let agent_running = g.kind == GuestKind::Lxc || st.b("agent");
    let live = if r.status == "running" && agent_running { live_ips(c, &g).await } else { None };
    if let Some(ips) = live {
        for ip in &ips {
            if !ip_lines.iter().any(|l| l.starts_with(&format!("{ip} "))) {
                ip_lines.push(format!("{ip} ({})", if g.kind == GuestKind::Qemu { "guest agent" } else { "container interface" }));
            }
        }
        view.cache_agent_ips(&ctx.server_id, vmid, ips);
    }
    let _ = writeln!(out, "ips: {}", if ip_lines.is_empty() { "none known".into() } else { ip_lines.join("; ") });
    if g.kind == GuestKind::Qemu {
        let _ = writeln!(
            out,
            "guest agent: {}",
            if !st.b("agent") {
                "not enabled in config"
            } else if r.status == "running" {
                "enabled"
            } else {
                "enabled (VM not running)"
            }
        );
    }
    match snaps {
        Ok(s) if !s.is_empty() => {
            let _ = writeln!(out, "snapshots: {}", s.iter().map(|x| x.name.as_str()).collect::<Vec<_>>().join(", "));
        }
        Ok(_) => out.push_str("snapshots: none\n"),
        Err(e) => {
            let _ = writeln!(out, "snapshots: unavailable ({})", crate::util::redact(&format!("{e:#}")));
        }
    }
    let summary = config_summary(g.kind, &cfg);
    if !summary.is_empty() {
        let _ = writeln!(out, "config: {}", summary.join(" | "));
    }
    Ok(out)
}

fn parse_ip(raw: &str) -> Result<IpAddr, String> {
    let s = raw.trim().split('/').next().unwrap_or("").trim().trim_start_matches('[').trim_end_matches(']');
    s.parse::<IpAddr>().map_err(|_| format!("Invalid arguments: '{raw}' is not an IPv4 or IPv6 address."))
}

fn guest_line(r: &Resource) -> String {
    format!(
        "{} {} '{}' | node {} | {} | pool {} | tags {}",
        r.kind().map(GuestKind::label).unwrap_or("?"),
        r.vmid,
        or_dash(&r.name),
        r.node,
        r.status,
        or_dash(&r.pool),
        tags_of(r)
    )
}

async fn find_by_ip(view: &AppView, ctx: &Ctx, args: &Value) -> R {
    let raw = arg_str(args, "ip").ok_or("Invalid arguments: 'ip' is required, e.g. \"10.0.0.15\".")?;
    let ip = parse_ip(&raw)?;
    let res = resources(ctx).await?;
    let by_vmid = |vmid: u64| res.iter().find(|r| r.kind().is_some() && r.vmid == vmid);
    let mut hits: Vec<(u64, String)> = Vec::new();
    let mut notes: Vec<String> = Vec::new();

    // 1. IPAM.
    if let Some(ipam) = &ctx.ipam {
        let db = ipam.lock();
        for a in db.assignments.iter().filter(|a| same_ip(&a.ip, &ip)) {
            let pool = db.pool(&a.pool_id).map(|p| p.name.clone()).unwrap_or_else(|| "?".into());
            match a.vmid {
                Some(v) if a.server_id == ctx.server_id => {
                    hits.push((v, format!("IPAM (pool {pool}, net{}{})", a.iface(), if a.main { ", main" } else { "" })))
                }
                Some(v) => notes.push(format!("IPAM assigns it to VMID {v} on another server profile.")),
                None if a.reserved => notes.push(format!(
                    "Reserved in IPAM pool {pool}{}.",
                    if a.note.is_empty() { String::new() } else { format!(" ({})", a.note) }
                )),
                None => {}
            }
        }
    }
    // 2. Nodes.
    let mut node_hit = None;
    if hits.is_empty()
        && let Ok(status) = ctx.client.get_list("/cluster/status").await
    {
        node_hit = status.iter().find(|n| n.s("type") == "node" && same_ip(&n.s("ip"), &ip)).map(|n| n.s("name"));
    }
    // 3. Agent addresses seen recently.
    if hits.is_empty() && node_hit.is_none() {
        for (vmid, a) in view.cached_agent_all(&ctx.server_id) {
            if same_ip(&a, &ip) {
                hits.push((vmid, "guest agent / container interface (recently seen)".into()));
            }
        }
    }
    let guests: Vec<Resource> = res.iter().filter(|r| r.kind().is_some() && !r.template).take(500).cloned().collect();
    // 4. Static network config.
    if hits.is_empty() && node_hit.is_none() {
        let futs: Vec<_> = guests
            .iter()
            .map(|r| {
                let c = ctx.client.clone();
                let g = guest_ref(r);
                async move {
                    let cfg = crate::ops::guest::config(&c, &g).await.ok()?;
                    crate::ops::ipassign::static_ips(g.kind, &cfg).into_iter().find(|(_, a)| same_ip(a, &ip)).map(|(n, _)| (g.vmid, n))
                }
            })
            .collect();
        hits.extend(run_bounded(futs).await.into_iter().map(|(v, n)| (v, format!("static network config (net{n})"))));
    }
    // 5. Live guest agents / container interfaces.
    if hits.is_empty() && node_hit.is_none() {
        let futs: Vec<_> = guests
            .iter()
            .filter(|r| r.status == "running")
            .take(200)
            .map(|r| {
                let c = ctx.client.clone();
                let g = guest_ref(r);
                async move { live_ips(&c, &g).await.map(|ips| (g.vmid, g.kind, ips)) }
            })
            .collect();
        for (vmid, kind, ips) in run_bounded(futs).await {
            if ips.iter().any(|a| same_ip(a, &ip)) {
                hits.push((vmid, if kind == GuestKind::Qemu { "guest agent".into() } else { "container interface".into() }));
            }
            view.cache_agent_ips(&ctx.server_id, vmid, ips);
        }
    }

    let mut out = String::new();
    if let Some(n) = node_hit {
        let _ = writeln!(out, "{ip} is the address of node {n} on {} (a Proxmox VE host, not a guest).", ctx.label);
    }
    hits.sort();
    hits.dedup();
    for (vmid, source) in &hits {
        match by_vmid(*vmid) {
            Some(r) => {
                let _ = writeln!(out, "{ip} → {} | via {source}", guest_line(r));
            }
            None => {
                let _ = writeln!(out, "{ip} → VMID {vmid} (no such guest on the cluster any more) | via {source}");
            }
        }
    }
    for n in &notes {
        let _ = writeln!(out, "note: {n}");
    }
    if out.is_empty() {
        let _ = writeln!(
            out,
            "No guest or node on {} uses {ip} (checked IPAM, nodes, static guest config, and the guest agents / interfaces of running guests). \
DHCP guests without a running guest agent cannot be matched.",
            ctx.label
        );
    }
    Ok(out)
}

/// Runs probes at most [`PROBE_PARALLEL`] at a time; failed ones (`None`) are dropped.
async fn run_bounded<T, F>(futs: Vec<F>) -> Vec<T>
where
    T: Send + 'static,
    F: std::future::Future<Output = Option<T>> + Send + 'static,
{
    let slots = Arc::new(tokio::sync::Semaphore::new(PROBE_PARALLEL));
    let mut set = tokio::task::JoinSet::new();
    for f in futs {
        let slots = slots.clone();
        set.spawn(async move {
            let _slot = slots.acquire_owned().await.ok()?;
            f.await
        });
    }
    let mut out = Vec::new();
    while let Some(r) = set.join_next().await {
        if let Ok(Some(v)) = r {
            out.push(v);
        }
    }
    out
}

fn ipam_lookup(ctx: &Ctx, args: &Value) -> R {
    let Some(ipam) = &ctx.ipam else { return Err("PMXSpan's IPAM database is not available.".into()) };
    let db = ipam.lock();
    let pool_filter = arg_str(args, "pool");
    let pools: Vec<&crate::ipam::Pool> = db
        .pools
        .iter()
        .filter(|p| p.server_id.is_empty() || p.server_id == ctx.server_id)
        .filter(|p| pool_filter.as_ref().is_none_or(|f| p.id == *f || p.name.eq_ignore_ascii_case(f)))
        .collect();
    if let Some(raw) = arg_str(args, "address") {
        let ip = parse_ip(&raw)?;
        let mut out = String::new();
        for a in db.assignments.iter().filter(|a| same_ip(&a.ip, &ip)) {
            let pool = db.pool(&a.pool_id).map(|p| format!("{} ({})", p.name, p.cidr)).unwrap_or_else(|| "?".into());
            let since = if a.assigned_at > 0 { format!(", since {}", fmt_epoch(a.assigned_at)) } else { String::new() };
            match a.vmid {
                Some(v) => {
                    let other = if a.server_id != ctx.server_id { " on another server profile" } else { "" };
                    let _ = writeln!(
                        out,
                        "{ip}: assigned to VMID {v}{other} (hostname {}), net{}{}, pool {pool}{since}",
                        or_dash(&a.hostname),
                        a.iface(),
                        if a.main { ", main address" } else { ", additional address" }
                    );
                }
                None => {
                    let _ = writeln!(
                        out,
                        "{ip}: {} in pool {pool}{}{since}",
                        if a.reserved { "reserved" } else { "held" },
                        if a.note.is_empty() { String::new() } else { format!(" ({})", a.note) }
                    );
                }
            }
        }
        if out.is_empty() {
            match pools.iter().find(|p| p.net().is_ok_and(|n| n.contains(&ip))) {
                Some(p) if p.gateway.trim().parse::<IpAddr>().ok() == Some(ip) => {
                    let _ = writeln!(out, "{ip}: the gateway of pool {} ({}); never handed out.", p.name, p.cidr);
                }
                Some(p) => {
                    let _ = writeln!(out, "{ip}: free in pool {} ({}, gateway {}).", p.name, p.cidr, or_dash(&p.gateway));
                }
                None => {
                    let _ = writeln!(out, "{ip}: not tracked by IPAM and inside no pool usable on {}.", ctx.label);
                }
            }
        }
        return Ok(out);
    }
    if pools.is_empty() {
        return Ok(match pool_filter {
            Some(f) => format!("No IPAM pool '{f}' usable on {}.", ctx.label),
            None => format!("No IPAM pools are usable on {} (PMXSpan → IP address management).", ctx.label),
        });
    }
    let mut out =
        format!("{} IPAM pool(s) usable on {}; name | cidr | gateway | bridge | vlan | used | free | id\n", pools.len(), ctx.label);
    for p in pools {
        let used = db.used_in_pool(&p.id) as u64;
        let _ = writeln!(
            out,
            "{} | {} | {} | {} | {} | {used} | {} | {}",
            p.name,
            p.cidr,
            or_dash(&p.gateway),
            if p.bridge.is_empty() { "any" } else { &p.bridge },
            p.vlan.map(|v| v.to_string()).unwrap_or_else(|| "-".into()),
            p.capacity().saturating_sub(used),
            p.id
        );
    }
    Ok(out)
}

fn task_state(t: &TaskInfo) -> String {
    if t.running() {
        "running".into()
    } else if t.ok() {
        t.status.clone()
    } else {
        format!("FAILED: {}", t.status)
    }
}

async fn list_tasks(ctx: &Ctx, args: &Value) -> R {
    let vmid = arg_u64(args, "vmid")?;
    let node = arg_str(args, "node");
    let limit = arg_limit(args, "limit", 20, 100)?;
    let list = ctx.client.get_list("/cluster/tasks").await.map_err(|e| api_err("Listing tasks", e))?;
    let mut tasks: Vec<TaskInfo> = list
        .iter()
        .map(TaskInfo::from_value)
        .filter(|t| vmid.is_none_or(|v| t.id == v.to_string()))
        .filter(|t| node.as_ref().is_none_or(|n| t.node.eq_ignore_ascii_case(n)))
        .collect();
    tasks.sort_by_key(|t| std::cmp::Reverse(t.starttime));
    if tasks.is_empty() {
        return Ok(format!("No recent tasks on {} match.", ctx.label));
    }
    let mut out =
        format!("{}: {} task(s), newest first; started | node | task | user | status | upid\n", ctx.label, tasks.len().min(limit));
    for t in tasks.iter().take(limit) {
        let _ = writeln!(out, "{} | {} | {} | {} | {} | {}", fmt_epoch(t.starttime), t.node, t.describe(), t.user, task_state(t), t.upid);
    }
    Ok(out)
}

async fn get_task_log(ctx: &Ctx, args: &Value) -> R {
    let upid = arg_str(args, "upid").ok_or("Invalid arguments: 'upid' is required (UPID:node:...), from list_tasks.")?;
    let lines = arg_limit(args, "lines", 50, 500)? as u64;
    let parsed = Upid::parse(&upid).ok_or_else(|| format!("Invalid arguments: '{upid}' is not a Proxmox task id (UPID:node:...)."))?;
    let path = format!("/nodes/{}/tasks/{}/status", parsed.node, urlencoding::encode(&upid));
    let st = ctx.client.get(&path).await.map_err(|e| api_err("Reading the task status", e))?;
    let log = ctx.client.task_log_tail(&parsed.node, &upid, lines).await.map_err(|e| api_err("Reading the task log", e))?;
    let state = if st.s("status") == "stopped" { format!("stopped, exit status {}", st.s("exitstatus")) } else { "running".into() };
    let mut out = format!(
        "task {} ({} {}) on node {} by {}, started {}: {state}\n--- last {lines} log line(s) ---\n",
        upid,
        describe_task_type(&parsed.task_type),
        parsed.id,
        parsed.node,
        parsed.user,
        fmt_epoch(parsed.starttime)
    );
    out.push_str(&crate::util::redact(&log));
    out.push('\n');
    Ok(out)
}

async fn get_node_status(ctx: &Ctx, args: &Value) -> R {
    let node = arg_str(args, "node").ok_or("Invalid arguments: 'node' is required; list_nodes shows the names.")?;
    if !node.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.' || c == '_') {
        return Err(format!("Invalid arguments: '{node}' is not a node name."));
    }
    let s =
        ctx.client.get(&format!("/nodes/{node}/status")).await.map_err(|e| api_err(&format!("Reading the status of node {node}"), e))?;
    let pair = |k: &str| {
        let v = s.get(k).cloned().unwrap_or(Value::Null);
        format!("{} / {}", fmt_bytes(v.f("used")), fmt_bytes(v.f("total")))
    };
    let load: Vec<String> = s
        .get("loadavg")
        .map(|l| l.arr())
        .unwrap_or_default()
        .iter()
        .map(|x| x.as_str().map(str::to_string).unwrap_or_else(|| x.to_string()))
        .collect();
    let cpuinfo = s.get("cpuinfo").cloned().unwrap_or(Value::Null);
    let mut out = format!("node {node} on {}\n", ctx.label);
    let _ = writeln!(
        out,
        "cpu: {:.0}% of {} ({}, {} socket(s))",
        s.f("cpu") * 100.0,
        cpuinfo.u("cpus"),
        cpuinfo.s("model"),
        cpuinfo.u("sockets")
    );
    let _ = writeln!(out, "load average: {}", if load.is_empty() { "-".into() } else { load.join(" ") });
    let _ = writeln!(out, "memory: {} | swap: {} | root disk: {}", pair("memory"), pair("swap"), pair("rootfs"));
    let _ = writeln!(out, "kernel: {} | version: {}", or_dash(&s.s("kversion")), or_dash(&s.s("pveversion")));
    let _ = writeln!(out, "uptime: {}", fmt_duration(s.u("uptime")));
    if ctx.maintenance.contains(&node) {
        out.push_str("PMXSpan has this node in maintenance (auto-placement skips it).\n");
    }
    Ok(out)
}

async fn list_backups(ctx: &Ctx, args: &Value) -> R {
    let vmid = arg_u64(args, "vmid")?;
    let node = arg_str(args, "node");
    let limit = arg_limit(args, "limit", 50, 500)?;
    let res = resources(ctx).await?;
    let nodes: Vec<String> = if let Some(v) = vmid {
        vec![find_guest(ctx, &res, v)?.node]
    } else if let Some(n) = node {
        vec![n]
    } else {
        let mut v: Vec<String> = res.iter().filter(|r| r.rtype == "node" && r.status == "online").map(|r| r.node.clone()).collect();
        v.sort();
        v
    };
    let mut seen = std::collections::HashSet::new();
    let mut items: Vec<(String, Value)> = Vec::new();
    let mut errors = Vec::new();
    for n in &nodes {
        let storages = match ctx.client.get_q(&format!("/nodes/{n}/storage"), &Params::new().set("content", "backup")).await {
            Ok(s) => s,
            Err(e) => {
                errors.push(api_err(&format!("Listing storages of {n}"), e));
                continue;
            }
        };
        for s in storages.arr() {
            if s.get("active").is_some() && !s.b("active") {
                continue;
            }
            let name = s.s("storage");
            let p = Params::new().set("content", "backup").opt("vmid", vmid);
            match ctx.client.get_q(&format!("/nodes/{n}/storage/{name}/content"), &p).await {
                Ok(list) => {
                    for i in list.arr() {
                        if seen.insert(i.s("volid")) {
                            items.push((name.clone(), i));
                        }
                    }
                }
                Err(e) => errors.push(api_err(&format!("Listing backups on {name} ({n})"), e)),
            }
        }
    }
    items.sort_by_key(|(_, i)| std::cmp::Reverse(i.u("ctime")));
    let mut out = format!(
        "{}: {} backup(s){}; created | vmid | size | protected | volid | notes\n",
        ctx.label,
        items.len(),
        if items.len() > limit { format!(", newest {limit} shown") } else { String::new() }
    );
    for (_, i) in items.iter().take(limit) {
        let notes: String = i.s("notes").lines().next().unwrap_or("").chars().take(80).collect();
        let _ = writeln!(
            out,
            "{} | {} | {} | {} | {} | {}",
            fmt_epoch(i.u("ctime") as i64),
            or_dash(&i.s("vmid")),
            fmt_bytes(i.f("size")),
            if i.b("protected") { "yes" } else { "no" },
            i.s("volid"),
            or_dash(&notes)
        );
    }
    for e in errors {
        let _ = writeln!(out, "warning: {e}");
    }
    Ok(out)
}

async fn list_snapshots(ctx: &Ctx, args: &Value) -> R {
    let vmid = need_vmid(args)?;
    let res = resources(ctx).await?;
    let r = find_guest(ctx, &res, vmid)?;
    let snaps = crate::ops::guest::snapshots(&ctx.client, &guest_ref(&r)).await.map_err(|e| api_err("Listing snapshots", e))?;
    if snaps.is_empty() {
        return Ok(format!("{} {vmid} '{}' has no snapshots.", r.kind().map(GuestKind::label).unwrap_or("?"), or_dash(&r.name)));
    }
    let mut out = format!("{} snapshot(s) of {vmid} '{}', oldest first; name | taken | ram | description\n", snaps.len(), or_dash(&r.name));
    for s in snaps {
        let desc: String = s.description.lines().next().unwrap_or("").chars().take(120).collect();
        let _ = writeln!(
            out,
            "{} | {} | {} | {}",
            s.name,
            if s.snaptime > 0 { fmt_epoch(s.snaptime) } else { "-".into() },
            if s.vmstate { "yes" } else { "no" },
            or_dash(&desc)
        );
    }
    Ok(out)
}

async fn list_ipam_drift(ctx: &Ctx) -> R {
    let Some(ipam) = &ctx.ipam else { return Err("PMXSpan's IPAM database is not available.".into()) };
    let res = resources(ctx).await?;
    let guests: Vec<Resource> = res.iter().filter(|r| r.kind().is_some()).cloned().collect();
    let all: Vec<u64> = guests.iter().map(|r| r.vmid).collect();
    let scanned = crate::ops::drift::scan(&ctx.client, guests).await;
    let items = {
        let db = ipam.lock();
        crate::ops::drift::compare(&ctx.server_id, &db, &scanned, &all, &ctx.managed_tag)
    };
    if items.is_empty() {
        return Ok(format!("No IPAM drift on {}: IPAM and the guests' static network config agree.", ctx.label));
    }
    let mut out = format!("{}: {} drift item(s); kind | vmid | name | interface | address | detail\n", ctx.label, items.len());
    for d in items {
        let detail = match &d.kind {
            crate::ops::drift::DriftKind::Mismatch { guest_has } => format!("guest has {guest_has}"),
            _ => "-".into(),
        };
        let _ = writeln!(
            out,
            "{} | {} | {} | {} | {} | {detail}",
            crate::ops::drift::kind_label(&d.kind),
            d.vmid,
            or_dash(&d.name),
            d.iface.map(|n| format!("net{n}")).unwrap_or_else(|| "-".into()),
            or_dash(&d.ip)
        );
    }
    Ok(out)
}

// ---- actions -------------------------------------------------------------------

enum Waited {
    Ok(String),
    Failed(String, String),
    Running,
}

/// Polls a task for up to [`ACTION_WAIT`].
async fn wait_brief(client: &PveClient, upid: &str) -> Waited {
    let Some(node) = Upid::parse(upid).map(|u| u.node) else { return Waited::Running };
    let path = format!("/nodes/{node}/tasks/{}/status", urlencoding::encode(upid));
    let started = std::time::Instant::now();
    while started.elapsed() < ACTION_WAIT {
        if let Ok(st) = client.get(&path).await
            && st.s("status") == "stopped"
        {
            let exit = st.s("exitstatus");
            if crate::api::types::task_exit_ok(&exit) {
                return Waited::Ok(exit);
            }
            let tail = client.task_log_tail(&node, upid, 15).await.unwrap_or_default();
            return Waited::Failed(exit, crate::util::redact(&tail));
        }
        tokio::time::sleep(Duration::from_millis(700)).await;
    }
    Waited::Running
}

async fn task_outcome(client: &PveClient, what: &str, upid: Value) -> R {
    let Some(upid) = upid.as_str().map(str::to_string) else { return Ok(format!("{what}: done.")) };
    match wait_brief(client, &upid).await {
        Waited::Ok(exit) => Ok(format!("{what}: done ({exit}).\nUPID: {upid}")),
        Waited::Running => Ok(format!(
            "{what}: started and still running after {} s.\nUPID: {upid}\nCheck later with get_task_log.",
            ACTION_WAIT.as_secs()
        )),
        Waited::Failed(exit, tail) => Err(format!("{what}: the task failed ({exit}).\nUPID: {upid}\n--- task log ---\n{tail}")),
    }
}

async fn power(ctx: &Ctx, args: &Value) -> R {
    let vmid = need_vmid(args)?;
    let action = arg_str(args, "action").unwrap_or_default().to_ascii_lowercase();
    if !["start", "shutdown", "reboot", "stop", "suspend", "resume"].contains(&action.as_str()) {
        return Err(if is_destructive_name(&action) || action == "reset" {
            format!("Refused: power action '{action}' is not available over MCP. Use start, shutdown, reboot, stop, suspend or resume.")
        } else {
            "Invalid arguments: 'action' must be one of start, shutdown, reboot, stop, suspend, resume.".into()
        });
    }
    let res = resources(ctx).await?;
    let r = find_guest(ctx, &res, vmid)?;
    if r.template {
        return Err(format!("{vmid} is a template; templates cannot be powered on or off."));
    }
    let g = guest_ref(&r);
    let mut p = Params::new();
    if action == "shutdown" {
        p.push("forceStop", 1);
        p.push("timeout", 180);
    }
    let upid =
        ctx.client.post(&format!("{}/status/{action}", g.path()), &p).await.map_err(|e| api_err(&format!("{action} of {vmid}"), e))?;
    task_outcome(&ctx.client, &format!("{action} {} {vmid} '{}' on {}", g.kind.label(), or_dash(&r.name), ctx.label), upid).await
}

async fn snapshot_create(ctx: &Ctx, args: &Value) -> R {
    let vmid = need_vmid(args)?;
    let name = arg_str(args, "name").ok_or("Invalid arguments: 'name' is required.")?;
    crate::ops::guest::validate_snapshot_name(&name).map_err(|e| format!("Invalid arguments: {e}."))?;
    let description = args.get("description").and_then(Value::as_str).unwrap_or("").to_string();
    let vmstate = arg_bool(args, "vmstate")?;
    let res = resources(ctx).await?;
    let r = find_guest(ctx, &res, vmid)?;
    let g = guest_ref(&r);
    if vmstate && g.kind == GuestKind::Lxc {
        return Err("Invalid arguments: 'vmstate' (RAM) snapshots exist only for QEMU VMs; omit it for containers.".into());
    }
    let mut p = Params::new().set("snapname", &name).opt("description", Some(description));
    if vmstate {
        p.push("vmstate", 1);
    }
    let upid = ctx.client.post(&format!("{}/snapshot", g.path()), &p).await.map_err(|e| api_err("Creating the snapshot", e))?;
    task_outcome(&ctx.client, &format!("snapshot '{name}' of {} {vmid} '{}'", g.kind.label(), or_dash(&r.name)), upid).await
}

fn clip(s: &str) -> (String, bool) {
    if s.len() <= MAX_EXEC_OUTPUT {
        return (s.to_string(), false);
    }
    let mut cut = MAX_EXEC_OUTPUT;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    (s[..cut].to_string(), true)
}

async fn agent_exec(ctx: &Ctx, args: &Value) -> R {
    let vmid = need_vmid(args)?;
    let argv =
        arg_strings(args, "argv")?.ok_or("Invalid arguments: 'argv' is required: the program and its arguments as an array of strings.")?;
    if argv.is_empty() || argv[0].trim().is_empty() {
        return Err("Invalid arguments: 'argv' needs at least the program to run.".into());
    }
    if argv.len() > 64 || argv.iter().any(|a| a.len() > 4096) {
        return Err("Invalid arguments: 'argv' allows at most 64 items of 4096 characters each.".into());
    }
    let stdin = args.get("stdin").and_then(Value::as_str);
    if stdin.is_some_and(|s| s.len() > 64 * 1024) {
        return Err("Invalid arguments: 'stdin' is limited to 64 KiB.".into());
    }
    let res = resources(ctx).await?;
    let r = find_guest(ctx, &res, vmid)?;
    let g = guest_ref(&r);
    if g.kind != GuestKind::Qemu {
        return Err(format!("{vmid} is a container; agent_exec needs a QEMU VM with the QEMU guest agent (containers have none)."));
    }
    if r.status != "running" {
        return Err(format!("VM {vmid} is {}, so its guest agent is not running. Start the VM first.", r.status));
    }
    match tokio::time::timeout(Duration::from_secs(5), crate::ops::guest::agent_ping(&ctx.client, &g)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            return Err(crate::util::redact(&format!(
                "The QEMU guest agent of VM {vmid} is not responding ({e:#}). Install/start qemu-guest-agent inside the VM and enable the 'agent' option."
            )));
        }
        Err(_) => return Err(format!("The QEMU guest agent of VM {vmid} did not answer within 5 s; it is probably not running.")),
    }
    let st = tokio::time::timeout(Duration::from_secs(70), crate::ops::guest::agent_exec(&ctx.client, &g, &argv, stdin))
        .await
        .map_err(|_| "The command did not finish within 70 s.".to_string())?
        .map_err(|e| api_err("Running the command", e))?;
    let (out_data, out_cut) = clip(&st.s("out-data"));
    let (err_data, err_cut) = clip(&st.s("err-data"));
    let mut out = format!("VM {vmid} '{}': {} exited with code {}", or_dash(&r.name), argv[0], st.s("exitcode"));
    if st.b("signal") {
        let _ = write!(out, " (signal {})", st.s("signal"));
    }
    out.push('\n');
    let trunc = |cut: bool, guest: bool| match (cut, guest) {
        (true, _) | (_, true) => " (truncated)",
        _ => "",
    };
    let _ = writeln!(out, "--- stdout{} ---\n{}", trunc(out_cut, st.b("out-truncated")), out_data.trim_end());
    if !err_data.is_empty() {
        let _ = writeln!(out, "--- stderr{} ---\n{}", trunc(err_cut, st.b("err-truncated")), err_data.trim_end());
    }
    Ok(out)
}

pub fn valid_tag(t: &str) -> bool {
    let mut chars = t.chars();
    t.len() <= 64
        && chars.next().is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+' | '.'))
}

async fn set_tags(ctx: &Ctx, args: &Value) -> R {
    let vmid = need_vmid(args)?;
    let raw = arg_strings(args, "tags")?.ok_or("Invalid arguments: 'tags' is required (an array; empty removes all tags).")?;
    if raw.len() > 32 {
        return Err("Invalid arguments: at most 32 tags.".into());
    }
    let mut tags: Vec<String> = Vec::new();
    for t in raw.iter().map(|t| t.trim()) {
        if !valid_tag(t) {
            return Err(format!(
                "Invalid arguments: tag '{t}' is not valid; use letters, digits, '_', '-', '+' and '.', starting with a letter, digit or '_' (max 64)."
            ));
        }
        if !tags.iter().any(|x| x.eq_ignore_ascii_case(t)) {
            tags.push(t.to_string());
        }
    }
    let res = resources(ctx).await?;
    let r = find_guest(ctx, &res, vmid)?;
    let g = guest_ref(&r);
    let p = if tags.is_empty() { Params::new().set("delete", "tags") } else { Params::new().set("tags", tags.join(";")) };
    ctx.client.put(&format!("{}/config", g.path()), &p).await.map_err(|e| api_err("Setting the tags", e))?;
    Ok(format!(
        "Tags of {} {vmid} '{}': {} → {}",
        g.kind.label(),
        or_dash(&r.name),
        tags_of(&r),
        if tags.is_empty() { "-".into() } else { tags.join(",") }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOLDEN: &str = include_str!("../../tests/mcp_tools.golden.json");

    fn view() -> AppView {
        let dir = std::env::temp_dir().join(format!("pmxspan-mcp-tools-{}", crate::config::new_id()));
        AppView::new(dir.join("mcp-audit.log"))
    }

    /// Fails when a definition changes. If the change is intended, run
    /// `PMXSPAN_UPDATE_GOLDEN=1 cargo test golden` and commit the file:
    /// clients that pin definitions will ask their users to approve again.
    #[test]
    fn tool_definitions_match_golden_file() {
        let now = serde_json::to_string_pretty(&all_definitions()).unwrap() + "\n";
        if std::env::var_os("PMXSPAN_UPDATE_GOLDEN").is_some() {
            std::fs::write(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/mcp_tools.golden.json"), &now).unwrap();
            return;
        }
        assert_eq!(now, GOLDEN, "MCP tool definitions changed; see the comment on this test");
    }

    #[test]
    fn definitions_fit_client_limits() {
        let defs = all_definitions();
        let names: Vec<&str> = defs.iter().map(|d| d["name"].as_str().unwrap()).collect();
        assert_eq!(names.len(), READ_TOOLS.len() + ACTION_TOOLS.len());
        for (d, n) in defs.iter().zip(READ_TOOLS.iter().chain(ACTION_TOOLS.iter())) {
            assert_eq!(d["name"], *n, "definitions follow READ_TOOLS then ACTION_TOOLS");
        }
        for d in &defs {
            let name = d["name"].as_str().unwrap();
            // SSHSpan namespaces tools as mcp__<server>__<tool>: [A-Za-z0-9_], no "__".
            assert!(name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !name.contains("__"), "{name}");
            assert!(d["description"].as_str().unwrap().chars().count() <= 1024, "{name}");
            assert!(d["inputSchema"].to_string().len() < 8 * 1024, "{name}");
            assert_eq!(d["inputSchema"]["type"], "object");
            assert!(!is_destructive_name(name), "{name} would refuse itself");
        }
        assert_eq!(definitions(false).len(), READ_TOOLS.len());
        assert!(definitions(false).iter().all(|d| d["annotations"]["readOnlyHint"] == true));
    }

    #[test]
    fn destructive_names_are_recognized() {
        for n in [
            "destroy",
            "destroy_guest",
            "delete_snapshot",
            "snapshot_delete",
            "rollback",
            "snapshot_rollback",
            "rebuild",
            "delete_backup",
            "restore_backup",
            "set_firewall",
            "firewall_rule_add",
            "set_acl",
            "add_user",
            "create_token",
            "reset",
            "migrate",
            "reset_password",
        ] {
            assert!(is_destructive_name(n), "{n}");
        }
        for n in READ_TOOLS.iter().chain(ACTION_TOOLS.iter()) {
            assert!(!is_destructive_name(n), "{n}");
        }
    }

    #[tokio::test]
    async fn actions_are_gated() {
        let v = view();
        // Off: refused with a pointer to the setting, and audited.
        let out = call(&v, "sess-1", "power", &json!({"vmid": 100, "action": "start"})).await.unwrap();
        assert!(out.is_error);
        assert!(out.text.contains("Allow actions over MCP"), "{}", out.text);
        // On, but no server connected: the action runs into that error, not a refusal.
        v.set_allow_actions(true);
        let out = call(&v, "sess-1", "power", &json!({"vmid": 100, "action": "start"})).await.unwrap();
        assert!(out.is_error && out.text.contains("not connected"), "{}", out.text);
        // Destructive names are refused even with actions on.
        for n in ["destroy", "snapshot_rollback", "delete_backup", "set_firewall"] {
            let out = call(&v, "sess-1", n, &json!({"vmid": 100})).await.unwrap();
            assert!(out.is_error && out.text.starts_with("Refused:"), "{n}: {}", out.text);
            assert!(out.text.contains("never runs these over MCP"));
        }
        // Unknown tools are a protocol error.
        let e = call(&v, "sess-1", "frobnicate", &json!({})).await.err().unwrap();
        assert_eq!(e.code, protocol::INVALID_PARAMS);
        let log = std::fs::read_to_string(v.audit.path()).unwrap();
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines.len(), 6, "{log}");
        assert!(lines[0].contains(" session=sess-1 tool=power ") && lines[0].ends_with("outcome=refused"));
        assert!(lines[1].ends_with("outcome=error"));
        assert!(lines[2].contains("tool=destroy ") && lines[2].ends_with("outcome=refused"));
    }

    #[tokio::test]
    async fn read_tools_need_a_connection() {
        let v = view();
        let out = call(&v, "s", "list_servers", &json!({})).await.unwrap();
        assert!(!out.is_error);
        assert!(out.text.contains("No server profiles"));
        for t in READ_TOOLS.iter().filter(|t| **t != "list_servers") {
            let out = call(&v, "s", t, &json!({})).await.unwrap();
            assert!(out.is_error, "{t}");
            assert!(out.text.contains("not connected"), "{t}: {}", out.text);
        }
        assert!(!v.audit.path().exists(), "read tools are not audited");
    }

    #[test]
    fn argument_helpers() {
        assert_eq!(need_vmid(&json!({"vmid": 100})).unwrap(), 100);
        assert_eq!(need_vmid(&json!({"vmid": "101"})).unwrap(), 101);
        assert!(need_vmid(&json!({})).unwrap_err().contains("required"));
        assert!(need_vmid(&json!({"vmid": -1})).unwrap_err().contains("positive integer"));
        assert!(need_vmid(&json!({"vmid": 0})).is_err());
        assert_eq!(arg_limit(&json!({"limit": 5000}), "limit", 20, 100).unwrap(), 100);
        assert_eq!(arg_limit(&json!({}), "limit", 20, 100).unwrap(), 20);
        assert!(arg_bool(&json!({"x": "yes"}), "x").is_err());
        assert_eq!(parse_ip("10.0.0.1/24").unwrap().to_string(), "10.0.0.1");
        assert_eq!(parse_ip("[2001:db8::1]").unwrap().to_string(), "2001:db8::1");
        assert!(parse_ip("web01").is_err());
        assert!(same_ip("2001:0db8:0::1/64", &"2001:db8::1".parse().unwrap()));
        assert!(valid_tag("prod") && valid_tag("web_1.a+b-c") && !valid_tag("-x") && !valid_tag("a b") && !valid_tag(""));
        assert!(arg_strings(&json!({"argv": ["a", 1]}), "argv").is_err());
    }

    #[test]
    fn config_summary_keeps_secrets_out() {
        let cfg: Map<String, Value> = serde_json::from_value(json!({
            "name": "web01", "cores": 2, "memory": 2048, "cipassword": "s3cret", "sshkeys": "ssh-ed25519%20AAAA",
            "description": "root password is hunter2", "scsi0": "local-lvm:vm-100-disk-0,iothread=1,size=20G",
            "net0": "virtio=BC:24:11:00:00:01,bridge=vmbr0,firewall=1", "ipconfig0": "ip=10.0.0.5/24,gw=10.0.0.1", "ciuser": "debian"
        }))
        .unwrap();
        let s = config_summary(GuestKind::Qemu, &cfg).join(" | ");
        assert!(s.contains("cores=2") && s.contains("scsi0=local-lvm:vm-100-disk-0 size=20G") && s.contains("ip=10.0.0.5/24"), "{s}");
        for secret in ["s3cret", "hunter2", "ssh-ed25519", "debian"] {
            assert!(!s.contains(secret), "{secret} leaked: {s}");
        }
    }
}
