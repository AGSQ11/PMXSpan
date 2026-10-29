//! Cluster overview.

use super::{Actions, Nav, Shared, theme, widgets};
use crate::api::types::{Resource, TaskInfo};
use crate::rt::Fetch;
use crate::util::{fmt_bytes, fmt_duration, fmt_epoch, pct};
use egui::RichText;

#[derive(Default)]
pub struct DashboardView {
    tasks: Fetch<Vec<TaskInfo>>,
    quorum: Fetch<Option<(String, bool)>>,
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
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.horizontal(|ui| {
                widgets::heading(ui, &format!("Datacenter — {}", sh.profile.display_name()));
                if let Some(Some((name, quorate))) = self.quorum.ok() {
                    ui.label(RichText::new(format!("cluster '{name}'")).color(theme::MUTED));
                    ui.label(if *quorate {
                        RichText::new("quorate").color(theme::OK)
                    } else {
                        RichText::new("NOT QUORATE").color(theme::ERR).strong()
                    });
                }
            });
            let nodes: Vec<&Resource> = sh.resources.iter().filter(|r| r.rtype == "node").collect();
            let guests: Vec<&Resource> = sh.guests().filter(|g| !g.template).collect();
            let online = nodes.iter().filter(|n| n.status == "online").count();
            let running = guests.iter().filter(|g| g.status == "running").count();
            let (cpu, maxcpu) = nodes.iter().fold((0.0, 0.0), |(a, b), n| (a + n.cpu * n.maxcpu, b + n.maxcpu));
            let (mem, maxmem) = nodes.iter().fold((0.0, 0.0), |(a, b), n| (a + n.mem, b + n.maxmem));
            let mut seen = std::collections::HashSet::new();
            let (disk, maxdisk) = sh
                .resources
                .iter()
                .filter(|r| r.rtype == "storage" && r.status == "available")
                .filter(|r| !r.shared || seen.insert(r.storage.clone()))
                .fold((0.0, 0.0), |(a, b), s| (a + s.disk, b + s.maxdisk));

            ui.horizontal_wrapped(|ui| {
                stat_card(ui, "Nodes", &format!("{online} / {}", nodes.len()), if online == nodes.len() { theme::OK } else { theme::ERR });
                stat_card(ui, "Guests running", &format!("{running} / {}", guests.len()), theme::INFO);
                stat_card(ui, "Templates", &sh.guests().filter(|g| g.template).count().to_string(), theme::MUTED);
                card(ui, "CPU", |ui| {
                    widgets::usage_bar(ui, pct(cpu, maxcpu), format!("{:.1}% of {maxcpu:.0} cores", pct(cpu, maxcpu) * 100.0), 200.0)
                });
                card(ui, "Memory", |ui| {
                    widgets::usage_bar(ui, pct(mem, maxmem), format!("{} / {}", fmt_bytes(mem), fmt_bytes(maxmem)), 200.0)
                });
                card(ui, "Storage", |ui| {
                    widgets::usage_bar(ui, pct(disk, maxdisk), format!("{} / {}", fmt_bytes(disk), fmt_bytes(maxdisk)), 200.0)
                });
            });

            widgets::section(ui, "Nodes");
            ui.horizontal_wrapped(|ui| {
                let mut nodes = nodes.clone();
                nodes.sort_by(|a, b| a.node.cmp(&b.node));
                for n in nodes {
                    let resp = egui::Frame::group(ui.style()).inner_margin(10.0).show(ui, |ui| {
                        // The parent lays out horizontally; cards stack their content vertically.
                        ui.vertical(|ui| {
                            ui.set_width(270.0);
                            ui.horizontal(|ui| {
                                widgets::status_dot(ui, &n.status);
                                ui.label(RichText::new(&n.node).strong().size(15.0));
                                ui.label(RichText::new(fmt_duration(n.uptime)).color(theme::MUTED).small());
                            });
                            if n.status == "online" {
                                widgets::usage_bar(ui, n.cpu as f32, format!("CPU {:.1}% of {:.0}", n.cpu * 100.0, n.maxcpu), 250.0);
                                widgets::usage_bar(
                                    ui,
                                    pct(n.mem, n.maxmem),
                                    format!("RAM {} / {}", fmt_bytes(n.mem), fmt_bytes(n.maxmem)),
                                    250.0,
                                );
                                widgets::usage_bar(
                                    ui,
                                    pct(n.disk, n.maxdisk),
                                    format!("Root {} / {}", fmt_bytes(n.disk), fmt_bytes(n.maxdisk)),
                                    250.0,
                                );
                                let count = sh.guests().filter(|g| g.node == n.node && !g.template).count();
                                let run = sh.guests().filter(|g| g.node == n.node && g.status == "running").count();
                                widgets::muted(ui, format!("{run} of {count} guests running"));
                            } else {
                                ui.label(RichText::new(format!("node is {}", n.status)).color(theme::ERR));
                            }
                        });
                    });
                    if resp.response.interact(egui::Sense::click()).on_hover_text("Open node").clicked() {
                        actions.nav(Nav::Node(n.node.clone()));
                    }
                }
            });

            widgets::section(ui, "Storage");
            let mut storages: Vec<&Resource> = sh.resources.iter().filter(|r| r.rtype == "storage").collect();
            storages.sort_by(|a, b| (a.storage.as_str(), a.node.as_str()).cmp(&(b.storage.as_str(), b.node.as_str())));
            let mut shown = std::collections::HashSet::new();
            egui::Grid::new("dash-storage").striped(true).num_columns(5).spacing([14.0, 4.0]).show(ui, |ui| {
                for h in ["Storage", "Node", "Type", "Content", "Usage"] {
                    ui.label(RichText::new(h).strong());
                }
                ui.end_row();
                for s in storages {
                    if s.shared && !shown.insert(s.storage.clone()) {
                        continue;
                    }
                    ui.horizontal(|ui| {
                        widgets::status_dot(ui, if s.status == "available" { "running" } else { "error" });
                        ui.label(&s.storage);
                    });
                    ui.label(if s.shared { "shared".to_string() } else { s.node.clone() });
                    ui.label(&s.plugintype);
                    ui.label(RichText::new(&s.content).small());
                    widgets::usage_bar(ui, pct(s.disk, s.maxdisk), format!("{} / {}", fmt_bytes(s.disk), fmt_bytes(s.maxdisk)), 220.0);
                    ui.end_row();
                }
            });

            widgets::section(ui, "Recent task failures");
            match self.tasks.poll() {
                Some(Ok(tasks)) => {
                    let failed: Vec<&TaskInfo> = tasks.iter().filter(|t| !t.running() && !t.ok()).take(12).collect();
                    if failed.is_empty() {
                        widgets::muted(ui, "No failed tasks in the recent cluster task log.");
                    }
                    egui::Grid::new("dash-fail").striped(true).num_columns(4).show(ui, |ui| {
                        for t in failed {
                            ui.label(fmt_epoch(t.starttime));
                            ui.label(&t.node);
                            ui.label(t.describe());
                            ui.label(RichText::new(&t.status).color(theme::ERR));
                            ui.end_row();
                        }
                    });
                    if ui.link("Open task log →").clicked() {
                        actions.nav(Nav::Tasks);
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

fn card(ui: &mut egui::Ui, title: &str, body: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::group(ui.style()).inner_margin(10.0).show(ui, |ui| {
        ui.vertical(|ui| {
            ui.label(RichText::new(title).color(theme::MUTED).small());
            body(ui);
        });
    });
}

fn stat_card(ui: &mut egui::Ui, title: &str, value: &str, color: egui::Color32) {
    card(ui, title, |ui| {
        ui.label(RichText::new(value).size(22.0).strong().color(color));
    });
}
