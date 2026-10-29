//! Application shell: vault unlock, server connection, navigation, and the
//! periodic cluster refresh that feeds every view.

mod access;
mod charts;
mod cluster;
mod create;
mod dashboard;
mod guest;
mod guests;
pub mod icon;
mod ipam_view;
mod jobs_panel;
mod node;
mod servers;
mod settings;
mod storage;
mod tasks;
pub mod theme;
mod unlock;
mod widgets;

use crate::api::types::{GuestKind, Resource};
use crate::api::{LoginOutcome, PveClient};
use crate::config::{AppConfig, AppPaths, ServerProfile, Settings, ThemePref};
use crate::ipam::IpamDb;
use crate::jobs::{JobCtx, JobStatus, Jobs};
use crate::rt::Fetch;
use crate::vault::Vault;
use egui::RichText;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq)]
pub enum Nav {
    Dashboard,
    Guests,
    Guest(GuestKind, u64),
    Node(String),
    Storage,
    Tasks,
    Cluster,
    Access,
    Ipam,
    Create,
    Servers,
    Settings,
}

impl Nav {
    fn needs_session(&self) -> bool {
        !matches!(self, Nav::Ipam | Nav::Servers | Nav::Settings)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ToastLevel {
    Info,
    Ok,
    Error,
}

pub enum Action {
    Nav(Nav),
    Toast(ToastLevel, String),
    Refresh,
    OpenCreate(Box<create::Prefill>),
    Connect(String),
    Disconnect,
    ConfigChanged,
}

#[derive(Default)]
pub struct Actions(pub Vec<Action>);

impl Actions {
    pub fn nav(&mut self, n: Nav) {
        self.0.push(Action::Nav(n));
    }
    pub fn toast_ok(&mut self, s: impl Into<String>) {
        self.0.push(Action::Toast(ToastLevel::Ok, s.into()));
    }
    pub fn toast_err(&mut self, s: impl Into<String>) {
        self.0.push(Action::Toast(ToastLevel::Error, s.into()));
    }
    pub fn toast(&mut self, s: impl Into<String>) {
        self.0.push(Action::Toast(ToastLevel::Info, s.into()));
    }
    pub fn refresh(&mut self) {
        self.0.push(Action::Refresh);
    }
}

/// Everything a connected view needs. Cheap to clone.
#[derive(Clone)]
pub struct Shared {
    pub client: PveClient,
    pub server_id: String,
    pub profile: ServerProfile,
    pub jobs: Jobs,
    pub settings: Settings,
    pub ipam: Arc<Mutex<IpamDb>>,
    pub resources: Arc<Vec<Resource>>,
}

impl Shared {
    pub fn env(&self) -> crate::ops::create::Env {
        crate::ops::create::Env {
            client: self.client.clone(),
            server_id: self.server_id.clone(),
            settings: self.settings.clone(),
            ipam: self.ipam.clone(),
        }
    }

    pub fn nodes(&self) -> Vec<String> {
        let mut v: Vec<String> = self.resources.iter().filter(|r| r.rtype == "node").map(|r| r.node.clone()).collect();
        v.sort();
        v
    }

    pub fn online_nodes(&self) -> Vec<String> {
        let mut v: Vec<String> =
            self.resources.iter().filter(|r| r.rtype == "node" && r.status == "online").map(|r| r.node.clone()).collect();
        v.sort();
        v
    }

    pub fn guests(&self) -> impl Iterator<Item = &Resource> {
        self.resources.iter().filter(|r| r.kind().is_some())
    }

    pub fn find_guest(&self, kind: GuestKind, vmid: u64) -> Option<&Resource> {
        self.guests().find(|r| r.kind() == Some(kind) && r.vmid == vmid)
    }

    /// Storages on a node that accept `content`.
    pub fn storages_with(&self, node: &str, content: &str) -> Vec<String> {
        let mut v: Vec<String> = self
            .resources
            .iter()
            .filter(|r| r.rtype == "storage" && r.node == node && r.has_content(content) && r.status == "available")
            .map(|r| r.storage.clone())
            .collect();
        v.sort();
        v.dedup();
        v
    }

    pub fn templates(&self, kind: GuestKind) -> Vec<&Resource> {
        let mut v: Vec<&Resource> = self.guests().filter(|r| r.template && r.kind() == Some(kind)).collect();
        v.sort_by_key(|r| r.vmid);
        v
    }

    pub fn pools(&self) -> Vec<String> {
        let mut v: Vec<String> = self.resources.iter().filter(|r| r.rtype == "pool").map(|r| r.pool.clone()).collect();
        v.sort();
        v
    }

    pub fn spawn<F, Fut>(&self, title: impl Into<String>, f: F) -> u64
    where
        F: FnOnce(JobCtx) -> Fut + Send + 'static,
        Fut: Future<Output = anyhow::Result<String>> + Send + 'static,
    {
        self.jobs.spawn(title, f)
    }

    /// Spawns an async closure over a client clone into a [`Fetch`].
    pub fn fetch<T, F, Fut>(&self, f: F) -> Fetch<T>
    where
        T: Send + 'static,
        F: FnOnce(PveClient) -> Fut,
        Fut: Future<Output = anyhow::Result<T>> + Send + 'static,
    {
        Fetch::spawn(f(self.client.clone()))
    }
}

struct Session {
    shared: Shared,
    resources_fetch: Fetch<Vec<Resource>>,
    last_fetch: Instant,
    last_gen: u64,
    error: Option<String>,
    loaded_once: bool,
    dashboard: dashboard::DashboardView,
    guests: guests::GuestsView,
    guest_views: HashMap<(GuestKind, u64), guest::GuestView>,
    node_views: HashMap<String, node::NodeView>,
    storage: storage::StorageView,
    tasks: tasks::TasksView,
    cluster: cluster::ClusterView,
    access: access::AccessView,
    create: create::CreateView,
}

impl Session {
    fn new(shared: Shared) -> Self {
        let mut s = Self {
            shared,
            resources_fetch: Fetch::default(),
            last_fetch: Instant::now(),
            last_gen: crate::rt::generation(),
            error: None,
            loaded_once: false,
            dashboard: Default::default(),
            guests: Default::default(),
            guest_views: HashMap::new(),
            node_views: HashMap::new(),
            storage: Default::default(),
            tasks: Default::default(),
            cluster: Default::default(),
            access: Default::default(),
            create: Default::default(),
        };
        s.refresh();
        s
    }

    fn refresh(&mut self) {
        if self.resources_fetch.is_loading() {
            return;
        }
        self.last_fetch = Instant::now();
        self.last_gen = crate::rt::generation();
        self.resources_fetch = self.shared.fetch(|c| async move {
            let list = c.get_list("/cluster/resources").await?;
            Ok(list.iter().map(Resource::from_value).collect())
        });
    }

    fn pump(&mut self, every: Duration) {
        if let Some(res) = self.resources_fetch.take() {
            match res {
                Ok(list) => {
                    self.shared.resources = Arc::new(list);
                    self.error = None;
                    self.loaded_once = true;
                }
                Err(e) => self.error = Some(e),
            }
        }
        if self.last_fetch.elapsed() >= every || self.last_gen != crate::rt::generation() {
            self.refresh();
        }
    }
}

struct Toast {
    level: ToastLevel,
    text: String,
    at: Instant,
}

struct Connecting {
    profile_id: String,
    fetch: Fetch<LoginOutcome>,
}

pub struct App {
    paths: AppPaths,
    config: AppConfig,
    vault: Option<Vault>,
    unlock: unlock::UnlockView,
    ipam: Arc<Mutex<IpamDb>>,
    jobs: Jobs,
    session: Option<Session>,
    connecting: Option<Connecting>,
    /// (profile id, code being typed, focus requested)
    totp: Option<(String, String, bool)>,
    nav: Nav,
    servers: servers::ServersView,
    settings: settings::SettingsView,
    ipam_view: ipam_view::IpamView,
    jobs_panel: jobs_panel::JobsPanel,
    toasts: Vec<Toast>,
    job_states: HashMap<u64, JobStatus>,
    sidebar_filter: String,
    applied_theme: Option<(ThemePref, bool)>,
    applied_scale: f32,
}

impl App {
    pub fn new(paths: AppPaths) -> Self {
        let config = AppConfig::load(&paths.config_file());
        let ipam = Arc::new(Mutex::new(IpamDb::load(&paths.ipam_file())));
        let vault = if Vault::exists(&paths.vault_file()) { Vault::open(&paths.vault_file()).ok() } else { None };
        let mut app = Self {
            unlock: unlock::UnlockView::new(vault.as_ref()),
            paths,
            config,
            vault,
            ipam,
            jobs: Jobs::default(),
            session: None,
            connecting: None,
            totp: None,
            nav: Nav::Servers,
            servers: Default::default(),
            settings: Default::default(),
            ipam_view: Default::default(),
            jobs_panel: Default::default(),
            toasts: Vec::new(),
            job_states: HashMap::new(),
            sidebar_filter: String::new(),
            applied_theme: None,
            applied_scale: 0.0,
        };
        // An unprotected vault opens without prompting.
        if let Some(v) = app.vault.as_mut()
            && !v.is_protected()
        {
            let _ = v.unlock("");
        }
        app
    }

    fn vault_ready(&self) -> bool {
        self.vault.as_ref().map(Vault::is_unlocked).unwrap_or(false)
    }

    fn save_config(&mut self) {
        if let Err(e) = self.config.save(&self.paths.config_file()) {
            self.toast(ToastLevel::Error, format!("saving config failed: {e:#}"));
        }
    }

    fn toast(&mut self, level: ToastLevel, text: impl Into<String>) {
        self.toasts.push(Toast { level, text: text.into(), at: Instant::now() });
    }

    fn connect(&mut self, profile_id: &str, otp: Option<String>) {
        let Some(profile) = self.config.profile(profile_id).cloned() else { return };
        let secret = self.vault.as_ref().and_then(|v| v.get(&profile.secret_key())).unwrap_or_default();
        self.session = None;
        self.config.last_profile = Some(profile.id.clone());
        self.save_config();
        tracing::info!("connecting to {} ({})", profile.display_name(), profile.web_url());
        self.connecting = Some(Connecting {
            profile_id: profile.id.clone(),
            fetch: Fetch::spawn(async move { PveClient::login(&profile, &secret, otp.as_deref()).await }),
        });
    }

    fn pump_connecting(&mut self) {
        let Some(c) = self.connecting.as_mut() else { return };
        let Some(res) = c.fetch.take() else { return };
        let profile_id = c.profile_id.clone();
        self.connecting = None;
        let Some(profile) = self.config.profile(&profile_id).cloned() else { return };
        match res {
            Ok(LoginOutcome::Ok(client)) => {
                self.toast(ToastLevel::Ok, format!("Connected to {}", profile.display_name()));
                let shared = Shared {
                    client,
                    server_id: profile.id.clone(),
                    profile,
                    jobs: self.jobs.clone(),
                    settings: self.config.settings.clone(),
                    ipam: self.ipam.clone(),
                    resources: Arc::new(Vec::new()),
                };
                self.session = Some(Session::new(shared));
                if !self.nav.needs_session() || matches!(self.nav, Nav::Servers) {
                    self.nav = Nav::Dashboard;
                }
            }
            Ok(LoginOutcome::NeedTotp) => {
                self.totp = Some((profile_id, String::new(), false));
            }
            Err(e) => {
                self.servers.last_error = Some(e.clone());
                self.toast(ToastLevel::Error, format!("Connection to {} failed: {e}", profile.display_name()));
                self.nav = Nav::Servers;
            }
        }
    }

    fn apply_theme(&mut self, ctx: &egui::Context) {
        let pref = self.config.settings.theme;
        let dark = match pref {
            ThemePref::Dark => true,
            ThemePref::Light => false,
            ThemePref::System => ctx.system_theme().map(|t| t == egui::Theme::Dark).unwrap_or(true),
        };
        if self.applied_theme != Some((pref, dark)) {
            theme::set_dark(ctx, dark);
            self.applied_theme = Some((pref, dark));
        }
        let scale = self.config.settings.ui_scale.clamp(0.6, 2.5);
        if (self.applied_scale - scale).abs() > f32::EPSILON {
            ctx.set_zoom_factor(scale);
            self.applied_scale = scale;
        }
    }

    fn watch_jobs(&mut self) {
        for job in self.jobs.snapshot() {
            let st = job.state.lock();
            let prev = self.job_states.insert(job.id, st.status);
            if prev == Some(JobStatus::Running) && st.status != JobStatus::Running {
                let (level, text) = match st.status {
                    JobStatus::Done => {
                        (ToastLevel::Ok, format!("✔ {}", st.result.clone().filter(|r| !r.is_empty()).unwrap_or(job.title.clone())))
                    }
                    _ => (
                        ToastLevel::Error,
                        format!("✖ {}: {}", job.title, st.result.clone().unwrap_or_default().lines().next().unwrap_or("")),
                    ),
                };
                drop(st);
                self.toast(level, text);
            }
        }
    }

    fn process(&mut self, actions: Actions) {
        for a in actions.0 {
            match a {
                Action::Nav(n) => self.nav = n,
                Action::Toast(l, t) => self.toast(l, t),
                Action::Refresh => {
                    if let Some(s) = self.session.as_mut() {
                        s.refresh();
                    }
                }
                Action::OpenCreate(prefill) => {
                    if let Some(s) = self.session.as_mut() {
                        s.create = create::CreateView::with_prefill(*prefill, &s.shared);
                    }
                    self.nav = Nav::Create;
                }
                Action::Connect(id) => self.connect(&id, None),
                Action::Disconnect => {
                    self.session = None;
                    self.nav = Nav::Servers;
                }
                Action::ConfigChanged => {
                    self.save_config();
                    if let Some(s) = self.session.as_mut() {
                        s.shared.settings = self.config.settings.clone();
                        if let Some(p) = self.config.profile(&s.shared.server_id) {
                            s.shared.profile = p.clone();
                        }
                    }
                }
            }
        }
    }

    fn top_bar(&mut self, ui: &mut egui::Ui, actions: &mut Actions) {
        egui::Panel::top("top").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("ProxmoxPanel").strong().color(theme::ACCENT).size(16.0));
                ui.separator();
                let current = self
                    .session
                    .as_ref()
                    .map(|s| s.shared.profile.display_name())
                    .or_else(|| self.connecting.as_ref().and_then(|c| self.config.profile(&c.profile_id)).map(|p| p.display_name()))
                    .unwrap_or_else(|| "Not connected".into());
                egui::ComboBox::from_id_salt("server-switch").selected_text(current).width(200.0).show_ui(ui, |ui| {
                    for p in &self.config.profiles {
                        let active = self.session.as_ref().map(|s| s.shared.server_id == p.id).unwrap_or(false);
                        if ui.selectable_label(active, p.display_name()).clicked() && !active {
                            actions.0.push(Action::Connect(p.id.clone()));
                        }
                    }
                    ui.separator();
                    if ui.selectable_label(false, "Manage servers…").clicked() {
                        actions.nav(Nav::Servers);
                    }
                });
                if self.connecting.is_some() {
                    ui.spinner();
                    ui.label("Connecting…");
                } else if let Some(s) = self.session.as_mut() {
                    if s.resources_fetch.is_loading() && !s.loaded_once {
                        ui.spinner();
                    }
                    ui.label(RichText::new(format!("● {}", s.shared.client.userid())).color(theme::OK))
                        .on_hover_text(s.shared.profile.web_url());
                    if let Some(e) = &s.error {
                        ui.label(RichText::new("⚠ refresh failed").color(theme::ERR)).on_hover_text(e);
                    }
                    if ui.button("⟳").on_hover_text("Refresh now").clicked() {
                        actions.refresh();
                    }
                    if ui.button("Disconnect").clicked() {
                        actions.0.push(Action::Disconnect);
                    }
                    ui.separator();
                    if ui.button(RichText::new("＋ Create guest").strong()).clicked() {
                        actions.0.push(Action::OpenCreate(Box::default()));
                    }
                    if ui.button("Open web UI").on_hover_text("Open the Proxmox web interface in your browser").clicked() {
                        let _ = webbrowser::open(&s.shared.profile.web_url());
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let running = self.jobs.running_count();
                    let label = if running > 0 { format!("Jobs ({running} running)") } else { "Jobs".into() };
                    if ui.selectable_label(self.jobs_panel.open, label).clicked() {
                        self.jobs_panel.open = !self.jobs_panel.open;
                    }
                    if running > 0 {
                        ui.spinner();
                    }
                });
            });
        });
    }

    fn sidebar(&mut self, ui: &mut egui::Ui, actions: &mut Actions) {
        egui::Panel::left("nav").default_size(250.0).min_size(180.0).show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                let connected = self.session.is_some();
                ui.add_space(4.0);
                ui.label(RichText::new("DATACENTER").small().color(theme::MUTED));
                for (label, nav) in [
                    ("Dashboard", Nav::Dashboard),
                    ("Guests", Nav::Guests),
                    ("Storage", Nav::Storage),
                    ("Tasks", Nav::Tasks),
                    ("Backup · Replication · HA", Nav::Cluster),
                    ("Users · Pools · Tenants", Nav::Access),
                ] {
                    let sel = self.nav == nav;
                    if ui.add_enabled(connected, egui::Button::selectable(sel, label)).clicked() {
                        actions.nav(nav);
                    }
                }
                ui.add_space(6.0);
                ui.label(RichText::new("LOCAL").small().color(theme::MUTED));
                for (label, nav) in [("IP address management", Nav::Ipam), ("Servers", Nav::Servers), ("Settings & logs", Nav::Settings)] {
                    if ui.selectable_label(self.nav == nav, label).clicked() {
                        actions.nav(nav);
                    }
                }
                if let Some(s) = &self.session {
                    ui.add_space(6.0);
                    ui.separator();
                    ui.add(egui::TextEdit::singleline(&mut self.sidebar_filter).hint_text("Filter guests…"));
                    let filter = self.sidebar_filter.to_lowercase();
                    let mut nodes: Vec<&Resource> = s.shared.resources.iter().filter(|r| r.rtype == "node").collect();
                    nodes.sort_by(|a, b| a.node.cmp(&b.node));
                    for n in nodes {
                        let mut guests: Vec<&Resource> = s
                            .shared
                            .guests()
                            .filter(|g| g.node == n.node)
                            .filter(|g| {
                                filter.is_empty()
                                    || g.name.to_lowercase().contains(&filter)
                                    || g.vmid.to_string().contains(&filter)
                                    || g.tags.to_lowercase().contains(&filter)
                            })
                            .collect();
                        guests.sort_by_key(|g| g.vmid);
                        let header = egui::CollapsingHeader::new(RichText::new(format!("{}  ({})", n.node, guests.len())).strong())
                            .id_salt(format!("node-{}", n.node))
                            .default_open(true)
                            .show(ui, |ui| {
                                for g in guests {
                                    let Some(kind) = g.kind() else { continue };
                                    let sel = self.nav == Nav::Guest(kind, g.vmid);
                                    ui.horizontal(|ui| {
                                        widgets::status_dot(ui, if g.template { "" } else { &g.status });
                                        let mut text = format!("{} {}", g.vmid, g.display_name());
                                        if g.template {
                                            text.push_str("  [template]");
                                        }
                                        if !g.lock.is_empty() {
                                            text.push_str(&format!("  🔒{}", g.lock));
                                        }
                                        let label =
                                            if kind == GuestKind::Lxc { RichText::new(text).italics() } else { RichText::new(text) };
                                        if ui
                                            .selectable_label(sel, label)
                                            .on_hover_text(format!("{} on {}", kind.label(), g.node))
                                            .clicked()
                                        {
                                            actions.nav(Nav::Guest(kind, g.vmid));
                                        }
                                    });
                                }
                            });
                        let r = header.header_response;
                        if r.secondary_clicked() || r.double_clicked() {
                            actions.nav(Nav::Node(n.node.clone()));
                        }
                        r.on_hover_text(format!("{} — double-click to open the node view", n.status));
                    }
                }
            });
        });
    }

    fn central(&mut self, ui: &mut egui::Ui, actions: &mut Actions) {
        if self.nav.needs_session() && self.session.is_none() {
            self.nav = Nav::Servers;
        }
        match self.nav.clone() {
            Nav::Servers => {
                let vault = self.vault.as_mut().expect("vault ready");
                let connected = self.session.as_ref().map(|s| s.shared.server_id.clone());
                self.servers.ui(ui, &mut self.config, vault, connected.as_deref(), self.connecting.is_some(), actions);
            }
            Nav::Settings => {
                let vault = self.vault.as_mut().expect("vault ready");
                self.settings.ui(ui, &mut self.config, vault, &self.paths, actions);
            }
            Nav::Ipam => {
                let shared = self.session.as_ref().map(|s| s.shared.clone());
                self.ipam_view.ui(ui, &self.ipam, &self.config, shared.as_ref(), actions);
            }
            nav => {
                let Some(s) = self.session.as_mut() else { return };
                let shared = s.shared.clone();
                if !s.loaded_once {
                    if let Some(e) = &s.error {
                        widgets::error_box(ui, &format!("Cannot read cluster resources: {e}"));
                    } else {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Loading cluster…");
                        });
                    }
                    return;
                }
                match nav {
                    Nav::Dashboard => s.dashboard.ui(ui, &shared, actions),
                    Nav::Guests => s.guests.ui(ui, &shared, actions),
                    Nav::Guest(kind, vmid) => {
                        let view = s.guest_views.entry((kind, vmid)).or_insert_with(|| guest::GuestView::new(kind, vmid));
                        view.ui(ui, &shared, actions);
                    }
                    Nav::Node(name) => {
                        let view = s.node_views.entry(name.clone()).or_insert_with(|| node::NodeView::new(name));
                        view.ui(ui, &shared, actions);
                    }
                    Nav::Storage => s.storage.ui(ui, &shared, actions),
                    Nav::Tasks => s.tasks.ui(ui, &shared, actions),
                    Nav::Cluster => s.cluster.ui(ui, &shared, actions),
                    Nav::Access => s.access.ui(ui, &shared, actions),
                    Nav::Create => s.create.ui(ui, &shared, actions),
                    Nav::Servers | Nav::Settings | Nav::Ipam => {}
                }
            }
        }
    }

    fn totp_window(&mut self, ctx: &egui::Context) {
        let mut submit = None;
        let mut cancel = false;
        if let Some((pid, code, focused)) = self.totp.as_mut() {
            egui::Window::new("Two-factor authentication")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label("This account requires a TOTP code.");
                    let r = ui.add(egui::TextEdit::singleline(code).hint_text("123456").desired_width(120.0));
                    if !*focused {
                        r.request_focus();
                        *focused = true;
                    }
                    ui.horizontal(|ui| {
                        if ui.button("Verify").clicked() || (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))) {
                            submit = Some((pid.clone(), code.clone()));
                        }
                        if ui.button("Cancel").clicked() {
                            cancel = true;
                        }
                    });
                });
        }
        if let Some((pid, code)) = submit {
            self.totp = None;
            self.connect(&pid, Some(code));
        } else if cancel {
            self.totp = None;
        }
    }

    fn toasts_ui(&mut self, ctx: &egui::Context) {
        self.toasts.retain(|t| t.at.elapsed() < Duration::from_secs(if t.level == ToastLevel::Error { 12 } else { 5 }));
        if self.toasts.is_empty() {
            return;
        }
        ctx.request_repaint_after(Duration::from_millis(500));
        egui::Area::new(egui::Id::unique("toasts")).anchor(egui::Align2::RIGHT_BOTTOM, [-12.0, -12.0]).order(egui::Order::Foreground).show(
            ctx,
            |ui| {
                let mut dismiss = None;
                for (i, t) in self.toasts.iter().enumerate().rev().take(6) {
                    let color = match t.level {
                        ToastLevel::Ok => theme::OK,
                        ToastLevel::Error => theme::ERR,
                        ToastLevel::Info => theme::INFO,
                    };
                    let r = egui::Frame::popup(ui.style())
                        .stroke(egui::Stroke::new(1.0, color))
                        .show(ui, |ui| {
                            ui.set_max_width(420.0);
                            ui.label(RichText::new(&t.text).color(color));
                        })
                        .response
                        .interact(egui::Sense::click());
                    if r.clicked() {
                        dismiss = Some(i);
                    }
                }
                if let Some(i) = dismiss {
                    self.toasts.remove(i);
                }
            },
        );
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.apply_theme(&ctx);

        if !self.vault_ready() {
            egui::CentralPanel::default().show(ui, |ui| {
                if self.unlock.ui(ui, &self.paths, &mut self.vault)
                    && let Some(last) = self.config.last_profile.clone()
                    && self.config.profile(&last).is_some()
                {
                    self.connect(&last, None);
                }
            });
            return;
        }

        self.pump_connecting();
        self.watch_jobs();
        let every = Duration::from_secs(self.config.settings.refresh_secs.max(2));
        if let Some(s) = self.session.as_mut() {
            s.pump(every);
        }
        ctx.request_repaint_after(every);

        let mut actions = Actions::default();
        self.top_bar(ui, &mut actions);
        if self.jobs_panel.open {
            let jobs = self.jobs.clone();
            egui::Panel::bottom("jobs").resizable(true).default_size(220.0).show(ui, |ui| {
                self.jobs_panel.ui(ui, &jobs);
            });
        }
        self.sidebar(ui, &mut actions);
        egui::CentralPanel::default().show(ui, |ui| {
            self.central(ui, &mut actions);
        });
        self.totp_window(&ctx);
        self.toasts_ui(&ctx);
        self.process(actions);
    }

    fn on_exit(&mut self) {
        let _ = self.config.save(&self.paths.config_file());
    }
}
