//! In-app consoles: a VNC viewer for VM displays and a terminal for container
//! consoles, VM serial ports and node shells.
//!
//! Both kinds follow the same two steps the Proxmox web UI takes, but from
//! PMXSpan's own API client (same TLS policy, same credentials):
//!
//! 1. `POST .../vncproxy` (VNC) or `POST .../termproxy` (terminal) asks the
//!    node to start a short-lived proxy and returns `{port, ticket, user}`;
//! 2. `GET .../vncwebsocket?port=..&vncticket=..` upgrades to a WebSocket that
//!    carries the console protocol: RFB for VNC, the pve-xtermjs framing for
//!    terminals. The proxy exits if nobody connects within ~10 seconds, so the
//!    two calls are made back to back.
//!
//! The protocol code lives here; the windows are in `ui::console`.

pub mod keysym;
pub mod vnc;
pub mod ws;

use crate::api::types::{GuestKind, GuestRef};
use crate::api::{Params, PveClient};
use crate::util::ValueExt;
use anyhow::{Context, anyhow, bail};
use serde_json::{Map, Value};

/// What a console window is attached to.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ConsoleTarget {
    /// The VM's display over VNC.
    VmDisplay(GuestRef),
    /// A VM serial port (`serial0`..`serial3`) as a terminal.
    VmSerial(GuestRef, u8),
    /// A container's console as a terminal.
    Container(GuestRef),
    /// A node shell (`login` on the node) as a terminal.
    NodeShell(String),
}

impl ConsoleTarget {
    /// True for the terminal (termproxy) kinds.
    pub fn is_terminal(&self) -> bool {
        !matches!(self, ConsoleTarget::VmDisplay(_))
    }

    /// Short label for titles and the status line.
    pub fn kind_label(&self) -> String {
        match self {
            ConsoleTarget::VmDisplay(_) => "Console".into(),
            ConsoleTarget::VmSerial(_, n) => format!("Serial {n}"),
            ConsoleTarget::Container(_) => "Console".into(),
            ConsoleTarget::NodeShell(_) => "Shell".into(),
        }
    }

    /// The API path of the ticket request and its form parameters.
    pub fn proxy_request(&self) -> (String, Params) {
        match self {
            ConsoleTarget::VmDisplay(g) => (format!("{}/vncproxy", g.path()), Params::new().set("websocket", 1)),
            ConsoleTarget::VmSerial(g, n) => (format!("{}/termproxy", g.path()), Params::new().set("serial", format!("serial{n}"))),
            ConsoleTarget::Container(g) => (format!("{}/termproxy", g.path()), Params::new()),
            ConsoleTarget::NodeShell(node) => (format!("/nodes/{node}/termproxy"), Params::new().set("cmd", "login")),
        }
    }

    /// The API path of the WebSocket upgrade.
    pub fn websocket_path(&self) -> String {
        match self {
            ConsoleTarget::VmDisplay(g) | ConsoleTarget::VmSerial(g, _) | ConsoleTarget::Container(g) => {
                format!("{}/vncwebsocket", g.path())
            }
            ConsoleTarget::NodeShell(node) => format!("/nodes/{node}/vncwebsocket"),
        }
    }
}

/// The reply of `vncproxy` / `termproxy`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProxyTicket {
    pub port: u16,
    pub ticket: String,
    /// The user the proxy authenticates (termproxy: the first half of the
    /// `user:ticket` line).
    pub user: String,
}

impl ProxyTicket {
    pub fn from_value(v: &Value) -> anyhow::Result<Self> {
        // `port` arrives as a number or as a string depending on the version.
        let port = v
            .get("port")
            .and_then(|p| p.as_u64().or_else(|| p.as_str().and_then(|s| s.trim().parse().ok())))
            .ok_or_else(|| anyhow!("the proxy reply has no port"))?;
        let port = u16::try_from(port).map_err(|_| anyhow!("the proxy reply has an invalid port {port}"))?;
        let ticket = v.s("ticket");
        if ticket.is_empty() {
            bail!("the proxy reply has no ticket");
        }
        Ok(Self { port, ticket, user: v.s("user") })
    }
}

/// Asks the node for a console proxy. Use the ticket right away.
pub async fn request_proxy(client: &PveClient, target: &ConsoleTarget) -> anyhow::Result<ProxyTicket> {
    let (path, params) = target.proxy_request();
    let v = client.post(&path, &params).await.with_context(|| format!("starting the console proxy ({path})"))?;
    ProxyTicket::from_value(&v)
}

/// Serial ports (`serial0`..`serial3`) of a VM config, in order.
pub fn serial_ports(cfg: &Map<String, Value>) -> Vec<u8> {
    (0u8..4).filter(|n| cfg.contains_key(&format!("serial{n}"))).collect()
}

/// True when the VM has a display a VNC console can show (`vga: none` or a
/// serial-only `vga: serialN` has none).
pub fn has_display(cfg: &Map<String, Value>) -> bool {
    match cfg.get("vga").and_then(Value::as_str) {
        Some(v) => {
            let ty = v.split(',').next().unwrap_or("").trim();
            let ty = ty.strip_prefix("type=").unwrap_or(ty);
            !(ty == "none" || ty.starts_with("serial"))
        }
        None => true,
    }
}

/// The message shown before a terminal console is tried on a token profile.
pub const TOKEN_TERMINAL_NOTE: &str = "Terminal consoles need a password login on this server profile (Proxmox's termproxy does not accept API tokens). VNC consoles work with tokens.";

/// The console kind a guest's primary "Console" action opens.
pub fn primary_target(g: &GuestRef) -> ConsoleTarget {
    match g.kind {
        GuestKind::Qemu => ConsoleTarget::VmDisplay(g.clone()),
        GuestKind::Lxc => ConsoleTarget::Container(g.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vm() -> GuestRef {
        GuestRef { node: "pve1".into(), kind: GuestKind::Qemu, vmid: 100 }
    }

    #[test]
    fn request_builders() {
        let (p, q) = ConsoleTarget::VmDisplay(vm()).proxy_request();
        assert_eq!(p, "/nodes/pve1/qemu/100/vncproxy");
        assert_eq!(q.get("websocket"), Some("1"));
        assert_eq!(ConsoleTarget::VmDisplay(vm()).websocket_path(), "/nodes/pve1/qemu/100/vncwebsocket");

        let (p, q) = ConsoleTarget::VmSerial(vm(), 1).proxy_request();
        assert_eq!(p, "/nodes/pve1/qemu/100/termproxy");
        assert_eq!(q.get("serial"), Some("serial1"));

        let ct = GuestRef { node: "pve2".into(), kind: GuestKind::Lxc, vmid: 102 };
        let (p, q) = ConsoleTarget::Container(ct.clone()).proxy_request();
        assert_eq!(p, "/nodes/pve2/lxc/102/termproxy");
        assert!(q.is_empty());
        assert_eq!(ConsoleTarget::Container(ct).websocket_path(), "/nodes/pve2/lxc/102/vncwebsocket");

        let (p, q) = ConsoleTarget::NodeShell("pve1".into()).proxy_request();
        assert_eq!(p, "/nodes/pve1/termproxy");
        assert_eq!(q.get("cmd"), Some("login"));
        assert_eq!(ConsoleTarget::NodeShell("pve1".into()).websocket_path(), "/nodes/pve1/vncwebsocket");
        assert!(ConsoleTarget::NodeShell("pve1".into()).is_terminal());
        assert!(!ConsoleTarget::VmDisplay(vm()).is_terminal());
    }

    #[test]
    fn proxy_ticket_parsing() {
        let v = serde_json::json!({"port": "5901", "ticket": "PVEVNC:abc", "user": "root@pam", "upid": "x"});
        assert_eq!(ProxyTicket::from_value(&v).unwrap(), ProxyTicket { port: 5901, ticket: "PVEVNC:abc".into(), user: "root@pam".into() });
        let v = serde_json::json!({"port": 5902, "ticket": "T"});
        assert_eq!(ProxyTicket::from_value(&v).unwrap().port, 5902);
        assert!(ProxyTicket::from_value(&serde_json::json!({"ticket": "T"})).is_err());
        assert!(ProxyTicket::from_value(&serde_json::json!({"port": 1})).is_err());
        assert!(ProxyTicket::from_value(&serde_json::json!({"port": 70000, "ticket": "T"})).is_err());
    }

    #[test]
    fn serial_and_display_detection() {
        let cfg: Map<String, Value> = serde_json::from_str(r#"{"serial0":"socket","serial2":"socket","vga":"std"}"#).unwrap();
        assert_eq!(serial_ports(&cfg), vec![0, 2]);
        assert!(has_display(&cfg));
        let cfg: Map<String, Value> = serde_json::from_str(r#"{"vga":"serial0"}"#).unwrap();
        assert!(!has_display(&cfg));
        let cfg: Map<String, Value> = serde_json::from_str(r#"{"vga":"type=none"}"#).unwrap();
        assert!(!has_display(&cfg));
        assert!(has_display(&Map::new()));
        assert!(serial_ports(&Map::new()).is_empty());
    }
}
