//! Automation: guest schedules (snapshots with retention, timed power
//! actions) and usage alerts.

use super::widgets::{self, Confirm};
use super::{Action, Actions, Shared, theme};
use crate::api::types::GuestKind;
use crate::automation::{AlertMonitor, AlertRule, AutomationStore, Metric, Schedule, ScheduleAction, When};
use crate::config::AppConfig;
use crate::util::fmt_epoch;
use egui::RichText;
use parking_lot::Mutex;
use std::sync::Arc;

#[derive(Clone, Copy, PartialEq, Default)]
enum Tab {
    #[default]
    Schedules,
    Alerts,
}

#[derive(Clone)]
enum Del {
    Schedule(String),
    Rule(String),
}

#[derive(Default)]
pub struct AutomationView {
    tab: Tab,
    draft: Option<Draft>,
    rule: Option<AlertRule>,
    rule_vmids: String,
    confirm: Option<Confirm<Del>>,
    error: Option<String>,
}

/// The new-schedule form.
struct Draft {
    guest: Option<(GuestKind, u64)>,
    snapshot: bool,
    keep: u32,
    power: String,
    hourly: bool,
    hours: u32,
    at: String,
    days: [bool; 7],
}

impl Default for Draft {
    fn default() -> Self {
        Self {
            guest: None,
            snapshot: true,
            keep: 7,
            power: "shutdown".into(),
            hourly: false,
            hours: 6,
            at: "03:00".into(),
            days: [true; 7],
        }
    }
}

impl AutomationView {
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        store: &Arc<Mutex<AutomationStore>>,
        monitor: &AlertMonitor,
        config: &AppConfig,
        shared: Option<&Shared>,
        actions: &mut Actions,
    ) {
        if let Some(d) = widgets::confirm_ui(ui.ctx(), &mut self.confirm) {
            let mut st = store.lock();
            match d {
                Del::Schedule(id) => st.schedules.retain(|s| s.id != id),
                Del::Rule(id) => st.alert_rules.retain(|r| r.id != id),
            }
            if let Err(e) = st.save() {
                actions.toast_err(format!("{e:#}"));
            }
        }
        let alerts = if monitor.active.is_empty() { "Alerts".to_string() } else { format!("Alerts ({})", monitor.active.len()) };
        widgets::tab_bar(ui, &mut self.tab, &[(Tab::Schedules, "Schedules"), (Tab::Alerts, alerts.as_str())]);
        ui.add_space(6.0);
        match self.tab {
            Tab::Schedules => self.schedules_ui(ui, store, config, shared, actions),
            Tab::Alerts => self.alerts_ui(ui, store, monitor, config, shared, actions),
        }
    }

    fn schedules_ui(
        &mut self,
        ui: &mut egui::Ui,
        store: &Arc<Mutex<AutomationStore>>,
        config: &AppConfig,
        shared: Option<&Shared>,
        actions: &mut Actions,
    ) {
        widgets::info_box(
            ui,
            "Automatic snapshots (the oldest beyond the kept number are deleted) and timed power actions. Proxmox has no scheduler for \
             these, so they run while PMXSpan is open and connected to the guest's server, or from `pmxspan --run-schedules` started \
             every few minutes by cron or Task Scheduler (see the README).",
        );
        ui.horizontal(|ui| {
            let can = shared.is_some();
            if ui.add_enabled(can, egui::Button::new(format!("{}  New schedule", theme::icon::PLUS))).clicked() {
                self.draft = Some(Draft::default());
                self.error = None;
            }
            if !can {
                widgets::muted(ui, "Connect to a server to add schedules for its guests.");
            }
        });
        if let (Some(sh), Some(_)) = (shared, self.draft.as_ref()) {
            self.draft_ui(ui, sh, store, actions);
        }
        ui.add_space(8.0);
        let rows = store.lock().schedules.clone();
        if rows.is_empty() {
            widgets::muted(ui, "No schedules yet.");
            return;
        }
        let mut toggled: Option<(String, bool)> = None;
        egui::ScrollArea::vertical().id_salt("sched-list").show(ui, |ui| {
            egui::Grid::new("sched-grid").striped(true).num_columns(7).spacing([14.0, 6.0]).show(ui, |ui| {
                for h in ["On", "Guest", "Server", "Action", "When", "Last run", ""] {
                    ui.label(RichText::new(h).strong());
                }
                ui.end_row();
                for s in rows {
                    let mut on = s.enabled;
                    if ui.checkbox(&mut on, "").changed() {
                        toggled = Some((s.id.clone(), on));
                    }
                    ui.label(format!("{} {}  {}", s.kind.label(), s.vmid, s.name));
                    ui.label(config.profile(&s.server_id).map(|p| p.display_name()).unwrap_or_else(|| "(deleted)".into()));
                    ui.label(s.action.label());
                    ui.label(s.when.label());
                    if s.last_run == 0 {
                        widgets::muted(ui, "never");
                    } else {
                        let color = if s.last_ok { theme::ok() } else { theme::err() };
                        ui.label(RichText::new(fmt_epoch(s.last_run)).color(color)).on_hover_text(&s.last_result);
                    }
                    ui.horizontal(|ui| {
                        let here = shared.is_some_and(|sh| sh.server_id == s.server_id);
                        if ui
                            .add_enabled(here, egui::Button::new("Run now").small())
                            .on_disabled_hover_text("Connect to this schedule's server to run it")
                            .clicked()
                        {
                            actions.0.push(Action::RunSchedule(s.id.clone()));
                        }
                        if ui.small_button(RichText::new("Delete").color(theme::err())).clicked() {
                            self.confirm = Some(Confirm::new(
                                "Delete schedule",
                                format!("Delete the schedule \"{}, {}\" of {}?", s.action.label(), s.when.label(), s.vmid),
                                Del::Schedule(s.id.clone()),
                            ));
                        }
                    });
                    ui.end_row();
                }
            });
        });
        if let Some((id, on)) = toggled {
            let mut st = store.lock();
            if let Some(s) = st.schedules.iter_mut().find(|s| s.id == id) {
                s.enabled = on;
            }
            if let Err(e) = st.save() {
                actions.toast_err(format!("{e:#}"));
            }
        }
    }

    fn draft_ui(&mut self, ui: &mut egui::Ui, sh: &Shared, store: &Arc<Mutex<AutomationStore>>, actions: &mut Actions) {
        let Some(d) = self.draft.as_mut() else { return };
        let mut close = false;
        widgets::card(ui, |ui| {
            egui::Grid::new("sched-form").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                ui.label("Guest");
                let mut guests: Vec<_> = sh.guests().filter(|g| !g.template).collect();
                guests.sort_by_key(|g| g.vmid);
                let label = d
                    .guest
                    .and_then(|(k, v)| sh.find_guest(k, v))
                    .map(|g| format!("{} {}  {}", g.kind().map(|k| k.label()).unwrap_or(""), g.vmid, g.display_name()))
                    .unwrap_or_else(|| "choose…".into());
                egui::ComboBox::from_id_salt("sched-guest").width(320.0).selected_text(label).show_ui(ui, |ui| {
                    for g in guests {
                        let Some(k) = g.kind() else { continue };
                        ui.selectable_value(&mut d.guest, Some((k, g.vmid)), format!("{} {}  {}", k.label(), g.vmid, g.display_name()));
                    }
                });
                ui.end_row();
                ui.label("Action");
                ui.horizontal(|ui| {
                    ui.radio_value(&mut d.snapshot, true, "Snapshot, keep");
                    ui.add_enabled(d.snapshot, egui::DragValue::new(&mut d.keep).range(1..=100));
                    ui.add_space(12.0);
                    ui.radio_value(&mut d.snapshot, false, "Power:");
                    ui.add_enabled_ui(!d.snapshot, |ui| {
                        egui::ComboBox::from_id_salt("sched-power").selected_text(d.power.clone()).show_ui(ui, |ui| {
                            for a in ["start", "shutdown", "reboot", "stop"] {
                                ui.selectable_value(&mut d.power, a.to_string(), a);
                            }
                        });
                    });
                });
                ui.end_row();
                ui.label("When");
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        ui.radio_value(&mut d.hourly, true, "Every");
                        ui.add_enabled(d.hourly, egui::DragValue::new(&mut d.hours).range(1..=720).suffix(" h"));
                    });
                    ui.horizontal(|ui| {
                        ui.radio_value(&mut d.hourly, false, "At");
                        ui.add_enabled(!d.hourly, egui::TextEdit::singleline(&mut d.at).desired_width(56.0).hint_text("03:00"));
                        ui.add_enabled_ui(!d.hourly, |ui| {
                            for (i, n) in ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"].iter().enumerate() {
                                ui.toggle_value(&mut d.days[i], *n);
                            }
                        });
                    });
                });
                ui.end_row();
            });
            if let Some(e) = &self.error {
                widgets::error_box(ui, e);
            }
            ui.horizontal(|ui| {
                if ui.button(RichText::new("Save schedule").strong()).clicked() {
                    let (kind, vmid) = d.guest.unwrap_or((GuestKind::Qemu, 0));
                    let days: Vec<u8> = (0..7u8).filter(|i| d.days[*i as usize]).collect();
                    let s = Schedule {
                        server_id: sh.server_id.clone(),
                        kind,
                        vmid,
                        name: sh.find_guest(kind, vmid).map(|g| g.display_name()).unwrap_or_default(),
                        action: if d.snapshot { ScheduleAction::Snapshot { keep: d.keep } } else { ScheduleAction::Power(d.power.clone()) },
                        when: if d.hourly {
                            When::EveryHours(d.hours)
                        } else {
                            When::Daily { at: d.at.trim().to_string(), days: if days.len() == 7 { Vec::new() } else { days } }
                        },
                        ..Default::default()
                    };
                    let days_ok = d.hourly || d.days.iter().any(|x| *x);
                    match s.validate().and_then(|()| if days_ok { Ok(()) } else { Err(anyhow::anyhow!("pick at least one day")) }) {
                        Ok(()) => {
                            let mut st = store.lock();
                            st.schedules.push(s);
                            match st.save() {
                                Ok(()) => {
                                    actions.toast_ok("Schedule saved");
                                    close = true;
                                }
                                Err(e) => self.error = Some(format!("{e:#}")),
                            }
                        }
                        Err(e) => self.error = Some(format!("{e:#}")),
                    }
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
        if close {
            self.draft = None;
            self.error = None;
        }
    }

    fn alerts_ui(
        &mut self,
        ui: &mut egui::Ui,
        store: &Arc<Mutex<AutomationStore>>,
        monitor: &AlertMonitor,
        config: &AppConfig,
        shared: Option<&Shared>,
        actions: &mut Actions,
    ) {
        widgets::info_box(
            ui,
            "Alerts watch the guests of the connected server at every refresh while PMXSpan is open, and notify you when a value \
             stays above its threshold for the given time.",
        );
        widgets::section(ui, "Firing now");
        if monitor.active.is_empty() {
            widgets::muted(ui, if shared.is_some() { "Nothing is firing." } else { "Not connected: no alerts are being checked." });
        } else {
            alert_table(ui, "active", &monitor.active, shared, actions);
        }

        widgets::section(ui, "Rules");
        if ui.button(format!("{}  New rule", theme::icon::PLUS)).clicked() {
            self.rule = Some(AlertRule { server_id: shared.map(|s| s.server_id.clone()).unwrap_or_default(), ..Default::default() });
            self.rule_vmids.clear();
        }
        let mut close = false;
        if let Some(r) = self.rule.as_mut() {
            widgets::card(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    egui::ComboBox::from_id_salt("rule-metric").selected_text(r.metric.label()).show_ui(ui, |ui| {
                        for m in Metric::ALL {
                            ui.selectable_value(&mut r.metric, m, m.label());
                        }
                    });
                    ui.label("above");
                    let range = if matches!(r.metric, Metric::Cpu | Metric::Memory) { 1.0..=100.0 } else { 0.1..=100_000.0 };
                    ui.add(egui::DragValue::new(&mut r.threshold).range(range).speed(0.5).suffix(format!(" {}", r.metric.unit())));
                    ui.label("for");
                    ui.add(egui::DragValue::new(&mut r.minutes).range(0..=1440).suffix(" min"));
                    ui.label("on");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.rule_vmids).hint_text("all guests (or VMIDs: 100, 101)").desired_width(200.0),
                    );
                    let here = shared.map(|s| s.server_id.clone()).unwrap_or_default();
                    egui::ComboBox::from_id_salt("rule-server")
                        .selected_text(if r.server_id.is_empty() { "any server".to_string() } else { server_name(config, &r.server_id) })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut r.server_id, String::new(), "any server");
                            if !here.is_empty() {
                                ui.selectable_value(&mut r.server_id, here.clone(), server_name(config, &here));
                            }
                        });
                });
                ui.horizontal(|ui| {
                    if ui.button(RichText::new("Save rule").strong()).clicked() {
                        r.vmids = self.rule_vmids.split([',', ' ', ';']).filter_map(|v| v.trim().parse().ok()).collect();
                        let mut st = store.lock();
                        st.alert_rules.push(r.clone());
                        match st.save() {
                            Ok(()) => close = true,
                            Err(e) => actions.toast_err(format!("{e:#}")),
                        }
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });
        }
        if close {
            self.rule = None;
        }
        let rules = store.lock().alert_rules.clone();
        if rules.is_empty() {
            widgets::muted(ui, "No rules yet.");
        } else {
            let mut toggled: Option<(String, bool)> = None;
            egui::Grid::new("rules-grid").striped(true).num_columns(4).spacing([14.0, 6.0]).show(ui, |ui| {
                for r in rules {
                    let mut on = r.enabled;
                    if ui.checkbox(&mut on, "").changed() {
                        toggled = Some((r.id.clone(), on));
                    }
                    ui.label(r.label());
                    let scope = if r.vmids.is_empty() {
                        "all running guests".to_string()
                    } else {
                        r.vmids.iter().map(u64::to_string).collect::<Vec<_>>().join(", ")
                    };
                    let server = if r.server_id.is_empty() { "any server".to_string() } else { server_name(config, &r.server_id) };
                    ui.label(format!("{scope} · {server}"));
                    if ui.small_button(RichText::new("Delete").color(theme::err())).clicked() {
                        self.confirm =
                            Some(Confirm::new("Delete rule", format!("Delete the rule \"{}\"?", r.label()), Del::Rule(r.id.clone())));
                    }
                    ui.end_row();
                }
            });
            if let Some((id, on)) = toggled {
                let mut st = store.lock();
                if let Some(r) = st.alert_rules.iter_mut().find(|r| r.id == id) {
                    r.enabled = on;
                }
                if let Err(e) = st.save() {
                    actions.toast_err(format!("{e:#}"));
                }
            }
        }
        if !monitor.recent.is_empty() {
            widgets::section(ui, "Recently resolved");
            let recent: Vec<_> = monitor.recent.iter().rev().take(50).cloned().collect();
            alert_table(ui, "recent", &recent, shared, actions);
        }
    }
}

fn server_name(config: &AppConfig, id: &str) -> String {
    config.profile(id).map(|p| p.display_name()).unwrap_or_else(|| "(deleted server)".into())
}

fn alert_table(ui: &mut egui::Ui, id: &str, list: &[crate::automation::Alert], shared: Option<&Shared>, actions: &mut Actions) {
    egui::Grid::new(format!("alerts-{id}")).striped(true).num_columns(5).spacing([14.0, 4.0]).show(ui, |ui| {
        for h in ["Guest", "Rule", "Value", "Since", "Resolved"] {
            ui.label(RichText::new(h).strong());
        }
        ui.end_row();
        for a in list {
            let text = format!("{}  {}", a.vmid, a.name);
            match shared.and_then(|s| s.guests().find(|g| g.vmid == a.vmid)).and_then(|g| g.kind()) {
                Some(k) => {
                    if ui.link(text).clicked() {
                        actions.nav(super::Nav::Guest(k, a.vmid));
                    }
                }
                None => {
                    ui.label(text);
                }
            }
            ui.label(&a.rule);
            let color = if a.resolved.is_some() { theme::muted() } else { theme::err() };
            ui.label(RichText::new(format!("{:.1} {}", a.value, a.unit)).color(color));
            ui.label(fmt_epoch(a.since));
            ui.label(a.resolved.map(fmt_epoch).unwrap_or_default());
            ui.end_row();
        }
    });
}
