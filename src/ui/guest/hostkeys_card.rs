//! The Summary's "SSH host keys" card: the fingerprints PMXSpan read from the
//! guest, to compare on the first connection with any SSH client.

use super::GCtx;
use crate::api::types::GuestKind;
use crate::hostkeys::Outcome;
use crate::rt::Fetch;
use crate::ui::{theme, widgets};
use crate::util::fmt_epoch;
use egui::RichText;

#[derive(Default)]
pub struct HostKeysCard {
    refresh: Fetch<Outcome>,
    note: Option<Result<String, String>>,
}

impl HostKeysCard {
    pub fn ui(&mut self, ui: &mut egui::Ui, gc: &GCtx) {
        let p = theme::p();
        if let Some(r) = self.refresh.take() {
            self.note = Some(r.map(|o| {
                match o {
                    Outcome::New => "Host keys read.",
                    Outcome::Unchanged => "Host keys unchanged.",
                    Outcome::Rotated => "Host keys changed (rebuild).",
                    Outcome::Changed => "Host keys changed since the last read.",
                }
                .to_string()
            }));
        }
        widgets::card(ui, |ui| {
            ui.horizontal(|ui| {
                widgets::card_title(ui, "SSH host keys");
                if gc.g.kind != GuestKind::Qemu {
                    return;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.refresh.is_loading() {
                        ui.spinner();
                    } else if ui
                        .add_enabled(gc.running(), egui::Button::new(format!("{}  Refresh", theme::icon::REFRESH)).small())
                        .on_hover_text("Read /etc/ssh/ssh_host_*_key.pub through the guest agent")
                        .on_disabled_hover_text("The guest must be running with its agent")
                        .clicked()
                    {
                        let (c, sid, g) = (gc.sh.client.clone(), gc.sh.server_id.clone(), gc.g.clone());
                        self.note = None;
                        self.refresh = Fetch::spawn(async move { crate::hostkeys::refresh(&c, &sid, &g).await });
                    }
                });
            });
            if gc.g.kind != GuestKind::Qemu {
                widgets::muted(ui, "Not available for containers: the Proxmox API offers no way to read them.");
                return;
            }
            let rec = crate::hostkeys::global().and_then(|s| s.lock().get(&gc.sh.server_id, gc.g.vmid).cloned());
            match rec.filter(|r| !r.keys.is_empty()) {
                None => widgets::muted(
                    ui,
                    "Not read yet. They are read through the guest agent: with Refresh, and automatically after PMXSpan creates or \
                     rebuilds the VM.",
                ),
                Some(r) => {
                    for k in &r.keys {
                        ui.horizontal(|ui| {
                            widgets::pill(ui, crate::sshkey::short_type(&k.key_type), widgets::Tone::Neutral);
                            ui.label(RichText::new(&k.fingerprint_sha256).monospace().size(12.0));
                            if ui.small_button(theme::icon::COPY).on_hover_text("Copy the fingerprint").clicked() {
                                ui.ctx().copy_text(k.fingerprint_sha256.clone());
                            }
                        });
                    }
                    ui.add_space(2.0);
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new(format!("read {}", fmt_epoch(r.fetched_at()))).size(12.0).color(p.muted));
                        if let Some(t) = r.rotated_at {
                            widgets::pill(ui, format!("rotated {}", fmt_epoch(t)), widgets::Tone::Warn)
                                .on_hover_text("A PMXSpan rebuild gave the guest new host keys; the previous ones are kept for SSHSpan.");
                        }
                        if let Some(t) = r.changed_at {
                            widgets::pill(ui, format!("{} changed {}", theme::icon::WARNING, fmt_epoch(t)), widgets::Tone::Err)
                                .on_hover_text("The keys changed without a PMXSpan rebuild. Make sure this was expected.");
                        }
                    });
                }
            }
            match &self.note {
                Some(Ok(m)) => {
                    ui.label(RichText::new(m).size(12.0).color(theme::ok()));
                }
                Some(Err(e)) => {
                    ui.label(
                        RichText::new(format!("Could not read them: {}", e.lines().next().unwrap_or(""))).size(12.0).color(theme::err()),
                    );
                }
                None => {}
            }
        });
    }
}
