//! Detail view for one VM or container.

mod agent;
mod backups;
pub mod cloudinit;
mod firewall;
mod hardware;
mod ips;
mod manage;
mod network;
mod snapshots;
mod summary;

pub use hardware::{ostype_label, ostypes};

use super::widgets::{self, Confirm};
use super::{Actions, Shared, theme};
use crate::api::types::{GuestKind, GuestRef, Resource, TaskInfo};
use crate::ops::guest::{self, PowerAction};
use crate::rt::Fetch;
use egui::RichText;
use serde_json::{Map, Value};

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Tab {
    #[default]
    Summary,
    Hardware,
    Network,
    Ips,
    CloudInit,
    Snapshots,
    Backups,
    Firewall,
    Agent,
    Tasks,
    Manage,
}

/// What every tab gets to work with.
pub struct GCtx<'a> {
    pub sh: &'a Shared,
    pub g: GuestRef,
    pub res: &'a Resource,
    pub config: Option<&'a Map<String, Value>>,
}

impl GCtx<'_> {
    pub fn title(&self, what: &str) -> String {
        format!("{what} — {} {} ({})", self.g.kind.label(), self.g.vmid, self.res.display_name())
    }
    pub fn running(&self) -> bool {
        self.res.status == "running"
    }
    pub fn cfg_str(&self, key: &str) -> String {
        self.config.and_then(|c| c.get(key)).map(value_str).unwrap_or_default()
    }
}

/// Changes whenever the guest's configuration content changes. Forms reload
/// on this, so an unrelated job finishing does not discard in-progress edits.
pub fn config_stamp(cfg: &Map<String, Value>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    serde_json::to_string(cfg).unwrap_or_default().hash(&mut h);
    h.finish()
}

pub fn value_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

pub struct GuestView {
    kind: GuestKind,
    vmid: u64,
    tab: Tab,
    config: Fetch<Map<String, Value>>,
    config_gen: u64,
    tasks: Fetch<Vec<TaskInfo>>,
    confirm_power: Option<Confirm<PowerAction>>,
    summary: summary::SummaryTab,
    hardware: hardware::HardwareTab,
    network: network::NetworkTab,
    ips: ips::IpsTab,
    cloudinit: cloudinit::CloudInitTab,
    snapshots: snapshots::SnapshotsTab,
    backups: backups::BackupsTab,
    firewall: firewall::FirewallTab,
    agent: agent::AgentTab,
    manage: manage::ManageTab,
}

impl GuestView {
    pub fn new(kind: GuestKind, vmid: u64) -> Self {
        Self {
            kind,
            vmid,
            tab: Tab::Summary,
            config: Fetch::default(),
            config_gen: u64::MAX,
            tasks: Fetch::default(),
            confirm_power: None,
            summary: Default::default(),
            hardware: Default::default(),
            network: Default::default(),
            ips: Default::default(),
            cloudinit: Default::default(),
            snapshots: Default::default(),
            backups: Default::default(),
            firewall: Default::default(),
            agent: Default::default(),
            manage: Default::default(),
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, sh: &Shared, actions: &mut Actions) {
        let Some(res) = sh.find_guest(self.kind, self.vmid).cloned() else {
            widgets::heading(ui, &format!("{} {}", self.kind.label(), self.vmid));
            widgets::warn_box(ui, "This guest no longer exists in the cluster (it may have been destroyed or the VMID changed).");
            return;
        };
        let g = res.guest_ref().expect("guest");

        // Reload config whenever a job finished (it may have changed it).
        if self.config_gen != crate::rt::generation() && !self.config.is_loading() {
            self.config_gen = crate::rt::generation();
            let gr = g.clone();
            self.config = sh.fetch(move |c| async move { guest::config(&c, &gr).await });
        }

        if let Some(a) = widgets::confirm_ui(ui.ctx(), &mut self.confirm_power) {
            spawn_power(sh, &g, &res, a);
        }

        self.header(ui, sh, &g, &res);
        let mut tabs = vec![(Tab::Summary, "Summary"), (Tab::Hardware, "Hardware"), (Tab::Network, "Network"), (Tab::Ips, "IP addresses")];
        if self.kind == GuestKind::Qemu {
            tabs.push((Tab::CloudInit, "Cloud-Init"));
        }
        tabs.extend([(Tab::Snapshots, "Snapshots"), (Tab::Backups, "Backups"), (Tab::Firewall, "Firewall")]);
        if self.kind == GuestKind::Qemu {
            tabs.push((Tab::Agent, "Guest agent"));
        }
        tabs.extend([(Tab::Tasks, "Tasks"), (Tab::Manage, "Manage")]);
        widgets::tab_bar(ui, &mut self.tab, &tabs);
        ui.add(egui::Separator::default().spacing(0.0));
        ui.add_space(14.0);

        let cfg_owned = self.config.ok().cloned();
        if let Some(Err(e)) = self.config.poll() {
            widgets::error_box(ui, &format!("Cannot read configuration: {e}"));
        }
        let gc = GCtx { sh, g: g.clone(), res: &res, config: cfg_owned.as_ref() };
        widgets::page(ui, "guest-tab", |ui| match self.tab {
            Tab::Summary => self.summary.ui(ui, &gc, actions),
            Tab::Hardware => self.hardware.ui(ui, &gc, actions),
            Tab::Network => self.network.ui(ui, &gc, actions),
            Tab::Ips => self.ips.ui(ui, &gc, actions),
            Tab::CloudInit => self.cloudinit.ui(ui, &gc, actions),
            Tab::Snapshots => self.snapshots.ui(ui, &gc, actions),
            Tab::Backups => self.backups.ui(ui, &gc, actions),
            Tab::Firewall => self.firewall.ui(ui, &gc, actions),
            Tab::Agent => self.agent.ui(ui, &gc, actions),
            Tab::Tasks => self.tasks_ui(ui, &gc),
            Tab::Manage => self.manage.ui(ui, &gc, actions),
        });
    }

    fn header(&mut self, ui: &mut egui::Ui, sh: &Shared, g: &GuestRef, res: &Resource) {
        let p = theme::p();
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(42.0, 42.0), egui::Sense::hover());
            ui.painter().rect(rect, egui::CornerRadius::same(10), p.surface, egui::Stroke::new(1.0, p.border2), egui::StrokeKind::Inside);
            let glyph = if g.kind == GuestKind::Qemu { theme::icon::DESKTOP } else { theme::icon::CUBE };
            ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, glyph, egui::FontId::proportional(21.0), p.accent);
            ui.add_space(4.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 3.0;
                ui.horizontal(|ui| {
                    ui.label(theme::bold(res.display_name(), 20.0));
                    ui.label(RichText::new(format!("{} {}", g.kind.label(), res.vmid)).monospace().color(p.muted));
                    if res.template {
                        widgets::pill(ui, "Template", widgets::Tone::Neutral);
                    } else {
                        widgets::status_pill(ui, &res.status);
                    }
                    if !res.lock.is_empty() {
                        widgets::pill(ui, format!("{} locked: {}", theme::icon::LOCK, res.lock), widgets::Tone::Warn);
                    }
                    for t in res.tag_list() {
                        widgets::pill(ui, t, widgets::Tone::Neutral);
                    }
                });
                let mut facts = vec![format!("on {}", g.node)];
                if res.status == "running" && res.uptime > 0 {
                    facts.push(format!("up {}", crate::util::fmt_duration(res.uptime)));
                }
                facts.push(format!("{:.0} vCPU", res.maxcpu));
                facts.push(crate::util::fmt_bytes(res.maxmem));
                if !res.pool.is_empty() {
                    facts.push(format!("pool {}", res.pool));
                }
                ui.label(RichText::new(facts.join("  ·  ")).size(12.5).color(p.muted));
            });
            if res.template {
                return;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let running = res.status == "running";
                let paused = res.status == "paused";
                let mut buttons: Vec<PowerAction> = Vec::new();
                if !running && !paused {
                    buttons.push(PowerAction::Start);
                }
                if running {
                    buttons.extend([PowerAction::Shutdown, PowerAction::Reboot]);
                }
                if paused {
                    buttons.push(PowerAction::Resume);
                }
                let mut more: Vec<PowerAction> = Vec::new();
                if running {
                    if g.kind == GuestKind::Qemu {
                        more.push(PowerAction::Suspend);
                    }
                    more.push(PowerAction::Stop);
                    if g.kind == GuestKind::Qemu {
                        more.push(PowerAction::Reset);
                    }
                }
                if paused {
                    more.push(PowerAction::Stop);
                }
                // Right-to-left: overflow menu first, then the main actions, then consoles.
                if !more.is_empty() {
                    let r = widgets::icon_button(ui, theme::icon::DOTS, "").on_hover_text("More power actions");
                    egui::Popup::menu(&r).show(|ui| {
                        for a in more {
                            let destructive = matches!(a, PowerAction::Stop | PowerAction::Reset);
                            let label = if destructive { RichText::new(a.label()).color(p.err) } else { RichText::new(a.label()) };
                            if ui.button(label).clicked() {
                                self.power(sh, g, res, a);
                            }
                        }
                    });
                }
                for a in buttons.into_iter().rev() {
                    let glyph = match a {
                        PowerAction::Start | PowerAction::Resume => theme::icon::PLAY,
                        PowerAction::Reboot => theme::icon::REFRESH,
                        _ => theme::icon::POWER,
                    };
                    let r = if matches!(a, PowerAction::Start) {
                        widgets::primary_button(ui, format!("{glyph}  {}", a.label()))
                    } else {
                        widgets::icon_button(ui, glyph, a.label())
                    };
                    if r.clicked() {
                        self.power(sh, g, res, a);
                    }
                }
                if g.kind == GuestKind::Qemu
                    && running
                    && widgets::icon_button(ui, theme::icon::DESKTOP, "SPICE")
                        .on_hover_text("Requires a SPICE display and virt-viewer")
                        .clicked()
                {
                    let (c, gr) = (sh.client.clone(), g.clone());
                    sh.spawn(format!("SPICE console {}", g.vmid), move |_ctx| async move { guest::open_spice(&c, &gr).await });
                }
                if widgets::icon_button(ui, theme::icon::TERMINAL, "Console")
                    .on_hover_text("Opens the Proxmox console in your browser (log in to the web UI there if asked)")
                    .clicked()
                {
                    let _ = webbrowser::open(&guest::console_url(&sh.client, g, &res.name));
                }
            });
        });
        ui.add_space(6.0);
    }

    fn power(&mut self, sh: &Shared, g: &GuestRef, res: &Resource, a: PowerAction) {
        let destructive = matches!(a, PowerAction::Stop | PowerAction::Reset);
        if destructive && sh.settings.confirm_destructive {
            self.confirm_power = Some(
                Confirm::new(
                    a.label(),
                    format!("{} {} '{}'? This is like pulling the power cord.", a.label(), res.vmid, res.display_name()),
                    a,
                )
                .danger(),
            );
        } else {
            spawn_power(sh, g, res, a);
        }
    }

    fn tasks_ui(&mut self, ui: &mut egui::Ui, gc: &GCtx) {
        if self.tasks.is_idle() || (self.tasks.is_stale() && !self.tasks.is_loading()) {
            let (node, vmid) = (gc.g.node.clone(), gc.g.vmid);
            self.tasks = gc.sh.fetch(move |c| async move {
                let list = c
                    .get_q(
                        &format!("/nodes/{node}/tasks"),
                        &crate::api::Params::new().set("vmid", vmid).set("limit", 100).set("source", "all"),
                    )
                    .await?;
                Ok(list.as_array().map(|a| a.iter().map(TaskInfo::from_value).collect()).unwrap_or_default())
            });
        }
        ui.horizontal(|ui| {
            widgets::section(ui, "Tasks on this node for this guest");
            if ui.small_button("⟳").clicked() {
                self.tasks = Fetch::default();
            }
        });
        let sh = gc.sh;
        widgets::fetch_ui(ui, &mut self.tasks, |ui, tasks| {
            super::tasks::task_table(ui, sh, tasks, "guest-tasks");
        });
    }
}

fn spawn_power(sh: &Shared, g: &GuestRef, res: &Resource, a: PowerAction) {
    let (c, gr) = (sh.client.clone(), g.clone());
    sh.spawn(format!("{} {} {} ({})", a.label(), g.kind.label(), g.vmid, res.display_name()), move |ctx| async move {
        guest::power(&c, &ctx, &gr, a).await
    });
}
