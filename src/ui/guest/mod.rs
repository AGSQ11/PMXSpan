//! Detail view for one VM or container.

mod agent;
mod backups;
mod cloudinit;
mod firewall;
mod hardware;
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
        ui.separator();
        ui.horizontal_wrapped(|ui| {
            let mut tabs = vec![(Tab::Summary, "Summary"), (Tab::Hardware, "Hardware"), (Tab::Network, "Network")];
            if self.kind == GuestKind::Qemu {
                tabs.push((Tab::CloudInit, "Cloud-Init"));
            }
            tabs.extend([(Tab::Snapshots, "Snapshots"), (Tab::Backups, "Backups"), (Tab::Firewall, "Firewall")]);
            if self.kind == GuestKind::Qemu {
                tabs.push((Tab::Agent, "Guest agent"));
            }
            tabs.extend([(Tab::Tasks, "Task history"), (Tab::Manage, "Manage")]);
            for (t, label) in tabs {
                ui.selectable_value(&mut self.tab, t, label);
            }
        });
        ui.separator();

        let cfg_owned = self.config.ok().cloned();
        if let Some(Err(e)) = self.config.poll() {
            widgets::error_box(ui, &format!("Cannot read configuration: {e}"));
        }
        let gc = GCtx { sh, g: g.clone(), res: &res, config: cfg_owned.as_ref() };
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| match self.tab {
            Tab::Summary => self.summary.ui(ui, &gc, actions),
            Tab::Hardware => self.hardware.ui(ui, &gc, actions),
            Tab::Network => self.network.ui(ui, &gc, actions),
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
        ui.horizontal(|ui| {
            widgets::status_dot(ui, &res.status);
            ui.label(RichText::new(format!("{} {}", res.vmid, res.display_name())).heading().strong());
            ui.label(
                RichText::new(format!("{} on {}", if res.template { "template" } else { g.kind.label() }, g.node)).color(theme::MUTED),
            );
            ui.label(RichText::new(&res.status).color(theme::status_color(&res.status)));
            if !res.lock.is_empty() {
                ui.label(RichText::new(format!("🔒 locked ({})", res.lock)).color(theme::WARN));
            }
            for t in res.tag_list() {
                egui::Frame::new().fill(theme::INFO.linear_multiply(0.25)).corner_radius(8.0).inner_margin(egui::vec2(6.0, 1.0)).show(
                    ui,
                    |ui| {
                        ui.label(RichText::new(t).small());
                    },
                );
            }
        });
        if res.template {
            return;
        }
        ui.horizontal_wrapped(|ui| {
            let running = res.status == "running";
            let paused = res.status == "paused";
            let mut buttons: Vec<PowerAction> = Vec::new();
            if !running && !paused {
                buttons.push(PowerAction::Start);
            }
            if running {
                buttons.extend([PowerAction::Shutdown, PowerAction::Reboot]);
                if g.kind == GuestKind::Qemu {
                    buttons.push(PowerAction::Suspend);
                }
                buttons.push(PowerAction::Stop);
                if g.kind == GuestKind::Qemu {
                    buttons.push(PowerAction::Reset);
                }
            }
            if paused {
                buttons.extend([PowerAction::Resume, PowerAction::Stop]);
            }
            for a in buttons {
                let destructive = matches!(a, PowerAction::Stop | PowerAction::Reset);
                let label = if destructive { RichText::new(a.label()).color(theme::ERR) } else { RichText::new(a.label()) };
                if ui.button(label).clicked() {
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
            }
            ui.separator();
            let console = if g.kind == GuestKind::Qemu { "Console (noVNC)" } else { "Console (xterm.js)" };
            if ui.button(console).on_hover_text("Opens the Proxmox console in your browser (log in to the web UI there if asked)").clicked()
            {
                let _ = webbrowser::open(&guest::console_url(&sh.client, g, &res.name));
            }
            if g.kind == GuestKind::Qemu
                && running
                && ui.button("SPICE").on_hover_text("Requires a SPICE display and virt-viewer").clicked()
            {
                let (c, gr) = (sh.client.clone(), g.clone());
                sh.spawn(format!("SPICE console {}", g.vmid), move |_ctx| async move { guest::open_spice(&c, &gr).await });
            }
        });
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
