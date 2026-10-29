//! Manage tab: password reset, migrate, clone, rebuild/reinstall, HA, pool,
//! template conversion, unlock and destroy.

use super::GCtx;
use crate::api::Params;
use crate::api::types::GuestKind;
use crate::ops::guest;
use crate::ops::rebuild::{self, RebuildSource, RebuildSpec};
use crate::rt::Fetch;
use crate::ui::widgets::{self, Confirm};
use crate::ui::{Actions, theme};
use crate::util::ValueExt;
use egui::RichText;

#[derive(Clone)]
enum Act {
    Destroy(bool),
    Rebuild,
    Template,
    Unlock,
}

#[derive(Default)]
pub struct ManageTab {
    pw_user: String,
    pw: String,
    show_pw: bool,
    mig_target: String,
    mig_online: bool,
    mig_storage: Option<String>,
    clone_id: Option<u64>,
    clone_name: String,
    clone_full: bool,
    clone_target: Option<String>,
    clone_storage: Option<String>,
    next_id: Fetch<u64>,
    rb_template: String,
    rb_catalog: bool,
    rb_catalog_id: Option<String>,
    rb_password: String,
    rb_show: bool,
    rb_keys: String,
    rb_start: Option<bool>,
    vztmpls: Fetch<Vec<String>>,
    purge: bool,
    pool: Option<String>,
    pool_loaded: bool,
    confirm: Option<Confirm<Act>>,
}

impl ManageTab {
    pub fn ui(&mut self, ui: &mut egui::Ui, gc: &GCtx, actions: &mut Actions) {
        let g = gc.g.clone();
        let sh = gc.sh;
        let qemu = g.kind == GuestKind::Qemu;
        if let Some(a) = widgets::confirm_ui(ui.ctx(), &mut self.confirm) {
            let c = sh.client.clone();
            let gr = g.clone();
            match a {
                Act::Destroy(purge) => {
                    let (ipam, sid) = (sh.ipam.clone(), sh.server_id.clone());
                    sh.spawn(gc.title("Destroy"), move |ctx| async move {
                        guest::destroy(&c, &ctx, &gr, purge, Some((&*ipam, sid.as_str()))).await
                    });
                }
                Act::Template => {
                    sh.spawn(gc.title("Convert to template"), move |ctx| async move { guest::convert_to_template(&c, &ctx, &gr).await });
                }
                Act::Unlock => {
                    sh.spawn(gc.title("Unlock"), move |ctx| async move { guest::unlock(&c, &ctx, &gr).await });
                }
                Act::Rebuild => {
                    let entry = self.rb_catalog_id.as_ref().and_then(|id| sh.catalog.lock().entries.iter().find(|e| &e.id == id).cloned());
                    let source = if let (true, Some(entry)) = (self.rb_catalog, entry) {
                        RebuildSource::Catalog { entry: Box::new(entry) }
                    } else if qemu {
                        let tid: u64 = self.rb_template.split_whitespace().next().and_then(|s| s.parse().ok()).unwrap_or(0);
                        let tnode = sh.find_guest(GuestKind::Qemu, tid).map(|r| r.node.clone()).unwrap_or_default();
                        RebuildSource::QemuTemplate { node: tnode, vmid: tid }
                    } else {
                        RebuildSource::LxcTemplate { volid: self.rb_template.clone() }
                    };
                    let spec = RebuildSpec {
                        guest: gr,
                        source,
                        password: self.rb_password.clone(),
                        ssh_keys: self.rb_keys.clone(),
                        start: self.rb_start,
                    };
                    let env = sh.env();
                    sh.spawn(gc.title("Rebuild"), move |ctx| rebuild::run(env, spec, ctx));
                    self.rb_password.clear();
                }
            }
        }
        if gc.res.template {
            widgets::info_box(ui, "This is a template. Use 'Deploy' from the guest list or the Create wizard to make guests from it.");
        }

        // ---- password ----
        if !gc.res.template {
            widgets::section(ui, "Reset password");
            if qemu {
                widgets::muted(ui, "Uses the guest agent when it responds; otherwise sets the cloud-init password (applies at next boot).");
                ui.horizontal(|ui| {
                    if self.pw_user.is_empty() {
                        self.pw_user = gc.cfg_str("ciuser");
                        if self.pw_user.is_empty() {
                            self.pw_user = "root".into();
                        }
                    }
                    ui.add(egui::TextEdit::singleline(&mut self.pw_user).desired_width(120.0));
                    widgets::password_field(ui, &mut self.pw, &mut self.show_pw, "reset-pw");
                    if ui.add_enabled(self.pw.len() >= 5, egui::Button::new("Reset")).clicked() {
                        let (c, gr, u, p) = (sh.client.clone(), g.clone(), self.pw_user.clone(), self.pw.clone());
                        sh.spawn(gc.title("Reset password"), move |ctx| async move { guest::reset_password(&c, &ctx, &gr, &u, &p).await });
                    }
                });
            } else {
                widgets::muted(
                    ui,
                    "Proxmox has no API to change a running container's password. Use the console (passwd) or rebuild with a new password.",
                );
            }
        }

        // ---- migrate ----
        widgets::section(ui, "Migrate");
        let nodes: Vec<String> = sh.online_nodes().into_iter().filter(|n| *n != g.node).collect();
        if nodes.is_empty() {
            widgets::muted(ui, "No other online node to migrate to.");
        } else {
            ui.horizontal_wrapped(|ui| {
                ui.label("Target node");
                widgets::combo(ui, "mig-node", &mut self.mig_target, &nodes, 140.0);
                ui.checkbox(&mut self.mig_online, if qemu { "online (live)" } else { "restart mode" });
                ui.label("target storage");
                let st = if self.mig_target.is_empty() {
                    Vec::new()
                } else {
                    sh.storages_with(&self.mig_target, if qemu { "images" } else { "rootdir" })
                };
                widgets::combo_opt(ui, "mig-storage", &mut self.mig_storage, &st, "same as source", 150.0);
                if ui.add_enabled(!self.mig_target.is_empty(), egui::Button::new("Migrate")).clicked() {
                    let (c, gr, t, o, s) =
                        (sh.client.clone(), g.clone(), self.mig_target.clone(), self.mig_online && gc.running(), self.mig_storage.clone());
                    sh.spawn(gc.title(&format!("Migrate to {t}")), move |ctx| async move {
                        guest::migrate(&c, &ctx, &gr, &t, o, s.as_deref()).await
                    });
                }
            });
        }

        // ---- clone ----
        widgets::section(ui, "Clone");
        if self.next_id.is_idle() {
            self.next_id = sh.fetch(|c| async move { guest::next_vmid(&c).await });
        }
        if self.clone_id.is_none() {
            self.clone_id = self.next_id.ok().copied();
        }
        ui.horizontal_wrapped(|ui| {
            ui.label("New VMID");
            let mut id = self.clone_id.unwrap_or(100);
            if ui.add(egui::DragValue::new(&mut id).range(100..=999_999_999)).changed() {
                self.clone_id = Some(id);
            }
            ui.label("Name");
            ui.add(egui::TextEdit::singleline(&mut self.clone_name).desired_width(160.0).hint_text(format!("{}-clone", gc.res.name)));
            ui.checkbox(&mut self.clone_full, "full clone");
            let all_nodes = sh.online_nodes();
            ui.label("node");
            widgets::combo_opt(ui, "clone-node", &mut self.clone_target, &all_nodes, "same node", 120.0);
            if self.clone_full {
                let node = self.clone_target.clone().unwrap_or(g.node.clone());
                let st = sh.storages_with(&node, if qemu { "images" } else { "rootdir" });
                ui.label("storage");
                widgets::combo_opt(ui, "clone-storage", &mut self.clone_storage, &st, "same as source", 140.0);
            }
            if ui.button("Clone").clicked() {
                let name =
                    if self.clone_name.trim().is_empty() { format!("{}-clone", gc.res.name) } else { self.clone_name.trim().to_string() };
                let name = crate::util::sanitize_hostname(&name);
                let (c, gr, id, full, t, s) =
                    (sh.client.clone(), g.clone(), id, self.clone_full, self.clone_target.clone(), self.clone_storage.clone());
                let pool = Some(gc.res.pool.clone()).filter(|p| !p.is_empty());
                sh.spawn(gc.title(&format!("Clone to {id}")), move |ctx| async move {
                    guest::clone_guest(&c, &ctx, &gr, id, &name, full, t.as_deref(), s.as_deref(), pool.as_deref()).await
                });
                self.clone_id = None;
                self.next_id = Fetch::default();
            }
        });
        if !gc.res.template {
            // Linked clones are only possible from templates.
            self.clone_full = true;
        }

        // ---- rebuild ----
        if !gc.res.template {
            widgets::section(ui, "Rebuild / reinstall");
            widgets::muted(
                ui,
                "Replaces the guest's disks with a fresh copy of a template or an OS catalog image. VMID, name, resources, \
                 NICs (MACs), IP addresses and pool are kept. ALL DATA ON THE GUEST'S DISKS IS LOST.",
            );
            ui.horizontal(|ui| {
                ui.radio_value(&mut self.rb_catalog, false, if qemu { "From a template VM" } else { "From a template on storage" });
                ui.radio_value(&mut self.rb_catalog, true, "From the OS image catalog");
            });
            let catalog_entries = sh.catalog.lock().for_virt(!qemu);
            if self.rb_catalog_id.as_ref().is_some_and(|id| !catalog_entries.iter().any(|e| &e.id == id)) {
                self.rb_catalog_id = None;
            }
            let options: Vec<String> = if qemu {
                sh.templates(GuestKind::Qemu).iter().map(|t| format!("{} {}", t.vmid, t.display_name())).collect()
            } else {
                if self.vztmpls.is_idle() {
                    let node = g.node.clone();
                    let storages = sh.storages_with(&node, "vztmpl");
                    self.vztmpls = sh.fetch(move |c| async move {
                        let mut out = Vec::new();
                        for s in storages {
                            if let Ok(v) =
                                c.get_q(&format!("/nodes/{node}/storage/{s}/content"), &Params::new().set("content", "vztmpl")).await
                            {
                                out.extend(v.arr().iter().map(|i| i.s("volid")));
                            }
                        }
                        out.sort();
                        Ok(out)
                    });
                }
                self.vztmpls.ok().cloned().unwrap_or_default()
            };
            egui::Grid::new("rb-form").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                if self.rb_catalog {
                    ui.label("Catalog image");
                    ui.vertical(|ui| {
                        if catalog_entries.is_empty() {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new("No matching images in the catalog.").color(theme::warn()));
                                if ui.link("Edit the OS image catalog →").clicked() {
                                    actions.nav(crate::ui::Nav::Catalog);
                                }
                            });
                        } else {
                            let label = self
                                .rb_catalog_id
                                .as_ref()
                                .and_then(|id| catalog_entries.iter().find(|e| &e.id == id))
                                .map(|e| e.label.clone())
                                .unwrap_or_else(|| "(choose image)".into());
                            egui::ComboBox::from_id_salt("rb-cat").selected_text(label).width(360.0).show_ui(ui, |ui| {
                                for e in &catalog_entries {
                                    ui.selectable_value(&mut self.rb_catalog_id, Some(e.id.clone()), &e.label).on_hover_text(&e.url);
                                }
                            });
                        }
                        if crate::ops::catalog::configured_storages(&sh.profile, !qemu).is_empty() {
                            ui.label(
                                RichText::new("No catalog download storage is set for this server (Servers → Edit).").color(theme::warn()),
                            );
                        } else {
                            widgets::muted(
                                ui,
                                "The image is downloaded before anything is touched; a failed download leaves the guest as it is.",
                            );
                        }
                    });
                } else {
                    ui.label(if qemu { "Template VM" } else { "Container template" });
                    widgets::combo(ui, "rb-tpl", &mut self.rb_template, &options, 360.0);
                }
                ui.end_row();
                ui.label(if qemu { "New password" } else { "Root password" });
                widgets::password_field(ui, &mut self.rb_password, &mut self.rb_show, "rb-pw");
                ui.end_row();
                ui.label("SSH keys");
                if self.rb_keys.is_empty() && !sh.settings.default_ssh_keys.is_empty() {
                    self.rb_keys = sh.settings.default_ssh_keys.clone();
                }
                ui.add(egui::TextEdit::multiline(&mut self.rb_keys).desired_rows(2).desired_width(420.0).font(egui::TextStyle::Monospace));
                ui.end_row();
                ui.label("");
                // Defaults to the guest's current power state.
                let mut start = self.rb_start.unwrap_or(gc.running());
                if ui.checkbox(&mut start, "Start after rebuild").changed() {
                    self.rb_start = Some(start);
                }
                ui.end_row();
            });
            let source_label = if self.rb_catalog {
                self.rb_catalog_id
                    .as_ref()
                    .and_then(|id| catalog_entries.iter().find(|e| &e.id == id))
                    .map(|e| format!("catalog image '{}'", e.label))
                    .unwrap_or_default()
            } else {
                self.rb_template.clone()
            };
            let ok = !source_label.is_empty() && (qemu || !self.rb_password.is_empty() || !self.rb_keys.trim().is_empty());
            if ui.add_enabled(ok, egui::Button::new(RichText::new("Rebuild…").color(theme::err()).strong())).clicked() {
                self.confirm = Some(
                    Confirm::new(
                        "Rebuild guest",
                        format!(
                            "Rebuild {} '{}' from {}? Every disk of this guest will be destroyed.",
                            g.vmid,
                            gc.res.display_name(),
                            source_label
                        ),
                        Act::Rebuild,
                    )
                    .danger()
                    .require(g.vmid.to_string()),
                );
            }
        }

        // ---- HA & pool ----
        widgets::section(ui, "High availability & pool");
        ui.horizontal(|ui| {
            let sid = format!("{}:{}", if qemu { "vm" } else { "ct" }, g.vmid);
            if gc.res.hastate.is_empty() {
                ui.label("Not HA-managed.");
                if ui.button("Add to HA").clicked() {
                    let c = sh.client.clone();
                    sh.spawn(gc.title("Enable HA"), move |_ctx| async move {
                        c.post("/cluster/ha/resources", &Params::new().set("sid", &sid).set("state", "started")).await?;
                        Ok(format!("{sid} is now HA-managed"))
                    });
                }
            } else {
                ui.label(format!("HA state: {}", gc.res.hastate));
                if ui.button("Remove from HA").clicked() {
                    let c = sh.client.clone();
                    sh.spawn(gc.title("Disable HA"), move |_ctx| async move {
                        c.delete(&format!("/cluster/ha/resources/{sid}"), &Params::new()).await?;
                        Ok(format!("{sid} removed from HA"))
                    });
                }
            }
        });
        ui.horizontal(|ui| {
            if !self.pool_loaded {
                self.pool = Some(gc.res.pool.clone()).filter(|p| !p.is_empty());
                self.pool_loaded = true;
            }
            ui.label("Pool");
            let pools = sh.pools();
            widgets::combo_opt(ui, "pool-pick", &mut self.pool, &pools, "none", 160.0);
            let current = Some(gc.res.pool.clone()).filter(|p| !p.is_empty());
            if self.pool != current && ui.button("Apply").clicked() {
                let (c, vmid, from, to) = (sh.client.clone(), g.vmid, current.clone(), self.pool.clone());
                sh.spawn(gc.title("Change pool"), move |_ctx| async move {
                    if let Some(f) = &from {
                        c.put(&format!("/pools/{f}"), &Params::new().set("vms", vmid).set("delete", 1)).await?;
                    }
                    if let Some(t) = &to {
                        c.put(&format!("/pools/{t}"), &Params::new().set("vms", vmid)).await?;
                    }
                    Ok(format!("{vmid} moved to pool {}", to.as_deref().unwrap_or("(none)")))
                });
            }
        });

        // ---- danger zone ----
        widgets::section(ui, "Danger zone");
        ui.horizontal_wrapped(|ui| {
            if !gc.res.template && ui.button("Convert to template").clicked() {
                self.confirm = Some(
                    Confirm::new("Convert to template", "Templates cannot be started. This cannot be undone.", Act::Template).danger(),
                );
            }
            if !gc.res.lock.is_empty() && ui.button(format!("Clear lock ({})", gc.res.lock)).clicked() {
                self.confirm = Some(Confirm::new(
                    "Clear lock",
                    format!("Remove the '{}' lock? Only do this if no task is still working on the guest.", gc.res.lock),
                    Act::Unlock,
                ));
            }
            ui.checkbox(&mut self.purge, "purge from backup/replication/HA jobs");
            if ui.button(RichText::new("Destroy…").color(theme::err()).strong()).clicked() {
                let ips: Vec<String> = sh.ipam.lock().guest_ips(&sh.server_id, g.vmid).iter().map(|a| a.ip.clone()).collect();
                let extra = if ips.is_empty() { String::new() } else { format!(" IPAM addresses {} will be released.", ips.join(", ")) };
                self.confirm = Some(
                    Confirm::new(
                        "Destroy guest",
                        format!("Permanently destroy {} {} '{}' and all its disks?{extra}", g.kind.label(), g.vmid, gc.res.display_name()),
                        Act::Destroy(self.purge),
                    )
                    .danger()
                    .require(g.vmid.to_string()),
                );
            }
        });
    }
}
