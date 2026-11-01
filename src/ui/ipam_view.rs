//! Local IPAM: pools, assignments, reservations.

use super::widgets::{self, Confirm};
use super::{Actions, Shared, theme};
use crate::config::AppConfig;
use crate::ipam::{IpamDb, Pool};
use crate::ops::drift::{self, DriftItem, DriftKind};
use crate::rt::Fetch;
use egui::RichText;
use parking_lot::Mutex;
use std::sync::Arc;

#[derive(Clone, Copy, PartialEq, Default)]
enum Tab {
    #[default]
    Pools,
    History,
    Drift,
}

#[derive(Default)]
pub struct IpamView {
    tab: Tab,
    history_filter: String,
    drift: Fetch<Vec<drift::GuestNet>>,
    drift_server: String,
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
        widgets::tab_bar(ui, &mut self.tab, &[(Tab::Pools, "Pools"), (Tab::History, "History"), (Tab::Drift, "Drift")]);
        ui.add_space(6.0);
        match self.tab {
            Tab::Pools => self.pools_ui(ui, db, config, shared, actions),
            Tab::History => self.history_ui(ui, db, config, shared, actions),
            Tab::Drift => self.drift_ui(ui, db, shared, actions),
        }
    }

    fn history_ui(
        &mut self,
        ui: &mut egui::Ui,
        db: &Arc<Mutex<IpamDb>>,
        config: &AppConfig,
        shared: Option<&Shared>,
        actions: &mut Actions,
    ) {
        widgets::muted(ui, "Every address assigned, adopted, released, made main, reserved or unreserved, newest first.");
        ui.add(
            egui::TextEdit::singleline(&mut self.history_filter).hint_text("Filter by IP, VMID, hostname or action…").desired_width(320.0),
        );
        let f = self.history_filter.trim().to_lowercase();
        let rows: Vec<_> = db
            .lock()
            .history
            .iter()
            .rev()
            .filter(|e| {
                f.is_empty()
                    || e.ip.contains(&f)
                    || e.action.contains(&f)
                    || e.hostname.to_lowercase().contains(&f)
                    || e.vmid.map(|v| v.to_string() == f).unwrap_or(false)
            })
            .take(2000)
            .cloned()
            .collect();
        if rows.is_empty() {
            widgets::muted(ui, "No changes recorded yet.");
            return;
        }
        egui::ScrollArea::vertical().id_salt("ipam-history").show(ui, |ui| {
            egui::Grid::new("ipam-history-grid").striped(true).num_columns(8).spacing([14.0, 4.0]).show(ui, |ui| {
                for h in ["When", "Action", "IP", "Server", "VMID", "Interface", "Hostname / note", "By"] {
                    ui.label(RichText::new(h).strong());
                }
                ui.end_row();
                for e in rows {
                    ui.label(crate::util::fmt_epoch(e.at));
                    let tone = match e.action.as_str() {
                        "released" | "unreserved" => theme::warn(),
                        "assigned" | "adopted" => theme::ok(),
                        _ => theme::info(),
                    };
                    ui.label(RichText::new(&e.action).color(tone));
                    ui.label(RichText::new(&e.ip).monospace());
                    ui.label(config.profile(&e.server_id).map(|p| p.display_name()).unwrap_or_else(|| "-".into()));
                    match e.vmid {
                        Some(v) => {
                            if ui.link(v.to_string()).clicked()
                                && let Some(s) = shared.filter(|s| s.server_id == e.server_id)
                                && let Some(k) = s.guests().find(|g| g.vmid == v).and_then(|r| r.kind())
                            {
                                actions.nav(super::Nav::Guest(k, v));
                            }
                        }
                        None => {
                            ui.label("-");
                        }
                    }
                    ui.label(e.iface.map(|n| format!("net{n}")).unwrap_or_default());
                    let text = [e.hostname.as_str(), e.note.as_str()].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · ");
                    ui.label(text);
                    ui.label(&e.by);
                    ui.end_row();
                }
            });
        });
    }

    fn drift_ui(&mut self, ui: &mut egui::Ui, db: &Arc<Mutex<IpamDb>>, shared: Option<&Shared>, actions: &mut Actions) {
        let Some(sh) = shared else {
            widgets::muted(ui, "Connect to a server to compare IPAM with its guests.");
            return;
        };
        widgets::muted(
            ui,
            "Compares IPAM with the connected cluster: addresses kept for guests that are gone, static addresses guests use that IPAM \
             doesn't know, main addresses that differ, and guests created by PMXSpan that have no IPAM record.",
        );
        let stale = self.drift_server != sh.server_id;
        ui.horizontal(|ui| {
            let busy = self.drift.is_loading();
            if ui.add_enabled(!busy, egui::Button::new(if self.drift.is_idle() || stale { "Scan" } else { "Scan again" })).clicked()
                || stale
            {
                self.drift_server = sh.server_id.clone();
                let guests: Vec<_> = sh.guests().cloned().collect();
                self.drift = sh.fetch(move |c| async move { Ok(drift::scan(&c, guests).await) });
            }
            if busy {
                ui.spinner();
                ui.label("reading guest configs…");
            }
        });
        let Some(Ok(nets)) = self.drift.poll().cloned() else {
            if let Some(Err(e)) = self.drift.poll() {
                widgets::error_box(ui, e);
            }
            return;
        };
        let all_vmids: Vec<u64> = sh.guests().map(|r| r.vmid).collect();
        let items = drift::compare(&sh.server_id, &db.lock(), &nets, &all_vmids, &sh.settings.managed_tag);
        let unreadable = nets.iter().filter(|n| n.ips.is_none()).count();
        if unreadable > 0 {
            widgets::warn_box(ui, &format!("{unreadable} guest config(s) could not be read and were skipped."));
        }
        if items.is_empty() {
            widgets::info_box(ui, "No drift: IPAM matches the cluster.");
            return;
        }
        let mut rescan = false;
        egui::ScrollArea::vertical().id_salt("ipam-drift").show(ui, |ui| {
            egui::Grid::new("ipam-drift-grid").striped(true).num_columns(6).spacing([14.0, 6.0]).show(ui, |ui| {
                for h in ["VMID", "Guest", "Issue", "Address", "Details", ""] {
                    ui.label(RichText::new(h).strong());
                }
                ui.end_row();
                for it in items {
                    drift_row(ui, sh, db, &it, actions, &mut rescan);
                    ui.end_row();
                }
            });
        });
        if rescan {
            actions.toast("Started - scan again once the job has finished");
        }
    }

    fn pools_ui(&mut self, ui: &mut egui::Ui, db: &Arc<Mutex<IpamDb>>, config: &AppConfig, shared: Option<&Shared>, actions: &mut Actions) {
        widgets::muted(
            ui,
            "Pools used by the create wizard and rebuilds. An explicitly requested IP is assigned exactly or the operation fails — never silently replaced.",
        );
        if let Some(id) = widgets::confirm_ui(ui.ctx(), &mut self.confirm_delete) {
            if let Err(e) = db.lock().delete_pool(&id) {
                actions.toast_err(format!("{e:#}"));
            }
            self.selected = None;
            self.draft = None;
        }
        if let Some(ip) = widgets::confirm_ui(ui.ctx(), &mut self.confirm_release) {
            let mut d = db.lock();
            // Guest addresses go through release_ip so a released main address is replaced;
            // reservations are simply dropped.
            if let Err(e) = d.unreserve(&ip).and_then(|()| d.release_ip(&ip)) {
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
                        if ui.button(RichText::new("Delete pool").color(theme::err())).clicked() {
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
                                    ui.label(RichText::new("reserved").color(theme::warn()));
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

fn drift_row(ui: &mut egui::Ui, sh: &Shared, db: &Arc<Mutex<IpamDb>>, it: &DriftItem, actions: &mut Actions, rescan: &mut bool) {
    match &it.guest {
        Some(g) => {
            if ui.link(it.vmid.to_string()).clicked() {
                actions.nav(super::Nav::Guest(g.kind, g.vmid));
            }
        }
        None => {
            ui.label(it.vmid.to_string());
        }
    }
    ui.label(&it.name);
    let tone = if it.kind == DriftKind::NoIpam { theme::info() } else { theme::warn() };
    ui.label(RichText::new(drift::kind_label(&it.kind)).color(tone));
    let iface = it.iface.map(|n| format!(" (net{n})")).unwrap_or_default();
    ui.label(RichText::new(format!("{}{iface}", it.ip)).monospace());
    match &it.kind {
        DriftKind::Orphan => {
            ui.label("IPAM still holds it for a VMID that no longer exists");
            if ui.small_button("Release").clicked() {
                match db.lock().release_guest_note(&sh.server_id, it.vmid, "guest gone (drift)") {
                    Ok(ips) => actions.toast_ok(format!("Released {}", ips.join(", "))),
                    Err(e) => actions.toast_err(format!("{e:#}")),
                }
            }
        }
        DriftKind::Untracked => {
            ui.label("configured on the guest, not recorded in IPAM");
            if let Some(g) = it.guest.clone()
                && ui.small_button("Adopt").clicked()
            {
                let env = sh.env();
                sh.spawn(format!("Adopt addresses of {}", g.vmid), move |ctx| crate::ops::ipassign::adopt(env, ctx, g));
                *rescan = true;
            }
        }
        DriftKind::Mismatch { guest_has } => {
            ui.label(format!("IPAM main address; the guest has {guest_has}"));
            if let Some(g) = it.guest.clone()
                && ui.small_button("Apply IPAM").on_hover_text("Rewrite the guest's network config from IPAM").clicked()
            {
                let env = sh.env();
                sh.spawn(format!("Apply IPAM to {}", g.vmid), move |ctx| async move {
                    crate::ops::ipassign::apply(&env, &ctx, &g, Default::default(), &Default::default()).await
                });
                *rescan = true;
            }
        }
        DriftKind::NoIpam => {
            ui.label(format!("tagged {} but has no static address in IPAM", sh.settings.managed_tag));
            ui.label("");
        }
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
        ui.label(RichText::new(format!("{e:#}")).color(theme::warn()));
    }
}
