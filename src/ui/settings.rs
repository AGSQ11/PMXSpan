//! Preferences, vault management and the application log.

use super::{Action, Actions, widgets};
use crate::config::{AppConfig, AppPaths, PlacementStrategy, Resource, ThemePref};
use crate::vault::Vault;
use egui::RichText;

#[derive(Default)]
pub struct SettingsView {
    new_master: String,
    new_master2: String,
    log_filter: String,
    tab: u8,
    pubkeys: super::pubkeys::Picker,
}

impl SettingsView {
    pub fn ui(&mut self, ui: &mut egui::Ui, config: &mut AppConfig, vault: &mut Vault, paths: &AppPaths, actions: &mut Actions) {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.tab, 0, "Preferences");
            ui.selectable_value(&mut self.tab, 1, "Security");
            ui.selectable_value(&mut self.tab, 2, "Application log");
        });
        ui.separator();
        match self.tab {
            0 => self.prefs(ui, config, actions),
            1 => self.security(ui, vault, paths, actions),
            _ => self.logs(ui, paths),
        }
    }

    fn prefs(&mut self, ui: &mut egui::Ui, config: &mut AppConfig, actions: &mut Actions) {
        let s = &mut config.settings;
        let before = serde_json::to_string(s).unwrap_or_default();
        egui::ScrollArea::vertical().show(ui, |ui| {
            egui::Grid::new("prefs").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
                ui.label("Theme");
                ui.horizontal(|ui| {
                    ui.radio_value(&mut s.theme, ThemePref::Dark, "Graphite (dark)");
                    ui.radio_value(&mut s.theme, ThemePref::Light, "Daylight (light)");
                    ui.radio_value(&mut s.theme, ThemePref::System, "Follow system");
                });
                ui.end_row();
                ui.label("UI scale");
                ui.add(egui::Slider::new(&mut s.ui_scale, 0.75..=2.0).step_by(0.05));
                ui.end_row();
                ui.label("Refresh interval");
                ui.add(egui::DragValue::new(&mut s.refresh_secs).range(2..=300).suffix(" s"));
                ui.end_row();
                ui.label("Confirm destructive actions");
                ui.checkbox(&mut s.confirm_destructive, "");
                ui.end_row();
                ui.label("Auto-lock after");
                ui.add(egui::DragValue::new(&mut s.auto_lock_mins).range(0..=1440).suffix(" min idle"))
                    .on_hover_text("Locks a password-protected vault and disconnects. 0 = never.");
                ui.end_row();

                ui.label(RichText::new("Provisioning").strong());
                ui.end_row();
                ui.label("Auto-placement");
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        ui.radio_value(&mut s.placement, PlacementStrategy::Sparse, "Sparse (least loaded)");
                        ui.radio_value(&mut s.placement, PlacementStrategy::Dense, "Dense (consolidate)");
                        ui.radio_value(&mut s.placement, PlacementStrategy::Random, "Random");
                    });
                    ui.horizontal(|ui| {
                        ui.label("Priority:");
                        let mut swap = None;
                        for (i, r) in s.resource_priority.iter().enumerate() {
                            ui.label(RichText::new(r.label()).strong());
                            if i + 1 < s.resource_priority.len() && ui.small_button("⇄").on_hover_text("swap order").clicked() {
                                swap = Some(i);
                            }
                        }
                        if let Some(i) = swap {
                            s.resource_priority.swap(i, i + 1);
                        }
                        if s.resource_priority.len() < 3 {
                            s.resource_priority = vec![Resource::Ram, Resource::Cpu, Resource::Disk];
                        }
                    });
                });
                ui.end_row();
                ui.label("Default bridge");
                ui.text_edit_singleline(&mut s.default_bridge);
                ui.end_row();
                ui.label("Default DNS servers");
                ui.add(egui::TextEdit::singleline(&mut s.default_dns).hint_text("1.1.1.1 9.9.9.9"));
                ui.end_row();
                ui.label("Default search domain");
                ui.text_edit_singleline(&mut s.default_search_domain);
                ui.end_row();
                ui.label("MAC address prefix");
                ui.add(egui::TextEdit::singleline(&mut s.mac_prefix).hint_text("BC:24:11"));
                ui.end_row();
                ui.label("Tag managed guests");
                ui.horizontal(|ui| {
                    ui.checkbox(&mut s.tag_managed_guests, "add tag");
                    ui.add_enabled(s.tag_managed_guests, egui::TextEdit::singleline(&mut s.managed_tag).desired_width(140.0));
                });
                ui.end_row();
                ui.label("Default SSH keys");
                ui.add(
                    egui::TextEdit::multiline(&mut s.default_ssh_keys)
                        .desired_rows(3)
                        .desired_width(520.0)
                        .font(egui::TextStyle::Monospace),
                );
                ui.end_row();
                ui.label("");
                self.pubkeys.button(ui);
                ui.end_row();
                ui.label("Task watch timeout");
                ui.add(egui::DragValue::new(&mut s.task_timeout_secs).range(60..=86400).suffix(" s"));
                ui.end_row();
                ui.label("Before a rebuild");
                ui.horizontal(|ui| {
                    ui.checkbox(&mut s.backup_before_rebuild, "back the guest up (default)");
                    ui.add_enabled(
                        s.backup_before_rebuild,
                        egui::TextEdit::singleline(&mut s.rebuild_backup_storage)
                            .hint_text("storage (empty = first backup storage)")
                            .desired_width(220.0),
                    );
                });
                ui.end_row();
                ui.label("Catalog downloads");
                ui.horizontal(|ui| {
                    ui.label("stalled after");
                    ui.add(egui::DragValue::new(&mut s.download_stall_secs).range(30..=3600).suffix(" s"))
                        .on_hover_text("A download whose task log stops growing for this long is aborted and retried elsewhere.");
                    ui.label("give up after");
                    ui.add(egui::DragValue::new(&mut s.download_max_wait_secs).range(60..=86400).suffix(" s"));
                    ui.label("min. free space");
                    let mut gib = s.download_min_bytes as f64 / 1073741824.0;
                    if ui
                        .add(egui::DragValue::new(&mut gib).range(0.0..=1024.0).speed(0.5).suffix(" GiB"))
                        .on_hover_text("0 = automatic (image size, at least 3 GiB / 1 GiB)")
                        .changed()
                    {
                        s.download_min_bytes = (gib * 1073741824.0) as u64;
                    }
                });
                ui.end_row();
            });
        });
        if let Some(m) = self.pubkeys.show(ui.ctx(), "settings", &mut s.default_ssh_keys) {
            actions.toast_ok(m);
        }
        if serde_json::to_string(s).unwrap_or_default() != before {
            actions.0.push(Action::ConfigChanged);
        }
    }

    fn security(&mut self, ui: &mut egui::Ui, vault: &mut Vault, paths: &AppPaths, actions: &mut Actions) {
        if vault.is_protected() {
            widgets::info_box(ui, "Stored credentials are encrypted with your master password (Argon2id + XChaCha20-Poly1305).");
        } else {
            widgets::warn_box(ui, "No master password is set: stored credentials are only obfuscated.");
        }
        ui.add_space(8.0);
        ui.label(RichText::new("Change master password").strong());
        ui.add(egui::TextEdit::singleline(&mut self.new_master).password(true).hint_text("New master password (empty = none)"));
        ui.add(egui::TextEdit::singleline(&mut self.new_master2).password(true).hint_text("Repeat"));
        if ui.button("Change").clicked() {
            if self.new_master != self.new_master2 {
                actions.toast_err("Passwords do not match");
            } else if !self.new_master.is_empty() && self.new_master.len() < 8 {
                actions.toast_err("Use at least 8 characters");
            } else {
                match vault.change_master(&self.new_master) {
                    Ok(()) => actions.toast_ok("Master password changed"),
                    Err(e) => actions.toast_err(format!("{e:#}")),
                }
                self.new_master.clear();
                self.new_master2.clear();
            }
        }
        ui.add_space(12.0);
        ui.label(RichText::new("Files").strong());
        egui::Grid::new("paths").num_columns(2).show(ui, |ui| {
            widgets::kv(ui, "Config", paths.config_file().display().to_string());
            widgets::kv(ui, "Vault", paths.vault_file().display().to_string());
            widgets::kv(ui, "IPAM database", paths.ipam_file().display().to_string());
            widgets::kv(ui, "SSH host keys", paths.hostkeys_file().display().to_string());
            widgets::kv(ui, "Logs", paths.log_dir.display().to_string());
        });
        widgets::muted(ui, "Tip: create an empty file named 'portable' next to the executable to keep all data beside it.");
    }

    fn logs(&mut self, ui: &mut egui::Ui, paths: &AppPaths) {
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.log_filter).hint_text("Filter…"));
            if ui.button("Open log folder").clicked() {
                let _ = crate::ops::guest::open_path(&paths.log_dir);
            }
            if ui.button("Clear view").clicked() {
                crate::logging::clear_memory();
            }
        });
        widgets::muted(ui, "Credentials are redacted before anything is written. Set PMXSPAN_LOG=debug for more detail.");
        let f = self.log_filter.to_lowercase();
        let lines: Vec<String> =
            crate::logging::recent_lines().into_iter().filter(|l| f.is_empty() || l.to_lowercase().contains(&f)).collect();
        egui::ScrollArea::both().stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
            for l in lines {
                let color = if l.contains(" ERROR ") {
                    super::theme::err()
                } else if l.contains(" WARN ") {
                    super::theme::warn()
                } else {
                    ui.visuals().text_color()
                };
                ui.label(RichText::new(l).monospace().size(11.5).color(color));
            }
        });
    }
}
