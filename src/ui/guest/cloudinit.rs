//! Cloud-Init (QEMU): user, password, SSH keys, per-NIC addressing, DNS.
//! Ported from the module's `_cloudinit_*` helpers, including its rule that a
//! working ipconfig is never replaced by an empty one.

use super::{GCtx, value_str};
use crate::api::Params;
use crate::api::types::is_net_key;
use crate::ipam::AllocRequest;
use crate::ops::create::encode_sshkeys;
use crate::ops::guest;
use crate::ui::{Actions, theme, widgets};
use crate::util::PropString;
use egui::RichText;

#[derive(Default)]
pub struct CloudInitTab {
    loaded: Option<u64>,
    ciuser: String,
    password: String,
    show_pw: bool,
    sshkeys: String,
    nameserver: String,
    searchdomain: String,
    ipconfig: Vec<(String, String)>,
    ipam_pool: Option<String>,
    ipam_preferred: String,
    regenerate: bool,
}

impl CloudInitTab {
    fn load(&mut self, gc: &GCtx) {
        let Some(cfg) = gc.config else { return };
        let stamp = super::config_stamp(cfg);
        if self.loaded == Some(stamp) {
            return;
        }
        self.loaded = Some(stamp);
        let s = |k: &str| cfg.get(k).map(value_str).unwrap_or_default();
        self.ciuser = s("ciuser");
        self.password.clear();
        self.sshkeys = urlencoding::decode(&s("sshkeys")).map(|c| c.into_owned()).unwrap_or_default();
        self.nameserver = s("nameserver");
        self.searchdomain = s("searchdomain");
        let mut nets: Vec<String> = cfg.keys().filter(|k| is_net_key(k)).cloned().collect();
        nets.sort();
        self.ipconfig = nets
            .iter()
            .map(|n| {
                let idx = n.trim_start_matches("net");
                let key = format!("ipconfig{idx}");
                let v = s(&key);
                (key, v)
            })
            .collect();
        self.regenerate = true;
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, gc: &GCtx, actions: &mut Actions) {
        self.load(gc);
        let Some(cfg) = gc.config else {
            ui.spinner();
            return;
        };
        let drive = cfg.iter().find(|(_, v)| value_str(v).contains("cloudinit")).map(|(k, _)| k.clone());
        match &drive {
            Some(d) => widgets::muted(ui, format!("Cloud-init drive: {d}. Changes apply on next boot (after regeneration).")),
            None => {
                widgets::warn_box(ui, "This VM has no cloud-init drive. Add one to use these settings.");
                let storages = gc.sh.storages_with(&gc.g.node, "images");
                ui.horizontal(|ui| {
                    for s in storages.iter().take(4) {
                        if ui.button(format!("Add cloud-init drive on {s}")).clicked() {
                            let slot = if !cfg.contains_key("ide2") {
                                "ide2".to_string()
                            } else {
                                guest::free_disk_slot(cfg, "ide").unwrap_or("sata5".into())
                            };
                            let (c, g, v) = (gc.sh.client.clone(), gc.g.clone(), format!("{s}:cloudinit"));
                            gc.sh.spawn(gc.title("Add cloud-init drive"), move |ctx| async move {
                                guest::set_config(&c, &ctx, &g, Params::new().set(&slot, v)).await?;
                                Ok(format!("cloud-init drive added at {slot}"))
                            });
                        }
                    }
                });
                return;
            }
        }

        egui::Grid::new("ci-form").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
            ui.label("User");
            ui.add(egui::TextEdit::singleline(&mut self.ciuser).hint_text("image default (e.g. debian, ubuntu)"));
            ui.end_row();
            ui.label("Password");
            ui.vertical(|ui| {
                widgets::password_field(ui, &mut self.password, &mut self.show_pw, "ci-pw");
                widgets::muted(
                    ui,
                    if cfg.contains_key("cipassword") { "A password is set. Leave empty to keep it." } else { "No password set." },
                );
            });
            ui.end_row();
            ui.label("SSH public keys");
            ui.add(egui::TextEdit::multiline(&mut self.sshkeys).desired_rows(3).desired_width(560.0).font(egui::TextStyle::Monospace));
            ui.end_row();
            ui.label("DNS servers");
            ui.add(egui::TextEdit::singleline(&mut self.nameserver).hint_text("use host settings"));
            ui.end_row();
            ui.label("Search domain");
            ui.add(egui::TextEdit::singleline(&mut self.searchdomain).hint_text("use host settings"));
            ui.end_row();
            for (key, val) in self.ipconfig.iter_mut() {
                ui.label(format!("{key} (net{})", key.trim_start_matches("ipconfig")));
                ui.vertical(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(val).desired_width(560.0).hint_text("ip=dhcp | ip=192.0.2.5/24,gw=192.0.2.1,ip6=auto"),
                    );
                    ui.horizontal(|ui| {
                        if ui.small_button("DHCP").clicked() {
                            let mut p = PropString::parse(val);
                            p.set("ip", "dhcp");
                            p.remove("gw");
                            *val = p.to_string();
                        }
                        if ui.small_button("SLAAC v6").clicked() {
                            let mut p = PropString::parse(val);
                            p.set("ip6", "auto");
                            p.remove("gw6");
                            *val = p.to_string();
                        }
                        for v6 in [false, true] {
                            if ui.small_button(if v6 { "IPv6 from IPAM" } else { "IPv4 from IPAM" }).clicked() {
                                let net_key = format!("net{}", key.trim_start_matches("ipconfig"));
                                let bridge = cfg
                                    .get(&net_key)
                                    .map(value_str)
                                    .and_then(|n| PropString::parse(&n).get("bridge").map(str::to_string))
                                    .unwrap_or_default();
                                let req = AllocRequest {
                                    server_id: gc.sh.server_id.clone(),
                                    vmid: gc.g.vmid,
                                    hostname: gc.res.name.clone(),
                                    bridge,
                                    v6,
                                    preferred: Some(self.ipam_preferred.clone()).filter(|s| !s.trim().is_empty()),
                                    pool_id: self.ipam_pool.clone(),
                                    iface: key.trim_start_matches("ipconfig").parse().unwrap_or(0),
                                };
                                match gc.sh.ipam.lock().allocate(&req) {
                                    Ok(a) => {
                                        let mut p = PropString::parse(val);
                                        p.set(if v6 { "ip6" } else { "ip" }, &a.cidr);
                                        p.set_opt(if v6 { "gw6" } else { "gw" }, Some(a.gateway.clone()));
                                        *val = p.to_string();
                                        if self.nameserver.is_empty() && !a.dns.is_empty() {
                                            self.nameserver = a.dns.clone();
                                        }
                                        actions.toast_ok(format!("IPAM assigned {} — save to apply", a.ip));
                                    }
                                    Err(e) => actions.toast_err(format!("IPAM: {e:#}")),
                                }
                            }
                        }
                    });
                });
                ui.end_row();
            }
            ui.label("IPAM options");
            ui.horizontal(|ui| {
                let pools: Vec<(String, String)> =
                    gc.sh.ipam.lock().pools.iter().map(|p| (p.id.clone(), format!("{} ({})", p.name, p.cidr))).collect();
                egui::ComboBox::from_id_salt("ci-ipam-pool")
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
                ui.add(egui::TextEdit::singleline(&mut self.ipam_preferred).hint_text("preferred IP (exact)").desired_width(160.0));
            });
            ui.end_row();
        });
        ui.checkbox(&mut self.regenerate, "Regenerate the cloud-init image after saving");
        ui.horizontal(|ui| {
            if ui.button(RichText::new("Save cloud-init").strong()).clicked() {
                let mut p = Params::new();
                let mut deletes = Vec::new();
                let cur = |k: &str| cfg.get(k).map(value_str).unwrap_or_default();
                for (k, v, cur_v) in [
                    ("ciuser", self.ciuser.trim().to_string(), cur("ciuser")),
                    ("nameserver", self.nameserver.trim().to_string(), cur("nameserver")),
                    ("searchdomain", self.searchdomain.trim().to_string(), cur("searchdomain")),
                ] {
                    if v != cur_v {
                        if v.is_empty() {
                            deletes.push(k.to_string());
                        } else {
                            p.push(k, v);
                        }
                    }
                }
                if !self.password.is_empty() {
                    p.push("cipassword", &self.password);
                }
                match encode_sshkeys(&self.sshkeys) {
                    Some(k) if k != cur("sshkeys") => p.push("sshkeys", k),
                    None if !cur("sshkeys").is_empty() => deletes.push("sshkeys".into()),
                    _ => {}
                }
                for (k, v) in &self.ipconfig {
                    if v.trim() != cur(k) {
                        if v.trim().is_empty() {
                            // Never trade a working config for an empty one silently.
                            actions.toast_err(format!("{k} left unchanged: clearing it would leave the guest without an address"));
                        } else {
                            p.push(k, v.trim());
                        }
                    }
                }
                if !deletes.is_empty() {
                    p.push("delete", deletes.join(","));
                }
                let (c, g, regen) = (gc.sh.client.clone(), gc.g.clone(), self.regenerate);
                gc.sh.spawn(gc.title("Update cloud-init"), move |ctx| async move {
                    guest::set_config(&c, &ctx, &g, p).await?;
                    if regen {
                        guest::regenerate_cloudinit(&c, &g).await?;
                        ctx.info("cloud-init image regenerated");
                    }
                    Ok("cloud-init updated; reboot the VM to apply".into())
                });
                self.password.clear();
            }
            if ui.button("Regenerate image only").clicked() {
                let (c, g) = (gc.sh.client.clone(), gc.g.clone());
                gc.sh.spawn(gc.title("Regenerate cloud-init"), move |_ctx| async move {
                    guest::regenerate_cloudinit(&c, &g).await?;
                    Ok("cloud-init image regenerated".into())
                });
            }
        });
        ui.add_space(6.0);
        ui.label(RichText::new("Applying requires a reboot of the VM.").color(theme::MUTED));
    }
}
