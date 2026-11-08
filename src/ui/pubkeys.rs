//! "Add from this computer…": pick SSH public keys from `~/.ssh/*.pub` and
//! append them to a key field (create wizard, Cloud-Init tab, Settings).

use super::{theme, widgets};
use crate::pubkeys::{self, PubKeyFile, Status};
use egui::RichText;
use std::collections::HashSet;
use std::path::PathBuf;

#[derive(Default)]
pub struct Picker {
    open: bool,
    dir: Option<PathBuf>,
    files: Vec<PubKeyFile>,
    selected: HashSet<String>,
}

impl Picker {
    /// The button that opens the picker.
    pub fn button(&mut self, ui: &mut egui::Ui) {
        if ui
            .button(format!("{}  Add from this computer…", theme::icon::DESKTOP))
            .on_hover_text("Pick public keys from the .ssh folder of this computer (ssh-keygen and SSHSpan keys alike)")
            .clicked()
        {
            self.scan();
            self.open = true;
        }
    }

    fn scan(&mut self) {
        self.dir = pubkeys::ssh_dir();
        self.files = self.dir.as_deref().map(pubkeys::scan).unwrap_or_default();
        self.selected.clear();
    }

    /// Shows the picker window while open and appends the chosen keys to
    /// `field`. Returns a message for a toast once keys were added.
    pub fn show(&mut self, ctx: &egui::Context, id: &str, field: &mut String) -> Option<String> {
        if !self.open {
            return None;
        }
        let p = theme::p();
        let present = crate::sshkey::wire_keys(field);
        let mut add = false;
        let mut close = false;
        let mut open = true;
        egui::Window::new(RichText::new(format!("{}  SSH keys on this computer", theme::icon::KEY)).strong())
            .id(egui::Id::unique(("pubkey-picker", id)))
            .collapsible(false)
            .resizable(true)
            .default_width(760.0)
            .open(&mut open)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                match &self.dir {
                    Some(d) => widgets::muted(ui, format!("Public keys (*.pub) in {}. Private keys are never read.", d.display())),
                    None => widgets::warn_box(ui, "Cannot find your home folder."),
                }
                ui.add_space(4.0);
                if self.files.is_empty() {
                    widgets::info_box(
                        ui,
                        "No .pub files found. Create a key with ssh-keygen (or SSHSpan's Deploy), or use \"Generate key pair…\".",
                    );
                }
                egui::ScrollArea::vertical().max_height(360.0).auto_shrink([false, true]).show(ui, |ui| {
                    egui::Grid::new(("pubkeys", id)).striped(true).num_columns(5).spacing([12.0, 5.0]).show(ui, |ui| {
                        for h in ["", "Comment", "Type", "Fingerprint", "File"] {
                            ui.label(RichText::new(h).size(12.0).family(theme::semibold()).color(p.muted));
                        }
                        ui.end_row();
                        for f in &self.files {
                            match &f.status {
                                Status::Ok(k) => {
                                    let already = present.contains(&k.base64);
                                    let mut on = already || self.selected.contains(&f.file_name);
                                    let r = ui.add_enabled(!already, egui::Checkbox::without_text(&mut on));
                                    if r.changed() {
                                        if on {
                                            self.selected.insert(f.file_name.clone());
                                        } else {
                                            self.selected.remove(&f.file_name);
                                        }
                                    }
                                    r.on_disabled_hover_text("Already in the list");
                                    ui.label(if k.comment.is_empty() { "-" } else { &k.comment });
                                    ui.label(k.short_type());
                                    ui.label(RichText::new(&k.fingerprint).monospace().size(11.5));
                                    ui.horizontal(|ui| {
                                        ui.label(RichText::new(&f.file_name).color(p.muted));
                                        if already {
                                            widgets::pill(ui, "already added", widgets::Tone::Neutral);
                                        }
                                    });
                                }
                                Status::Unreadable(why) => {
                                    ui.add_enabled(false, egui::Checkbox::without_text(&mut false));
                                    ui.label(RichText::new("unreadable").color(p.muted));
                                    ui.label("");
                                    ui.label(RichText::new(why).size(12.0).color(theme::warn()));
                                    ui.label(RichText::new(&f.file_name).color(p.muted));
                                }
                            }
                            ui.end_row();
                        }
                    });
                });
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let n = self.selected.len();
                    let label = format!("Add {n} key{}", if n == 1 { "" } else { "s" });
                    if ui.add_enabled_ui(n > 0, |ui| widgets::primary_button(ui, label)).inner.clicked() {
                        add = true;
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                    if ui.button(format!("{}  Rescan", theme::icon::REFRESH)).clicked() {
                        self.scan();
                    }
                });
            });
        if !open || close {
            self.open = false;
            return None;
        }
        if !add {
            return None;
        }
        self.open = false;
        let keys: Vec<_> = self
            .files
            .iter()
            .filter(|f| self.selected.contains(&f.file_name))
            .filter_map(|f| match &f.status {
                Status::Ok(k) => Some(k.clone()),
                Status::Unreadable(_) => None,
            })
            .collect();
        let (added, dupes) = pubkeys::append(field, &keys);
        Some(if dupes > 0 {
            format!("Added {added} key(s); {dupes} already in the list")
        } else {
            format!("Added {added} key{}", if added == 1 { "" } else { "s" })
        })
    }
}
