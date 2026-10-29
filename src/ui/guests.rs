//! Sortable, filterable list of all guests with quick power actions.

use super::widgets::{self, Confirm};
use super::{Actions, Nav, Shared, theme};
use crate::api::types::{GuestKind, GuestRef, Resource};
use crate::ops::guest::{self, PowerAction};
use crate::util::{fmt_bytes, fmt_duration, pct};
use egui::RichText;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum SortBy {
    #[default]
    Vmid,
    Name,
    Node,
    Status,
    Cpu,
    Mem,
}

#[derive(Default)]
pub struct GuestsView {
    filter: String,
    kind: Option<GuestKind>,
    status: String,
    node: Option<String>,
    show_templates: bool,
    sort: SortBy,
    desc: bool,
    confirm: Option<Confirm<(GuestRef, PowerAction)>>,
}

impl GuestsView {
    pub fn ui(&mut self, ui: &mut egui::Ui, sh: &Shared, actions: &mut Actions) {
        if let Some((g, a)) = widgets::confirm_ui(ui.ctx(), &mut self.confirm) {
            let c = sh.client.clone();
            sh.spawn(format!("{} {} {}", a.label(), g.kind.label(), g.vmid), move |ctx| async move { guest::power(&c, &ctx, &g, a).await });
        }
        ui.horizontal_wrapped(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("Search name, VMID, tag, IP pool…").desired_width(240.0));
            egui::ComboBox::from_id_salt("g-kind").selected_text(self.kind.map(|k| k.label()).unwrap_or("All types")).show_ui(ui, |ui| {
                ui.selectable_value(&mut self.kind, None, "All types");
                ui.selectable_value(&mut self.kind, Some(GuestKind::Qemu), "VMs");
                ui.selectable_value(&mut self.kind, Some(GuestKind::Lxc), "Containers");
            });
            egui::ComboBox::from_id_salt("g-status")
                .selected_text(if self.status.is_empty() { "Any status".to_string() } else { self.status.clone() })
                .show_ui(ui, |ui| {
                    for s in ["", "running", "stopped", "paused"] {
                        ui.selectable_value(&mut self.status, s.to_string(), if s.is_empty() { "Any status" } else { s });
                    }
                });
            let nodes = sh.nodes();
            widgets::combo_opt(ui, "g-node", &mut self.node, &nodes, "All nodes", 120.0);
            ui.checkbox(&mut self.show_templates, "Templates");
        });
        let f = self.filter.to_lowercase();
        let mut rows: Vec<&Resource> = sh
            .guests()
            .filter(|g| self.show_templates || !g.template)
            .filter(|g| self.kind.map(|k| g.kind() == Some(k)).unwrap_or(true))
            .filter(|g| self.status.is_empty() || g.status == self.status)
            .filter(|g| self.node.as_ref().map(|n| &g.node == n).unwrap_or(true))
            .filter(|g| {
                f.is_empty()
                    || g.name.to_lowercase().contains(&f)
                    || g.vmid.to_string().contains(&f)
                    || g.tags.to_lowercase().contains(&f)
                    || g.pool.to_lowercase().contains(&f)
            })
            .collect();
        rows.sort_by(|a, b| {
            let o = match self.sort {
                SortBy::Vmid => a.vmid.cmp(&b.vmid),
                SortBy::Name => a.name.cmp(&b.name),
                SortBy::Node => a.node.cmp(&b.node),
                SortBy::Status => a.status.cmp(&b.status),
                SortBy::Cpu => a.cpu.total_cmp(&b.cpu),
                SortBy::Mem => pct(a.mem, a.maxmem).total_cmp(&pct(b.mem, b.maxmem)),
            };
            if self.desc { o.reverse() } else { o }
        });
        widgets::muted(ui, format!("{} guests", rows.len()));
        ui.add_space(4.0);
        widgets::card(ui, |ui| {
            egui::ScrollArea::both().auto_shrink([false, true]).show(ui, |ui| {
                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
                egui::Grid::new("guests").striped(true).num_columns(10).spacing([12.0, 6.0]).show(ui, |ui| {
                    for (label, s) in [
                        ("VMID", Some(SortBy::Vmid)),
                        ("Name", Some(SortBy::Name)),
                        ("Type", None),
                        ("Node", Some(SortBy::Node)),
                        ("Status", Some(SortBy::Status)),
                        ("CPU", Some(SortBy::Cpu)),
                        ("Memory", Some(SortBy::Mem)),
                        ("Disk", None),
                        ("Uptime", None),
                        ("Actions", None),
                    ] {
                        match s {
                            Some(s) => {
                                let arrow = if self.sort == s { if self.desc { " ▼" } else { " ▲" } } else { "" };
                                if ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new(format!("{label}{arrow}"))
                                                .size(12.0)
                                                .family(theme::semibold())
                                                .color(theme::muted()),
                                        )
                                        .frame(false),
                                    )
                                    .clicked()
                                {
                                    if self.sort == s {
                                        self.desc = !self.desc;
                                    } else {
                                        self.sort = s;
                                        self.desc = false;
                                    }
                                }
                            }
                            None => {
                                ui.label(RichText::new(label).size(12.0).family(theme::semibold()).color(theme::muted()));
                            }
                        }
                    }
                    ui.end_row();
                    for g in rows {
                        let Some(kind) = g.kind() else { continue };
                        if ui.link(g.vmid.to_string()).clicked() {
                            actions.nav(Nav::Guest(kind, g.vmid));
                        }
                        ui.vertical(|ui| {
                            if ui.link(g.display_name()).clicked() {
                                actions.nav(Nav::Guest(kind, g.vmid));
                            }
                            let tags = g.tag_list();
                            if !tags.is_empty() {
                                ui.label(RichText::new(tags.join(" · ")).small().color(theme::muted()));
                            }
                        });
                        ui.label(if g.template { format!("{} template", kind.label()) } else { kind.label().to_string() });
                        ui.label(&g.node);
                        ui.horizontal(|ui| {
                            widgets::status_dot(ui, &g.status);
                            ui.label(&g.status);
                            if !g.lock.is_empty() {
                                ui.label(RichText::new(format!("🔒 {}", g.lock)).color(theme::warn()));
                            }
                            if !g.hastate.is_empty() {
                                ui.label(RichText::new(format!("HA {}", g.hastate)).small().color(theme::info()));
                            }
                        });
                        if g.status == "running" {
                            widgets::usage_bar(ui, (g.cpu as f32).min(1.0), format!("{:.0}% of {:.0}", g.cpu * 100.0, g.maxcpu), 110.0);
                            widgets::usage_bar(ui, pct(g.mem, g.maxmem), format!("{} / {}", fmt_bytes(g.mem), fmt_bytes(g.maxmem)), 150.0);
                        } else {
                            ui.label(format!("{:.0} cores", g.maxcpu));
                            ui.label(fmt_bytes(g.maxmem));
                        }
                        ui.label(fmt_bytes(g.maxdisk));
                        ui.label(fmt_duration(g.uptime));
                        ui.horizontal(|ui| {
                            if g.template {
                                if ui.small_button("Deploy").on_hover_text("Create a new guest from this template").clicked() {
                                    actions.0.push(super::Action::OpenCreate(Box::new(super::create::Prefill::from_template(g))));
                                }
                                return;
                            }
                            let gr = GuestRef { node: g.node.clone(), kind, vmid: g.vmid };
                            let running = g.status == "running";
                            let actions_list: &[PowerAction] = if running {
                                &[PowerAction::Shutdown, PowerAction::Reboot, PowerAction::Stop]
                            } else {
                                &[PowerAction::Start]
                            };
                            for a in actions_list {
                                if ui.small_button(a.label()).clicked() {
                                    let destructive = matches!(a, PowerAction::Stop | PowerAction::Reset);
                                    if destructive && sh.settings.confirm_destructive {
                                        self.confirm = Some(
                                            Confirm::new(
                                                a.label(),
                                                format!("{} {} '{}'? Unsaved guest data may be lost.", a.label(), g.vmid, g.display_name()),
                                                (gr.clone(), *a),
                                            )
                                            .danger(),
                                        );
                                    } else {
                                        let (c, gr, a) = (sh.client.clone(), gr.clone(), *a);
                                        sh.spawn(format!("{} {} {}", a.label(), kind.label(), g.vmid), move |ctx| async move {
                                            guest::power(&c, &ctx, &gr, a).await
                                        });
                                    }
                                }
                            }
                            if ui.small_button("Console").clicked() {
                                let _ = webbrowser::open(&guest::console_url(&sh.client, &gr, &g.name));
                            }
                        });
                        ui.end_row();
                    }
                });
            })
        });
    }
}
