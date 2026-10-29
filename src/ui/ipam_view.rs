//! Local IPAM: pools, assignments, reservations.

use super::widgets::{self, Confirm};
use super::{Actions, Shared, theme};
use crate::config::AppConfig;
use crate::ipam::{IpamDb, Pool};
use egui::RichText;
use parking_lot::Mutex;
use std::sync::Arc;

#[derive(Default)]
pub struct IpamView {
    selected: Option<String>,
    draft: Option<Pool>,
    reserve_ip: String,
    reserve_note: String,
    filter: String,
    confirm_delete: Option<Confirm<String>>,
    confirm_release: Option<Confirm<String>>,
}

impl IpamView {
    pub fn ui(&mut self, ui: &mut egui::Ui, db: &Arc<Mutex<IpamDb>>, config: &AppConfig, shared: Option<&Shared>, actions: &mut Actions) {
        widgets::heading(ui, "IP address management");
        widgets::muted(
            ui,
            "Pools used by the create wizard and rebuilds. An explicitly requested IP is assigned exactly or the operation fails — never silently replaced.",
        );
        if let Some(id) = widgets::confirm_ui(ui.ctx(), &mut self.confirm_delete) {
            let mut d = db.lock();
            d.pools.retain(|p| p.id != id);
            d.assignments.retain(|a| a.pool_id != id);
            if let Err(e) = d.save() {
                actions.toast_err(format!("{e:#}"));
            }
            self.selected = None;
            self.draft = None;
        }
        if let Some(ip) = widgets::confirm_ui(ui.ctx(), &mut self.confirm_release) {
            let mut d = db.lock();
            // Guest addresses go through release_ip so a released main address is replaced;
            // reservations are simply dropped.
            d.assignments.retain(|a| a.ip != ip || !a.reserved);
            if let Err(e) = d.release_ip(&ip) {
                actions.toast_err(format!("{e:#}"));
            } else {
                actions.toast("Released in IPAM only - re-apply on the guest's IP addresses tab to update its config");
            }
        }
        ui.add_space(6.0);
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(260.0);
                if ui.button("＋ New pool").clicked() {
                    let mut p = Pool::default();
                    if let Some(s) = shared {
                        p.bridge = s.settings.default_bridge.clone();
                    }
                    self.selected = Some(p.id.clone());
                    self.draft = Some(p);
                }
                ui.add_space(4.0);
                let d = db.lock();
                for p in &d.pools {
                    let used = d.used_in_pool(&p.id);
                    let label = format!("{}  {}  ({used}/{})", p.name, p.cidr, p.capacity());
                    if ui.selectable_label(self.selected.as_deref() == Some(p.id.as_str()), label).clicked() {
                        self.selected = Some(p.id.clone());
                        self.draft = Some(p.clone());
                    }
                }
            });
            ui.separator();
            ui.vertical(|ui| {
                if let Some(draft) = self.draft.as_mut() {
                    pool_form(ui, draft, config);
                    ui.horizontal(|ui| {
                        if ui.button(RichText::new("Save pool").strong()).clicked() {
                            match draft.validate() {
                                Ok(()) => {
                                    let mut d = db.lock();
                                    match d.pools.iter_mut().find(|p| p.id == draft.id) {
                                        Some(slot) => *slot = draft.clone(),
                                        None => d.pools.push(draft.clone()),
                                    }
                                    match d.save() {
                                        Ok(()) => actions.toast_ok(format!("Pool '{}' saved", draft.name)),
                                        Err(e) => actions.toast_err(format!("{e:#}")),
                                    }
                                }
                                Err(e) => actions.toast_err(format!("{e:#}")),
                            }
                        }
                        if ui.button(RichText::new("Delete pool").color(theme::ERR)).clicked() {
                            self.confirm_delete = Some(
                                Confirm::new(
                                    "Delete pool",
                                    format!("Delete pool '{}' and forget all its assignments?", draft.name),
                                    draft.id.clone(),
                                )
                                .danger(),
                            );
                        }
                    });
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.label("Reserve address");
                        ui.add(egui::TextEdit::singleline(&mut self.reserve_ip).desired_width(160.0).hint_text("IP"));
                        ui.add(egui::TextEdit::singleline(&mut self.reserve_note).desired_width(200.0).hint_text("note (e.g. router)"));
                        if ui.button("Reserve").clicked() {
                            match db.lock().reserve(&draft.id, &self.reserve_ip, &self.reserve_note) {
                                Ok(()) => {
                                    self.reserve_ip.clear();
                                    self.reserve_note.clear();
                                }
                                Err(e) => actions.toast_err(format!("{e:#}")),
                            }
                        }
                    });
                }
                widgets::section(ui, "Assignments");
                ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("Filter by IP, VMID, hostname…"));
                let d = db.lock();
                let f = self.filter.to_lowercase();
                let rows: Vec<_> = d
                    .assignments
                    .iter()
                    .filter(|a| self.selected.as_ref().map(|s| &a.pool_id == s).unwrap_or(true))
                    .filter(|a| {
                        f.is_empty()
                            || a.ip.contains(&f)
                            || a.hostname.to_lowercase().contains(&f)
                            || a.vmid.map(|v| v.to_string().contains(&f)).unwrap_or(false)
                    })
                    .cloned()
                    .collect();
                drop(d);
                egui::ScrollArea::vertical().id_salt("ipam-rows").show(ui, |ui| {
                    egui::Grid::new("ipam-assign").striped(true).num_columns(7).spacing([14.0, 4.0]).show(ui, |ui| {
                        for h in ["IP", "Server", "VMID", "Interface", "Hostname / note", "Since", ""] {
                            ui.label(RichText::new(h).strong());
                        }
                        ui.end_row();
                        for a in rows {
                            ui.label(RichText::new(&a.ip).monospace());
                            let server = config.profile(&a.server_id).map(|p| p.display_name()).unwrap_or_else(|| "-".into());
                            ui.label(server);
                            match a.vmid {
                                Some(v) => {
                                    let link = ui.link(v.to_string());
                                    if link.clicked()
                                        && let Some(s) = shared.filter(|s| s.server_id == a.server_id)
                                        && let Some(r) = s.guests().find(|g| g.vmid == v)
                                        && let Some(k) = r.kind()
                                    {
                                        actions.nav(super::Nav::Guest(k, v));
                                    }
                                }
                                None => {
                                    ui.label(RichText::new("reserved").color(theme::WARN));
                                }
                            }
                            if a.vmid.is_some() {
                                ui.label(format!("net{}{}", a.iface(), if a.main { " (main)" } else { "" }));
                            } else {
                                ui.label("");
                            }
                            ui.label(if a.hostname.is_empty() { a.note.clone() } else { a.hostname.clone() });
                            ui.label(crate::util::fmt_epoch(a.assigned_at));
                            if ui.small_button("Release").clicked() {
                                self.confirm_release = Some(Confirm::new("Release address", format!("Release {}?", a.ip), a.ip.clone()));
                            }
                            ui.end_row();
                        }
                    });
                });
            });
        });
    }
}

fn pool_form(ui: &mut egui::Ui, p: &mut Pool, config: &AppConfig) {
    egui::Grid::new("pool-form").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
        ui.label("Name");
        ui.text_edit_singleline(&mut p.name);
        ui.end_row();
        ui.label("Network (CIDR)");
        ui.add(egui::TextEdit::singleline(&mut p.cidr).hint_text("203.0.113.0/24 or 2001:db8:1::/64"));
        ui.end_row();
        ui.label("Gateway");
        ui.text_edit_singleline(&mut p.gateway);
        ui.end_row();
        ui.label("Range");
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut p.range_start).desired_width(150.0).hint_text("first (optional)"));
            ui.label("–");
            ui.add(egui::TextEdit::singleline(&mut p.range_end).desired_width(150.0).hint_text("last (optional)"));
        });
        ui.end_row();
        ui.label("DNS servers");
        ui.text_edit_singleline(&mut p.dns);
        ui.end_row();
        ui.label("Only for bridge");
        ui.add(egui::TextEdit::singleline(&mut p.bridge).hint_text("empty = any bridge"));
        ui.end_row();
        ui.label("VLAN tag");
        ui.horizontal(|ui| {
            let mut has = p.vlan.is_some();
            ui.checkbox(&mut has, "");
            if has {
                let mut v = p.vlan.unwrap_or(1);
                ui.add(egui::DragValue::new(&mut v).range(1..=4094));
                p.vlan = Some(v);
            } else {
                p.vlan = None;
            }
        });
        ui.end_row();
        ui.label("Only for server");
        egui::ComboBox::from_id_salt("pool-server")
            .selected_text(config.profile(&p.server_id).map(|s| s.display_name()).unwrap_or_else(|| "any".into()))
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut p.server_id, String::new(), "any");
                for s in &config.profiles {
                    ui.selectable_value(&mut p.server_id, s.id.clone(), s.display_name());
                }
            });
        ui.end_row();
        ui.label("Notes");
        ui.text_edit_singleline(&mut p.notes);
        ui.end_row();
    });
    if let Err(e) = p.validate()
        && !p.cidr.is_empty()
    {
        ui.label(RichText::new(format!("{e:#}")).color(theme::WARN));
    }
}
