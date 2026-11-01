//! Backups of this guest: back up now, restore, protect, delete.

use super::GCtx;
use crate::api::types::StorageItem;
use crate::ops::guest::{self, RestoreEntry};
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

/// Browsing the files inside a PBS backup (single-file restore).
struct FileBrowser {
    storage: String,
    volid: String,
    title: String,
    /// (label, base64 path) from the top down; the top is ("/", "/").
    stack: Vec<(String, String)>,
    list: Fetch<Vec<RestoreEntry>>,
    tar: bool,
}

pub struct BackupsTab {
    list: Fetch<Vec<(String, StorageItem)>>,
    /// Also list backups of other guests of the same type.
    all: bool,
    browser: Option<FileBrowser>,
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
            all: false,
            browser: None,
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
            let (gr, all) = (g.clone(), self.all);
            self.list = gc.sh.fetch(move |c| async move { guest::backups_on_node(&c, &gr, all).await });
        }
        self.browser_window(ui.ctx(), gc);
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
            if ui
                .checkbox(&mut self.all, "include other guests' backups")
                .on_hover_text("Restore another guest's backup over this one (same type only)")
                .changed()
            {
                self.list = Fetch::default();
            }
        });
        let mut browse: Option<FileBrowser> = None;
        let this_vmid = g.vmid;
        let mut pending = None;
        let node = gc.g.node.clone();
        let client = gc.sh.client.clone();
        let mut toggle: Option<(String, String, bool)> = None;
        widgets::fetch_ui(ui, &mut self.list, |ui, list| {
            if list.is_empty() {
                widgets::muted(ui, "No backups found on storages visible from this node.");
                return;
            }
            egui::Grid::new("bk-list").striped(true).num_columns(7).spacing([12.0, 6.0]).show(ui, |ui| {
                for h in ["Date", "Guest", "Storage", "File", "Size", "Notes", ""] {
                    ui.label(RichText::new(h).strong());
                }
                ui.end_row();
                for (storage, b) in list.iter() {
                    ui.label(fmt_epoch(b.ctime));
                    if b.vmid == this_vmid || b.vmid == 0 {
                        ui.label(b.vmid.to_string());
                    } else {
                        ui.label(RichText::new(b.vmid.to_string()).color(theme::warn())).on_hover_text("Backup of another guest");
                    }
                    ui.label(storage);
                    ui.label(RichText::new(b.file_name()).monospace().small());
                    ui.label(fmt_bytes(b.size));
                    ui.label(b.notes.lines().next().unwrap_or(""));
                    ui.horizontal(|ui| {
                        if ui.small_button("Restore").clicked() {
                            let other = if b.vmid != 0 && b.vmid != this_vmid {
                                format!(" This is a backup of guest {}, not of {this_vmid}.", b.vmid)
                            } else {
                                String::new()
                            };
                            let confirm = Confirm::new(
                                "Restore backup",
                                format!(
                                    "Restore {} over the current guest? Its disks are replaced; the guest is stopped first.{other}",
                                    b.file_name()
                                ),
                                Act::Restore(b.volid.clone()),
                            )
                            .danger();
                            pending = Some(if other.is_empty() { confirm } else { confirm.require(this_vmid.to_string()) });
                        }
                        if guest::is_pbs_backup(b)
                            && ui.small_button("Browse files…").on_hover_text("Restore single files or folders from this backup").clicked()
                        {
                            browse = Some(FileBrowser {
                                storage: storage.clone(),
                                volid: b.volid.clone(),
                                title: format!("{} · {}", b.vmid, fmt_epoch(b.ctime)),
                                stack: vec![("/".into(), "/".into())],
                                list: Fetch::default(),
                                tar: false,
                            });
                        }
                        let lock = if b.protected { "Unprotect" } else { "Protect" };
                        if ui.small_button(lock).clicked() {
                            toggle = Some((storage.clone(), b.volid.clone(), !b.protected));
                        }
                        if !b.protected && ui.small_button(RichText::new("Delete").color(theme::err())).clicked() {
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
        if browse.is_some() {
            self.browser = browse;
        }
    }

    fn browser_window(&mut self, ctx: &egui::Context, gc: &GCtx) {
        let Some(b) = self.browser.as_mut() else { return };
        let node = gc.g.node.clone();
        if b.list.is_idle() {
            let (storage, volid, path, n) =
                (b.storage.clone(), b.volid.clone(), b.stack.last().map(|s| s.1.clone()).unwrap_or_default(), node.clone());
            b.list = gc.sh.fetch(move |c| async move { guest::file_restore_list(&c, &n, &storage, &volid, &path).await });
        }
        let mut open = true;
        let mut go: Option<usize> = None;
        let mut enter: Option<(String, String)> = None;
        let mut download: Option<RestoreEntry> = None;
        let center = ctx.content_rect().center();
        egui::Window::new(format!("Files in backup {}", b.title))
            .open(&mut open)
            .pivot(egui::Align2::CENTER_TOP)
            .default_pos([center.x, 90.0])
            .min_width(620.0)
            .collapsible(false)
            .show(ctx, |ui| {
                ui.horizontal_wrapped(|ui| {
                    for (i, (label, _)) in b.stack.iter().enumerate() {
                        if i > 0 {
                            ui.label(RichText::new("›").color(theme::muted()));
                        }
                        if ui.link(label).clicked() {
                            go = Some(i);
                        }
                    }
                });
                ui.horizontal(|ui| {
                    ui.checkbox(&mut b.tar, "folders as .tar.zst (instead of .zip)");
                });
                ui.separator();
                if b.list.is_loading() {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Reading the backup… the first listing can take a minute while Proxmox starts its restore helper.");
                    });
                }
                widgets::fetch_ui(ui, &mut b.list, |ui, entries| {
                    if entries.is_empty() {
                        widgets::muted(ui, "Empty.");
                        return;
                    }
                    egui::ScrollArea::vertical().id_salt("fr-list").max_height(360.0).show(ui, |ui| {
                        egui::Grid::new("fr-grid").striped(true).num_columns(4).spacing([12.0, 4.0]).show(ui, |ui| {
                            for e in entries.iter() {
                                let icon = if e.is_dir() { theme::icon::ARCHIVE } else { theme::icon::COPY };
                                if e.is_dir() {
                                    if ui.link(format!("{icon}  {}", e.text)).clicked() {
                                        enter = Some((e.text.clone(), e.filepath.clone()));
                                    }
                                } else {
                                    ui.label(format!("{icon}  {}", e.text));
                                }
                                ui.label(e.size.map(|s| fmt_bytes(s as f64)).unwrap_or_default());
                                ui.label(e.mtime.map(fmt_epoch).unwrap_or_default());
                                if ui.small_button(format!("{}  Download", theme::icon::DOWNLOAD)).clicked() {
                                    download = Some(e.clone());
                                }
                                ui.end_row();
                            }
                        });
                    });
                });
            });
        if let Some(i) = go {
            b.stack.truncate(i + 1);
            b.list = Fetch::default();
        }
        if let Some(step) = enter {
            b.stack.push(step);
            b.list = Fetch::default();
        }
        if let Some(e) = download {
            let name = if e.is_dir() { format!("{}{}", e.text, if b.tar { ".tar.zst" } else { ".zip" }) } else { e.text.clone() };
            if let Some(dest) = rfd::FileDialog::new().set_file_name(name).save_file() {
                let (storage, volid, tar) = (b.storage.clone(), b.volid.clone(), b.tar && e.is_dir());
                let c = gc.sh.client.clone();
                gc.sh.spawn(format!("Restore file {}", e.text), move |_ctx| async move {
                    guest::file_restore_download(&c, &node, &storage, &volid, &e.filepath, tar, &dest).await
                });
            }
        }
        if !open {
            self.browser = None;
        }
    }
}
