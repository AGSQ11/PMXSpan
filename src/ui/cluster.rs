//! Datacenter-level jobs: scheduled backups, storage replication, HA resources.

use super::widgets::{self, Confirm};
use super::{Actions, Shared, theme};
use crate::api::Params;
use crate::rt::Fetch;
use crate::util::{ValueExt, fmt_epoch};
use egui::RichText;
use serde_json::Value;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Tab {
    #[default]
    Backup,
    Replication,
    Ha,
}

#[derive(Clone)]
enum Del {
    Backup(String),
    Replication(String),
    Ha(String),
}

struct BackupForm {
    schedule: String,
    storage: String,
    all: bool,
    vmids: String,
    node: Option<String>,
    mode: String,
    compress: String,
    keep_last: u32,
    keep_daily: u32,
    keep_weekly: u32,
    keep_monthly: u32,
    mailto: String,
    comment: String,
    enabled: bool,
}

impl Default for BackupForm {
    fn default() -> Self {
        Self {
            schedule: "21:00".into(),
            storage: String::new(),
            all: false,
            vmids: String::new(),
            node: None,
            mode: "snapshot".into(),
            compress: "zstd".into(),
            keep_last: 3,
            keep_daily: 7,
            keep_weekly: 4,
            keep_monthly: 0,
            mailto: String::new(),
            comment: String::new(),
            enabled: true,
        }
    }
}

#[derive(Default)]
pub struct ClusterView {
    tab: Tab,
    backups: Fetch<Vec<Value>>,
    replication: Fetch<Vec<Value>>,
    ha: Fetch<Vec<Value>>,
    ha_status: Fetch<Vec<Value>>,
    bform: Option<BackupForm>,
    rep_guest: String,
    rep_target: String,
    rep_schedule: String,
    rep_rate: f64,
    ha_sid: String,
    ha_state: String,
    ha_max_restart: u32,
    ha_max_relocate: u32,
    confirm: Option<Confirm<Del>>,
}

impl ClusterView {
    pub fn ui(&mut self, ui: &mut egui::Ui, sh: &Shared, _actions: &mut Actions) {
        if let Some(d) = widgets::confirm_ui(ui.ctx(), &mut self.confirm) {
            let c = sh.client.clone();
            let (title, path) = match d {
                Del::Backup(id) => (format!("Delete backup job {id}"), format!("/cluster/backup/{id}")),
                Del::Replication(id) => (format!("Delete replication {id}"), format!("/cluster/replication/{id}")),
                Del::Ha(sid) => (format!("Remove HA resource {sid}"), format!("/cluster/ha/resources/{sid}")),
            };
            sh.spawn(title.clone(), move |_ctx| async move {
                c.delete(&path, &Params::new()).await?;
                Ok(title)
            });
        }
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.tab, Tab::Backup, "Backup jobs");
            ui.selectable_value(&mut self.tab, Tab::Replication, "Replication");
            ui.selectable_value(&mut self.tab, Tab::Ha, "High availability");
        });
        ui.separator();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| match self.tab {
            Tab::Backup => self.backup_ui(ui, sh),
            Tab::Replication => self.replication_ui(ui, sh),
            Tab::Ha => self.ha_ui(ui, sh),
        });
    }

    fn backup_ui(&mut self, ui: &mut egui::Ui, sh: &Shared) {
        if self.backups.is_idle() || (self.backups.is_stale() && !self.backups.is_loading()) {
            self.backups = sh.fetch(|c| async move { c.get_list("/cluster/backup").await });
        }
        if self.bform.is_none() && ui.button("＋ New backup job").clicked() {
            self.bform = Some(BackupForm::default());
        }
        let mut submit = None;
        let mut cancel = false;
        if let Some(f) = self.bform.as_mut() {
            let mut storages: Vec<String> =
                sh.resources.iter().filter(|r| r.rtype == "storage" && r.has_content("backup")).map(|r| r.storage.clone()).collect();
            storages.sort();
            storages.dedup();
            crate::ui::theme::card_frame().inner_margin(egui::Margin::symmetric(14, 12)).show(ui, |ui| {
                egui::Grid::new("bj-form").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                    ui.label("Schedule");
                    ui.vertical(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut f.schedule).desired_width(200.0));
                        widgets::muted(ui, "systemd calendar: '21:00', 'sat 02:30', 'mon..fri 23:00', '*/6:00'");
                    });
                    ui.end_row();
                    ui.label("Storage");
                    widgets::combo(ui, "bj-storage", &mut f.storage, &storages, 180.0);
                    ui.end_row();
                    ui.label("Guests");
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut f.all, "all guests");
                        if !f.all {
                            ui.add(egui::TextEdit::singleline(&mut f.vmids).hint_text("100,101,105").desired_width(200.0));
                        }
                    });
                    ui.end_row();
                    ui.label("Node");
                    let nodes = sh.nodes();
                    widgets::combo_opt(ui, "bj-node", &mut f.node, &nodes, "all nodes", 160.0);
                    ui.end_row();
                    ui.label("Mode / compression");
                    ui.horizontal(|ui| {
                        egui::ComboBox::from_id_salt("bj-mode").selected_text(f.mode.clone()).show_ui(ui, |ui| {
                            for m in ["snapshot", "suspend", "stop"] {
                                ui.selectable_value(&mut f.mode, m.to_string(), m);
                            }
                        });
                        egui::ComboBox::from_id_salt("bj-comp").selected_text(f.compress.clone()).show_ui(ui, |ui| {
                            for m in ["zstd", "lzo", "gzip", "0"] {
                                ui.selectable_value(&mut f.compress, m.to_string(), m);
                            }
                        });
                    });
                    ui.end_row();
                    ui.label("Retention");
                    ui.horizontal(|ui| {
                        ui.add(egui::DragValue::new(&mut f.keep_last).range(0..=1000).prefix("last "));
                        ui.add(egui::DragValue::new(&mut f.keep_daily).range(0..=1000).prefix("daily "));
                        ui.add(egui::DragValue::new(&mut f.keep_weekly).range(0..=1000).prefix("weekly "));
                        ui.add(egui::DragValue::new(&mut f.keep_monthly).range(0..=1000).prefix("monthly "));
                    });
                    ui.end_row();
                    ui.label("Mail to");
                    ui.add(egui::TextEdit::singleline(&mut f.mailto).hint_text("optional"));
                    ui.end_row();
                    ui.label("Comment");
                    ui.text_edit_singleline(&mut f.comment);
                    ui.end_row();
                    ui.label("");
                    ui.checkbox(&mut f.enabled, "enabled");
                    ui.end_row();
                });
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            !f.storage.is_empty() && (f.all || !f.vmids.trim().is_empty()),
                            egui::Button::new(RichText::new("Create job").strong()),
                        )
                        .clicked()
                    {
                        let mut keep = Vec::new();
                        for (k, v) in [
                            ("keep-last", f.keep_last),
                            ("keep-daily", f.keep_daily),
                            ("keep-weekly", f.keep_weekly),
                            ("keep-monthly", f.keep_monthly),
                        ] {
                            if v > 0 {
                                keep.push(format!("{k}={v}"));
                            }
                        }
                        let mut p = Params::new()
                            .set("schedule", f.schedule.trim())
                            .set("storage", &f.storage)
                            .set("mode", &f.mode)
                            .set("compress", &f.compress)
                            .flag("enabled", f.enabled)
                            .opt("node", f.node.as_deref())
                            .opt("mailto", Some(f.mailto.trim()))
                            .opt("comment", Some(f.comment.trim()));
                        if !keep.is_empty() {
                            p.push("prune-backups", keep.join(","));
                        }
                        if f.all {
                            p.push("all", 1);
                        } else {
                            p.push("vmid", f.vmids.replace(' ', ""));
                        }
                        submit = Some(p);
                    }
                    cancel = ui.button("Cancel").clicked();
                });
            });
        }
        if let Some(p) = submit {
            let c = sh.client.clone();
            sh.spawn("Create backup job", move |_ctx| async move {
                c.post("/cluster/backup", &p).await?;
                Ok("backup job created".into())
            });
            self.bform = None;
        }
        if cancel {
            self.bform = None;
        }
        let mut del = None;
        let mut run: Option<Value> = None;
        let mut toggle: Option<(String, bool)> = None;
        widgets::fetch_ui(ui, &mut self.backups, |ui, jobs| {
            if jobs.is_empty() {
                widgets::muted(ui, "No scheduled backup jobs.");
                return;
            }
            egui::Grid::new("bj").striped(true).num_columns(8).spacing([12.0, 4.0]).show(ui, |ui| {
                for h in ["On", "Schedule", "Storage", "Guests", "Node", "Mode", "Retention / comment", ""] {
                    ui.label(RichText::new(h).strong());
                }
                ui.end_row();
                for j in jobs.iter() {
                    let id = j.s("id");
                    let mut en = j.get("enabled").map(|_| j.b("enabled")).unwrap_or(true);
                    if ui.checkbox(&mut en, "").changed() {
                        toggle = Some((id.clone(), en));
                    }
                    ui.label(j.s("schedule"));
                    ui.label(j.s("storage"));
                    ui.label(if j.b("all") { "all".to_string() } else { j.s("vmid") });
                    ui.label(j.opt_s("node").unwrap_or_else(|| "all".into()));
                    ui.label(j.s("mode"));
                    ui.label(format!("{} {}", j.s("prune-backups"), j.s("comment")));
                    ui.horizontal(|ui| {
                        if ui.small_button("Run now").clicked() {
                            run = Some(j.clone());
                        }
                        if ui.small_button(RichText::new("Delete").color(theme::err())).clicked() {
                            del = Some(id.clone());
                        }
                    });
                    ui.end_row();
                }
            });
        });
        if let Some(id) = del {
            self.confirm = Some(Confirm::new("Delete backup job", format!("Delete backup job {id}?"), Del::Backup(id)).danger());
        }
        if let Some((id, en)) = toggle {
            let c = sh.client.clone();
            sh.spawn(format!("{} backup job", if en { "Enable" } else { "Disable" }), move |_ctx| async move {
                c.put(&format!("/cluster/backup/{id}"), &Params::new().flag("enabled", en)).await?;
                Ok(format!("backup job {id} {}", if en { "enabled" } else { "disabled" }))
            });
        }
        if let Some(j) = run {
            // Run immediately on every node that holds selected guests.
            let c = sh.client.clone();
            let guests: Vec<(String, u64)> = sh.guests().filter(|g| !g.template).map(|g| (g.node.clone(), g.vmid)).collect();
            sh.spawn(format!("Run backup job {}", j.s("id")), move |ctx| async move {
                let all = j.b("all");
                let wanted: Vec<u64> = j.s("vmid").split(',').filter_map(|v| v.trim().parse().ok()).collect();
                let node_filter = j.opt_s("node");
                let mut by_node: std::collections::BTreeMap<String, Vec<u64>> = Default::default();
                for (n, id) in guests {
                    if (all || wanted.contains(&id)) && node_filter.as_ref().map(|f| f == &n).unwrap_or(true) {
                        by_node.entry(n).or_default().push(id);
                    }
                }
                if by_node.is_empty() {
                    anyhow::bail!("no matching guests for this job");
                }
                for (n, ids) in by_node {
                    let list = ids.iter().map(u64::to_string).collect::<Vec<_>>().join(",");
                    let p = Params::new()
                        .set("vmid", &list)
                        .set("storage", j.s("storage"))
                        .set("mode", j.s("mode"))
                        .set("compress", j.opt_s("compress").unwrap_or("zstd".into()));
                    let r = c.post(&format!("/nodes/{n}/vzdump"), &p).await?;
                    crate::ops::guest::run_task(&c, &ctx, &format!("backup {list} on {n}"), r).await?;
                }
                Ok("backup job run complete".into())
            });
        }
    }

    fn replication_ui(&mut self, ui: &mut egui::Ui, sh: &Shared) {
        if self.replication.is_idle() || (self.replication.is_stale() && !self.replication.is_loading()) {
            self.replication = sh.fetch(|c| async move { c.get_list("/cluster/replication").await });
        }
        widgets::muted(ui, "Storage replication requires ZFS on both nodes.");
        ui.horizontal_wrapped(|ui| {
            ui.label("Guest");
            let guests: Vec<String> = sh.guests().filter(|g| !g.template).map(|g| format!("{} {}", g.vmid, g.display_name())).collect();
            widgets::combo(ui, "rep-guest", &mut self.rep_guest, &guests, 200.0);
            ui.label("Target");
            let nodes = sh.online_nodes();
            widgets::combo(ui, "rep-target", &mut self.rep_target, &nodes, 130.0);
            ui.label("Schedule");
            if self.rep_schedule.is_empty() {
                self.rep_schedule = "*/15".into();
            }
            ui.add(egui::TextEdit::singleline(&mut self.rep_schedule).desired_width(90.0));
            ui.label("Rate");
            ui.add(egui::DragValue::new(&mut self.rep_rate).range(0.0..=10_000.0).suffix(" MB/s")).on_hover_text("0 = unlimited");
            if ui.add_enabled(!self.rep_guest.is_empty() && !self.rep_target.is_empty(), egui::Button::new("Add")).clicked() {
                let vmid: u64 = self.rep_guest.split_whitespace().next().and_then(|v| v.parse().ok()).unwrap_or(0);
                let existing = self.replication.ok().map(|l| l.iter().filter(|j| j.u("guest") == vmid).count()).unwrap_or(0);
                let p = Params::new()
                    .set("id", format!("{vmid}-{existing}"))
                    .set("type", "local")
                    .set("target", &self.rep_target)
                    .set("schedule", &self.rep_schedule)
                    .opt("rate", (self.rep_rate > 0.0).then_some(self.rep_rate));
                let c = sh.client.clone();
                sh.spawn(format!("Add replication for {vmid}"), move |_ctx| async move {
                    c.post("/cluster/replication", &p).await?;
                    Ok("replication job created".into())
                });
            }
        });
        let mut del = None;
        widgets::fetch_ui(ui, &mut self.replication, |ui, jobs| {
            if jobs.is_empty() {
                widgets::muted(ui, "No replication jobs.");
                return;
            }
            egui::Grid::new("rep").striped(true).num_columns(6).spacing([12.0, 4.0]).show(ui, |ui| {
                for h in ["ID", "Guest", "Source", "Target", "Schedule", ""] {
                    ui.label(RichText::new(h).strong());
                }
                ui.end_row();
                for j in jobs.iter() {
                    ui.label(j.s("id"));
                    ui.label(j.s("guest"));
                    ui.label(j.s("source"));
                    ui.label(j.s("target"));
                    ui.label(j.s("schedule"));
                    if ui.small_button(RichText::new("Delete").color(theme::err())).clicked() {
                        del = Some(j.s("id"));
                    }
                    ui.end_row();
                }
            });
        });
        if let Some(id) = del {
            self.confirm = Some(
                Confirm::new(
                    "Delete replication",
                    format!("Delete replication job {id}? Replicated data on the target is removed."),
                    Del::Replication(id),
                )
                .danger(),
            );
        }
    }

    fn ha_ui(&mut self, ui: &mut egui::Ui, sh: &Shared) {
        if self.ha.is_idle() || (self.ha.is_stale() && !self.ha.is_loading()) {
            self.ha = sh.fetch(|c| async move { c.get_list("/cluster/ha/resources").await });
        }
        if self.ha_status.is_idle() || (self.ha_status.age_secs() > 10.0 && !self.ha_status.is_loading()) {
            self.ha_status = sh.fetch(|c| async move { c.get_list("/cluster/ha/status/current").await });
        }
        if let Some(Ok(st)) = self.ha_status.poll() {
            for s in st.iter().filter(|s| matches!(s.s("type").as_str(), "quorum" | "master")) {
                ui.label(format!("{}: {}", s.s("type"), s.s("status")));
            }
            for s in st.iter().filter(|s| s.s("type") == "lrm") {
                ui.label(RichText::new(format!("{} — {}", s.s("node"), s.s("status"))).small().color(theme::muted()));
            }
        }
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            if self.ha_state.is_empty() {
                self.ha_state = "started".into();
                self.ha_max_restart = 1;
                self.ha_max_relocate = 1;
            }
            ui.label("Guest");
            let guests: Vec<String> = sh
                .guests()
                .filter(|g| !g.template && g.hastate.is_empty())
                .map(|g| format!("{}:{} {}", if g.rtype == "qemu" { "vm" } else { "ct" }, g.vmid, g.display_name()))
                .collect();
            widgets::combo(ui, "ha-sid", &mut self.ha_sid, &guests, 220.0);
            egui::ComboBox::from_id_salt("ha-state").selected_text(self.ha_state.clone()).show_ui(ui, |ui| {
                for s in ["started", "stopped", "ignored", "disabled"] {
                    ui.selectable_value(&mut self.ha_state, s.to_string(), s);
                }
            });
            ui.add(egui::DragValue::new(&mut self.ha_max_restart).range(0..=10).prefix("max restart "));
            ui.add(egui::DragValue::new(&mut self.ha_max_relocate).range(0..=10).prefix("max relocate "));
            if ui.add_enabled(!self.ha_sid.is_empty(), egui::Button::new("Add")).clicked() {
                let sid = self.ha_sid.split_whitespace().next().unwrap_or("").to_string();
                let p = Params::new()
                    .set("sid", &sid)
                    .set("state", &self.ha_state)
                    .set("max_restart", self.ha_max_restart)
                    .set("max_relocate", self.ha_max_relocate);
                let c = sh.client.clone();
                sh.spawn(format!("Add HA resource {sid}"), move |_ctx| async move {
                    c.post("/cluster/ha/resources", &p).await?;
                    Ok(format!("{sid} is now HA-managed"))
                });
                self.ha_sid.clear();
            }
        });
        let mut del = None;
        let mut set_state: Option<(String, String)> = None;
        widgets::fetch_ui(ui, &mut self.ha, |ui, list| {
            if list.is_empty() {
                widgets::muted(ui, "No HA resources.");
                return;
            }
            egui::Grid::new("ha").striped(true).num_columns(5).spacing([12.0, 4.0]).show(ui, |ui| {
                for h in ["Resource", "State", "Group / rule", "Max restart/relocate", ""] {
                    ui.label(RichText::new(h).strong());
                }
                ui.end_row();
                for r in list.iter() {
                    let sid = r.s("sid");
                    ui.label(&sid);
                    let state = r.s("state");
                    egui::ComboBox::from_id_salt(format!("ha-st-{sid}")).selected_text(state.clone()).show_ui(ui, |ui| {
                        for s in ["started", "stopped", "ignored", "disabled"] {
                            if ui.selectable_label(state == s, s).clicked() && state != s {
                                set_state = Some((sid.clone(), s.to_string()));
                            }
                        }
                    });
                    ui.label(r.s("group"));
                    ui.label(format!("{} / {}", r.s("max_restart"), r.s("max_relocate")));
                    if ui.small_button(RichText::new("Remove").color(theme::err())).clicked() {
                        del = Some(sid.clone());
                    }
                    ui.end_row();
                }
            });
        });
        if let Some(sid) = del {
            self.confirm = Some(Confirm::new("Remove HA resource", format!("Stop managing {sid} with HA?"), Del::Ha(sid)));
        }
        if let Some((sid, state)) = set_state {
            let c = sh.client.clone();
            sh.spawn(format!("Set HA state of {sid}"), move |_ctx| async move {
                c.put(&format!("/cluster/ha/resources/{sid}"), &Params::new().set("state", &state)).await?;
                Ok(format!("{sid} → {state}"))
            });
        }
        let _ = fmt_epoch;
    }
}
