//! Storage browser: content listing, uploads, URL downloads, appliance templates.

use super::widgets::{self, Confirm};
use super::{Actions, Shared, theme};
use crate::api::Params;
use crate::api::types::{NodeStorage, StorageItem};
use crate::rt::Fetch;
use crate::util::{ValueExt, fmt_bytes, fmt_epoch, pct};
use egui::RichText;
use serde_json::Value;

#[derive(Default)]
pub struct StorageView {
    node: String,
    storages: Fetch<Vec<NodeStorage>>,
    storages_for: String,
    selected: Option<String>,
    content: Fetch<Vec<StorageItem>>,
    content_key: String,
    filter_type: String,
    url: String,
    url_name: String,
    url_content: String,
    url_checksum: String,
    url_algo: String,
    aplinfo: Fetch<Vec<Value>>,
    show_aplinfo: bool,
    apl_filter: String,
    confirm_delete: Option<Confirm<String>>,
}

impl StorageView {
    pub fn ui(&mut self, ui: &mut egui::Ui, sh: &Shared, _actions: &mut Actions) {
        let nodes = sh.online_nodes();
        if self.node.is_empty() || !nodes.contains(&self.node) {
            self.node = nodes.first().cloned().unwrap_or_default();
        }
        if self.storages_for != self.node || (self.storages.is_stale() && !self.storages.is_loading()) {
            self.storages_for = self.node.clone();
            let n = self.node.clone();
            self.storages = sh.fetch(move |c| async move {
                let mut v: Vec<NodeStorage> =
                    c.get_list(&format!("/nodes/{n}/storage")).await?.iter().map(NodeStorage::from_value).collect();
                v.sort_by(|a, b| a.storage.cmp(&b.storage));
                Ok(v)
            });
        }
        if let Some(volid) = widgets::confirm_ui(ui.ctx(), &mut self.confirm_delete)
            && let Some(st) = self.selected.clone()
        {
            let (c, n) = (sh.client.clone(), self.node.clone());
            sh.spawn(
                format!("Delete {volid}"),
                move |ctx| async move { crate::ops::guest::delete_volume(&c, &ctx, &n, &st, &volid).await },
            );
        }

        ui.horizontal(|ui| {
            ui.label("Node");
            widgets::combo(ui, "st-node", &mut self.node, &nodes, 140.0);
        });
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(300.0);
                let mut pick = None;
                widgets::fetch_ui(ui, &mut self.storages, |ui, list| {
                    for s in list.iter() {
                        let sel = self.selected.as_deref() == Some(s.storage.as_str());
                        let resp = crate::ui::theme::card_frame().inner_margin(egui::Margin::symmetric(14, 12)).show(ui, |ui| {
                            ui.set_width(280.0);
                            ui.horizontal(|ui| {
                                widgets::status_dot(ui, if s.usable() { "running" } else { "error" });
                                ui.label(RichText::new(&s.storage).strong().color(if sel {
                                    theme::accent()
                                } else {
                                    ui.visuals().text_color()
                                }));
                                ui.label(
                                    RichText::new(format!("{}{}", s.stype, if s.shared { ", shared" } else { "" }))
                                        .small()
                                        .color(theme::muted()),
                                );
                            });
                            ui.label(RichText::new(&s.content).small());
                            if s.total > 0.0 {
                                widgets::usage_bar(
                                    ui,
                                    pct(s.used, s.total),
                                    format!("{} / {}", fmt_bytes(s.used), fmt_bytes(s.total)),
                                    260.0,
                                );
                            }
                        });
                        if resp.response.interact(egui::Sense::click()).clicked() {
                            pick = Some(s.storage.clone());
                        }
                    }
                });
                if let Some(p) = pick {
                    self.selected = Some(p);
                    self.filter_type.clear();
                }
            });
            ui.separator();
            ui.vertical(|ui| self.content_ui(ui, sh));
        });
    }

    fn content_ui(&mut self, ui: &mut egui::Ui, sh: &Shared) {
        let Some(storage) = self.selected.clone() else {
            widgets::muted(ui, "Select a storage.");
            return;
        };
        let info = self.storages.ok().and_then(|l| l.iter().find(|s| s.storage == storage).cloned());
        let key = format!("{}/{}", self.node, storage);
        if self.content_key != key || (self.content.is_stale() && !self.content.is_loading()) {
            self.content_key = key;
            let n = self.node.clone();
            let s = storage.clone();
            self.content = sh.fetch(move |c| async move {
                let mut v: Vec<StorageItem> =
                    c.get_list(&format!("/nodes/{n}/storage/{s}/content")).await?.iter().map(StorageItem::from_value).collect();
                v.sort_by(|a, b| a.volid.cmp(&b.volid));
                Ok(v)
            });
        }
        widgets::section(ui, &format!("{storage} on {}", self.node));
        let content_types = info.as_ref().map(|i| i.content.clone()).unwrap_or_default();
        ui.horizontal_wrapped(|ui| {
            ui.selectable_value(&mut self.filter_type, String::new(), "All");
            for ct in content_types.split(',').filter(|c| !c.is_empty()) {
                ui.selectable_value(&mut self.filter_type, ct.to_string(), ct);
            }
            if ui.small_button("⟳").clicked() {
                self.content_key.clear();
            }
        });

        // Upload / download for media-capable storages.
        let uploadable: Vec<&str> =
            ["iso", "vztmpl", "import", "snippets"].into_iter().filter(|c| content_types.split(',').any(|x| x == *c)).collect();
        if !uploadable.is_empty() {
            ui.horizontal_wrapped(|ui| {
                for ct in &uploadable {
                    if *ct == "snippets" {
                        continue;
                    }
                    if ui.button(format!("Upload {ct}…")).clicked() {
                        let filter: &[&str] = match *ct {
                            "iso" => &["iso", "img"],
                            "vztmpl" => &["tar", "gz", "xz", "zst", "tgz"],
                            _ => &["*"],
                        };
                        if let Some(path) = rfd::FileDialog::new().add_filter(*ct, filter).pick_file() {
                            let (c, n, s, ct) = (sh.client.clone(), self.node.clone(), storage.clone(), ct.to_string());
                            let name = path.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
                            sh.spawn(format!("Upload {name} to {s}"), move |ctx| async move {
                                ctx.info(format!(
                                    "uploading {} ({})",
                                    path.display(),
                                    fmt_bytes(std::fs::metadata(&path).map(|m| m.len() as f64).unwrap_or(0.0))
                                ));
                                let pctx = ctx.clone();
                                let upid = c
                                    .upload(&n, &s, &ct, &path, move |sent, total| {
                                        if total > 0 {
                                            pctx.progress(sent as f32 / total as f32);
                                        }
                                    })
                                    .await?;
                                if upid.starts_with("UPID:") {
                                    crate::ops::guest::run_task(&c, &ctx, "import upload", Value::String(upid)).await?;
                                }
                                Ok(format!("{name} uploaded to {s}"))
                            });
                        }
                    }
                }
                if uploadable.iter().any(|c| *c == "iso" || *c == "vztmpl") {
                    ui.separator();
                    if self.url_content.is_empty() {
                        self.url_content = uploadable[0].to_string();
                    }
                    ui.label("Download from URL:");
                    ui.add(egui::TextEdit::singleline(&mut self.url).hint_text("https://…/debian-12.iso").desired_width(300.0));
                    if ui.small_button("name from URL").clicked() {
                        self.url_name = self.url.rsplit('/').next().unwrap_or("").split('?').next().unwrap_or("").to_string();
                    }
                }
            });
            if uploadable.iter().any(|c| *c == "iso" || *c == "vztmpl") && !self.url.is_empty() {
                ui.horizontal_wrapped(|ui| {
                    ui.label("File name");
                    ui.add(egui::TextEdit::singleline(&mut self.url_name).desired_width(220.0));
                    egui::ComboBox::from_id_salt("url-ct").selected_text(self.url_content.clone()).show_ui(ui, |ui| {
                        for ct in uploadable.iter().filter(|c| **c == "iso" || **c == "vztmpl") {
                            ui.selectable_value(&mut self.url_content, ct.to_string(), *ct);
                        }
                    });
                    egui::ComboBox::from_id_salt("url-algo")
                        .selected_text(if self.url_algo.is_empty() { "no checksum".into() } else { self.url_algo.clone() })
                        .show_ui(ui, |ui| {
                            for a in ["", "sha256", "sha512", "sha1", "md5"] {
                                ui.selectable_value(&mut self.url_algo, a.to_string(), if a.is_empty() { "no checksum" } else { a });
                            }
                        });
                    if !self.url_algo.is_empty() {
                        ui.add(egui::TextEdit::singleline(&mut self.url_checksum).hint_text("checksum").desired_width(240.0));
                    }
                    if ui.add_enabled(!self.url_name.is_empty(), egui::Button::new("Download")).clicked() {
                        let p = Params::new()
                            .set("url", self.url.trim())
                            .set("filename", self.url_name.trim())
                            .set("content", &self.url_content)
                            .opt("checksum-algorithm", Some(&self.url_algo))
                            .opt("checksum", Some(self.url_checksum.trim()));
                        let (c, n, s, name) = (sh.client.clone(), self.node.clone(), storage.clone(), self.url_name.clone());
                        sh.spawn(format!("Download {name} to {s}"), move |ctx| async move {
                            let r = c.post(&format!("/nodes/{n}/storage/{s}/download-url"), &p).await?;
                            crate::ops::guest::run_task(&c, &ctx, "download", r).await?;
                            Ok(format!("{name} downloaded to {s}"))
                        });
                        self.url.clear();
                        self.url_name.clear();
                    }
                });
            }
            if uploadable.contains(&"vztmpl") {
                if ui.toggle_value(&mut self.show_aplinfo, "Browse container appliance templates").changed() && self.show_aplinfo {
                    self.aplinfo = Fetch::default();
                }
                if self.show_aplinfo {
                    self.aplinfo_ui(ui, sh, &storage);
                }
            }
        }

        let filter = self.filter_type.clone();
        let mut del = None;
        widgets::fetch_ui(ui, &mut self.content, |ui, items| {
            let rows: Vec<&StorageItem> = items.iter().filter(|i| filter.is_empty() || i.content == filter).collect();
            if rows.is_empty() {
                widgets::muted(ui, "Empty.");
                return;
            }
            egui::ScrollArea::both().id_salt("st-content").show(ui, |ui| {
                egui::Grid::new("st-items").striped(true).num_columns(6).spacing([12.0, 4.0]).show(ui, |ui| {
                    for h in ["Volume", "Type", "Format", "Size", "Created", ""] {
                        ui.label(RichText::new(h).strong());
                    }
                    ui.end_row();
                    for i in rows {
                        ui.label(RichText::new(i.file_name()).monospace()).on_hover_text(&i.volid);
                        ui.label(if i.vmid > 0 { format!("{} ({})", i.content, i.vmid) } else { i.content.clone() });
                        ui.label(&i.format);
                        ui.label(fmt_bytes(i.size));
                        ui.label(fmt_epoch(i.ctime));
                        ui.horizontal(|ui| {
                            if ui.small_button("Copy ID").clicked() {
                                ui.ctx().copy_text(i.volid.clone());
                            }
                            // Guest disks are managed from the guest.
                            if !matches!(i.content.as_str(), "images" | "rootdir")
                                && !i.protected
                                && ui.small_button(RichText::new("Delete").color(theme::err())).clicked()
                            {
                                del = Some(i.volid.clone());
                            }
                        });
                        ui.end_row();
                    }
                });
            });
        });
        if let Some(v) = del {
            self.confirm_delete = Some(Confirm::new("Delete volume", format!("Permanently delete {v}?"), v).danger());
        }
    }

    fn aplinfo_ui(&mut self, ui: &mut egui::Ui, sh: &Shared, storage: &str) {
        if self.aplinfo.is_idle() {
            let n = self.node.clone();
            self.aplinfo = sh.fetch(move |c| async move { c.get_list(&format!("/nodes/{n}/aplinfo")).await });
        }
        ui.add(egui::TextEdit::singleline(&mut self.apl_filter).hint_text("filter (debian, ubuntu, alpine…)"));
        let f = self.apl_filter.to_lowercase();
        let mut download = None;
        widgets::fetch_ui(ui, &mut self.aplinfo, |ui, list| {
            if list.is_empty() {
                widgets::muted(ui, "Appliance index is empty — run 'pveam update' on the node.");
            }
            egui::ScrollArea::vertical().id_salt("apl").max_height(260.0).show(ui, |ui| {
                egui::Grid::new("apl-grid").striped(true).num_columns(4).show(ui, |ui| {
                    for a in list.iter().filter(|a| {
                        f.is_empty() || a.s("template").to_lowercase().contains(&f) || a.s("headline").to_lowercase().contains(&f)
                    }) {
                        ui.label(RichText::new(a.s("section")).small().color(theme::muted()));
                        ui.label(a.s("template"));
                        ui.label(a.s("headline"));
                        if ui.small_button("Download").clicked() {
                            download = Some(a.s("template"));
                        }
                        ui.end_row();
                    }
                });
            });
        });
        if let Some(t) = download {
            let (c, n, s) = (sh.client.clone(), self.node.clone(), storage.to_string());
            sh.spawn(format!("Download template {t}"), move |ctx| async move {
                let r = c.post(&format!("/nodes/{n}/aplinfo"), &Params::new().set("storage", &s).set("template", &t)).await?;
                crate::ops::guest::run_task(&c, &ctx, "download template", r).await?;
                Ok(format!("{t} downloaded to {s}"))
            });
        }
    }
}
