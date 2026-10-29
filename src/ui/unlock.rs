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
        let p = theme::p();
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.16);
            let (rect, _) = ui.allocate_exact_size(egui::vec2(52.0, 52.0), egui::Sense::hover());
            ui.painter().rect_filled(rect, egui::CornerRadius::same(14), p.accent);
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                theme::icon::HARD_DRIVES,
                egui::FontId::proportional(30.0),
                p.on_accent,
            );
            ui.add_space(10.0);
            ui.label(theme::bold("PMXSpan", 26.0));
            ui.label(RichText::new("Desktop manager for Proxmox VE").color(p.muted));
            ui.add_space(22.0);
            ui.set_max_width(420.0);
            theme::card_frame().inner_margin(egui::Margin::same(22)).show(ui, |ui| {
                ui.set_max_width(380.0);
                if self.exists {
                    ui.label("Enter the master password to unlock stored server credentials.");
                    ui.add_space(4.0);
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut self.password)
                            .password(true)
                            .hint_text("Master password")
                            .desired_width(f32::INFINITY)
                            .margin(egui::Margin::symmetric(10, 8)),
                    );
                    if !self.focused {
                        r.request_focus();
                        self.focused = true;
                    }
                    let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    ui.add_space(4.0);
                    if widgets::primary_button(ui, format!("{}  Unlock", theme::icon::LOCK)).clicked() || enter {
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
                    ui.label(RichText::new(format!("Vault: {}", paths.vault_file().display())).size(11.5).color(p.muted));
                } else {
                    ui.label(RichText::new("Welcome! Choose a master password.").strong());
                    ui.label("It encrypts the API tokens and passwords you save for your Proxmox servers.");
                    ui.add_space(8.0);
                    ui.checkbox(&mut self.no_password, "Don't use a master password (not recommended)");
                    if !self.no_password {
                        ui.add(egui::TextEdit::singleline(&mut self.password).password(true).hint_text("Master password"));
                        ui.add(egui::TextEdit::singleline(&mut self.confirm).password(true).hint_text("Repeat password"));
                    } else {
                        widgets::warn_box(
                            ui,
                            "Credentials will be stored obfuscated but readable by anyone with access to your user profile.",
                        );
                    }
                    ui.add_space(6.0);
                    if widgets::primary_button(ui, "Create vault").clicked() {
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
        });
        done
    }
}
