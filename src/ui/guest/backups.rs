//! Backups of this guest: back up now, restore, protect, delete.

use super::GCtx;
use crate::api::types::StorageItem;
use crate::ops::guest;
use crate::rt::Fetch;
use crate::ui::widgets::{self, Confirm};
use crate::ui::{Actions, theme};
use crate::util::{fmt_bytes, fmt_epoch};
use egui::RichText;

#[derive(Clone)]
enum Act {
    Restore(String),
    Delete(String, String),
}

pub struct BackupsTab {
    list: Fetch<Vec<(String, StorageItem)>>,
    storage: String,
    mode: String,
    compress: String,
    notes: String,
    protected: bool,
    confirm: Option<Confirm<Act>>,
}

impl Default for BackupsTab {
    fn default() -> Self {
        Self {
            list: Fetch::default(),
            storage: String::new(),
            mode: "snapshot".into(),
            compress: "zstd".into(),
            notes: "{{guestname}}".into(),
            protected: false,
            confirm: None,
        }
    }
}

impl BackupsTab {
    pub fn ui(&mut self, ui: &mut egui::Ui, gc: &GCtx, _actions: &mut Actions) {
        let g = gc.g.clone();
        if self.list.is_idle() || (self.list.is_stale() && !self.list.is_loading()) {
            let gr = g.clone();
            self.list = gc.sh.fetch(move |c| async move { guest::backups(&c, &gr).await });
        }
        if let Some(a) = widgets::confirm_ui(ui.ctx(), &mut self.confirm) {
            let (c, gr) = (gc.sh.client.clone(), g.clone());
            match a {
                Act::Restore(volid) => {
                    gc.sh.spawn(gc.title("Restore backup"), move |ctx| async move { guest::restore(&c, &ctx, &gr, &volid, None).await });
                }
                Act::Delete(storage, volid) => {
                    gc.sh.spawn(gc.title("Delete backup"), move |ctx| async move {
                        guest::delete_volume(&c, &ctx, &gr.node, &storage, &volid).await
                    });
                }
            }
        }
        let storages = gc.sh.storages_with(&gc.g.node, "backup");
        if self.storage.is_empty()
            && let Some(s) = storages.first()
        {
            self.storage = s.clone();
        }
        widgets::section(ui, "Back up now");
        if storages.is_empty() {
            widgets::warn_box(ui, "No storage on this node accepts backups (content type 'backup').");
        }
        ui.horizontal_wrapped(|ui| {
            ui.label("Storage");
            widgets::combo(ui, "bk-storage", &mut self.storage, &storages, 150.0);
            ui.label("Mode");
            egui::ComboBox::from_id_salt("bk-mode").selected_text(self.mode.clone()).show_ui(ui, |ui| {
                for m in ["snapshot", "suspend", "stop"] {
                    ui.selectable_value(&mut self.mode, m.to_string(), m);
                }
            });
            ui.label("Compression");
            egui::ComboBox::from_id_salt("bk-comp").selected_text(self.compress.clone()).show_ui(ui, |ui| {
                for m in ["zstd", "lzo", "gzip", "0"] {
                    ui.selectable_value(&mut self.compress, m.to_string(), if m == "0" { "none" } else { m });
                }
            });
            ui.checkbox(&mut self.protected, "protected");
        });
        ui.horizontal(|ui| {
            ui.label("Notes");
            ui.add(egui::TextEdit::singleline(&mut self.notes).desired_width(300.0))
                .on_hover_text("Variables: {{guestname}}, {{node}}, {{vmid}}, {{cluster}}");
            if ui.add_enabled(!self.storage.is_empty(), egui::Button::new(RichText::new("Back up now").strong())).clicked() {
                let (c, gr, s, m, z, n, p) = (
                    gc.sh.client.clone(),
                    g.clone(),
                    self.storage.clone(),
                    self.mode.clone(),
                    self.compress.clone(),
                    self.notes.clone(),
                    self.protected,
                );
                gc.sh.spawn(gc.title("Backup"), move |ctx| async move { guest::backup_now(&c, &ctx, &gr, &s, &m, &z, &n, p).await });
            }
        });

        ui.horizontal(|ui| {
            widgets::section(ui, "Existing backups");
            if ui.small_button("⟳").clicked() {
                self.list = Fetch::default();
            }
        });
        let mut pending = None;
        let node = gc.g.node.clone();
        let client = gc.sh.client.clone();
        let mut toggle: Option<(String, String, bool)> = None;
        widgets::fetch_ui(ui, &mut self.list, |ui, list| {
            if list.is_empty() {
                widgets::muted(ui, "No backups found on storages visible from this node.");
                return;
            }
            egui::Grid::new("bk-list").striped(true).num_columns(6).spacing([12.0, 6.0]).show(ui, |ui| {
                for h in ["Date", "Storage", "File", "Size", "Notes", ""] {
                    ui.label(RichText::new(h).strong());
                }
                ui.end_row();
                for (storage, b) in list.iter() {
                    ui.label(fmt_epoch(b.ctime));
                    ui.label(storage);
                    ui.label(RichText::new(b.file_name()).monospace().small());
                    ui.label(fmt_bytes(b.size));
                    ui.label(b.notes.lines().next().unwrap_or(""));
                    ui.horizontal(|ui| {
                        if ui.small_button("Restore").clicked() {
                            pending = Some(
                                Confirm::new(
                                    "Restore backup",
                                    format!(
                                        "Restore {} over the current guest? Its disks are replaced; the guest is stopped first.",
                                        b.file_name()
                                    ),
                                    Act::Restore(b.volid.clone()),
                                )
                                .danger(),
                            );
                        }
                        let lock = if b.protected { "Unprotect" } else { "Protect" };
                        if ui.small_button(lock).clicked() {
                            toggle = Some((storage.clone(), b.volid.clone(), !b.protected));
                        }
                        if !b.protected && ui.small_button(RichText::new("Delete").color(theme::ERR)).clicked() {
                            pending = Some(
                                Confirm::new(
                                    "Delete backup",
                                    format!("Permanently delete {}?", b.file_name()),
                                    Act::Delete(storage.clone(), b.volid.clone()),
                                )
                                .danger(),
                            );
                        }
                    });
                    ui.end_row();
                }
            });
        });
        if let Some((storage, volid, protect)) = toggle {
            let c = client.clone();
            gc.sh.spawn(format!("{} backup", if protect { "Protect" } else { "Unprotect" }), move |_ctx| async move {
                guest::set_volume_protected(&c, &node, &storage, &volid, protect).await?;
                Ok(format!("{volid} {}", if protect { "protected" } else { "unprotected" }))
            });
        }
        if pending.is_some() {
            self.confirm = pending;
        }
    }
}
