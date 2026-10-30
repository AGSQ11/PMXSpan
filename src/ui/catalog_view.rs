//! Local OS image catalog: the images offered by "create from catalog" and
//! "reinstall from catalog".

use super::widgets::{self, Confirm};
use super::{Actions, theme};
use crate::ops::catalog::{self, CatalogEntry, CatalogStore, QEMU_OSTYPES};
use crate::rt::Fetch;
use egui::RichText;
use parking_lot::Mutex;
use std::sync::Arc;

/// The entry being edited, with list/map fields as editable text.
#[derive(Clone, Default)]
struct Draft {
    /// Id of the stored entry this draft replaces (None for a new one).
    original: Option<String>,
    entry: CatalogEntry,
    nodes: String,
    config: String,
    cicustom: String,
}

impl Draft {
    fn from_entry(e: &CatalogEntry, store: &CatalogStore) -> Self {
        Self {
            cicustom: store.cicustom.get(&e.id).cloned().unwrap_or_default(),
            original: Some(e.id.clone()),
            entry: e.clone(),
            nodes: e.nodes.join(", "),
            config: e.config.iter().map(|(k, v)| format!("{k}: {v}")).collect::<Vec<_>>().join("\n"),
        }
    }

    fn build(&self) -> anyhow::Result<CatalogEntry> {
        let mut e = self.entry.clone();
        e.nodes = self.nodes.split([',', ' ', ';']).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect();
        e.config.clear();
        for (i, line) in self.config.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once([':', '=']) else {
                anyhow::bail!("config line {} must look like `key: value`", i + 1);
            };
            e.config.insert(k.trim().to_string(), v.trim().to_string());
        }
        e.validate()
    }
}

#[derive(Default)]
pub struct CatalogView {
    draft: Option<Draft>,
    filter: String,
    import_text: Option<String>,
    /// Parsed import awaiting confirmation.
    import_preview: Option<Vec<CatalogEntry>>,
    export_text: Option<String>,
    probe: Fetch<String>,
    error: Option<String>,
    confirm_delete: Option<Confirm<String>>,
}

impl CatalogView {
    pub fn ui(&mut self, ui: &mut egui::Ui, store: &Arc<Mutex<CatalogStore>>, actions: &mut Actions) {
        widgets::muted(
            ui,
            "Images for creating and reinstalling guests straight from a URL instead of a VM template. VM images (qcow2/raw/vmdk/ova) are \
             downloaded to an `import` storage and imported as the boot disk; container templates go to a `vztmpl` storage. \
             Catalogs can be imported and exported as JSON.",
        );
        if let Some(id) = widgets::confirm_ui(ui.ctx(), &mut self.confirm_delete) {
            match store.lock().remove(&id) {
                Ok(()) => actions.toast("Catalog entry deleted"),
                Err(e) => actions.toast_err(format!("{e:#}")),
            }
            self.draft = None;
        }
        self.import_window(ui.ctx(), store, actions);
        self.export_window(ui.ctx());

        ui.add_space(6.0);
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(300.0);
                ui.horizontal(|ui| {
                    if ui.button("＋ New image").clicked() {
                        self.draft = Some(Draft {
                            entry: CatalogEntry { virt: "qemu".into(), ostype: "l26".into(), ..Default::default() },
                            ..Default::default()
                        });
                        self.error = None;
                    }
                    if ui.button("Import JSON…").clicked() {
                        self.import_text = Some(String::new());
                    }
                    if ui.button("Export").clicked() {
                        self.export_text = Some(store.lock().export_json());
                    }
                });
                ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("Filter…"));
                ui.add_space(4.0);
                let filter = self.filter.to_lowercase();
                let entries = store.lock().entries.clone();
                if entries.is_empty() {
                    widgets::muted(ui, "No images yet.");
                }
                egui::ScrollArea::vertical().id_salt("cat-list").show(ui, |ui| {
                    for e in entries
                        .iter()
                        .filter(|e| filter.is_empty() || format!("{} {} {}", e.id, e.label, e.url).to_lowercase().contains(&filter))
                    {
                        let sel = self.draft.as_ref().and_then(|d| d.original.as_deref()) == Some(e.id.as_str());
                        let tag = if e.is_lxc() { "CT" } else { "VM" };
                        if ui.selectable_label(sel, format!("[{tag}]  {}", e.label)).on_hover_text(&e.url).clicked() {
                            self.draft = Some(Draft::from_entry(e, &store.lock()));
                            self.error = None;
                            self.probe = Fetch::default();
                        }
                    }
                });
            });
            ui.separator();
            ui.vertical(|ui| {
                if self.draft.is_some() {
                    self.form(ui, store, actions);
                } else {
                    widgets::muted(ui, "Select an image or add a new one.");
                }
            });
        });
    }

    fn form(&mut self, ui: &mut egui::Ui, store: &Arc<Mutex<CatalogStore>>, actions: &mut Actions) {
        let Some(d) = self.draft.as_mut() else { return };
        let e = &mut d.entry;
        egui::Grid::new("cat-form").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
            ui.label("Type");
            ui.horizontal(|ui| {
                if ui.radio_value(&mut e.virt, "qemu".to_string(), "Virtual machine image").changed() {
                    e.ostype = "l26".into();
                }
                if ui.radio_value(&mut e.virt, "lxc".to_string(), "Container template").changed() {
                    e.ostype.clear();
                    e.cloud_user.clear();
                    e.image_format.clear();
                }
            });
            ui.end_row();
            ui.label("Label");
            ui.add(egui::TextEdit::singleline(&mut e.label).hint_text("Debian 12 (Bookworm)").desired_width(360.0));
            ui.end_row();
            ui.label("Id");
            ui.add(egui::TextEdit::singleline(&mut e.id).hint_text("debian-12").desired_width(200.0))
                .on_hover_text("Lowercase letters, digits, dot, dash and underscore. Changing it keeps guests unaffected.");
            ui.end_row();
            ui.label("URL");
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut e.url)
                        .hint_text("https://cloud.debian.org/…/debian-12-genericcloud-amd64.qcow2")
                        .desired_width(440.0),
                );
                if ui.add_enabled(!self.probe.is_loading() && !e.url.trim().is_empty(), egui::Button::new("Test")).clicked() {
                    let url = e.url.trim().to_string();
                    self.probe = Fetch::spawn(async move { catalog::probe_url(&url).await });
                }
                if self.probe.is_loading() {
                    ui.spinner();
                }
            });
            ui.end_row();
            if let Some(r) = self.probe.poll() {
                ui.label("");
                match r {
                    Ok(s) => ui.label(RichText::new(format!("✔ reachable from this PC: {s}")).color(theme::ok())),
                    Err(err) => ui.label(RichText::new(format!("✖ {err}")).color(theme::err())),
                };
                ui.end_row();
            }
            if !e.is_lxc() {
                ui.label("Image format");
                let label = if e.image_format.is_empty() { "auto (from URL)".to_string() } else { e.image_format.clone() };
                egui::ComboBox::from_id_salt("cat-fmt").selected_text(label).show_ui(ui, |ui| {
                    ui.selectable_value(&mut e.image_format, String::new(), "auto (from URL)");
                    for f in ["qcow2", "raw", "vmdk", "ova"] {
                        ui.selectable_value(&mut e.image_format, f.to_string(), f);
                    }
                });
                ui.end_row();
                ui.label("OS type");
                egui::ComboBox::from_id_salt("cat-ostype").selected_text(super::guest::ostype_label(&e.ostype)).show_ui(ui, |ui| {
                    for v in QEMU_OSTYPES {
                        ui.selectable_value(&mut e.ostype, v.to_string(), super::guest::ostype_label(v));
                    }
                });
                ui.end_row();
                ui.label("Cloud-init user");
                ui.add(
                    egui::TextEdit::singleline(&mut e.cloud_user).hint_text("debian, ubuntu, cloud-user… (optional)").desired_width(200.0),
                );
                ui.end_row();
            } else {
                ui.label("OS type");
                ui.add(
                    egui::TextEdit::singleline(&mut e.ostype).hint_text("debian, ubuntu, alpine… (empty = detect)").desired_width(200.0),
                );
                ui.end_row();
            }
            ui.label("Checksum");
            ui.horizontal(|ui| {
                let label = if e.checksum_algorithm.is_empty() { "none".to_string() } else { e.checksum_algorithm.clone() };
                egui::ComboBox::from_id_salt("cat-csalg").selected_text(label).width(90.0).show_ui(ui, |ui| {
                    ui.selectable_value(&mut e.checksum_algorithm, String::new(), "none");
                    for a in ["md5", "sha1", "sha224", "sha256", "sha384", "sha512"] {
                        ui.selectable_value(&mut e.checksum_algorithm, a.to_string(), a);
                    }
                });
                if !e.checksum_algorithm.is_empty() {
                    ui.add(egui::TextEdit::singleline(&mut e.checksum).font(egui::TextStyle::Monospace).desired_width(340.0));
                }
            });
            ui.end_row();
            ui.label("Nodes");
            ui.add(egui::TextEdit::singleline(&mut d.nodes).hint_text("any node (or: pve1, pve2)").desired_width(260.0));
            ui.end_row();
            if !e.is_lxc() {
                ui.label("Custom cloud-init");
                ui.vertical(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut d.cicustom)
                            .hint_text("user=local:snippets/web.yml,vendor=local:snippets/vendor.yml")
                            .desired_width(440.0)
                            .font(egui::TextStyle::Monospace),
                    );
                    widgets::muted(ui, "Snippet files on the nodes (`cicustom`). Kept on this computer only: never exported or imported.");
                });
                ui.end_row();
            }
            ui.label("High availability");
            ui.horizontal(|ui| {
                ui.checkbox(&mut e.ha, "add new guests to HA");
                if e.ha {
                    ui.add(egui::TextEdit::singleline(&mut e.ha_group).hint_text("HA group (optional)").desired_width(160.0));
                }
            });
            ui.end_row();
            ui.label("Config overrides");
            ui.vertical(|ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut d.config)
                        .font(egui::TextStyle::Monospace)
                        .desired_rows(3)
                        .desired_width(360.0)
                        .hint_text(if e.is_lxc() { "features: nesting=1,keyctl=1" } else { "bios: ovmf\nmachine: q35" }),
                );
                widgets::muted(
                    ui,
                    "Applied when creating or reinstalling. Only CPU, memory, firmware and similar keys are accepted; \
                     anything touching disks, network, cloud-init, host access or container privileges is rejected.",
                );
            });
            ui.end_row();
            ui.label("Notes");
            ui.add(egui::TextEdit::singleline(&mut e.notes).desired_width(360.0));
            ui.end_row();
        });
        if let Some(err) = &self.error {
            widgets::error_box(ui, err);
        }
        ui.add_space(6.0);
        let mut close = false;
        ui.horizontal(|ui| {
            if ui.button(RichText::new("Save image").strong()).clicked() {
                match d.build() {
                    Ok(entry) => {
                        let saved = {
                            let mut st = store.lock();
                            st.upsert(d.original.as_deref(), entry.clone())
                                .and_then(|()| st.set_cicustom(d.original.as_deref(), &entry.id, &d.cicustom))
                        };
                        match saved {
                            Ok(()) => {
                                actions.toast_ok(format!("Saved '{}'", entry.label));
                                *d = Draft::from_entry(&entry, &store.lock());
                                self.error = None;
                            }
                            Err(err) => self.error = Some(format!("{err:#}")),
                        }
                    }
                    Err(err) => self.error = Some(format!("{err:#}")),
                }
            }
            if let Some(id) = d.original.clone()
                && ui.button("Delete").clicked()
            {
                self.confirm_delete = Some(
                    Confirm::new(
                        "Delete catalog image",
                        format!("Remove '{}' from the catalog? Existing guests are not affected.", d.entry.label),
                        id,
                    )
                    .danger(),
                );
            }
            if ui.button("Close").clicked() {
                close = true;
            }
        });
        if close {
            self.draft = None;
            self.error = None;
        }
    }

    fn import_window(&mut self, ctx: &egui::Context, store: &Arc<Mutex<CatalogStore>>, actions: &mut Actions) {
        let Some(text) = self.import_text.as_mut() else { return };
        let mut open = true;
        let mut done = false;
        egui::Window::new("Import catalog JSON").open(&mut open).default_width(680.0).collapsible(false).show(ctx, |ui| {
            let Some(entries) = self.import_preview.as_ref() else {
                widgets::muted(ui, "Paste a catalog: `{\"entries\": [...]}` or a plain array. You review every entry before it is saved.");
                egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                    ui.add(egui::TextEdit::multiline(text).font(egui::TextStyle::Monospace).desired_rows(14).desired_width(f32::INFINITY));
                });
                if ui.button(RichText::new("Review…").strong()).clicked() {
                    match catalog::parse_catalog(text) {
                        Ok(entries) => self.import_preview = Some(entries),
                        Err(e) => actions.toast_err(format!("{e:#}")),
                    }
                }
                return;
            };
            widgets::muted(ui, "Only import catalogs from sources you trust: these images become the disks of new and reinstalled guests.");
            let store_now = store.lock().clone();
            egui::ScrollArea::vertical().max_height(400.0).show(ui, |ui| {
                for e in entries {
                    ui.add_space(4.0);
                    let existing = store_now.existing(&e.id);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("[{}]  {}", if e.is_lxc() { "CT" } else { "VM" }, e.label)).strong());
                        ui.label(RichText::new(&e.id).monospace().color(theme::muted()));
                        match existing {
                            None => ui.label(RichText::new("new").color(theme::ok())),
                            Some(old) if old == e => ui.label(RichText::new("unchanged").color(theme::muted())),
                            Some(_) => ui.label(RichText::new("replaces existing entry").color(theme::warn())),
                        };
                    });
                    let url_changed = existing.is_some_and(|old| old.url != e.url);
                    ui.label(RichText::new(&e.url).monospace().color(if url_changed { theme::warn() } else { theme::p().text }));
                    if url_changed && let Some(old) = existing {
                        ui.label(RichText::new(format!("was: {}", old.url)).monospace().small().color(theme::muted()));
                    }
                    if e.checksum.is_empty() {
                        ui.label(RichText::new("no checksum: the download is not verified").small().color(theme::warn()));
                    }
                    if !e.config.is_empty() {
                        let cfg = e.config.iter().map(|(k, v)| format!("{k}: {v}")).collect::<Vec<_>>().join(", ");
                        ui.label(RichText::new(format!("config: {cfg}")).monospace().small());
                    }
                    if e.ha || !e.nodes.is_empty() {
                        let mut parts = Vec::new();
                        if e.ha {
                            parts.push(if e.ha_group.is_empty() { "HA".to_string() } else { format!("HA group {}", e.ha_group) });
                        }
                        if !e.nodes.is_empty() {
                            parts.push(format!("nodes {}", e.nodes.join(", ")));
                        }
                        ui.label(RichText::new(parts.join(" · ")).small());
                    }
                    ui.separator();
                }
            });
            let mut back = false;
            ui.horizontal(|ui| {
                if ui.button(RichText::new(format!("Import {} entries", entries.len())).strong()).clicked() {
                    match store.lock().import(entries.clone()) {
                        Ok((added, replaced)) => {
                            actions.toast_ok(format!("Catalog imported: {added} added, {replaced} replaced"));
                            done = true;
                        }
                        Err(e) => actions.toast_err(format!("{e:#}")),
                    }
                }
                back = ui.button("Back").clicked();
            });
            if back {
                self.import_preview = None;
            }
        });
        if !open || done {
            self.import_text = None;
            self.import_preview = None;
        }
    }

    fn export_window(&mut self, ctx: &egui::Context) {
        let Some(text) = self.export_text.as_ref() else { return };
        let mut open = true;
        egui::Window::new("Export catalog JSON").open(&mut open).default_width(620.0).collapsible(false).show(ctx, |ui| {
            if ui.button("Copy to clipboard").clicked() {
                ui.ctx().copy_text(text.clone());
            }
            egui::ScrollArea::vertical().max_height(400.0).show(ui, |ui| {
                ui.add(egui::Label::new(RichText::new(text).monospace()).selectable(true));
            });
        });
        if !open {
            self.export_text = None;
        }
    }
}
