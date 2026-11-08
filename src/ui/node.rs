//! Node view: status, graphs, network, guests, power and bulk actions.

use super::charts::{self, Timeframe};
use super::widgets::{self, Confirm};
use super::{Actions, Nav, Shared, theme};
use crate::api::Params;
use crate::rt::Fetch;
use crate::util::{ValueExt, fmt_bytes, fmt_duration, pct};
use egui::RichText;
use serde_json::Value;

#[derive(Clone)]
enum Act {
    Reboot,
    Shutdown,
    StartAll,
    StopAll,
}

pub struct NodeView {
    node: String,
    status: Fetch<Value>,
    rrd: Fetch<Vec<Value>>,
    tf: Timeframe,
    network: Fetch<Vec<Value>>,
    updates: Fetch<usize>,
    confirm: Option<Confirm<Act>>,
}

impl NodeView {
    pub fn new(node: String) -> Self {
        Self {
            node,
            status: Fetch::default(),
            rrd: Fetch::default(),
            tf: Timeframe::Hour,
            network: Fetch::default(),
            updates: Fetch::default(),
            confirm: None,
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, sh: &Shared, actions: &mut Actions) {
        let node = self.node.clone();
        if self.status.is_idle() || (self.status.age_secs() > 5.0 && !self.status.is_loading()) {
            let n = node.clone();
            self.status = sh.fetch(move |c| async move { c.get(&format!("/nodes/{n}/status")).await });
        }
        if self.rrd.is_idle() || (self.rrd.age_secs() > 60.0 && !self.rrd.is_loading()) {
            let (n, tf) = (node.clone(), self.tf);
            self.rrd = sh.fetch(move |c| async move { c.get_list(&format!("/nodes/{n}/rrddata?timeframe={}&cf=AVERAGE", tf.api())).await });
        }
        if self.network.is_idle() {
            let n = node.clone();
            self.network = sh.fetch(move |c| async move { c.get_list(&format!("/nodes/{n}/network")).await });
        }
        if self.updates.is_idle() {
            let n = node.clone();
            self.updates = sh.fetch(move |c| async move { Ok(c.get_list(&format!("/nodes/{n}/apt/update")).await?.len()) });
        }
        if let Some(a) = widgets::confirm_ui(ui.ctx(), &mut self.confirm) {
            let (c, n) = (sh.client.clone(), node.clone());
            let (title, path, params) = match a {
                Act::Reboot => ("Reboot node", format!("/nodes/{n}/status"), Params::new().set("command", "reboot")),
                Act::Shutdown => ("Shut down node", format!("/nodes/{n}/status"), Params::new().set("command", "shutdown")),
                Act::StartAll => ("Start all guests", format!("/nodes/{n}/startall"), Params::new()),
                Act::StopAll => ("Stop all guests", format!("/nodes/{n}/stopall"), Params::new()),
            };
            sh.spawn(format!("{title} on {n}"), move |ctx| async move {
                let r = c.post(&path, &params).await?;
                crate::ops::guest::run_task(&c, &ctx, title, r).await?;
                Ok(format!("{title} on {n}: done"))
            });
        }

        let res = sh.resources.iter().find(|r| r.rtype == "node" && r.node == node).cloned();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.horizontal(|ui| {
                widgets::status_dot(ui, res.as_ref().map(|r| r.status.as_str()).unwrap_or(""));
                widgets::heading(ui, &format!("Node {node}"));
                if let Some(Ok(n)) = self.updates.poll()
                    && *n > 0
                {
                    ui.label(RichText::new(format!("{n} package updates available")).color(theme::warn()));
                }
                if sh.profile.in_maintenance(&node) {
                    widgets::pill(ui, format!("{}  In maintenance", theme::icon::WRENCH), widgets::Tone::Warn)
                        .on_hover_text("Auto-placement skips this node. Existing guests are not affected.");
                }
            });
            ui.horizontal_wrapped(|ui| {
                let maint = sh.profile.in_maintenance(&node);
                if ui
                    .button(if maint {
                        format!("{}  End maintenance", theme::icon::WRENCH)
                    } else {
                        format!("{}  Maintenance", theme::icon::WRENCH)
                    })
                    .on_hover_text("Maintenance keeps new guests off this node (auto-placement skips it). Stored in PMXSpan only.")
                    .clicked()
                {
                    actions.0.push(super::Action::SetMaintenance(node.clone(), !maint));
                }
                if ui.button("Shell (browser)").clicked() {
                    let _ = webbrowser::open(&crate::ops::guest::node_shell_url(&sh.client, &node));
                }
                if super::integration::sshspan_button(
                    ui,
                    &sh.settings,
                    "SSH with SSHSpan",
                    "Open an SSH session to this node as root in SSHSpan (you confirm the address first)",
                    super::integration::ButtonStyle::Normal,
                ) {
                    actions.0.push(super::Action::Integration(super::integration::Request::ConnectNode(node.clone())));
                }
                if ui.button("Start all guests").clicked() {
                    self.confirm =
                        Some(Confirm::new("Start all", format!("Start all guests with 'start at boot' on {node}?"), Act::StartAll));
                }
                if ui.button("Stop all guests").clicked() {
                    self.confirm = Some(Confirm::new("Stop all", format!("Stop all guests on {node}?"), Act::StopAll).danger());
                }
                if ui.button(RichText::new("Reboot node").color(theme::warn())).clicked() {
                    self.confirm = Some(
                        Confirm::new("Reboot node", format!("Reboot {node}? Guests without HA will go down."), Act::Reboot)
                            .danger()
                            .require(node.clone()),
                    );
                }
                if ui.button(RichText::new("Shut down node").color(theme::err())).clicked() {
                    self.confirm =
                        Some(Confirm::new("Shut down node", format!("Power off {node}?"), Act::Shutdown).danger().require(node.clone()));
                }
            });
            widgets::section(ui, "Status");
            if let Some(Ok(st)) = self.status.poll() {
                let mem = st.get("memory").cloned().unwrap_or_default();
                let swap = st.get("swap").cloned().unwrap_or_default();
                let root = st.get("rootfs").cloned().unwrap_or_default();
                let cpuinfo = st.get("cpuinfo").cloned().unwrap_or_default();
                egui::Grid::new("node-status").num_columns(2).spacing([12.0, 5.0]).show(ui, |ui| {
                    ui.label(RichText::new("CPU").color(theme::muted()));
                    widgets::usage_bar(ui, st.f("cpu") as f32, format!("{:.1}%", st.f("cpu") * 100.0), 300.0);
                    ui.end_row();
                    ui.label(RichText::new("Memory").color(theme::muted()));
                    widgets::usage_bar(
                        ui,
                        pct(mem.f("used"), mem.f("total")),
                        format!("{} / {}", fmt_bytes(mem.f("used")), fmt_bytes(mem.f("total"))),
                        300.0,
                    );
                    ui.end_row();
                    ui.label(RichText::new("Swap").color(theme::muted()));
                    widgets::usage_bar(
                        ui,
                        pct(swap.f("used"), swap.f("total")),
                        format!("{} / {}", fmt_bytes(swap.f("used")), fmt_bytes(swap.f("total"))),
                        300.0,
                    );
                    ui.end_row();
                    ui.label(RichText::new("Root FS").color(theme::muted()));
                    widgets::usage_bar(
                        ui,
                        pct(root.f("used"), root.f("total")),
                        format!("{} / {}", fmt_bytes(root.f("used")), fmt_bytes(root.f("total"))),
                        300.0,
                    );
                    ui.end_row();
                    let load: Vec<String> = st
                        .get("loadavg")
                        .map(|l| l.arr().iter().map(|x| x.as_str().unwrap_or("").to_string()).collect())
                        .unwrap_or_default();
                    widgets::kv(ui, "Load average", load.join(" "));
                    widgets::kv(
                        ui,
                        "CPU model",
                        format!("{} ({} sockets, {} threads)", cpuinfo.s("model"), cpuinfo.s("sockets"), cpuinfo.s("cpus")),
                    );
                    widgets::kv(ui, "Kernel", st.s("kversion"));
                    widgets::kv(ui, "PVE manager", st.s("pveversion"));
                    widgets::kv(ui, "Uptime", fmt_duration(st.u("uptime")));
                });
            } else if let Some(Err(e)) = self.status.poll() {
                widgets::error_box(ui, e);
            } else {
                ui.spinner();
            }

            widgets::section(ui, "Performance");
            if charts::timeframe_picker(ui, &mut self.tf) {
                self.rrd = Fetch::default();
            }
            let id = format!("node-{node}");
            widgets::fetch_ui(ui, &mut self.rrd, |ui, data| charts::rrd_charts(ui, &id, data, true));

            widgets::section(ui, "Guests");
            let mut guests: Vec<_> = sh.guests().filter(|g| g.node == node).collect();
            guests.sort_by_key(|g| g.vmid);
            egui::Grid::new("node-guests").striped(true).num_columns(4).show(ui, |ui| {
                for g in guests {
                    ui.horizontal(|ui| {
                        widgets::status_dot(ui, &g.status);
                        if ui.link(format!("{} {}", g.vmid, g.display_name())).clicked()
                            && let Some(k) = g.kind()
                        {
                            actions.nav(Nav::Guest(k, g.vmid));
                        }
                    });
                    ui.label(g.kind().map(|k| k.label()).unwrap_or(""));
                    ui.label(if g.template { "template".to_string() } else { g.status.clone() });
                    ui.label(fmt_bytes(g.maxmem));
                    ui.end_row();
                }
            });

            widgets::section(ui, "Network interfaces");
            widgets::fetch_ui(ui, &mut self.network, |ui, ifaces| {
                let mut ifaces = ifaces.clone();
                ifaces.sort_by_key(|i| i.s("iface"));
                egui::Grid::new("node-net").striped(true).num_columns(6).spacing([12.0, 4.0]).show(ui, |ui| {
                    for h in ["Interface", "Type", "Active", "Address", "Gateway", "Ports / slaves"] {
                        ui.label(RichText::new(h).strong());
                    }
                    ui.end_row();
                    for i in ifaces {
                        ui.label(i.s("iface"));
                        ui.label(i.s("type"));
                        ui.label(if i.b("active") {
                            RichText::new("yes").color(theme::ok())
                        } else {
                            RichText::new("no").color(theme::muted())
                        });
                        let mut addr = i.s("cidr");
                        if !i.s("cidr6").is_empty() {
                            addr = format!("{addr} {}", i.s("cidr6"));
                        }
                        ui.label(addr.trim());
                        ui.label(i.s("gateway"));
                        ui.label(format!("{}{}", i.s("bridge_ports"), i.s("slaves")));
                        ui.end_row();
                    }
                });
            });
        });
    }
}
