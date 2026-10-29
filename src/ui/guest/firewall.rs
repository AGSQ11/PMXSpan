//! Guest firewall: options, rules (add / toggle / delete / reorder) and log.

use super::GCtx;
use crate::api::Params;
use crate::api::types::FirewallRule;
use crate::rt::Fetch;
use crate::ui::widgets::{self, Confirm};
use crate::ui::{Actions, theme};
use crate::util::ValueExt;
use egui::RichText;
use serde_json::Value;

#[derive(Clone)]
struct RuleForm {
    rtype: String,
    action: String,
    macro_: String,
    proto: String,
    source: String,
    dest: String,
    sport: String,
    dport: String,
    iface: String,
    comment: String,
    enable: bool,
    log: String,
}

impl Default for RuleForm {
    fn default() -> Self {
        Self {
            rtype: "in".into(),
            action: "ACCEPT".into(),
            macro_: String::new(),
            proto: "tcp".into(),
            source: String::new(),
            dest: String::new(),
            sport: String::new(),
            dport: String::new(),
            iface: String::new(),
            comment: String::new(),
            enable: true,
            log: "nolog".into(),
        }
    }
}

impl RuleForm {
    fn params(&self) -> Params {
        let mut p = Params::new().set("type", &self.rtype).set("action", &self.action).flag("enable", self.enable);
        if !self.macro_.is_empty() {
            p.push("macro", &self.macro_);
        } else if !self.proto.is_empty() {
            p.push("proto", &self.proto);
        }
        for (k, v) in [("source", &self.source), ("dest", &self.dest), ("iface", &self.iface), ("comment", &self.comment)] {
            if !v.trim().is_empty() {
                p.push(k, v.trim());
            }
        }
        if self.macro_.is_empty() && matches!(self.proto.as_str(), "tcp" | "udp") {
            if !self.sport.trim().is_empty() {
                p.push("sport", self.sport.trim());
            }
            if !self.dport.trim().is_empty() {
                p.push("dport", self.dport.trim());
            }
        }
        if self.log != "nolog" {
            p.push("log", &self.log);
        }
        p
    }
}

#[derive(Default)]
pub struct FirewallTab {
    options: Fetch<Value>,
    rules: Fetch<Vec<FirewallRule>>,
    macros: Fetch<Vec<String>>,
    log: Fetch<Vec<String>>,
    form: Option<RuleForm>,
    confirm_delete: Option<Confirm<u64>>,
    show_log: bool,
}

impl FirewallTab {
    pub fn ui(&mut self, ui: &mut egui::Ui, gc: &GCtx, _actions: &mut Actions) {
        let base = format!("{}/firewall", gc.g.path());
        if self.options.is_idle() || (self.options.is_stale() && !self.options.is_loading()) {
            let b = base.clone();
            self.options = gc.sh.fetch(move |c| async move { c.get(&format!("{b}/options")).await });
        }
        if self.rules.is_idle() || (self.rules.is_stale() && !self.rules.is_loading()) {
            let b = base.clone();
            self.rules = gc.sh.fetch(move |c| async move {
                let list = c.get_list(&format!("{b}/rules")).await?;
                Ok(list.iter().map(FirewallRule::from_value).collect())
            });
        }
        if self.macros.is_idle() {
            self.macros = gc.sh.fetch(|c| async move {
                let mut v: Vec<String> = c.get_list("/cluster/firewall/macros").await?.iter().map(|m| m.s("macro")).collect();
                v.sort();
                Ok(v)
            });
        }
        if let Some(pos) = widgets::confirm_ui(ui.ctx(), &mut self.confirm_delete) {
            let (c, b) = (gc.sh.client.clone(), base.clone());
            gc.sh.spawn(gc.title(&format!("Delete firewall rule {pos}")), move |_ctx| async move {
                c.delete(&format!("{b}/rules/{pos}"), &Params::new()).await?;
                Ok(format!("rule {pos} deleted"))
            });
        }

        widgets::section(ui, "Options");
        let mut opt_change: Option<Params> = None;
        widgets::fetch_ui(ui, &mut self.options, |ui, o| {
            ui.horizontal_wrapped(|ui| {
                let mut enable = o.b("enable");
                if ui.checkbox(&mut enable, "Firewall enabled").changed() {
                    opt_change = Some(Params::new().flag("enable", enable));
                }
                for (k, label) in [("macfilter", "MAC filter"), ("ipfilter", "IP filter"), ("dhcp", "Allow DHCP"), ("ndp", "Allow NDP")] {
                    let mut v = o.b(k);
                    if ui.checkbox(&mut v, label).changed() {
                        opt_change = Some(Params::new().flag(k, v));
                    }
                }
            });
            ui.horizontal(|ui| {
                for (k, label) in [("policy_in", "Input policy"), ("policy_out", "Output policy")] {
                    let cur = o.opt_s(k).unwrap_or_else(|| if k == "policy_in" { "DROP".into() } else { "ACCEPT".into() });
                    ui.label(label);
                    egui::ComboBox::from_id_salt(k).selected_text(cur.clone()).show_ui(ui, |ui| {
                        for pol in ["ACCEPT", "DROP", "REJECT"] {
                            if ui.selectable_label(cur == pol, pol).clicked() && cur != pol {
                                opt_change = Some(Params::new().set(k, pol));
                            }
                        }
                    });
                }
            });
            if !o.b("enable") {
                widgets::muted(ui, "Rules are not enforced while the firewall is disabled (and the NIC needs firewall=1).");
            }
        });
        if let Some(p) = opt_change {
            let (c, b) = (gc.sh.client.clone(), base.clone());
            gc.sh.spawn(gc.title("Firewall options"), move |_ctx| async move {
                c.put(&format!("{b}/options"), &p).await?;
                Ok("firewall options updated".into())
            });
        }

        ui.horizontal(|ui| {
            widgets::section(ui, "Rules");
            if ui.button("＋ Add rule").clicked() {
                self.form = Some(RuleForm::default());
            }
        });
        if let Some(f) = self.form.as_mut() {
            let macros = self.macros.ok().cloned().unwrap_or_default();
            let mut submit = false;
            let mut cancel = false;
            crate::ui::theme::card_frame().inner_margin(egui::Margin::symmetric(14, 12)).show(ui, |ui| {
                egui::Grid::new("fw-form").num_columns(4).spacing([10.0, 6.0]).show(ui, |ui| {
                    ui.label("Direction");
                    egui::ComboBox::from_id_salt("fw-dir").selected_text(f.rtype.clone()).show_ui(ui, |ui| {
                        for d in ["in", "out"] {
                            ui.selectable_value(&mut f.rtype, d.to_string(), d);
                        }
                    });
                    ui.label("Action");
                    egui::ComboBox::from_id_salt("fw-act").selected_text(f.action.clone()).show_ui(ui, |ui| {
                        for a in ["ACCEPT", "DROP", "REJECT"] {
                            ui.selectable_value(&mut f.action, a.to_string(), a);
                        }
                    });
                    ui.end_row();
                    ui.label("Macro");
                    let mut m = (!f.macro_.is_empty()).then(|| f.macro_.clone());
                    widgets::combo_opt(ui, "fw-macro", &mut m, &macros, "none", 160.0);
                    f.macro_ = m.unwrap_or_default();
                    ui.label("Protocol");
                    ui.add_enabled_ui(f.macro_.is_empty(), |ui| {
                        egui::ComboBox::from_id_salt("fw-proto")
                            .selected_text(if f.proto.is_empty() { "any".into() } else { f.proto.clone() })
                            .show_ui(ui, |ui| {
                                for p in ["", "tcp", "udp", "icmp", "ipv6-icmp"] {
                                    ui.selectable_value(&mut f.proto, p.to_string(), if p.is_empty() { "any" } else { p });
                                }
                            });
                    });
                    ui.end_row();
                    ui.label("Source");
                    ui.add(egui::TextEdit::singleline(&mut f.source).hint_text("any / CIDR / ipset").desired_width(160.0));
                    ui.label("Destination");
                    ui.add(egui::TextEdit::singleline(&mut f.dest).hint_text("any").desired_width(160.0));
                    ui.end_row();
                    ui.label("Src port");
                    ui.add(egui::TextEdit::singleline(&mut f.sport).desired_width(100.0));
                    ui.label("Dst port");
                    ui.add(egui::TextEdit::singleline(&mut f.dport).hint_text("22 or 8000:8100").desired_width(120.0));
                    ui.end_row();
                    ui.label("Interface");
                    ui.add(egui::TextEdit::singleline(&mut f.iface).hint_text("net0").desired_width(80.0));
                    ui.label("Log level");
                    egui::ComboBox::from_id_salt("fw-log").selected_text(f.log.clone()).show_ui(ui, |ui| {
                        for l in ["nolog", "info", "notice", "warning", "err"] {
                            ui.selectable_value(&mut f.log, l.to_string(), l);
                        }
                    });
                    ui.end_row();
                    ui.label("Comment");
                    ui.add(egui::TextEdit::singleline(&mut f.comment).desired_width(260.0));
                    ui.checkbox(&mut f.enable, "Enabled");
                    ui.end_row();
                });
                ui.horizontal(|ui| {
                    submit = ui.button(RichText::new("Add rule").strong()).clicked();
                    cancel = ui.button("Cancel").clicked();
                });
            });
            if submit {
                let (c, b, p) = (gc.sh.client.clone(), base.clone(), f.params());
                gc.sh.spawn(gc.title("Add firewall rule"), move |_ctx| async move {
                    c.post(&format!("{b}/rules"), &p).await?;
                    Ok("firewall rule added".into())
                });
                self.form = None;
            } else if cancel {
                self.form = None;
            }
        }

        let mut toggle: Option<(u64, bool)> = None;
        let mut moveto: Option<(u64, u64)> = None;
        let mut del = None;
        widgets::fetch_ui(ui, &mut self.rules, |ui, rules| {
            if rules.is_empty() {
                widgets::muted(ui, "No rules.");
                return;
            }
            let n = rules.len() as u64;
            egui::Grid::new("fw-rules").striped(true).num_columns(9).spacing([10.0, 4.0]).show(ui, |ui| {
                for h in ["#", "On", "Dir", "Action", "Macro/Proto", "Source", "Dest", "Port", "Comment"] {
                    ui.label(RichText::new(h).strong());
                }
                ui.end_row();
                for r in rules.iter() {
                    ui.horizontal(|ui| {
                        ui.label(r.pos.to_string());
                        if r.pos > 0 && ui.small_button("▲").clicked() {
                            moveto = Some((r.pos, r.pos - 1));
                        }
                        if r.pos + 1 < n && ui.small_button("▼").clicked() {
                            moveto = Some((r.pos, r.pos + 1));
                        }
                    });
                    let mut en = r.enable;
                    if ui.checkbox(&mut en, "").changed() {
                        toggle = Some((r.pos, en));
                    }
                    ui.label(&r.rtype);
                    let color = if r.action == "ACCEPT" { theme::ok() } else { theme::err() };
                    ui.label(RichText::new(&r.action).color(color));
                    ui.label(if r.macro_.is_empty() { r.proto.clone() } else { r.macro_.clone() });
                    ui.label(&r.source);
                    ui.label(&r.dest);
                    ui.label(&r.dport);
                    ui.horizontal(|ui| {
                        ui.label(&r.comment);
                        if ui.small_button(RichText::new("✖").color(theme::err())).on_hover_text("Delete rule").clicked() {
                            del = Some(r.pos);
                        }
                    });
                    ui.end_row();
                }
            });
        });
        if let Some(pos) = del {
            self.confirm_delete = Some(Confirm::new("Delete rule", format!("Delete firewall rule #{pos}?"), pos));
        }
        if let Some((pos, en)) = toggle {
            let (c, b) = (gc.sh.client.clone(), base.clone());
            gc.sh.spawn(gc.title(&format!("Toggle rule {pos}")), move |_ctx| async move {
                c.put(&format!("{b}/rules/{pos}"), &Params::new().flag("enable", en)).await?;
                Ok(format!("rule {pos} {}", if en { "enabled" } else { "disabled" }))
            });
        }
        if let Some((pos, to)) = moveto {
            let (c, b) = (gc.sh.client.clone(), base.clone());
            gc.sh.spawn(gc.title(&format!("Move rule {pos}")), move |_ctx| async move {
                c.put(&format!("{b}/rules/{pos}"), &Params::new().set("moveto", if to > pos { to + 1 } else { to })).await?;
                Ok(format!("rule moved to {to}"))
            });
        }

        ui.add_space(6.0);
        if ui.checkbox(&mut self.show_log, "Show firewall log").changed() && self.show_log {
            self.log = Fetch::default();
        }
        if self.show_log {
            if self.log.is_idle() {
                let b = base.clone();
                self.log = gc.sh.fetch(move |c| async move {
                    let list = c.get_q(&format!("{b}/log"), &Params::new().set("limit", 200)).await?;
                    Ok(list.arr().iter().map(|l| l.s("t")).collect())
                });
            }
            if ui.small_button("⟳ Reload log").clicked() {
                self.log = Fetch::default();
            }
            widgets::fetch_ui(ui, &mut self.log, |ui, lines| {
                egui::ScrollArea::vertical().id_salt("fw-log").max_height(260.0).show(ui, |ui| {
                    for l in lines.iter() {
                        ui.label(RichText::new(l).monospace().size(11.5));
                    }
                    if lines.is_empty() {
                        widgets::muted(ui, "Log is empty (set a rule's log level to record matches).");
                    }
                });
            });
        }
    }
}
