//! Network interfaces: bridge, VLAN, rate limit, firewall flag, MAC, and
//! (for containers) addresses — with IPAM assignment.

use super::{GCtx, value_str};
use crate::api::Params;
use crate::api::types::{GuestKind, is_net_key};
use crate::ipam::AllocRequest;
use crate::ops::guest;
use crate::rt::Fetch;
use crate::ui::widgets::{self, Confirm};
use crate::ui::{Actions, theme};
use crate::util::{PropString, ValueExt, generate_mac, validate_mac};
use egui::RichText;

#[derive(Clone, Default)]
struct NicForm {
    key: String,
    model: String,
    mac: String,
    bridge: String,
    vlan: String,
    rate: String,
    firewall: bool,
    link_down: bool,
    // LXC only
    name: String,
    ip: String,
    gw: String,
    ip6: String,
    gw6: String,
}

impl NicForm {
    fn parse(kind: GuestKind, key: &str, v: &str) -> Self {
        let p = PropString::parse(v);
        let (model, mac) = match kind {
            GuestKind::Qemu => p.qemu_nic_model().map(|(m, a)| (m.to_string(), a.to_string())).unwrap_or_default(),
            GuestKind::Lxc => (String::new(), p.get("hwaddr").unwrap_or("").to_string()),
        };
        let g = |k: &str| p.get(k).unwrap_or("").to_string();
        Self {
            key: key.to_string(),
            model,
            mac,
            bridge: g("bridge"),
            vlan: g("tag"),
            rate: g("rate"),
            firewall: g("firewall") == "1",
            link_down: g("link_down") == "1",
            name: g("name"),
            ip: g("ip"),
            gw: g("gw"),
            ip6: g("ip6"),
            gw6: g("gw6"),
        }
    }

    /// Rebuilds the property string, preserving unknown keys from `original`.
    fn build(&self, kind: GuestKind, original: &str) -> anyhow::Result<String> {
        if !self.mac.is_empty() && !validate_mac(&self.mac) {
            anyhow::bail!("'{}' is not a valid MAC address", self.mac);
        }
        if self.bridge.is_empty() {
            anyhow::bail!("a bridge is required");
        }
        let mut p = PropString::parse(original);
        match kind {
            GuestKind::Qemu => {
                if let Some((old_model, _)) = p.qemu_nic_model().map(|(m, a)| (m.to_string(), a.to_string())) {
                    p.remove(&old_model);
                }
                let mut parts = vec![(self.model.clone(), self.mac.clone())];
                parts.append(&mut p.parts);
                p.parts = parts;
                if self.mac.is_empty() {
                    // Let Proxmox generate one.
                    p.parts[0] = (String::new(), self.model.clone());
                }
            }
            GuestKind::Lxc => {
                p.set("name", if self.name.is_empty() { "eth0" } else { &self.name });
                p.set_opt("hwaddr", Some(self.mac.clone()));
                p.set_opt("ip", Some(self.ip.clone()));
                p.set_opt("gw", Some(self.gw.clone()));
                p.set_opt("ip6", Some(self.ip6.clone()));
                p.set_opt("gw6", Some(self.gw6.clone()));
            }
        }
        p.set("bridge", &self.bridge);
        p.set_opt("tag", Some(self.vlan.trim().to_string()));
        p.set_opt("rate", Some(self.rate.trim().to_string()));
        p.set_opt("firewall", self.firewall.then(|| "1".to_string()));
        p.set_opt("link_down", self.link_down.then(|| "1".to_string()));
        Ok(p.to_string())
    }
}

#[derive(Default)]
pub struct NetworkTab {
    editing: Option<NicForm>,
    bridges: Fetch<Vec<String>>,
    confirm_remove: Option<Confirm<String>>,
    ipam_pool: Option<String>,
    ipam_preferred: String,
}

impl NetworkTab {
    pub fn ui(&mut self, ui: &mut egui::Ui, gc: &GCtx, actions: &mut Actions) {
        let Some(cfg) = gc.config else {
            ui.spinner();
            return;
        };
        if self.bridges.is_idle() {
            let node = gc.g.node.clone();
            self.bridges = gc.sh.fetch(move |c| async move {
                let list = c.get_list(&format!("/nodes/{node}/network?type=any_bridge")).await?;
                let mut v: Vec<String> = list.iter().map(|i| i.s("iface")).collect();
                if let Ok(vnets) = c.get_list("/cluster/sdn/vnets").await {
                    v.extend(vnets.iter().map(|n| n.s("vnet")));
                }
                v.sort();
                v.dedup();
                Ok(v)
            });
        }
        if let Some(key) = widgets::confirm_ui(ui.ctx(), &mut self.confirm_remove) {
            // Removing an interface also releases its IPAM addresses and IP filter set.
            let (env, g) = (gc.sh.env(), gc.g.clone());
            let n: u32 = key.trim_start_matches("net").parse().unwrap_or(0);
            gc.sh.spawn(gc.title(&format!("Remove {key}")), move |ctx| crate::ops::ipassign::remove_interface(env, ctx, g, n));
        }
        let kind = gc.g.kind;
        let mut nics: Vec<(String, String)> = cfg.iter().filter(|(k, _)| is_net_key(k)).map(|(k, v)| (k.clone(), value_str(v))).collect();
        nics.sort_by_key(|(k, _)| k.trim_start_matches("net").parse::<u32>().unwrap_or(0));

        widgets::section(ui, "Interfaces");
        egui::Grid::new("nics").striped(true).num_columns(3).spacing([12.0, 6.0]).show(ui, |ui| {
            for (k, v) in &nics {
                ui.label(RichText::new(k).strong());
                ui.label(RichText::new(v).monospace());
                ui.horizontal(|ui| {
                    if ui.small_button("Edit").clicked() {
                        self.editing = Some(NicForm::parse(kind, k, v));
                    }
                    if ui.small_button(RichText::new("Remove").color(theme::err())).clicked() {
                        self.confirm_remove = Some(
                            Confirm::new(
                                "Remove interface",
                                format!("Remove {k} from this guest? Its IPAM addresses are released."),
                                k.clone(),
                            )
                            .danger(),
                        );
                    }
                });
                ui.end_row();
            }
        });
        if nics.is_empty() {
            widgets::muted(ui, "No network interfaces.");
        }
        if self.editing.is_none() && ui.button("＋ Add interface").clicked() {
            let idx = (0..32).find(|i| !cfg.contains_key(&format!("net{i}"))).unwrap_or(0);
            self.editing = Some(NicForm {
                key: format!("net{idx}"),
                model: "virtio".into(),
                mac: generate_mac(&gc.sh.settings.mac_prefix),
                bridge: gc.sh.settings.default_bridge.clone(),
                firewall: true,
                name: format!("eth{idx}"),
                ip: if kind == GuestKind::Lxc { "dhcp".into() } else { String::new() },
                ..Default::default()
            });
        }

        let bridges = self.bridges.ok().cloned().unwrap_or_default();
        let mut save = None;
        let mut cancel = false;
        if let Some(f) = self.editing.as_mut() {
            widgets::section(ui, &format!("Edit {}", f.key));
            egui::Grid::new("nic-form").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                if kind == GuestKind::Qemu {
                    ui.label("Model");
                    egui::ComboBox::from_id_salt("nic-model").selected_text(f.model.clone()).show_ui(ui, |ui| {
                        for m in ["virtio", "e1000", "e1000e", "rtl8139", "vmxnet3"] {
                            ui.selectable_value(&mut f.model, m.to_string(), m);
                        }
                    });
                    ui.end_row();
                } else {
                    ui.label("Name in guest");
                    ui.text_edit_singleline(&mut f.name);
                    ui.end_row();
                }
                ui.label("MAC address");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut f.mac).font(egui::TextStyle::Monospace).desired_width(170.0));
                    if ui.small_button("Generate").clicked() {
                        f.mac = generate_mac(&gc.sh.settings.mac_prefix);
                    }
                });
                ui.end_row();
                ui.label("Bridge");
                ui.horizontal(|ui| {
                    widgets::combo(ui, "nic-bridge", &mut f.bridge, &bridges, 140.0);
                    ui.add(egui::TextEdit::singleline(&mut f.bridge).desired_width(100.0));
                });
                ui.end_row();
                ui.label("VLAN tag");
                ui.add(egui::TextEdit::singleline(&mut f.vlan).desired_width(80.0).hint_text("none"));
                ui.end_row();
                ui.label("Rate limit");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut f.rate).desired_width(80.0).hint_text("unlimited"));
                    widgets::muted(ui, "MB/s (megabytes)");
                });
                ui.end_row();
                ui.label("Flags");
                ui.horizontal(|ui| {
                    ui.checkbox(&mut f.firewall, "Firewall");
                    ui.checkbox(&mut f.link_down, "Disconnected");
                });
                ui.end_row();
                if kind == GuestKind::Lxc {
                    ui.label("IPv4");
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut f.ip).hint_text("dhcp | 192.0.2.5/24").desired_width(170.0));
                        ui.label("gw");
                        ui.add(egui::TextEdit::singleline(&mut f.gw).desired_width(130.0));
                    });
                    ui.end_row();
                    ui.label("IPv6");
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut f.ip6).hint_text("auto | dhcp | 2001:db8::5/64").desired_width(170.0));
                        ui.label("gw");
                        ui.add(egui::TextEdit::singleline(&mut f.gw6).desired_width(130.0));
                    });
                    ui.end_row();
                    ui.label("From IPAM");
                    ui.horizontal(|ui| {
                        let pools: Vec<(String, String)> =
                            gc.sh.ipam.lock().pools.iter().map(|p| (p.id.clone(), format!("{} ({})", p.name, p.cidr))).collect();
                        egui::ComboBox::from_id_salt("nic-ipam")
                            .selected_text(
                                self.ipam_pool
                                    .as_ref()
                                    .and_then(|id| pools.iter().find(|p| &p.0 == id))
                                    .map(|p| p.1.clone())
                                    .unwrap_or("any matching pool".into()),
                            )
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut self.ipam_pool, None, "any matching pool");
                                for (id, label) in &pools {
                                    ui.selectable_value(&mut self.ipam_pool, Some(id.clone()), label);
                                }
                            });
                        ui.add(
                            egui::TextEdit::singleline(&mut self.ipam_preferred).hint_text("preferred IP (optional)").desired_width(150.0),
                        );
                        for v6 in [false, true] {
                            if ui.small_button(if v6 { "Assign IPv6" } else { "Assign IPv4" }).clicked() {
                                let req = AllocRequest {
                                    server_id: gc.sh.server_id.clone(),
                                    vmid: gc.g.vmid,
                                    hostname: gc.res.name.clone(),
                                    bridge: f.bridge.clone(),
                                    v6,
                                    preferred: Some(self.ipam_preferred.clone()).filter(|s| !s.trim().is_empty()),
                                    pool_id: self.ipam_pool.clone(),
                                    iface: f.key.trim_start_matches("net").parse().unwrap_or(0),
                                };
                                match gc.sh.ipam.lock().allocate(&req) {
                                    Ok(a) => {
                                        if v6 {
                                            f.ip6 = a.cidr;
                                            f.gw6 = a.gateway;
                                        } else {
                                            f.ip = a.cidr;
                                            f.gw = a.gateway;
                                        }
                                        actions.toast_ok(format!("IPAM assigned {} — apply to save", a.ip));
                                    }
                                    Err(e) => actions.toast_err(format!("IPAM: {e:#}")),
                                }
                            }
                        }
                    });
                    ui.end_row();
                }
            });
            ui.horizontal(|ui| {
                if ui.button(RichText::new("Apply").strong()).clicked() {
                    let original = cfg.get(&f.key).map(value_str).unwrap_or_default();
                    match f.build(kind, &original) {
                        Ok(line) => save = Some((f.key.clone(), line)),
                        Err(e) => actions.toast_err(format!("{e:#}")),
                    }
                }
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
            });
            if kind == GuestKind::Qemu {
                widgets::muted(ui, "VM addresses are configured in the Cloud-Init tab (or inside the guest).");
            }
        }
        if let Some((key, line)) = save {
            let (c, g) = (gc.sh.client.clone(), gc.g.clone());
            gc.sh.spawn(gc.title(&format!("Update {key}")), move |ctx| async move {
                guest::set_config(&c, &ctx, &g, Params::new().set(&key, &line)).await?;
                Ok(format!("{key} = {line}"))
            });
            self.editing = None;
        }
        if cancel {
            self.editing = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nic_roundtrip_keeps_unknown_keys() {
        let orig = "virtio=BC:24:11:00:00:01,bridge=vmbr0,queues=4,firewall=1";
        let mut f = NicForm::parse(GuestKind::Qemu, "net0", orig);
        assert_eq!(f.model, "virtio");
        assert!(f.firewall);
        f.vlan = "30".into();
        f.firewall = false;
        f.model = "e1000".into();
        assert_eq!(f.build(GuestKind::Qemu, orig).unwrap(), "e1000=BC:24:11:00:00:01,bridge=vmbr0,queues=4,tag=30");
        let lxc = "name=eth0,bridge=vmbr0,hwaddr=BC:24:11:00:00:02,ip=dhcp,type=veth";
        let mut f = NicForm::parse(GuestKind::Lxc, "net0", lxc);
        f.ip = "192.0.2.9/24".into();
        f.gw = "192.0.2.1".into();
        assert_eq!(
            f.build(GuestKind::Lxc, lxc).unwrap(),
            "name=eth0,bridge=vmbr0,hwaddr=BC:24:11:00:00:02,ip=192.0.2.9/24,type=veth,gw=192.0.2.1"
        );
    }
}
