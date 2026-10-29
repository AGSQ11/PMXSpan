//! Hardware & options: resources, disks, CD-ROM / boot order, raw config.

use super::{GCtx, value_str};
use crate::api::Params;
use crate::api::types::GuestKind;
use crate::ops::guest;
use crate::rt::Fetch;
use crate::ui::widgets::{self, Confirm};
use crate::ui::{Actions, theme};
use crate::util::{PropString, ValueExt, fmt_bytes, parse_size};
use egui::RichText;

#[derive(Default, Clone)]
struct Form {
    loaded_for: u64,
    name: String,
    cores: u32,
    sockets: u32,
    memory: u64,
    balloon: u64,
    swap: u64,
    cpu: String,
    cpulimit: f64,
    onboot: bool,
    protection: bool,
    tags: String,
    description: String,
    ostype: String,
    boot: String,
}

#[derive(Default)]
pub struct HardwareTab {
    form: Form,
    stamp: u64,
    resize: Option<(String, u64)>,
    move_disk: Option<(String, String, bool)>,
    add_disk: Option<(String, String, u64, String)>,
    isos: Fetch<Vec<String>>,
    iso_choice: String,
    iso_boot: bool,
    confirm_detach: Option<Confirm<String>>,
}

impl HardwareTab {
    fn load(&mut self, gc: &GCtx) {
        let Some(cfg) = gc.config else { return };
        let stamp = super::config_stamp(cfg);
        if self.form.loaded_for == gc.g.vmid && self.stamp == stamp {
            return;
        }
        self.stamp = stamp;
        let s = |k: &str| cfg.get(k).map(value_str).unwrap_or_default();
        let n = |k: &str, d: f64| cfg.get(k).map(|v| value_str(v).parse::<f64>().unwrap_or(d)).unwrap_or(d);
        self.form = Form {
            loaded_for: gc.g.vmid,
            name: if gc.g.kind == GuestKind::Qemu { s("name") } else { s("hostname") },
            cores: n("cores", 1.0) as u32,
            sockets: n("sockets", 1.0) as u32,
            memory: n("memory", 512.0) as u64,
            balloon: n("balloon", 0.0) as u64,
            swap: n("swap", 0.0) as u64,
            cpu: s("cpu"),
            cpulimit: n("cpulimit", 0.0),
            onboot: s("onboot") == "1",
            protection: s("protection") == "1",
            tags: s("tags"),
            description: s("description"),
            ostype: s("ostype"),
            boot: s("boot"),
        };
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, gc: &GCtx, _actions: &mut Actions) {
        self.load(gc);
        let Some(cfg) = gc.config else {
            ui.spinner();
            return;
        };
        if let Some(disk) = widgets::confirm_ui(ui.ctx(), &mut self.confirm_detach) {
            let (c, g) = (gc.sh.client.clone(), gc.g.clone());
            gc.sh.spawn(gc.title(&format!("Detach {disk}")), move |ctx| async move {
                guest::set_config(&c, &ctx, &g, Params::new().set("delete", &disk)).await?;
                Ok(format!("{disk} detached (the volume is kept as an unused disk)"))
            });
        }
        let qemu = gc.g.kind == GuestKind::Qemu;

        widgets::section(ui, "Resources & options");
        let f = &mut self.form;
        egui::Grid::new("hw-form").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
            ui.label(if qemu { "Name" } else { "Hostname" });
            ui.text_edit_singleline(&mut f.name);
            ui.end_row();
            ui.label("CPU");
            ui.horizontal(|ui| {
                if qemu {
                    ui.add(egui::DragValue::new(&mut f.sockets).range(1..=8).prefix("sockets "));
                }
                ui.add(egui::DragValue::new(&mut f.cores).range(1..=512).prefix("cores "));
                ui.label("limit");
                ui.add(egui::DragValue::new(&mut f.cpulimit).range(0.0..=512.0).speed(0.1)).on_hover_text("0 = unlimited");
            });
            ui.end_row();
            if qemu {
                ui.label("CPU type");
                ui.add(egui::TextEdit::singleline(&mut f.cpu).hint_text("host, x86-64-v2-AES, kvm64 …"));
                ui.end_row();
            }
            ui.label("Memory");
            ui.horizontal(|ui| {
                ui.add(egui::DragValue::new(&mut f.memory).range(16..=4_194_304).speed(64).suffix(" MiB"));
                if qemu {
                    ui.label("min (balloon)");
                    ui.add(egui::DragValue::new(&mut f.balloon).range(0..=4_194_304).speed(64).suffix(" MiB"))
                        .on_hover_text("0 disables ballooning");
                } else {
                    ui.label("swap");
                    ui.add(egui::DragValue::new(&mut f.swap).range(0..=4_194_304).speed(64).suffix(" MiB"));
                }
            });
            ui.end_row();
            if qemu {
                ui.label("OS type");
                egui::ComboBox::from_id_salt("ostype").selected_text(ostype_label(&f.ostype)).show_ui(ui, |ui| {
                    for (v, l) in OSTYPES {
                        ui.selectable_value(&mut f.ostype, v.to_string(), *l);
                    }
                });
                ui.end_row();
            }
            ui.label("Options");
            ui.horizontal(|ui| {
                ui.checkbox(&mut f.onboot, "Start at boot");
                ui.checkbox(&mut f.protection, "Protection (block destroy)");
            });
            ui.end_row();
            ui.label("Tags");
            ui.add(egui::TextEdit::singleline(&mut f.tags).hint_text("semicolon separated"));
            ui.end_row();
            ui.label("Notes");
            ui.add(egui::TextEdit::multiline(&mut f.description).desired_rows(3).desired_width(420.0));
            ui.end_row();
        });
        if ui.button(RichText::new("Apply changes").strong()).clicked() {
            let p = self.diff(gc);
            if p.is_empty() {
                widgets::muted(ui, "Nothing changed.");
            } else {
                let (c, g) = (gc.sh.client.clone(), gc.g.clone());
                gc.sh.spawn(gc.title("Update options"), move |ctx| async move {
                    guest::set_config(&c, &ctx, &g, p).await?;
                    Ok("configuration updated (some changes may apply after a reboot)".into())
                });
            }
        }
        if gc.running() {
            widgets::muted(ui, "Hot-plug depends on guest support; pending changes apply at next boot.");
        }

        widgets::section(ui, "Disks");
        let disks = guest::disk_keys(gc.g.kind, cfg);
        let boot = guest::boot_disk(gc.g.kind, cfg);
        egui::Grid::new("disks").striped(true).num_columns(4).spacing([12.0, 6.0]).show(ui, |ui| {
            for d in &disks {
                let v = cfg.get(d).map(value_str).unwrap_or_default();
                let p = PropString::parse(&v);
                let size = p.get("size").and_then(parse_size).unwrap_or(0);
                ui.label(RichText::new(if Some(d) == boot.as_ref() { format!("{d} (boot)") } else { d.clone() }).strong());
                ui.label(RichText::new(p.positional().unwrap_or(&v)).monospace());
                ui.label(fmt_bytes(size as f64));
                ui.horizontal(|ui| {
                    if ui.small_button("Resize").clicked() {
                        self.resize = Some((d.clone(), size.div_ceil(1024 * 1024 * 1024) + 10));
                    }
                    if ui.small_button("Move").clicked() {
                        self.move_disk = Some((d.clone(), String::new(), true));
                    }
                    if d != "rootfs" && Some(d) != boot.as_ref() && ui.small_button("Detach").clicked() {
                        self.confirm_detach = Some(Confirm::new(
                            "Detach disk",
                            format!("Detach {d}? It stays on storage as 'unused' until removed."),
                            d.clone(),
                        ));
                    }
                });
                ui.end_row();
            }
            for (k, v) in cfg.iter().filter(|(k, _)| k.starts_with("unused")) {
                ui.label(RichText::new(k).color(theme::MUTED));
                ui.label(RichText::new(value_str(v)).monospace().color(theme::MUTED));
                ui.label("");
                ui.label("");
                ui.end_row();
            }
        });

        let mut close = false;
        if let Some((disk, gb)) = self.resize.as_mut() {
            ui.horizontal(|ui| {
                ui.label(format!("Grow {disk} to"));
                ui.add(egui::DragValue::new(gb).range(1..=65536).suffix(" GiB"));
                if ui.button("Resize").clicked() {
                    let (c, g, d, size) = (gc.sh.client.clone(), gc.g.clone(), disk.clone(), *gb);
                    gc.sh.spawn(gc.title(&format!("Resize {d}")), move |ctx| async move {
                        guest::grow_disk(&c, &ctx, &g, &d, size).await?;
                        Ok(format!("{d} is now {size} GiB (grow the partition/filesystem inside the guest)"))
                    });
                    close = true;
                } else if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
            widgets::muted(ui, "Disks can only grow. Shrinking is not supported by Proxmox.");
        }
        if close {
            self.resize = None;
        }
        let mut close = false;
        if let Some((disk, target, delete)) = self.move_disk.as_mut() {
            let content = if qemu { "images" } else { "rootdir" };
            let targets = gc.sh.storages_with(&gc.g.node, content);
            ui.horizontal(|ui| {
                ui.label(format!("Move {disk} to"));
                widgets::combo(ui, "move-target", target, &targets, 160.0);
                ui.checkbox(delete, "delete source");
                if ui.add_enabled(!target.is_empty(), egui::Button::new("Move")).clicked() {
                    let (c, g, d, t, del) = (gc.sh.client.clone(), gc.g.clone(), disk.clone(), target.clone(), *delete);
                    gc.sh
                        .spawn(gc.title(&format!("Move {d}")), move |ctx| async move { guest::move_disk(&c, &ctx, &g, &d, &t, del).await });
                    close = true;
                } else if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        }
        if close {
            self.move_disk = None;
        }
        let mut close = false;
        if self.add_disk.is_none() && ui.button("＋ Add disk").clicked() {
            self.add_disk = Some((if qemu { "scsi".into() } else { "mp".into() }, String::new(), 10, "/mnt/data".into()));
        }
        if let Some((bus, storage, gb, path)) = self.add_disk.as_mut() {
            let content = if qemu { "images" } else { "rootdir" };
            let storages = gc.sh.storages_with(&gc.g.node, content);
            ui.horizontal(|ui| {
                if qemu {
                    egui::ComboBox::from_id_salt("add-bus").selected_text(bus.clone()).show_ui(ui, |ui| {
                        for b in ["scsi", "virtio", "sata", "ide"] {
                            ui.selectable_value(bus, b.to_string(), b);
                        }
                    });
                }
                widgets::combo(ui, "add-storage", storage, &storages, 150.0);
                ui.add(egui::DragValue::new(gb).range(1..=65536).suffix(" GiB"));
                if !qemu {
                    ui.label("mount");
                    ui.add(egui::TextEdit::singleline(path).desired_width(120.0));
                }
                let slot = guest::free_disk_slot(cfg, bus);
                if ui.add_enabled(!storage.is_empty() && slot.is_some(), egui::Button::new("Add")).clicked() {
                    let slot = slot.expect("slot");
                    let value = if qemu { format!("{storage}:{gb}") } else { format!("{storage}:{gb},mp={path}") };
                    let (c, g) = (gc.sh.client.clone(), gc.g.clone());
                    gc.sh.spawn(gc.title(&format!("Add {slot}")), move |ctx| async move {
                        guest::set_config(&c, &ctx, &g, Params::new().set(&slot, value)).await?;
                        Ok(format!("added {slot}"))
                    });
                    close = true;
                } else if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        }
        if close {
            self.add_disk = None;
        }

        if qemu {
            self.cdrom_ui(ui, gc);
        }

        ui.add_space(8.0);
        egui::CollapsingHeader::new("Raw configuration").show(ui, |ui| {
            egui::Grid::new("raw-cfg").striped(true).num_columns(2).show(ui, |ui| {
                let mut keys: Vec<&String> = cfg.keys().collect();
                keys.sort();
                for k in keys {
                    if k == "digest" {
                        continue;
                    }
                    let v = value_str(&cfg[k]);
                    let v = if k == "cipassword" { "********".to_string() } else { v };
                    ui.label(RichText::new(k).monospace().color(theme::MUTED));
                    ui.label(RichText::new(v).monospace());
                    ui.end_row();
                }
            });
        });
    }

    fn cdrom_ui(&mut self, ui: &mut egui::Ui, gc: &GCtx) {
        let Some(cfg) = gc.config else { return };
        widgets::section(ui, "CD-ROM & boot");
        let cd = cfg
            .iter()
            .find(|(k, v)| {
                crate::api::types::is_qemu_disk_key(k) && value_str(v).contains("media=cdrom") && !value_str(v).contains("cloudinit")
            })
            .map(|(k, v)| (k.clone(), value_str(v)));
        match &cd {
            Some((k, v)) => ui.label(format!("{k}: {}", PropString::parse(v).positional().unwrap_or("none"))),
            None => ui.label(format!("No CD-ROM drive (inserting adds one as {}).", guest::cdrom_slot(cfg).unwrap_or_else(|| "?".into()))),
        };
        if self.isos.is_idle() {
            let node = gc.g.node.clone();
            let storages = gc.sh.storages_with(&node, "iso");
            self.isos = gc.sh.fetch(move |c| async move {
                let mut out = Vec::new();
                for s in storages {
                    if let Ok(items) = c.get_q(&format!("/nodes/{node}/storage/{s}/content"), &Params::new().set("content", "iso")).await {
                        out.extend(items.arr().iter().map(|i| i.s("volid")));
                    }
                }
                out.sort();
                Ok(out)
            });
        }
        ui.horizontal(|ui| {
            let isos = self.isos.ok().cloned().unwrap_or_default();
            widgets::combo(ui, "iso-pick", &mut self.iso_choice, &isos, 320.0);
            ui.checkbox(&mut self.iso_boot, "boot from it first");
            if ui.add_enabled(!self.iso_choice.is_empty(), egui::Button::new("Insert")).clicked() {
                let (c, g, iso, boot) = (gc.sh.client.clone(), gc.g.clone(), self.iso_choice.clone(), self.iso_boot);
                gc.sh.spawn(gc.title("Insert ISO"), move |ctx| async move { guest::set_cdrom(&c, &ctx, &g, Some(&iso), boot).await });
            }
            if cd.is_some() && ui.button("Eject").clicked() {
                let (c, g) = (gc.sh.client.clone(), gc.g.clone());
                gc.sh.spawn(gc.title("Eject ISO"), move |ctx| async move { guest::set_cdrom(&c, &ctx, &g, None, false).await });
            }
            if ui.small_button("⟳").on_hover_text("Reload ISO list").clicked() {
                self.isos = Fetch::default();
            }
        });
        ui.horizontal(|ui| {
            ui.label("Boot order");
            ui.add(egui::TextEdit::singleline(&mut self.form.boot).desired_width(320.0).hint_text("order=scsi0;ide2;net0"));
        });
        widgets::muted(ui, "Boot order is saved with 'Apply changes' above.");
    }

    fn diff(&self, gc: &GCtx) -> Params {
        let Some(cfg) = gc.config else { return Params::new() };
        let f = &self.form;
        let qemu = gc.g.kind == GuestKind::Qemu;
        let cur = |k: &str| cfg.get(k).map(value_str).unwrap_or_default();
        let mut p = Params::new();
        let mut deletes: Vec<&str> = Vec::new();
        let mut set = |k: &'static str, v: String, default: &str| {
            if cur(k) != v && !(cur(k).is_empty() && v == default) {
                p.push(k, v);
            }
        };
        set(if qemu { "name" } else { "hostname" }, crate::util::sanitize_hostname(&f.name), "");
        set("cores", f.cores.to_string(), "1");
        set("memory", f.memory.to_string(), "");
        set("onboot", if f.onboot { "1" } else { "0" }.into(), "0");
        set("protection", if f.protection { "1" } else { "0" }.into(), "0");
        if qemu {
            set("sockets", f.sockets.to_string(), "1");
            if !f.cpu.is_empty() {
                set("cpu", f.cpu.clone(), "");
            }
            set("balloon", f.balloon.to_string(), "");
            if !f.ostype.is_empty() {
                set("ostype", f.ostype.clone(), "");
            }
            if !f.boot.is_empty() {
                set("boot", f.boot.clone(), "");
            }
        } else {
            set("swap", f.swap.to_string(), "");
        }
        if f.cpulimit > 0.0 {
            set("cpulimit", format!("{}", f.cpulimit), "0");
        } else if !cur("cpulimit").is_empty() {
            deletes.push("cpulimit");
        }
        if f.tags.trim().is_empty() {
            if !cur("tags").is_empty() {
                deletes.push("tags");
            }
        } else {
            set("tags", f.tags.trim().to_string(), "");
        }
        if f.description.trim().is_empty() {
            if !cur("description").is_empty() {
                deletes.push("description");
            }
        } else {
            set("description", f.description.clone(), "");
        }
        if !deletes.is_empty() {
            p.push("delete", deletes.join(","));
        }
        if let Some(d) = cfg.get("digest").map(value_str)
            && !p.is_empty()
        {
            // Optimistic concurrency: fail if someone else changed the config meanwhile.
            p.push("digest", d);
        }
        p
    }
}

const OSTYPES: &[(&str, &str)] = &[
    ("l26", "Linux 6.x - 2.6 kernel"),
    ("l24", "Linux 2.4 kernel"),
    ("win11", "Windows 11/2022/2025"),
    ("win10", "Windows 10/2016/2019"),
    ("win8", "Windows 8/2012/2012r2"),
    ("win7", "Windows 7/2008r2"),
    ("wvista", "Windows Vista/2008"),
    ("wxp", "Windows XP/2003"),
    ("w2k", "Windows 2000"),
    ("solaris", "Solaris"),
    ("other", "Other"),
];

pub fn ostype_label(v: &str) -> String {
    OSTYPES
        .iter()
        .find(|(k, _)| *k == v)
        .map(|(_, l)| l.to_string())
        .unwrap_or_else(|| if v.is_empty() { "(unset)".into() } else { v.into() })
}

pub fn ostypes() -> &'static [(&'static str, &'static str)] {
    OSTYPES
}
