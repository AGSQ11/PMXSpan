//! Cluster overview.

use super::widgets::Tone;
use super::{Actions, Nav, Shared, theme, widgets};
use crate::api::types::{Resource, TaskInfo};
use crate::rt::Fetch;
use crate::util::{fmt_bytes, fmt_duration, fmt_epoch, pct};
use egui::RichText;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum GuestFilter {
    #[default]
    All,
    Running,
    Stopped,
}

#[derive(Default)]
pub struct DashboardView {
    tasks: Fetch<Vec<TaskInfo>>,
    quorum: Fetch<Option<(String, bool)>>,
    filter: GuestFilter,
}

/// One summary tile.
struct Kpi {
    label: &'static str,
    value: String,
    unit: String,
    frac: f32,
    color: egui::Color32,
    note: String,
}

impl DashboardView {
    pub fn ui(&mut self, ui: &mut egui::Ui, sh: &Shared, actions: &mut Actions) {
        if self.tasks.is_idle() || (self.tasks.age_secs() > 15.0 && !self.tasks.is_loading()) {
            self.tasks = sh.fetch(|c| async move {
                let list = c.get_list("/cluster/tasks").await?;
                Ok(list.iter().map(TaskInfo::from_value).collect())
            });
        }
        if self.quorum.is_idle() || (self.quorum.age_secs() > 30.0 && !self.quorum.is_loading()) {
            self.quorum = sh.fetch(|c| async move {
                let items = c.get_list("/cluster/status").await?;
                Ok(items.iter().find(|i| i.get("type").and_then(|t| t.as_str()) == Some("cluster")).map(|i| {
                    use crate::util::ValueExt;
                    (i.s("name"), i.b("quorate"))
                }))
            });
        }
        let p = theme::p();
        let nodes: Vec<&Resource> = {
            let mut v: Vec<&Resource> = sh.resources.iter().filter(|r| r.rtype == "node").collect();
            v.sort_by(|a, b| a.node.cmp(&b.node));
            v
        };
        let guests: Vec<&Resource> = sh.guests().filter(|g| !g.template).collect();
        let templates = sh.guests().filter(|g| g.template).count();
        let online = nodes.iter().filter(|n| n.status == "online").count();
        let running = guests.iter().filter(|g| g.status == "running").count();
        let (cpu, maxcpu) = nodes.iter().fold((0.0, 0.0), |(a, b), n| (a + n.cpu * n.maxcpu, b + n.maxcpu));
        let (mem, maxmem) = nodes.iter().fold((0.0, 0.0), |(a, b), n| (a + n.mem, b + n.maxmem));
        let quorum_note = match self.quorum.ok() {
            Some(Some((_, true))) => "Cluster quorate".to_string(),
            Some(Some((_, false))) => "NOT QUORATE".to_string(),
            Some(None) => "Single node".to_string(),
            None => String::new(),
        };

        widgets::page(ui, "dashboard", |ui| {
            let kpis = [
                Kpi {
                    label: "Nodes online",
                    value: online.to_string(),
                    unit: format!("/ {}", nodes.len()),
                    frac: if nodes.is_empty() { 0.0 } else { online as f32 / nodes.len() as f32 },
                    color: if online == nodes.len() { p.ok } else { p.err },
                    note: quorum_note,
                },
                Kpi {
                    label: "Guests running",
                    value: running.to_string(),
                    unit: format!("/ {}", guests.len()),
                    frac: if guests.is_empty() { 0.0 } else { running as f32 / guests.len() as f32 },
                    color: p.accent,
                    note: format!("{} stopped · {templates} templates", guests.len() - running),
                },
                Kpi {
                    label: "CPU",
                    value: format!("{:.1}", pct(cpu, maxcpu) * 100.0),
                    unit: "%".into(),
                    frac: pct(cpu, maxcpu),
                    color: p.cpu,
                    note: format!("{maxcpu:.0} cores in total"),
                },
                Kpi {
                    label: "Memory",
                    value: fmt_bytes(mem),
                    unit: format!("/ {}", fmt_bytes(maxmem)),
                    frac: pct(mem, maxmem),
                    color: p.mem,
                    note: format!("{} free", fmt_bytes((maxmem - mem).max(0.0))),
                },
            ];
            let cols = if ui.available_width() > 900.0 { 4 } else { 2 };
            for row in kpis.chunks(cols) {
                ui.columns(cols, |c| {
                    for (i, k) in row.iter().enumerate() {
                        kpi_card(&mut c[i], k);
                    }
                });
                ui.add_space(6.0);
            }
            ui.add_space(8.0);

            self.guests_card(ui, sh, actions, &guests);
            ui.add_space(14.0);

            let wide = ui.available_width() > 900.0;
            if wide {
                ui.columns(2, |c| {
                    nodes_card(&mut c[0], sh, actions, &nodes);
                    self.activity_card(&mut c[1], actions);
                });
            } else {
                nodes_card(ui, sh, actions, &nodes);
                ui.add_space(14.0);
                self.activity_card(ui, actions);
            }
            ui.add_space(14.0);
            storage_card(ui, sh);
            ui.add_space(8.0);
        });
    }

    fn guests_card(&mut self, ui: &mut egui::Ui, sh: &Shared, actions: &mut Actions, guests: &[&Resource]) {
        let p = theme::p();
        let running = guests.iter().filter(|g| g.status == "running").count();
        widgets::card(ui, |ui| {
            ui.horizontal(|ui| {
                widgets::card_title(ui, "Guests");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    for (f, label) in [
                        (GuestFilter::Stopped, format!("Stopped {}", guests.len() - running)),
                        (GuestFilter::Running, format!("Running {running}")),
                        (GuestFilter::All, format!("All {}", guests.len())),
                    ] {
                        let selected = self.filter == f;
                        let text = if selected { RichText::new(label).color(p.nav_on_fg) } else { RichText::new(label).color(p.text2) };
                        let b = egui::Button::new(text.size(12.0))
                            .fill(if selected { p.nav_on_bg } else { p.surface })
                            .stroke(egui::Stroke::new(1.0, if selected { p.accent.gamma_multiply(0.5) } else { p.border2 }))
                            .corner_radius(egui::CornerRadius::same(255));
                        if ui.add(b).clicked() {
                            self.filter = f;
                        }
                    }
                });
            });
            ui.add_space(4.0);
            let mut list: Vec<&&Resource> = guests
                .iter()
                .filter(|g| match self.filter {
                    GuestFilter::All => true,
                    GuestFilter::Running => g.status == "running",
                    GuestFilter::Stopped => g.status != "running",
                })
                .collect();
            list.sort_by_key(|g| g.vmid);
            let total = list.len();
            list.truncate(10);
            let ipam = sh.ipam.lock();
            egui::Grid::new("dash-guests").num_columns(6).spacing([18.0, 10.0]).min_col_width(40.0).show(ui, |ui| {
                for h in ["Name", "Status", "Node", "Main IP", "CPU", "Memory"] {
                    ui.label(RichText::new(h).size(12.0).family(theme::semibold()).color(p.muted));
                }
                ui.end_row();
                for g in list {
                    let Some(kind) = g.kind() else { continue };
                    ui.horizontal(|ui| {
                        widgets::pill(ui, if kind == crate::api::types::GuestKind::Lxc { "CT" } else { "VM" }, widgets::Tone::Neutral);
                        let name =
                            ui.add(egui::Label::new(RichText::new(g.display_name()).family(theme::semibold())).sense(egui::Sense::click()));
                        if name.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                            actions.nav(Nav::Guest(kind, g.vmid));
                        }
                        ui.label(RichText::new(g.vmid.to_string()).monospace().size(12.0).color(p.muted));
                    });
                    ui.horizontal(|ui| {
                        widgets::status_dot(ui, &g.status);
                        ui.label(RichText::new(capitalize(&g.status)).color(theme::status_color(&g.status)));
                    });
                    ui.label(RichText::new(&g.node).color(p.text2));
                    let ip = ipam
                        .main_for(&sh.server_id, g.vmid, 0, false)
                        .or_else(|| ipam.main_for(&sh.server_id, g.vmid, 0, true))
                        .map(|(a, _)| a.ip)
                        .unwrap_or_else(|| "—".into());
                    ui.label(RichText::new(ip).monospace().size(12.0));
                    ui.label(
                        RichText::new(if g.status == "running" { format!("{:.0}%", g.cpu * 100.0) } else { "—".into() }).color(p.text2),
                    );
                    ui.horizontal(|ui| {
                        widgets::meter(ui, pct(g.mem, g.maxmem), p.accent, 100.0);
                        ui.label(RichText::new(format!("{} / {}", fmt_bytes(g.mem), fmt_bytes(g.maxmem))).size(12.0).color(p.text2));
                    });
                    ui.end_row();
                }
            });
            if total > 10 && ui.link(format!("All {total} guests  {}", theme::icon::ARROW_RIGHT)).clicked() {
                actions.nav(Nav::Guests);
            }
        });
    }

    fn activity_card(&mut self, ui: &mut egui::Ui, actions: &mut Actions) {
        let p = theme::p();
        widgets::card(ui, |ui| {
            ui.horizontal(|ui| {
                widgets::card_title(ui, "Recent tasks");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.link("Open task log").clicked() {
                        actions.nav(Nav::Tasks);
                    }
                });
            });
            match self.tasks.poll() {
                Some(Ok(tasks)) => {
                    if tasks.is_empty() {
                        widgets::muted(ui, "No tasks in the recent cluster task log.");
                    }
                    for t in tasks.iter().take(7) {
                        ui.separator();
                        ui.horizontal(|ui| {
                            let (label, tone) = if t.running() {
                                ("Running", Tone::Info)
                            } else if t.ok() {
                                ("Done", Tone::Ok)
                            } else {
                                ("Failed", Tone::Err)
                            };
                            widgets::pill(ui, label, tone);
                            ui.label(format!("{} · {}", t.describe(), t.node)).on_hover_text(&t.status);
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                let when = fmt_epoch(t.starttime);
                                ui.label(RichText::new(when.get(11..16).unwrap_or(&when)).monospace().size(12.0).color(p.muted));
                            });
                        });
                    }
                }
                Some(Err(e)) => widgets::muted(ui, format!("task list unavailable: {e}")),
                None => {
                    ui.spinner();
                }
            }
        });
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

fn kpi_card(ui: &mut egui::Ui, k: &Kpi) {
    let p = theme::p();
    widgets::card(ui, |ui| {
        ui.label(RichText::new(k.label).size(12.0).color(p.muted));
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            ui.label(RichText::new(&k.value).family(theme::semibold()).size(26.0));
            ui.label(RichText::new(&k.unit).color(p.muted));
        });
        widgets::meter(ui, k.frac, k.color, ui.available_width());
        ui.add_space(2.0);
        ui.label(RichText::new(&k.note).size(12.0).color(if k.note == "NOT QUORATE" { p.err } else { p.muted }));
    });
}

fn nodes_card(ui: &mut egui::Ui, sh: &Shared, actions: &mut Actions, nodes: &[&Resource]) {
    let p = theme::p();
    widgets::card(ui, |ui| {
        widgets::card_title(ui, "Nodes");
        for (i, n) in nodes.iter().enumerate() {
            if i > 0 {
                ui.add_space(6.0);
            }
            ui.horizontal(|ui| {
                widgets::status_dot(ui, &n.status);
                let name =
                    ui.add(egui::Label::new(RichText::new(&n.node).family(theme::semibold()).size(14.5)).sense(egui::Sense::click()));
                if name.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("Open node").clicked() {
                    actions.nav(Nav::Node(n.node.clone()));
                }
                ui.label(RichText::new(format!("up {}", fmt_duration(n.uptime))).size(12.0).color(p.muted));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let count = sh.guests().filter(|g| g.node == n.node && !g.template).count();
                    let run = sh.guests().filter(|g| g.node == n.node && g.status == "running").count();
                    ui.label(RichText::new(format!("{run} of {count} running")).size(12.0).color(p.muted));
                });
            });
            if n.status != "online" {
                ui.label(RichText::new(format!("node is {}", n.status)).color(p.err));
                continue;
            }
            widgets::meter_row(ui, "CPU", n.cpu as f32, p.cpu, format!("{:.1}% of {:.0}", n.cpu * 100.0, n.maxcpu));
            widgets::meter_row(ui, "Memory", pct(n.mem, n.maxmem), p.mem, format!("{} / {}", fmt_bytes(n.mem), fmt_bytes(n.maxmem)));
            widgets::meter_row(ui, "Root", pct(n.disk, n.maxdisk), p.disk, format!("{} / {}", fmt_bytes(n.disk), fmt_bytes(n.maxdisk)));
        }
    });
}

fn storage_card(ui: &mut egui::Ui, sh: &Shared) {
    let p = theme::p();
    let mut storages: Vec<&Resource> = sh.resources.iter().filter(|r| r.rtype == "storage").collect();
    storages.sort_by(|a, b| (a.storage.as_str(), a.node.as_str()).cmp(&(b.storage.as_str(), b.node.as_str())));
    let mut shown = std::collections::HashSet::new();
    widgets::card(ui, |ui| {
        widgets::card_title(ui, "Storage");
        egui::Grid::new("dash-storage").num_columns(5).spacing([18.0, 10.0]).show(ui, |ui| {
            for h in ["Storage", "Node", "Type", "Content", "Usage"] {
                ui.label(RichText::new(h).size(12.0).family(theme::semibold()).color(p.muted));
            }
            ui.end_row();
            for s in storages {
                if s.shared && !shown.insert(s.storage.clone()) {
                    continue;
                }
                ui.horizontal(|ui| {
                    widgets::status_dot(ui, if s.status == "available" { "running" } else { "error" });
                    ui.label(RichText::new(&s.storage).family(theme::semibold()));
                });
                ui.label(RichText::new(if s.shared { "shared".to_string() } else { s.node.clone() }).color(p.text2));
                ui.label(RichText::new(&s.plugintype).color(p.text2));
                ui.label(RichText::new(s.content.replace(',', ", ")).size(12.0).color(p.muted));
                ui.horizontal(|ui| {
                    widgets::meter(ui, pct(s.disk, s.maxdisk), theme::usage_color(pct(s.disk, s.maxdisk)), 140.0);
                    ui.label(
                        RichText::new(format!("{} / {}", fmt_bytes(s.disk), fmt_bytes(s.maxdisk))).monospace().size(12.0).color(p.text2),
                    );
                });
                ui.end_row();
            }
        });
    });
}
