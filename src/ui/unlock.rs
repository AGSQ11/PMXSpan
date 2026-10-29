//! First-run vault setup and the unlock screen.

use super::{theme, widgets};
use crate::config::AppPaths;
use crate::vault::Vault;
use egui::RichText;

#[derive(Default)]
pub struct UnlockView {
    exists: bool,
    password: String,
    confirm: String,
    no_password: bool,
    focused: bool,
    error: Option<String>,
}

impl UnlockView {
    pub fn new(existing: Option<&Vault>) -> Self {
        Self { exists: existing.is_some(), ..Default::default() }
    }

    /// Creates or unlocks `vault` in place. Returns true once it is unlocked.
    pub fn ui(&mut self, ui: &mut egui::Ui, paths: &AppPaths, vault: &mut Option<Vault>) -> bool {
        let mut done = false;
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.18);
            ui.label(RichText::new("ProxmoxPanel").size(32.0).strong().color(theme::ACCENT));
            ui.label(RichText::new("Standalone Proxmox VE manager").color(theme::MUTED));
            ui.add_space(24.0);
            ui.set_max_width(380.0);
            if self.exists {
                ui.label("Enter the master password to unlock stored server credentials.");
                let r = ui.add(egui::TextEdit::singleline(&mut self.password).password(true).hint_text("Master password"));
                if !self.focused {
                    r.request_focus();
                    self.focused = true;
                }
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if ui.button("Unlock").clicked() || enter {
                    if vault.is_none() {
                        match Vault::open(&paths.vault_file()) {
                            Ok(v) => *vault = Some(v),
                            Err(e) => self.error = Some(format!("{e:#}")),
                        }
                    }
                    if let Some(v) = vault.as_mut() {
                        match v.unlock(&self.password) {
                            Ok(()) => done = true,
                            Err(e) => self.error = Some(e.to_string()),
                        }
                    }
                    self.password.clear();
                }
                ui.add_space(8.0);
                widgets::muted(ui, format!("Vault: {}", paths.vault_file().display()));
            } else {
                ui.label(RichText::new("Welcome! Choose a master password.").strong());
                ui.label("It encrypts the API tokens and passwords you save for your Proxmox servers.");
                ui.add_space(8.0);
                ui.checkbox(&mut self.no_password, "Don't use a master password (not recommended)");
                if !self.no_password {
                    ui.add(egui::TextEdit::singleline(&mut self.password).password(true).hint_text("Master password"));
                    ui.add(egui::TextEdit::singleline(&mut self.confirm).password(true).hint_text("Repeat password"));
                } else {
                    widgets::warn_box(ui, "Credentials will be stored obfuscated but readable by anyone with access to your user profile.");
                }
                ui.add_space(6.0);
                if ui.button("Create vault").clicked() {
                    let pw = if self.no_password { String::new() } else { self.password.clone() };
                    if !self.no_password && pw.len() < 8 {
                        self.error = Some("use at least 8 characters".into());
                    } else if !self.no_password && pw != self.confirm {
                        self.error = Some("passwords do not match".into());
                    } else {
                        match Vault::create(&paths.vault_file(), &pw) {
                            Ok(v) => {
                                *vault = Some(v);
                                done = true;
                            }
                            Err(e) => self.error = Some(format!("{e:#}")),
                        }
                    }
                }
            }
            if let Some(e) = &self.error {
                ui.add_space(8.0);
                widgets::error_box(ui, e);
            }
        });
        done
    }
}
