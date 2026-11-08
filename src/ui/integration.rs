//! SSHSpan in the window: "Connect with SSHSpan", "Send to SSHSpan…", the
//! launch confirmation, and the `pmxspan://` links SSHSpan sends back.
//!
//! Nothing received is applied on its own: navigation links only navigate
//! (after unlocking and, if needed, a confirmed server switch) and offered
//! keys land in a card the user confirms.

use super::widgets::{self, Confirm};
use super::{Action, Actions, App, Nav, Shared, ToastLevel, theme};
use crate::api::types::GuestKind;
use crate::config::Settings;
use crate::hostkeys::{HostKey, HostKeyRecord};
use crate::integrations::link::{self, Incoming, TargetKind};
use crate::integrations::{bundle, ipc, resolve, sshspan};
use crate::rt::Fetch;
use crate::sshkey::PublicKey;
use egui::RichText;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Received links wait this long for an unlock or a connection.
const PENDING_FOR: Duration = Duration::from_secs(600);

/// What a view asks the window to do.
pub enum Request {
    ConnectGuest(GuestKind, u64),
    ConnectNode(String),
    /// Open the bulk review, with these guests ticked (empty = all).
    SendBulk(Vec<u64>),
}

/// An SSHSpan action button. Hidden when the integration is switched off,
/// disabled with the "not found" hint when SSHSpan is not on this computer.
/// Returns true when clicked.
pub fn sshspan_button(ui: &mut egui::Ui, settings: &Settings, label: &str, tip: &str, style: ButtonStyle) -> bool {
    if !settings.sshspan_enabled {
        return false;
    }
    let status = sshspan::status(&settings.sshspan_path);
    let available = status.as_ref().is_some_and(sshspan::Discovery::available);
    let text = format!("{}  {label}", theme::icon::TERMINAL);
    let r = ui
        .add_enabled_ui(available, |ui| match style {
            ButtonStyle::Primary => widgets::primary_button(ui, text),
            ButtonStyle::Normal => widgets::icon_button(ui, theme::icon::TERMINAL, label),
            ButtonStyle::Small => ui.small_button(label),
        })
        .inner;
    let hint = if status.is_none() { "Looking for SSHSpan…" } else { sshspan::NOT_FOUND_HINT };
    r.on_hover_text(tip).on_disabled_hover_text(hint).clicked()
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ButtonStyle {
    Primary,
    Normal,
    Small,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum TargetKey {
    Guest(String, u64),
    Node(String, String),
}

/// A link waiting to be launched once the user confirmed the found path.
#[derive(Clone)]
struct PendingLaunch {
    exe: PathBuf,
    url: String,
    done: String,
    bundle: Option<PathBuf>,
}

struct ConnectCard {
    key: TargetKey,
    kind: TargetKind,
    vmid: Option<u64>,
    node: String,
    pool: String,
    name: String,
    subtitle: String,
    host: String,
    user: String,
    port: String,
    source: String,
    /// (host, where it came from, user)
    resolving: Option<Fetch<(Option<String>, &'static str, String)>>,
    keys: Option<HostKeyRecord>,
}

struct BulkRow {
    include: bool,
    kind: TargetKind,
    vmid: Option<u64>,
    node: String,
    name: String,
    pool: Option<String>,
    tags: Vec<String>,
    host: String,
    user: String,
    keys: Vec<HostKey>,
    rotated: Option<i64>,
}

impl BulkRow {
    fn problem(&self) -> Option<String> {
        link::check_target(self.host.trim(), 22, self.user.trim(), &self.name).err()
    }
    fn category(&self, profile: &str) -> Vec<String> {
        let last = match self.kind {
            TargetKind::Node => "Nodes".to_string(),
            _ => self.pool.clone().unwrap_or_else(|| "no pool".into()),
        };
        vec!["Proxmox".into(), profile.to_string(), last]
    }
}

type BulkResolved = (HashMap<u64, resolve::Resolved>, HashMap<String, String>);

struct BulkDialog {
    server_id: String,
    rows: Vec<BulkRow>,
    include_nodes: bool,
    filter: String,
    loading: Fetch<BulkResolved>,
    filled: bool,
}

struct AddKeyCard {
    key: PublicKey,
    name: String,
    source: String,
}

#[derive(Default)]
pub struct IntegrationUi {
    rx: Option<mpsc::Receiver<ipc::Message>>,
    started: bool,
    /// A received link waiting for the vault to be unlocked.
    pending_link: Option<(Incoming, Instant)>,
    /// A navigation waiting for the server connection.
    pending_nav: Option<(Incoming, Instant)>,
    switch_confirm: Option<Confirm<Incoming>>,
    launch_confirm: Option<Confirm<PendingLaunch>>,
    connect: Option<ConnectCard>,
    bulk: Option<BulkDialog>,
    add_key: Option<AddKeyCard>,
    /// Host/user/port the user typed, per target, for this session.
    remembered: HashMap<TargetKey, (String, String, String)>,
}

fn raise(ctx: &egui::Context) {
    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
}

fn key_types(keys: &[HostKey]) -> String {
    keys.iter().map(|k| crate::sshkey::short_type(&k.key_type)).collect::<Vec<_>>().join(", ")
}

impl App {
    /// Runs every frame, also while the window is hidden or the vault locked:
    /// picks up links forwarded by a second PMXSpan start.
    pub(super) fn integration_logic(&mut self, ctx: &egui::Context) {
        let ig = &mut self.integration;
        if !ig.started {
            ig.started = true;
            ig.rx = ipc::take_receiver();
            // Old handoff bundles go at startup (the window app only).
            if let Some(dir) = bundle::handoff_dir() {
                let _ = std::thread::Builder::new().name("handoff-cleanup".into()).spawn(move || {
                    let n = bundle::cleanup(&dir, bundle::MAX_AGE);
                    if n > 0 {
                        tracing::info!("removed {n} old SSHSpan handoff bundle(s)");
                    }
                });
            }
        }
        let mut msgs = Vec::new();
        if let Some(rx) = &ig.rx {
            while let Ok(m) = rx.try_recv() {
                msgs.push(m);
            }
        }
        for m in msgs {
            raise(ctx);
            for arg in m.args {
                match link::parse_incoming(&arg) {
                    // A newer link replaces one still waiting.
                    Ok(inc) => self.integration.pending_link = Some((inc, Instant::now())),
                    Err(e) => {
                        tracing::warn!("pmxspan:// link refused: {e}");
                        self.toast(ToastLevel::Error, format!("Link refused: {e}"));
                    }
                }
            }
        }
        let ig = &mut self.integration;
        if ig.pending_link.as_ref().is_some_and(|(_, t)| t.elapsed() > PENDING_FOR) {
            ig.pending_link = None;
        }
        if ig.pending_nav.as_ref().is_some_and(|(_, t)| t.elapsed() > PENDING_FOR) {
            ig.pending_nav = None;
        }
        if ig.pending_link.is_some() || ig.pending_nav.is_some() {
            ctx.request_repaint_after(Duration::from_secs(1));
        }
    }

    /// Acts on received links once unlocked; draws the integration windows.
    pub(super) fn integration_windows(&mut self, ctx: &egui::Context, actions: &mut Actions) {
        if let Some((inc, _)) = self.integration.pending_link.take() {
            self.dispatch_link(inc);
        }
        self.pump_pending_nav();

        if let Some(inc) = widgets::confirm_ui(ctx, &mut self.integration.switch_confirm)
            && let Some(id) = inc.cluster().map(str::to_string)
        {
            self.integration.pending_nav = Some((inc, Instant::now()));
            self.connect(&id, None);
        }
        let had_launch = self.integration.launch_confirm.as_ref().map(|c| c.payload.clone());
        if let Some(p) = widgets::confirm_ui(ctx, &mut self.integration.launch_confirm) {
            self.config.settings.sshspan_approved_path = p.exe.to_string_lossy().into_owned();
            actions.0.push(Action::ConfigChanged);
            self.spawn_sshspan(&p.exe, &p.url, &p.done, p.bundle.as_ref());
        } else if let (Some(p), None) = (had_launch, self.integration.launch_confirm.as_ref()) {
            // Cancelled: nothing was handed over, so the bundle goes too.
            if let Some(b) = p.bundle {
                let _ = std::fs::remove_file(b);
            }
        }
        self.connect_card(ctx, actions);
        self.bulk_dialog(ctx);
        self.add_key_card(ctx, actions);
    }

    pub(super) fn integration_request(&mut self, r: Request) {
        let Some(sess) = self.session.as_ref() else { return };
        let sh = sess.shared.clone();
        match r {
            Request::ConnectGuest(kind, vmid) => self.open_connect_guest(&sh, kind, vmid),
            Request::ConnectNode(node) => self.open_connect_node(&sh, node),
            Request::SendBulk(ticked) => self.integration.bulk = Some(BulkDialog::new(&sh, &ticked)),
        }
    }

    // ---- launching -------------------------------------------------------------

    /// Delivers a URL to SSHSpan in the contract's order (executable, OS
    /// handler); a found path is confirmed once first.
    fn launch_sshspan(&mut self, url: String, done: String, bundle: Option<PathBuf>) {
        let s = &self.config.settings;
        let Some(d) = sshspan::status(&s.sshspan_path) else {
            self.toast(ToastLevel::Info, "Still looking for SSHSpan; try again in a moment.");
            return;
        };
        match sshspan::plan(&d, &s.sshspan_approved_path) {
            sshspan::Plan::Exe(exe) => self.spawn_sshspan(&exe, &url, &done, bundle.as_ref()),
            sshspan::Plan::Confirm(exe) => {
                let body = format!(
                    "PMXSpan found SSHSpan at\n\n{}\n\nStart this program to open the link? PMXSpan remembers your answer and asks again \
                     if the path changes. You can also set the path in Settings.",
                    exe.display()
                );
                self.integration.launch_confirm = Some(Confirm::new("Start SSHSpan?", body, PendingLaunch { exe, url, done, bundle }));
            }
            sshspan::Plan::OsOpen => match sshspan::os_open(&url) {
                Ok(()) => self.toast(ToastLevel::Ok, done),
                Err(e) => self.launch_failed(&e.to_string(), bundle.as_ref()),
            },
            sshspan::Plan::Unavailable => self.launch_failed(sshspan::NOT_FOUND_HINT, bundle.as_ref()),
        }
    }

    fn spawn_sshspan(&mut self, exe: &std::path::Path, url: &str, done: &str, bundle: Option<&PathBuf>) {
        match sshspan::spawn_exe(exe, url) {
            Ok(()) => {
                tracing::info!("handed a link to SSHSpan ({})", exe.display());
                self.toast(ToastLevel::Ok, done.to_string());
            }
            Err(e) => {
                sshspan::recheck();
                self.launch_failed(&format!("starting {} failed: {e}", exe.display()), bundle);
            }
        }
    }

    fn launch_failed(&mut self, why: &str, bundle: Option<&PathBuf>) {
        if let Some(b) = bundle {
            let _ = std::fs::remove_file(b);
        }
        self.toast(ToastLevel::Error, format!("Could not open SSHSpan: {why}"));
    }

    // ---- received links ------------------------------------------------------------

    fn dispatch_link(&mut self, inc: Incoming) {
        match &inc {
            Incoming::AddKey { key, name, source } => {
                self.integration.add_key = Some(AddKeyCard { key: key.clone(), name: name.clone(), source: source.clone() });
            }
            Incoming::Guest { cluster, .. } | Incoming::Node { cluster, .. } => {
                let Some(profile) = self.config.profile(cluster).cloned() else {
                    self.toast(ToastLevel::Error, format!("No server profile {cluster} in PMXSpan"));
                    return;
                };
                let connected = self.session.as_ref().map(|s| s.shared.server_id.clone());
                let connecting = self.connecting.as_ref().map(|c| c.profile_id.clone());
                if connected.as_deref() == Some(cluster) || connecting.as_deref() == Some(cluster) {
                    self.integration.pending_nav = Some((inc, Instant::now()));
                } else if let Some(cur) = self.session.as_ref().map(|s| s.shared.profile.display_name()) {
                    let what = match &inc {
                        Incoming::Guest { vmid, .. } => format!("guest {vmid}"),
                        Incoming::Node { node, .. } => format!("node {node}"),
                        Incoming::AddKey { .. } => String::new(),
                    };
                    let body = format!(
                        "SSHSpan asks to open {what} on {}. PMXSpan is connected to {cur}. Switch to {}?",
                        profile.display_name(),
                        profile.display_name()
                    );
                    self.integration.switch_confirm = Some(Confirm::new("Switch server?", body, inc));
                } else {
                    let id = profile.id.clone();
                    self.integration.pending_nav = Some((inc, Instant::now()));
                    self.connect(&id, None);
                }
            }
        }
    }

    /// Opens the page a link asked for once its server is connected and loaded.
    fn pump_pending_nav(&mut self) {
        let Some((inc, _)) = self.integration.pending_nav.as_ref() else { return };
        let Some(sess) = self.session.as_ref() else { return };
        if inc.cluster() != Some(sess.shared.server_id.as_str()) || !sess.loaded_once {
            return;
        }
        let Some((inc, _)) = self.integration.pending_nav.take() else { return };
        let name = sess.shared.profile.display_name();
        match inc {
            Incoming::Guest { vmid, .. } => {
                let kind = sess.shared.guests().find(|g| g.vmid == vmid).and_then(|g| g.kind());
                match kind {
                    Some(kind) => self.nav = Nav::Guest(kind, vmid),
                    None => self.toast(ToastLevel::Error, format!("Guest {vmid} was not found on {name}")),
                }
            }
            Incoming::Node { node, .. } => {
                let found = sess.shared.nodes().contains(&node);
                if found {
                    self.nav = Nav::Node(node);
                } else {
                    self.toast(ToastLevel::Error, format!("Node {node} was not found on {name}"));
                }
            }
            Incoming::AddKey { .. } => {}
        }
    }

    fn add_key_card(&mut self, ctx: &egui::Context, actions: &mut Actions) {
        let Some(card) = self.integration.add_key.as_ref() else { return };
        let p = theme::p();
        let create_open = self.session.is_some() && self.nav == Nav::Create;
        let ci_guest = match &self.nav {
            Nav::Guest(GuestKind::Qemu, vmid) => self
                .session
                .as_ref()
                .and_then(|s| s.guest_views.get(&(GuestKind::Qemu, *vmid)))
                .filter(|v| v.cloudinit_open())
                .map(|_| *vmid),
            _ => None,
        };
        enum Choice {
            Settings,
            Create,
            CloudInit(u64),
            Cancel,
        }
        let mut choice = None;
        let mut open = true;
        egui::Window::new(RichText::new(format!("{}  Public key from SSHSpan", theme::icon::KEY)).strong())
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_max_width(560.0);
                egui::Grid::new("addkey").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                    widgets::kv(ui, "Key name", if card.name.is_empty() { "(none)".to_string() } else { card.name.clone() });
                    widgets::kv(ui, "Type", card.key.key_type.clone());
                    ui.label(RichText::new("Fingerprint").color(p.muted));
                    widgets::copyable(ui, &card.key.fingerprint);
                    ui.end_row();
                    widgets::kv(ui, "Comment", if card.key.comment.is_empty() { "-".to_string() } else { card.key.comment.clone() });
                    if !card.source.is_empty() {
                        widgets::kv(ui, "Sent by", card.source.clone());
                    }
                });
                ui.add_space(6.0);
                widgets::muted(ui, "Only the public key was received. Nothing is added until you choose where.");
                ui.add_space(8.0);
                ui.horizontal_wrapped(|ui| {
                    if widgets::primary_button(ui, "Add to default SSH keys (Settings)").clicked() {
                        choice = Some(Choice::Settings);
                    }
                    if create_open && ui.button("Add to the open Create wizard").clicked() {
                        choice = Some(Choice::Create);
                    }
                    if let Some(vmid) = ci_guest
                        && ui.button(format!("Add to the Cloud-Init tab of {vmid}")).clicked()
                    {
                        choice = Some(Choice::CloudInit(vmid));
                    }
                    if ui.button("Cancel").clicked() {
                        choice = Some(Choice::Cancel);
                    }
                });
            });
        if !open {
            choice = Some(Choice::Cancel);
        }
        let Some(choice) = choice else { return };
        let Some(card) = self.integration.add_key.take() else { return };
        let label = if card.name.is_empty() { card.key.fingerprint.clone() } else { card.name.clone() };
        let (added, place) = match choice {
            Choice::Cancel => return,
            Choice::Settings => {
                let added = crate::sshkey::append_unique(&mut self.config.settings.default_ssh_keys, &card.key);
                if added {
                    actions.0.push(Action::ConfigChanged);
                }
                (added, "the default SSH keys")
            }
            Choice::Create => {
                let added = self.session.as_mut().is_some_and(|s| s.create.add_ssh_key(&card.key));
                (added, "the Create wizard")
            }
            Choice::CloudInit(vmid) => {
                let added = self
                    .session
                    .as_mut()
                    .and_then(|s| s.guest_views.get_mut(&(GuestKind::Qemu, vmid)))
                    .is_some_and(|v| v.add_cloudinit_key(&card.key));
                (added, "the Cloud-Init tab (click Save there to apply it)")
            }
        };
        if added {
            self.toast(ToastLevel::Ok, format!("Key {label} added to {place}"));
        } else {
            self.toast(ToastLevel::Info, format!("Key {label} is already in {place}; not added again"));
        }
    }

    // ---- connect ------------------------------------------------------------------

    fn open_connect_guest(&mut self, sh: &Shared, kind: GuestKind, vmid: u64) {
        let Some(res) = sh.find_guest(kind, vmid).cloned() else { return };
        let Some(g) = res.guest_ref() else { return };
        let (ipam4, ipam6) = {
            let db = sh.ipam.lock();
            (resolve::ipam_main(&db, &sh.server_id, vmid, false), resolve::ipam_main(&db, &sh.server_id, vmid, true))
        };
        let record = crate::hostkeys::global().and_then(|s| s.lock().get(&sh.server_id, vmid).cloned());
        let catalog_user = record
            .as_ref()
            .filter(|r| !r.catalog_id.is_empty())
            .and_then(|r| sh.catalog.lock().entries.iter().find(|e| e.id == r.catalog_id).map(|e| e.cloud_user.clone()));
        let running = res.status == "running";
        let key = TargetKey::Guest(sh.server_id.clone(), vmid);
        let c = sh.client.clone();
        let resolving = Fetch::spawn(async move {
            let r = resolve::resolve_guest(&c, &g, running, ipam4, ipam6, catalog_user).await;
            Ok((r.host, r.host_source, r.user))
        });
        let mut subtitle = format!("{} {vmid} on {}", kind.label(), res.node);
        if !res.pool.is_empty() {
            subtitle.push_str(&format!(" · pool {}", res.pool));
        }
        self.show_connect(ConnectCard {
            key,
            kind: if kind == GuestKind::Qemu { TargetKind::Vm } else { TargetKind::Ct },
            vmid: Some(vmid),
            node: res.node.clone(),
            pool: res.pool.clone(),
            name: res.display_name(),
            subtitle,
            host: String::new(),
            user: String::new(),
            port: "22".into(),
            source: String::new(),
            resolving: Some(resolving),
            keys: if kind == GuestKind::Qemu { record } else { None },
        });
    }

    fn open_connect_node(&mut self, sh: &Shared, node: String) {
        let (c, n, host, count) = (sh.client.clone(), node.clone(), sh.profile.host.clone(), sh.nodes().len());
        let resolving = Fetch::spawn(async move {
            let st = c.get_list("/cluster/status").await.unwrap_or_default();
            let h = resolve::node_host(&st, &n, &host, count);
            Ok((h, "cluster status", "root".to_string()))
        });
        self.show_connect(ConnectCard {
            key: TargetKey::Node(sh.server_id.clone(), node.clone()),
            kind: TargetKind::Node,
            vmid: None,
            node: node.clone(),
            pool: String::new(),
            name: node.clone(),
            subtitle: format!("Proxmox node of {}", sh.profile.display_name()),
            host: String::new(),
            user: String::new(),
            port: "22".into(),
            source: String::new(),
            resolving: Some(resolving),
            keys: None,
        });
    }

    fn show_connect(&mut self, mut card: ConnectCard) {
        if let Some((h, u, p)) = self.integration.remembered.get(&card.key) {
            // The user's own values win over a new lookup.
            card.host = h.clone();
            card.user = u.clone();
            card.port = p.clone();
            card.source = "as you set it earlier".into();
            card.resolving = None;
        }
        self.integration.connect = Some(card);
    }

    fn connect_card(&mut self, ctx: &egui::Context, _actions: &mut Actions) {
        let Some(sess) = self.session.as_ref() else {
            self.integration.connect = None;
            return;
        };
        let profile = sess.shared.profile.clone();
        let Some(card) = self.integration.connect.as_mut() else { return };
        if let Some(f) = card.resolving.as_mut()
            && let Some(Ok((host, source, user))) = f.poll()
        {
            card.host = host.clone().unwrap_or_default();
            card.source = if host.is_some() { source.to_string() } else { "no address found: enter one".into() };
            card.user = user.clone();
            card.resolving = None;
        }
        let p = theme::p();
        let mut open = true;
        let mut go = false;
        let mut cancel = false;
        egui::Window::new(RichText::new(format!("{}  Connect with SSHSpan", theme::icon::TERMINAL)).strong())
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_max_width(520.0);
                ui.label(theme::bold(&card.name, 16.0));
                ui.label(RichText::new(&card.subtitle).size(12.5).color(p.muted));
                ui.add_space(8.0);
                if card.resolving.is_some() {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        widgets::muted(ui, "Looking up the address and user…");
                    });
                    return;
                }
                egui::Grid::new("ssh-connect").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                    ui.label("Host");
                    ui.vertical(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut card.host).desired_width(300.0).hint_text("IP address or host name"));
                        if !card.source.is_empty() {
                            ui.label(RichText::new(&card.source).size(11.5).color(p.muted));
                        }
                    });
                    ui.end_row();
                    ui.label("User");
                    ui.add(egui::TextEdit::singleline(&mut card.user).desired_width(180.0));
                    ui.end_row();
                    ui.label("Port");
                    ui.add(egui::TextEdit::singleline(&mut card.port).desired_width(70.0));
                    ui.end_row();
                    ui.label("Host keys");
                    ui.vertical(|ui| host_key_status(ui, card.kind, card.keys.as_ref()));
                    ui.end_row();
                });
                let port = card.port.trim().parse::<u16>().unwrap_or(0);
                let check = link::check_target(card.host.trim(), port, card.user.trim(), &card.name);
                ui.add_space(6.0);
                if let Err(e) = &check {
                    ui.label(RichText::new(e).color(theme::err()));
                }
                widgets::muted(ui, "SSHSpan opens its Quick connect dialog with these details. No password or private key is sent.");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.add_enabled_ui(check.is_ok(), |ui| widgets::primary_button(ui, "Open in SSHSpan")).inner.clicked() {
                        go = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
        if !open || cancel {
            self.integration.connect = None;
            return;
        }
        if !go {
            return;
        }
        let Some(card) = self.integration.connect.take() else { return };
        let (host, user, port) = (card.host.trim().to_string(), card.user.trim().to_string(), card.port.trim().to_string());
        self.integration.remembered.insert(card.key.clone(), (host.clone(), user.clone(), port.clone()));
        let keys = card.keys.as_ref();
        let target = link::ConnectTarget {
            host,
            port: port.parse().unwrap_or(22),
            user,
            name: card.name.clone(),
            cluster_id: profile.id.clone(),
            cluster_name: profile.display_name(),
            kind: card.kind,
            vmid: card.vmid,
            node: card.node.clone(),
            pool: card.pool.clone(),
            host_keys: keys.map(|r| r.keys.iter().map(|k| k.key.clone()).collect()).unwrap_or_default(),
            host_key_rotated: keys.and_then(|r| r.rotated_at),
        };
        match link::connect_url(&target) {
            Ok(url) => self.launch_sshspan(url, format!("Opened {} in SSHSpan", card.name), None),
            Err(e) => self.toast(ToastLevel::Error, format!("Cannot build the SSHSpan link: {e}")),
        }
    }

    // ---- bulk -----------------------------------------------------------------------

    fn bulk_dialog(&mut self, ctx: &egui::Context) {
        let Some(sess) = self.session.as_ref() else {
            self.integration.bulk = None;
            return;
        };
        let shared = sess.shared.clone();
        let Some(d) = self.integration.bulk.as_mut() else { return };
        if d.server_id != shared.server_id {
            self.integration.bulk = None;
            return;
        }
        if !d.filled
            && let Some(Ok((guests, nodes))) = d.loading.poll()
        {
            for r in d.rows.iter_mut() {
                match (r.kind, r.vmid) {
                    (TargetKind::Node, _) => r.host = nodes.get(&r.node).cloned().unwrap_or_default(),
                    (_, Some(vmid)) => {
                        if let Some(res) = guests.get(&vmid) {
                            r.host = res.host.clone().unwrap_or_default();
                            r.user = res.user.clone();
                        }
                    }
                    _ => {}
                }
                if r.problem().is_some() {
                    r.include = false;
                }
            }
            d.filled = true;
        }
        let profile_name = shared.profile.display_name();
        let p = theme::p();
        let mut open = true;
        let mut send = false;
        let mut cancel = false;
        egui::Window::new(RichText::new(format!("{}  Send to SSHSpan", theme::icon::TERMINAL)).strong())
            .collapsible(false)
            .resizable(true)
            .default_size([960.0, 520.0])
            .open(&mut open)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                widgets::muted(
                    ui,
                    "SSHSpan receives names, addresses, user names and public host keys only, and shows its own import review \
                     before it saves anything.",
                );
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut d.filter).hint_text("Filter…").desired_width(200.0));
                    ui.checkbox(&mut d.include_nodes, "Include nodes");
                    if ui.small_button("Tick all").clicked() {
                        for r in d.rows.iter_mut().filter(|r| r.problem().is_none()) {
                            r.include = true;
                        }
                    }
                    if ui.small_button("Tick none").clicked() {
                        for r in d.rows.iter_mut() {
                            r.include = false;
                        }
                    }
                    if !d.filled {
                        ui.spinner();
                        widgets::muted(ui, "Looking up addresses…");
                    }
                });
                ui.add_space(4.0);
                let f = d.filter.to_lowercase();
                let show_nodes = d.include_nodes;
                let filled = d.filled;
                egui::ScrollArea::vertical().max_height(ui.available_height() - 50.0).auto_shrink([false, true]).show(ui, |ui| {
                    egui::Grid::new("bulk-rows").striped(true).num_columns(7).spacing([10.0, 5.0]).show(ui, |ui| {
                        for h in ["", "Name", "Host", "User", "Category in SSHSpan", "Host keys", ""] {
                            ui.label(RichText::new(h).size(12.0).family(theme::semibold()).color(p.muted));
                        }
                        ui.end_row();
                        for (i, r) in d.rows.iter_mut().enumerate() {
                            if r.kind == TargetKind::Node && !show_nodes {
                                continue;
                            }
                            if !f.is_empty()
                                && !format!("{} {:?} {} {}", r.name, r.vmid, r.host, r.pool.clone().unwrap_or_default())
                                    .to_lowercase()
                                    .contains(&f)
                            {
                                continue;
                            }
                            let problem = if filled { r.problem() } else { None };
                            ui.add_enabled(problem.is_none(), egui::Checkbox::without_text(&mut r.include));
                            ui.vertical(|ui| {
                                ui.spacing_mut().item_spacing.y = 0.0;
                                ui.label(RichText::new(&r.name).family(theme::semibold()));
                                let what = match (r.kind, r.vmid) {
                                    (TargetKind::Node, _) => "node".to_string(),
                                    (k, Some(v)) => format!("{} {v} · {}", if k == TargetKind::Vm { "VM" } else { "CT" }, r.node),
                                    _ => String::new(),
                                };
                                ui.label(RichText::new(what).size(11.5).color(p.muted));
                            });
                            ui.add(egui::TextEdit::singleline(&mut r.host).desired_width(170.0).id_salt(("bh", i)).hint_text("address"));
                            ui.add(egui::TextEdit::singleline(&mut r.user).desired_width(90.0).id_salt(("bu", i)));
                            ui.label(RichText::new(r.category(&profile_name).join(" › ")).size(12.0));
                            match (r.kind, r.keys.is_empty(), r.rotated) {
                                (TargetKind::Vm, false, Some(t)) => {
                                    widgets::pill(ui, format!("rotated {}", crate::util::fmt_epoch(t)), widgets::Tone::Warn)
                                        .on_hover_text(key_types(&r.keys));
                                }
                                (TargetKind::Vm, false, None) => {
                                    widgets::pill(ui, format!("{} known", r.keys.len()), widgets::Tone::Ok)
                                        .on_hover_text(key_types(&r.keys));
                                }
                                (TargetKind::Vm, true, _) => {
                                    widgets::pill(ui, "not read yet", widgets::Tone::Neutral);
                                }
                                _ => {
                                    widgets::pill(ui, "not available", widgets::Tone::Neutral)
                                        .on_hover_text("Proxmox offers no way to read host keys of containers and nodes");
                                }
                            }
                            match &problem {
                                Some(e) => ui.label(RichText::new(e).size(12.0).color(theme::err())),
                                None => ui.label(""),
                            };
                            if problem.is_some() {
                                r.include = false;
                            }
                            ui.end_row();
                        }
                    });
                });
                ui.separator();
                let n = d.rows.iter().filter(|r| r.include && (r.kind != TargetKind::Node || show_nodes)).count();
                ui.horizontal(|ui| {
                    let label = format!("Send {n} host{} to SSHSpan", if n == 1 { "" } else { "s" });
                    if ui.add_enabled_ui(n > 0 && filled, |ui| widgets::primary_button(ui, label)).inner.clicked() {
                        send = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
        if !open || cancel {
            self.integration.bulk = None;
            return;
        }
        if !send {
            return;
        }
        let Some(d) = self.integration.bulk.take() else { return };
        let hosts: Vec<bundle::BundleHost> = d
            .rows
            .iter()
            .filter(|r| r.include && (r.kind != TargetKind::Node || d.include_nodes) && r.problem().is_none())
            .map(|r| bundle::BundleHost {
                kind: r.kind.as_str().into(),
                vmid: r.vmid,
                node: r.node.clone(),
                name: r.name.clone(),
                pool: r.pool.clone(),
                tags: r.tags.clone(),
                host: r.host.trim().to_string(),
                port: 22,
                user: r.user.trim().to_string(),
                host_keys: r
                    .keys
                    .iter()
                    .map(|k| bundle::BundleHostKey {
                        key_type: k.key_type.clone(),
                        key: k.key.clone(),
                        fetched_at: bundle::rfc3339(k.fetched_at),
                    })
                    .collect(),
                host_key_rotated_at: r.rotated.map(bundle::rfc3339),
                category: r.category(&profile_name),
            })
            .collect();
        let n = hosts.len();
        let pr = &shared.profile;
        let b = bundle::Bundle::new(
            bundle::BundleCluster { id: pr.id.clone(), name: profile_name.clone(), host: pr.host.trim().to_string(), port: pr.port },
            hosts,
        );
        let Some(dir) = bundle::handoff_dir() else {
            self.toast(ToastLevel::Error, "Cannot find a private folder for the handoff file");
            return;
        };
        match bundle::write(&dir, &b).and_then(|(path, sum)| Ok((link::import_url(&path, &sum).map_err(anyhow::Error::msg)?, path))) {
            Ok((url, path)) => self.launch_sshspan(
                url,
                format!("Sent {n} host{} to SSHSpan: review the import there", if n == 1 { "" } else { "s" }),
                Some(path),
            ),
            Err(e) => self.toast(ToastLevel::Error, format!("Writing the handoff file failed: {e:#}")),
        }
    }
}

/// The host-key line of the connect card.
fn host_key_status(ui: &mut egui::Ui, kind: TargetKind, rec: Option<&HostKeyRecord>) {
    let p = theme::p();
    let small = |t: String| RichText::new(t).size(12.0);
    match (kind, rec.filter(|r| !r.keys.is_empty())) {
        (TargetKind::Vm, Some(r)) => {
            ui.label(
                small(format!(
                    "{} {} known ({}), read {}",
                    theme::icon::CHECK,
                    r.keys.len(),
                    key_types(&r.keys),
                    crate::util::fmt_epoch(r.fetched_at())
                ))
                .color(theme::ok()),
            );
            ui.label(small("SSHSpan checks the server against them on the first connection.".into()).color(p.muted));
            if let Some(t) = r.rotated_at {
                ui.label(
                    small(format!("Rotated {} by a rebuild: SSHSpan offers to replace its saved key.", crate::util::fmt_epoch(t)))
                        .color(theme::warn()),
                );
            }
            if let Some(t) = r.changed_at {
                ui.label(
                    small(format!("{} Changed {} without a PMXSpan rebuild.", theme::icon::WARNING, crate::util::fmt_epoch(t)))
                        .color(theme::warn()),
                );
            }
        }
        (TargetKind::Vm, None) => {
            ui.label(
                small(
                    "Not read yet: use Refresh under SSH host keys on the Summary tab. Otherwise SSHSpan asks you to trust the key.".into(),
                )
                .color(p.muted),
            );
        }
        (TargetKind::Ct, _) => {
            ui.label(small("Not available for containers: SSHSpan asks you to trust the key on first connect.".into()).color(p.muted));
        }
        (TargetKind::Node, _) => {
            ui.label(small("Not available for nodes: SSHSpan asks you to trust the key on first connect.".into()).color(p.muted));
        }
    }
}

impl BulkDialog {
    fn new(sh: &Shared, ticked: &[u64]) -> Self {
        let store = crate::hostkeys::global().map(|s| s.lock().clone());
        let mut rows = Vec::new();
        let mut guests: Vec<_> = sh.guests().filter(|g| !g.template).cloned().collect();
        guests.sort_by_key(|g| g.vmid);
        let mut jobs = Vec::new();
        for g in &guests {
            let Some(kind) = g.kind() else { continue };
            let rec = store.as_ref().and_then(|s| s.get(&sh.server_id, g.vmid).cloned()).filter(|_| kind == GuestKind::Qemu);
            let catalog_user = rec
                .as_ref()
                .filter(|r| !r.catalog_id.is_empty())
                .and_then(|r| sh.catalog.lock().entries.iter().find(|e| e.id == r.catalog_id).map(|e| e.cloud_user.clone()));
            let (ip4, ip6) = {
                let db = sh.ipam.lock();
                (resolve::ipam_main(&db, &sh.server_id, g.vmid, false), resolve::ipam_main(&db, &sh.server_id, g.vmid, true))
            };
            rows.push(BulkRow {
                include: ticked.is_empty() || ticked.contains(&g.vmid),
                kind: if kind == GuestKind::Qemu { TargetKind::Vm } else { TargetKind::Ct },
                vmid: Some(g.vmid),
                node: g.node.clone(),
                name: g.display_name(),
                pool: Some(g.pool.clone()).filter(|p| !p.is_empty()),
                tags: g.tag_list(),
                host: String::new(),
                user: String::new(),
                keys: rec.as_ref().map(|r| r.keys.clone()).unwrap_or_default(),
                rotated: rec.as_ref().and_then(|r| r.rotated_at),
            });
            jobs.push((g.guest_ref().expect("guest"), g.status == "running", ip4, ip6, catalog_user));
        }
        let nodes = sh.nodes();
        for n in &nodes {
            rows.push(BulkRow {
                include: true,
                kind: TargetKind::Node,
                vmid: None,
                node: n.clone(),
                name: n.clone(),
                pool: None,
                tags: vec![],
                host: String::new(),
                user: "root".into(),
                keys: vec![],
                rotated: None,
            });
        }
        let (c, host, count) = (sh.client.clone(), sh.profile.host.clone(), nodes.len());
        let loading = Fetch::spawn(async move {
            use futures_util::StreamExt;
            let results: Vec<(u64, resolve::Resolved)> = futures_util::stream::iter(jobs)
                .map(|(g, running, ip4, ip6, cu)| {
                    let c = c.clone();
                    async move { (g.vmid, resolve::resolve_guest(&c, &g, running, ip4, ip6, cu).await) }
                })
                .buffer_unordered(8)
                .collect()
                .await;
            let st = c.get_list("/cluster/status").await.unwrap_or_default();
            let node_hosts = nodes.iter().filter_map(|n| resolve::node_host(&st, n, &host, count).map(|h| (n.clone(), h))).collect();
            Ok((results.into_iter().collect(), node_hosts))
        });
        Self { server_id: sh.server_id.clone(), rows, include_nodes: false, filter: String::new(), loading, filled: false }
    }
}

// ---- Settings ------------------------------------------------------------------------

/// The "SSHSpan" section at the end of Settings > Preferences.
pub fn settings_section(ui: &mut egui::Ui, s: &mut Settings) {
    let p = theme::p();
    widgets::section(ui, "SSHSpan");
    widgets::muted(
        ui,
        "SSHSpan is an SSH client and key manager. With it, guests and nodes open in one click and their host keys are checked \
         against the ones PMXSpan read. PMXSpan works the same without it.",
    );
    ui.add_space(4.0);
    egui::Grid::new("sshspan-settings").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
        ui.label("SSHSpan actions");
        ui.checkbox(&mut s.sshspan_enabled, "show them (off hides every SSHSpan button)");
        ui.end_row();
        ui.label("Status");
        ui.horizontal(|ui| {
            match sshspan::status(&s.sshspan_path) {
                None => {
                    ui.spinner();
                    widgets::muted(ui, "Looking for SSHSpan…");
                }
                Some(d) => {
                    let color = if d.available() { theme::ok() } else { p.muted };
                    ui.label(RichText::new(d.describe()).color(color));
                    if let Some(e) = &d.user_path_error {
                        ui.label(RichText::new(format!("(path below: {e})")).color(theme::err()));
                    }
                }
            }
            if ui.small_button("Check again").clicked() {
                sshspan::recheck();
            }
        });
        ui.end_row();
        ui.label("Path to SSHSpan");
        ui.vertical(|ui| {
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut s.sshspan_path).desired_width(420.0).hint_text(if cfg!(windows) {
                    r"C:\Program Files\SSHSpan\sshspan.exe"
                } else {
                    "/usr/bin/sshspan"
                }));
                if ui.button("Browse…").clicked()
                    && let Some(f) = rfd::FileDialog::new().pick_file()
                {
                    s.sshspan_path = f.display().to_string();
                }
                if !s.sshspan_path.is_empty() && ui.small_button("Clear").clicked() {
                    s.sshspan_path.clear();
                }
            });
            if s.sshspan_path.trim().is_empty() {
                widgets::muted(
                    ui,
                    "Empty: PMXSpan looks for it (PATH, the usual install folders) and asks once before starting what it found.",
                );
            } else {
                match sshspan::validate_user_path(&s.sshspan_path) {
                    Ok(_) => {
                        ui.label(RichText::new(format!("{} usable", theme::icon::CHECK)).size(12.0).color(theme::ok()));
                    }
                    Err(e) => {
                        ui.label(RichText::new(e).size(12.0).color(theme::err()));
                    }
                }
            }
            if !s.sshspan_approved_path.is_empty() {
                ui.horizontal(|ui| {
                    widgets::muted(ui, format!("Confirmed earlier: {}", s.sshspan_approved_path));
                    if ui.small_button("Forget").on_hover_text("Ask again before starting it").clicked() {
                        s.sshspan_approved_path.clear();
                    }
                });
            }
        });
        ui.end_row();
    });
}
