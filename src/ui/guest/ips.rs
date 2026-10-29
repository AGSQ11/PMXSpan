//! IP addresses tab: per-interface IPAM addresses with a main address per
//! family, assign/release/make-main, new interfaces with addresses, adoption
//! of existing addresses, and the anti-spoofing IP filter.

use super::{GCtx, value_str};
use crate::api::types::GuestKind;
use crate::ipam::Assignment;
use crate::ops::ipassign::{self, ApplyOpts, AssignSpec, IpFilter, NewInterface};
use crate::rt::Fetch;
use crate::ui::widgets::{self, Confirm};
use crate::ui::{Actions, theme};
use crate::util::{PropString, ValueExt};
use egui::RichText;
use serde_json::Value;

#[derive(Clone)]
struct AssignForm {
    v6: bool,
    pool: Option<String>,
    count: usize,
    preferred: String,
    make_main: bool,
}

impl Default for AssignForm {
    fn default() -> Self {
        Self { v6: false, pool: None, count: 1, preferred: String::new(), make_main: false }
    }
}

struct IfaceForm {
    bridge: String,
    pool4: Option<String>,
    pool6: Option<String>,
    count4: usize,
    count6: usize,
    preferred4: String,
    preferred6: String,
    model: String,
    firewall: bool,
}

impl Default for IfaceForm {
    fn default() -> Self {
        Self {
            bridge: String::new(),
            pool4: None,
            pool6: None,
            count4: 1,
            count6: 0,
            preferred4: String::new(),
            preferred6: String::new(),
            model: "virtio".into(),
            firewall: true,
        }
    }
}

#[derive(Clone)]
enum Act {
    Release(Vec<String>),
    RemoveIface(u32),
}

#[derive(Default)]
pub struct IpsTab {
    reboot: bool,
    fw: Fetch<Value>,
    assign: Option<(u32, AssignForm)>,
    new_iface: Option<IfaceForm>,
    confirm: Option<Confirm<Act>>,
}

impl IpsTab {
    fn opts(&self) -> ApplyOpts {
        ApplyOpts { reboot: self.reboot, ipfilter: IpFilter::Keep }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, gc: &GCtx, _actions: &mut Actions) {
        let Some(cfg) = gc.config else {
            ui.spinner();
            return;
        };
        let sh = gc.sh;
        let g = gc.g.clone();
        if self.fw.is_idle() || (self.fw.is_stale() && !self.fw.is_loading()) {
            let path = format!("{}/firewall/options", g.path());
            self.fw = sh.fetch(move |c| async move { c.get(&path).await });
        }
        if let Some(a) = widgets::confirm_ui(ui.ctx(), &mut self.confirm) {
            let (env, gr, opts) = (sh.env(), g.clone(), self.opts());
            match a {
                Act::Release(ips) => {
                    sh.spawn(gc.title(&format!("Release {}", ips.join(", "))), move |ctx| ipassign::release(env, ctx, gr, ips, opts));
                }
                Act::RemoveIface(n) => {
                    sh.spawn(gc.title(&format!("Remove net{n}")), move |ctx| ipassign::remove_interface(env, ctx, gr, n));
                }
            }
        }

        let assigned: Vec<Assignment> = sh.ipam.lock().guest_ips(&sh.server_id, g.vmid).into_iter().cloned().collect();
        let pools: Vec<(String, String, bool)> =
            sh.ipam.lock().pools.iter().map(|p| (p.id.clone(), format!("{} ({})", p.name, p.cidr), p.is_v6())).collect();
        let pool_label = |id: &str| pools.iter().find(|p| p.0 == id).map(|p| p.1.clone()).unwrap_or_else(|| "-".into());
        let qemu = g.kind == GuestKind::Qemu;

        widgets::muted(
            ui,
            "Each interface can hold several IPv4/IPv6 addresses from IPAM. The main address of each family is written into the guest's \
             network config (containers: live; VMs: cloud-init, at next boot). Additional addresses are allowed by the IP filter and \
             configured inside the guest.",
        );
        ui.horizontal_wrapped(|ui| {
            if qemu {
                ui.checkbox(&mut self.reboot, "Reboot the VM to apply cloud-init changes");
                ui.separator();
            }
            let ipfilter = self.fw.ok().map(|o| o.b("ipfilter"));
            let fw_on = self.fw.ok().map(|o| o.b("enable")).unwrap_or(false);
            ui.label("Anti-spoofing IP filter:");
            match ipfilter {
                Some(true) => {
                    ui.label(RichText::new("on").color(theme::OK));
                    if ui.small_button("Sync").on_hover_text("Rewrite ipfilter-netN with the current addresses").clicked() {
                        self.spawn_apply(gc, IpFilter::On);
                    }
                    if ui.small_button("Turn off").clicked() {
                        self.spawn_apply(gc, IpFilter::Off);
                    }
                }
                Some(false) => {
                    ui.label(RichText::new("off").color(theme::MUTED));
                    if ui.small_button("Turn on").clicked() {
                        self.spawn_apply(gc, IpFilter::On);
                    }
                }
                None => {
                    ui.spinner();
                }
            }
            if ipfilter == Some(true) && !fw_on {
                ui.label(RichText::new("(not enforced: the guest firewall is disabled)").color(theme::WARN));
            }
        });
        ui.add_space(4.0);

        let ifaces = ipassign::interfaces(cfg);
        if ifaces.is_empty() {
            widgets::warn_box(ui, "This guest has no network interfaces.");
        }
        let orphans: Vec<&Assignment> = assigned.iter().filter(|a| !ifaces.contains_key(&a.iface())).collect();
        for (n, nic) in &ifaces {
            let mine: Vec<&Assignment> = assigned.iter().filter(|a| a.iface() == *n).collect();
            egui::Frame::group(ui.style()).inner_margin(8.0).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("net{n}")).strong().size(15.0));
                    let mut desc = format!("bridge {}", nic.get("bridge").unwrap_or("?"));
                    if let Some(t) = nic.get("tag") {
                        desc.push_str(&format!(", VLAN {t}"));
                    }
                    let mac = nic.qemu_nic_model().map(|(_, m)| m.to_string()).or_else(|| nic.get("hwaddr").map(str::to_string));
                    if let Some(m) = mac {
                        desc.push_str(&format!(", MAC {m}"));
                    }
                    if nic.get("firewall") == Some("1") {
                        desc.push_str(", firewall");
                    }
                    widgets::muted(ui, desc);
                });
                let configured = if qemu { cfg.get(&format!("ipconfig{n}")).map(value_str) } else { Some(nic.to_string()) };
                let cp = PropString::parse(&configured.clone().unwrap_or_default());
                let shown: Vec<String> = ["ip", "gw", "ip6", "gw6"].iter().filter_map(|k| cp.get(k).map(|v| format!("{k}={v}"))).collect();
                widgets::muted(
                    ui,
                    format!(
                        "In guest config{}: {}",
                        if qemu { format!(" (ipconfig{n})") } else { String::new() },
                        if shown.is_empty() { "no addressing".to_string() } else { shown.join(", ") }
                    ),
                );
                if mine.is_empty() {
                    widgets::muted(ui, "No IPAM addresses on this interface.");
                } else {
                    egui::Grid::new(format!("ips-{n}")).striped(true).num_columns(5).spacing([14.0, 4.0]).show(ui, |ui| {
                        for a in &mine {
                            ui.label(RichText::new(&a.ip).monospace().strong());
                            ui.label(if a.is_v6() { "IPv6" } else { "IPv4" });
                            ui.label(pool_label(&a.pool_id));
                            if a.main {
                                ui.label(RichText::new("main").color(theme::OK).strong());
                            } else {
                                ui.label(RichText::new("additional").color(theme::MUTED));
                            }
                            ui.horizontal(|ui| {
                                if !a.main && ui.small_button("Make main").clicked() {
                                    let (env, gr, ip, opts) = (sh.env(), g.clone(), a.ip.clone(), self.opts());
                                    sh.spawn(gc.title(&format!("Main address {ip}")), move |ctx| ipassign::make_main(env, ctx, gr, ip, opts));
                                }
                                if ui.small_button(RichText::new("Release").color(theme::ERR)).clicked() {
                                    let body = if a.main {
                                        format!("Release {} from net{n}? It is the main address; another address on net{n} becomes main, or the guest falls back to DHCP.", a.ip)
                                    } else {
                                        format!("Release {} from net{n}?", a.ip)
                                    };
                                    self.confirm = Some(Confirm::new("Release address", body, Act::Release(vec![a.ip.clone()])).danger());
                                }
                            });
                            ui.end_row();
                        }
                    });
                }
                ui.horizontal(|ui| {
                    let open = self.assign.as_ref().map(|(i, _)| i == n).unwrap_or(false);
                    if !open && ui.button("＋ Assign addresses").clicked() {
                        self.assign = Some((*n, AssignForm::default()));
                    }
                    if ui.small_button(RichText::new("Remove interface").color(theme::ERR)).clicked() {
                        let ips: Vec<String> = mine.iter().map(|a| a.ip.clone()).collect();
                        let extra = if ips.is_empty() { String::new() } else { format!(" Its addresses {} are released.", ips.join(", ")) };
                        self.confirm = Some(Confirm::new("Remove interface", format!("Remove net{n} from the guest?{extra}"), Act::RemoveIface(*n)).danger());
                    }
                });
                if let Some((i, f)) = self.assign.as_mut()
                    && *i == *n
                {
                    let mut submit = false;
                    let mut cancel = false;
                    ui.horizontal_wrapped(|ui| {
                        ui.radio_value(&mut f.v6, false, "IPv4");
                        ui.radio_value(&mut f.v6, true, "IPv6");
                        let fam: Vec<&(String, String, bool)> = pools.iter().filter(|p| p.2 == f.v6).collect();
                        egui::ComboBox::from_id_salt(format!("as-pool-{n}"))
                            .selected_text(f.pool.as_ref().map(|id| pool_label(id)).unwrap_or_else(|| "any matching pool".into()))
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut f.pool, None, "any matching pool");
                                for p in fam {
                                    ui.selectable_value(&mut f.pool, Some(p.0.clone()), &p.1);
                                }
                            });
                        ui.add(egui::DragValue::new(&mut f.count).range(1..=64).prefix("count "));
                        ui.add(egui::TextEdit::singleline(&mut f.preferred).hint_text("exact IP (optional)").desired_width(170.0));
                        let has_main = mine.iter().any(|a| a.main && a.is_v6() == f.v6);
                        if has_main {
                            ui.checkbox(&mut f.make_main, "make it the main address");
                        }
                        submit = ui.button(RichText::new("Assign").strong()).clicked();
                        cancel = ui.button("Cancel").clicked();
                    });
                    if submit {
                        let spec = AssignSpec {
                            iface: *n,
                            v6: f.v6,
                            pool_id: f.pool.clone(),
                            preferred: Some(f.preferred.trim().to_string()).filter(|s| !s.is_empty()),
                            count: f.count,
                            make_main: f.make_main,
                        };
                        let (env, gr, opts) = (sh.env(), g.clone(), ApplyOpts { reboot: self.reboot, ipfilter: IpFilter::Keep });
                        sh.spawn(gc.title(&format!("Assign addresses to net{n}")), move |ctx| ipassign::assign(env, ctx, gr, spec, opts));
                        cancel = true;
                    }
                    if cancel {
                        self.assign = None;
                    }
                }
            });
            ui.add_space(4.0);
        }
        if !orphans.is_empty() {
            let ips: Vec<String> = orphans.iter().map(|a| a.ip.clone()).collect();
            widgets::warn_box(ui, &format!("IPAM lists {} for interfaces this guest no longer has.", ips.join(", ")));
            if ui.button("Release them").clicked() {
                self.confirm = Some(Confirm::new("Release addresses", format!("Release {}?", ips.join(", ")), Act::Release(ips)));
            }
        }

        ui.horizontal_wrapped(|ui| {
            if self.new_iface.is_none() && ui.button("＋ Add interface with addresses").clicked() {
                self.new_iface = Some(IfaceForm::default());
            }
            if ui
                .button("Re-apply IPAM to guest config")
                .on_hover_text("Write the main addresses into the guest network config again")
                .clicked()
            {
                self.spawn_apply(gc, IpFilter::Keep);
            }
            if ui
                .button("Adopt addresses from guest config")
                .on_hover_text("Record static addresses already configured on the guest into IPAM (they must lie in a pool and be free)")
                .clicked()
            {
                let (env, gr) = (sh.env(), g.clone());
                sh.spawn(gc.title("Adopt addresses"), move |ctx| ipassign::adopt(env, ctx, gr));
            }
        });

        let mut submit = None;
        let mut cancel = false;
        if let Some(f) = self.new_iface.as_mut() {
            widgets::section(ui, "New interface");
            egui::Grid::new("new-iface").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                ui.label("Bridge");
                ui.add(egui::TextEdit::singleline(&mut f.bridge).hint_text("empty = the pool's bridge").desired_width(160.0));
                ui.end_row();
                for v6 in [false, true] {
                    ui.label(if v6 { "IPv6" } else { "IPv4" });
                    ui.horizontal(|ui| {
                        let (pool, count, pref) = if v6 {
                            (&mut f.pool6, &mut f.count6, &mut f.preferred6)
                        } else {
                            (&mut f.pool4, &mut f.count4, &mut f.preferred4)
                        };
                        ui.add(egui::DragValue::new(count).range(0..=64).prefix("count "));
                        egui::ComboBox::from_id_salt(format!("ni-pool-{v6}"))
                            .selected_text(pool.as_ref().map(|id| pool_label(id)).unwrap_or_else(|| "any matching pool".into()))
                            .show_ui(ui, |ui| {
                                ui.selectable_value(pool, None, "any matching pool");
                                for p in pools.iter().filter(|p| p.2 == v6) {
                                    ui.selectable_value(pool, Some(p.0.clone()), &p.1);
                                }
                            });
                        ui.add(egui::TextEdit::singleline(pref).hint_text("exact IP (optional)").desired_width(170.0));
                    });
                    ui.end_row();
                }
                if qemu {
                    ui.label("Model");
                    egui::ComboBox::from_id_salt("ni-model").selected_text(f.model.clone()).show_ui(ui, |ui| {
                        for m in ["virtio", "e1000", "e1000e", "vmxnet3", "rtl8139"] {
                            ui.selectable_value(&mut f.model, m.to_string(), m);
                        }
                    });
                    ui.end_row();
                }
                ui.label("");
                ui.checkbox(&mut f.firewall, "firewall");
                ui.end_row();
            });
            ui.horizontal(|ui| {
                if ui.button(RichText::new("Add interface").strong()).clicked() {
                    submit = Some(NewInterface {
                        bridge: Some(f.bridge.trim().to_string()).filter(|s| !s.is_empty()),
                        pool4: f.pool4.clone(),
                        pool6: f.pool6.clone(),
                        count4: f.count4,
                        count6: f.count6,
                        preferred4: Some(f.preferred4.trim().to_string()).filter(|s| !s.is_empty()),
                        preferred6: Some(f.preferred6.trim().to_string()).filter(|s| !s.is_empty()),
                        model: f.model.clone(),
                        firewall: f.firewall,
                    });
                }
                cancel = ui.button("Cancel").clicked();
            });
        }
        if let Some(spec) = submit {
            let (env, gr, opts) = (sh.env(), g.clone(), self.opts());
            sh.spawn(gc.title("Add interface"), move |ctx| ipassign::add_interface(env, ctx, gr, spec, opts));
            cancel = true;
        }
        if cancel {
            self.new_iface = None;
        }
    }

    fn spawn_apply(&mut self, gc: &GCtx, filter: IpFilter) {
        let (env, gr) = (gc.sh.env(), gc.g.clone());
        let opts = ApplyOpts { reboot: self.reboot, ipfilter: filter };
        let what = match filter {
            IpFilter::Keep => "Apply IPAM addresses",
            IpFilter::On => "Enable IP filter",
            IpFilter::Off => "Disable IP filter",
        };
        gc.sh.spawn(gc.title(what), move |ctx| async move { ipassign::apply(&env, &ctx, &gr, opts, &Default::default()).await });
        self.fw = Fetch::default();
    }
}
