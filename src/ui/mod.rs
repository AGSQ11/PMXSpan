//! Application shell: vault unlock, server connection, navigation, and the
//! periodic cluster refresh that feeds every view.

mod access;
mod catalog_view;
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
use crate::ops::catalog::CatalogStore;
use crate::rt::Fetch;
use crate::vault::Vault;
use egui::RichText;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
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
    Catalog,
    Create,
    Servers,
    Settings,
}

impl Nav {
    fn needs_session(&self) -> bool {
        !matches!(self, Nav::Ipam | Nav::Catalog | Nav::Servers | Nav::Settings)
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
    pub catalog: Arc<Mutex<CatalogStore>>,
    pub resources: Arc<Vec<Resource>>,
}

impl Shared {
    pub fn env(&self) -> crate::ops::create::Env {
        crate::ops::create::Env {
            client: self.client.clone(),
            server_id: self.server_id.clone(),
            profile: self.profile.clone(),
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
    catalog: Arc<Mutex<CatalogStore>>,
    jobs: Jobs,
    session: Option<Session>,
    connecting: Option<Connecting>,
    /// (profile id, code being typed, focus requested)
    totp: Option<(String, String, bool)>,
    nav: Nav,
    servers: servers::ServersView,
    settings: settings::SettingsView,
    ipam_view: ipam_view::IpamView,
    catalog_view: catalog_view::CatalogView,
    jobs_panel: jobs_panel::JobsPanel,
    toasts: Vec<Toast>,
    job_states: HashMap<u64, JobStatus>,
    sidebar_filter: String,
    search: String,
    collapsed_nodes: HashSet<String>,
    applied_theme: Option<(ThemePref, bool)>,
    applied_scale: f32,
}

impl App {
    pub fn new(paths: AppPaths) -> Self {
        let config = AppConfig::load(&paths.config_file());
        let ipam = Arc::new(Mutex::new(IpamDb::load(&paths.ipam_file())));
        let catalog = Arc::new(Mutex::new(CatalogStore::load(&paths.catalog_file())));
        let vault = if Vault::exists(&paths.vault_file()) { Vault::open(&paths.vault_file()).ok() } else { None };
        let mut app = Self {
            unlock: unlock::UnlockView::new(vault.as_ref()),
            paths,
            config,
            vault,
            ipam,
            catalog,
            jobs: Jobs::default(),
            session: None,
            connecting: None,
            totp: None,
            nav: Nav::Servers,
            servers: Default::default(),
            settings: Default::default(),
            ipam_view: Default::default(),
            catalog_view: Default::default(),
            jobs_panel: Default::default(),
            toasts: Vec::new(),
            job_states: HashMap::new(),
            sidebar_filter: String::new(),
            search: String::new(),
            collapsed_nodes: HashSet::new(),
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
                    catalog: self.catalog.clone(),
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

    /// Breadcrumb for the current view: (context, title).
    fn crumbs(&self) -> (String, String) {
        let cluster = self.session.as_ref().map(|s| s.shared.profile.display_name()).unwrap_or_else(|| "PMXSpan".into());
        let title = match &self.nav {
            Nav::Dashboard => "Dashboard".to_string(),
            Nav::Guests => "Guests".into(),
            Nav::Guest(kind, vmid) => {
                let name = self.session.as_ref().and_then(|s| s.shared.find_guest(*kind, *vmid)).map(|r| r.display_name());
                return (format!("{cluster}  ›  Guests"), name.unwrap_or_else(|| format!("{} {vmid}", kind.label())));
            }
            Nav::Node(n) => return (format!("{cluster}  ›  Nodes"), n.clone()),
            Nav::Storage => "Storage".into(),
            Nav::Tasks => "Tasks".into(),
            Nav::Cluster => "Backup · Replication · HA".into(),
            Nav::Access => "Users · Pools · Tenants".into(),
            Nav::Create => "Create guest".into(),
            Nav::Ipam => return ("Local".into(), "IP address management".into()),
            Nav::Catalog => return ("Local".into(), "OS image catalog".into()),
            Nav::Servers => return ("Local".into(), "Servers".into()),
            Nav::Settings => return ("Local".into(), "Settings & logs".into()),
        };
        (cluster, title)
    }

    fn top_bar(&mut self, ui: &mut egui::Ui, actions: &mut Actions) {
        let p = theme::p();
        let frame = egui::Frame::new().fill(p.side).inner_margin(egui::Margin::symmetric(24, 10));
        egui::Panel::top("top").frame(frame).show(ui, |ui| {
            ui.horizontal(|ui| {
                let (context, title) = self.crumbs();
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    ui.label(RichText::new(context).size(12.0).color(p.muted));
                    ui.label(theme::bold(title, 18.0));
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.session.is_some() && widgets::primary_button(ui, format!("{}  Create guest", theme::icon::PLUS)).clicked() {
                        actions.0.push(Action::OpenCreate(Box::default()));
                    }
                    let (glyph, tip) = if theme::is_dark() {
                        (theme::icon::SUN, "Switch to the light theme")
                    } else {
                        (theme::icon::MOON, "Switch to the dark theme")
                    };
                    if widgets::icon_button(ui, glyph, "").on_hover_text(tip).clicked() {
                        self.config.settings.theme = if theme::is_dark() { ThemePref::Light } else { ThemePref::Dark };
                        actions.0.push(Action::ConfigChanged);
                    }
                    let running = self.jobs.running_count();
                    let label = if running > 0 {
                        format!("{}  Jobs · {running} running", theme::icon::TASKS)
                    } else {
                        format!("{}  Jobs", theme::icon::TASKS)
                    };
                    let jobs = ui.add(
                        egui::Button::new(label)
                            .selected(self.jobs_panel.open)
                            .corner_radius(egui::CornerRadius::same(9))
                            .min_size(egui::vec2(0.0, 32.0)),
                    );
                    if jobs.clicked() {
                        self.jobs_panel.open = !self.jobs_panel.open;
                    }
                    if let Some(s) = self.session.as_mut() {
                        if widgets::icon_button(ui, theme::icon::REFRESH, "").on_hover_text("Refresh now").clicked() {
                            actions.refresh();
                        }
                        if s.resources_fetch.is_loading() {
                            ui.spinner();
                        }
                        if let Some(e) = &s.error {
                            ui.label(RichText::new(format!("{} refresh failed", theme::icon::WARNING)).color(theme::err()))
                                .on_hover_text(e);
                        }
                        self.search_box(ui, actions);
                    } else if self.connecting.is_some() {
                        ui.spinner();
                        ui.label("Connecting…");
                    }
                });
            });
        });
    }

    /// Global search: guests by name, VMID, tag or IPAM address.
    fn search_box(&mut self, ui: &mut egui::Ui, actions: &mut Actions) {
        let Some(s) = self.session.as_ref() else { return };
        let p = theme::p();
        let r = ui.add(
            egui::TextEdit::singleline(&mut self.search)
                .hint_text(format!("{}  Search guests, IPs, tags…", theme::icon::SEARCH))
                .desired_width(280.0)
                .margin(egui::Margin::symmetric(10, 7)),
        );
        let q = self.search.trim().to_lowercase();
        if q.is_empty() {
            return;
        }
        let ipam = s.shared.ipam.lock();
        let mut hits: Vec<(GuestKind, u64, String, String)> = s
            .shared
            .guests()
            .filter_map(|g| {
                let kind = g.kind()?;
                let ips: Vec<String> = ipam.guest_ips(&s.shared.server_id, g.vmid).into_iter().map(|a| a.ip.clone()).collect();
                let hay = format!("{} {} {} {}", g.vmid, g.name, g.tags, ips.join(" ")).to_lowercase();
                hay.contains(&q).then(|| (kind, g.vmid, g.display_name(), format!("{} {} · {}", kind.label(), g.vmid, g.node)))
            })
            .collect();
        drop(ipam);
        hits.sort_by_key(|h| h.1);
        hits.truncate(8);
        let mut go = None;
        egui::Area::new(egui::Id::unique("search-results"))
            .order(egui::Order::Foreground)
            .fixed_pos(r.rect.left_bottom() + egui::vec2(0.0, 4.0))
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_width(r.rect.width().max(300.0));
                    if hits.is_empty() {
                        ui.label(RichText::new("No matching guests").color(p.muted));
                    }
                    for (kind, vmid, name, meta) in &hits {
                        let resp = ui.add(
                            egui::Button::new(RichText::new(format!("{name}   ")).family(theme::semibold()))
                                .right_text(RichText::new(meta.clone()).color(p.muted).size(12.0))
                                .frame(false)
                                .min_size(egui::vec2(ui.available_width(), 28.0)),
                        );
                        if resp.clicked() {
                            go = Some(Nav::Guest(*kind, *vmid));
                        }
                    }
                    if ui.input(|i| i.key_pressed(egui::Key::Enter))
                        && let Some((kind, vmid, ..)) = hits.first()
                    {
                        go = Some(Nav::Guest(*kind, *vmid));
                    }
                });
            });
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.search.clear();
        }
        if let Some(n) = go {
            self.search.clear();
            actions.nav(n);
        }
    }

    fn sidebar(&mut self, ui: &mut egui::Ui, actions: &mut Actions) {
        let p = theme::p();
        let frame = egui::Frame::new().fill(p.side).inner_margin(egui::Margin::symmetric(12, 14));
        egui::Panel::left("nav").frame(frame).default_size(250.0).min_size(200.0).max_size(380.0).show(ui, |ui| {
            // Brand
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(egui::vec2(28.0, 28.0), egui::Sense::hover());
                ui.painter().rect_filled(rect, egui::CornerRadius::same(8), p.accent);
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    theme::icon::HARD_DRIVES,
                    egui::FontId::proportional(17.0),
                    p.on_accent,
                );
                ui.label(theme::bold("PMXSpan", 15.5));
            });
            ui.add_space(10.0);
            self.cluster_card(ui, actions);
            ui.add_space(8.0);

            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                let connected = self.session.is_some();
                ui.spacing_mut().item_spacing.y = 2.0;
                let group = |ui: &mut egui::Ui, text: &str| {
                    ui.add_space(8.0);
                    ui.label(RichText::new(format!("  {text}")).size(11.0).family(theme::semibold()).color(p.muted));
                    ui.add_space(2.0);
                };
                group(ui, "DATACENTER");
                use theme::icon as i;
                for (glyph, label, nav) in [
                    (i::DASHBOARD, "Dashboard", Nav::Dashboard),
                    (i::DESKTOP, "Guests", Nav::Guests),
                    (i::DATABASE, "Storage", Nav::Storage),
                    (i::TASKS, "Tasks", Nav::Tasks),
                    (i::ARCHIVE, "Backup · HA", Nav::Cluster),
                    (i::USERS, "Users · Tenants", Nav::Access),
                ] {
                    if widgets::nav_item(ui, glyph, label, self.nav == nav, connected).clicked() {
                        actions.nav(nav);
                    }
                }
                group(ui, "LOCAL");
                for (glyph, label, nav) in [
                    (i::NETWORK, "IP addresses", Nav::Ipam),
                    (i::STACK, "OS image catalog", Nav::Catalog),
                    (i::SERVER, "Servers", Nav::Servers),
                    (i::GEAR, "Settings", Nav::Settings),
                ] {
                    if widgets::nav_item(ui, glyph, label, self.nav == nav, true).clicked() {
                        actions.nav(nav);
                    }
                }
                if self.session.is_some() {
                    ui.add_space(10.0);
                    ui.separator();
                    ui.add_space(4.0);
                    self.guest_tree(ui, actions);
                }
            });
        });
    }

    /// The connected cluster, with a menu to switch, open the web UI or disconnect.
    fn cluster_card(&mut self, ui: &mut egui::Ui, actions: &mut Actions) {
        let p = theme::p();
        let (title, sub, color) = if let Some(s) = &self.session {
            let quorum = if s.loaded_once { "connected" } else { "loading…" };
            (s.shared.profile.display_name(), format!("{} · {quorum}", s.shared.client.userid()), p.ok)
        } else if let Some(c) = &self.connecting {
            let name = self.config.profile(&c.profile_id).map(|p| p.display_name()).unwrap_or_default();
            (name, "connecting…".to_string(), p.warn)
        } else {
            ("Not connected".to_string(), "choose a server".to_string(), p.stopped)
        };
        let resp = egui::Frame::new()
            .fill(p.surface2)
            .stroke(egui::Stroke::new(1.0, p.border2))
            .corner_radius(egui::CornerRadius::same(10))
            .inner_margin(egui::Margin::symmetric(10, 8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    widgets::dot(ui, color);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        ui.label(RichText::new(title).family(theme::semibold()));
                        ui.label(RichText::new(sub).size(12.0).color(p.muted));
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(RichText::new(theme::icon::CARET_DOWN).color(p.muted));
                    });
                });
            })
            .response
            .interact(egui::Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        egui::Popup::menu(&resp).width(resp.rect.width()).show(|ui| {
            for prof in &self.config.profiles {
                let active = self.session.as_ref().map(|s| s.shared.server_id == prof.id).unwrap_or(false);
                if ui.selectable_label(active, prof.display_name()).clicked() && !active {
                    actions.0.push(Action::Connect(prof.id.clone()));
                }
            }
            ui.separator();
            if let Some(s) = &self.session {
                if ui.button(format!("{}  Open web UI", theme::icon::EXTERNAL)).clicked() {
                    let _ = webbrowser::open(&s.shared.profile.web_url());
                }
                if ui.button(format!("{}  Disconnect", theme::icon::SIGN_OUT)).clicked() {
                    actions.0.push(Action::Disconnect);
                }
            }
            if ui.button(format!("{}  Manage servers…", theme::icon::SERVER)).clicked() {
                actions.nav(Nav::Servers);
            }
        });
    }

    fn guest_tree(&mut self, ui: &mut egui::Ui, actions: &mut Actions) {
        let Some(s) = &self.session else { return };
        let p = theme::p();
        ui.add(
            egui::TextEdit::singleline(&mut self.sidebar_filter)
                .hint_text(format!("{}  Filter guests", theme::icon::FUNNEL))
                .desired_width(f32::INFINITY)
                .margin(egui::Margin::symmetric(8, 5)),
        );
        ui.add_space(4.0);
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
            let running = guests.iter().filter(|g| g.status == "running").count();
            let collapsed = self.collapsed_nodes.contains(&n.node);
            let width = ui.available_width();
            let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, 28.0), egui::Sense::click());
            let sel = self.nav == Nav::Node(n.node.clone());
            if sel {
                ui.painter().rect_filled(rect, egui::CornerRadius::same(8), p.nav_on_bg);
            } else if resp.hovered() {
                ui.painter().rect_filled(rect, egui::CornerRadius::same(8), p.surface2);
            }
            let caret_rect = egui::Rect::from_min_size(rect.min, egui::vec2(22.0, rect.height()));
            let painter = ui.painter();
            painter.text(
                caret_rect.center(),
                egui::Align2::CENTER_CENTER,
                if collapsed { theme::icon::CARET_RIGHT } else { theme::icon::CARET_DOWN },
                egui::FontId::proportional(12.0),
                p.muted,
            );
            painter.circle_filled(egui::pos2(rect.left() + 28.0, rect.center().y), 3.5, theme::status_color(&n.status));
            painter.text(
                egui::pos2(rect.left() + 38.0, rect.center().y),
                egui::Align2::LEFT_CENTER,
                &n.node,
                egui::FontId::new(13.5, theme::semibold()),
                p.text,
            );
            painter.text(
                egui::pos2(rect.right() - 8.0, rect.center().y),
                egui::Align2::RIGHT_CENTER,
                format!("{running} / {}", guests.len()),
                egui::FontId::proportional(12.0),
                p.muted,
            );
            let resp = resp.on_hover_text(format!("{}: open the node view", n.status));
            if resp.clicked() {
                let on_caret = resp.interact_pointer_pos().map(|pos| caret_rect.contains(pos)).unwrap_or(false);
                if on_caret {
                    if collapsed {
                        self.collapsed_nodes.remove(&n.node);
                    } else {
                        self.collapsed_nodes.insert(n.node.clone());
                    }
                } else {
                    actions.nav(Nav::Node(n.node.clone()));
                }
            }
            if collapsed {
                continue;
            }
            for g in guests {
                let Some(kind) = g.kind() else { continue };
                let sel = self.nav == Nav::Guest(kind, g.vmid);
                let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, 26.0), egui::Sense::click());
                if sel {
                    ui.painter().rect_filled(rect, egui::CornerRadius::same(7), p.nav_on_bg);
                } else if resp.hovered() {
                    ui.painter().rect_filled(rect, egui::CornerRadius::same(7), p.surface2);
                }
                let painter = ui.painter();
                let dot = if g.template { p.stopped } else { theme::status_color(&g.status) };
                painter.circle_filled(egui::pos2(rect.left() + 28.0, rect.center().y), 3.0, dot);
                let id_galley = painter.layout_no_wrap(g.vmid.to_string(), egui::FontId::monospace(11.5), p.muted);
                let id_w = id_galley.size().x;
                painter.galley(egui::pos2(rect.left() + 38.0, rect.center().y - id_galley.size().y / 2.0), id_galley, p.muted);
                let fg = if sel {
                    p.nav_on_fg
                } else if g.template {
                    p.muted
                } else {
                    p.text2
                };
                let mut name = g.display_name();
                if !g.lock.is_empty() {
                    name.push_str(&format!("  {}", theme::icon::KEY));
                }
                let font = if sel { egui::FontId::new(13.0, theme::semibold()) } else { egui::FontId::proportional(13.0) };
                let clip = ui.painter().with_clip_rect(rect.shrink2(egui::vec2(4.0, 0.0)));
                clip.text(egui::pos2(rect.left() + 46.0 + id_w, rect.center().y), egui::Align2::LEFT_CENTER, name, font, fg);
                let resp = resp
                    .on_hover_text(format!(
                        "{} {} on {} · {}",
                        kind.label(),
                        g.vmid,
                        g.node,
                        if g.template { "template" } else { &g.status }
                    ))
                    .on_hover_cursor(egui::CursorIcon::PointingHand);
                if resp.clicked() {
                    actions.nav(Nav::Guest(kind, g.vmid));
                }
            }
            ui.add_space(4.0);
        }
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
            Nav::Catalog => self.catalog_view.ui(ui, &self.catalog, actions),
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
                    Nav::Servers | Nav::Settings | Nav::Ipam | Nav::Catalog => {}
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
                        ToastLevel::Ok => theme::ok(),
                        ToastLevel::Error => theme::err(),
                        ToastLevel::Info => theme::info(),
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
        self.sidebar(ui, &mut actions);
        self.top_bar(ui, &mut actions);
        if self.jobs_panel.open {
            let jobs = self.jobs.clone();
            let frame = egui::Frame::new().fill(theme::p().side).inner_margin(egui::Margin::symmetric(16, 10));
            egui::Panel::bottom("jobs").frame(frame).resizable(true).default_size(220.0).show(ui, |ui| {
                self.jobs_panel.ui(ui, &jobs);
            });
        }
        let central = egui::Frame::new().fill(theme::p().bg).inner_margin(egui::Margin { left: 28, right: 28, top: 20, bottom: 12 });
        egui::CentralPanel::default().frame(central).show(ui, |ui| {
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
