//! Snapshots: list, create (optionally with RAM), rollback, delete.

use super::GCtx;
use crate::api::types::{GuestKind, Snapshot};
use crate::ops::guest;
use crate::rt::Fetch;
use crate::ui::widgets::{self, Confirm};
use crate::ui::{Actions, theme};
use crate::util::fmt_epoch;
use egui::RichText;

#[derive(Clone)]
enum Act {
    Rollback(String, bool),
    Delete(String),
}

#[derive(Default)]
pub struct SnapshotsTab {
    list: Fetch<Vec<Snapshot>>,
    can_snapshot: Fetch<bool>,
    name: String,
    desc: String,
    vmstate: bool,
    start_after: bool,
    confirm: Option<Confirm<Act>>,
}

impl SnapshotsTab {
    pub fn ui(&mut self, ui: &mut egui::Ui, gc: &GCtx, _actions: &mut Actions) {
        let g = gc.g.clone();
        if self.list.is_idle() || (self.list.is_stale() && !self.list.is_loading()) {
            let gr = g.clone();
            self.list = gc.sh.fetch(move |c| async move { guest::snapshots(&c, &gr).await });
        }
        if self.can_snapshot.is_idle() {
            let gr = g.clone();
            self.can_snapshot = gc.sh.fetch(move |c| async move {
                let v = c.get(&format!("{}/feature?feature=snapshot", gr.path())).await?;
                Ok(v.get("hasFeature").map(|h| h.as_bool().unwrap_or(h.as_u64() == Some(1))).unwrap_or(false))
            });
        }
        if let Some(a) = widgets::confirm_ui(ui.ctx(), &mut self.confirm) {
            let (c, gr) = (gc.sh.client.clone(), g.clone());
            match a {
                Act::Rollback(name, start) => {
                    gc.sh.spawn(gc.title(&format!("Rollback to {name}")), move |ctx| async move {
                        guest::snapshot_rollback(&c, &ctx, &gr, &name, start).await
                    });
                }
                Act::Delete(name) => {
                    gc.sh.spawn(gc.title(&format!("Delete snapshot {name}")), move |ctx| async move {
                        guest::snapshot_delete(&c, &ctx, &gr, &name).await
                    });
                }
            }
        }
        if let Some(false) = self.can_snapshot.ok().copied() {
            widgets::warn_box(
                ui,
                "The storage of this guest's disks does not support snapshots (e.g. raw on directory storage, or a passthrough disk).",
            );
        }
        widgets::section(ui, "Take snapshot");
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.name).hint_text("name (letters, digits, - _)").desired_width(200.0));
            ui.add(egui::TextEdit::singleline(&mut self.desc).hint_text("description").desired_width(260.0));
            if gc.g.kind == GuestKind::Qemu {
                ui.checkbox(&mut self.vmstate, "include RAM")
                    .on_hover_text("Saves running memory so rollback resumes exactly where it was");
            }
            let valid = guest::validate_snapshot_name(&self.name).is_ok();
            if ui.add_enabled(valid, egui::Button::new(RichText::new("Take snapshot").strong())).clicked() {
                let (c, gr, n, d, vm) = (gc.sh.client.clone(), g.clone(), self.name.clone(), self.desc.clone(), self.vmstate);
                gc.sh.spawn(gc.title(&format!("Snapshot {n}")), move |ctx| async move {
                    guest::snapshot_create(&c, &ctx, &gr, &n, &d, vm).await
                });
                self.name.clear();
                self.desc.clear();
            }
            if ui.small_button("Use timestamp").clicked() {
                self.name = format!("snap_{}", chrono::Local::now().format("%Y%m%d_%H%M%S"));
            }
        });
        widgets::section(ui, "Snapshots");
        ui.checkbox(&mut self.start_after, "Start the guest after a rollback");
        let start_after = self.start_after;
        let mut pending = None;
        widgets::fetch_ui(ui, &mut self.list, |ui, list| {
            if list.is_empty() {
                widgets::muted(ui, "No snapshots.");
                return;
            }
            egui::Grid::new("snaps").striped(true).num_columns(5).spacing([14.0, 6.0]).show(ui, |ui| {
                for h in ["Name", "Taken", "RAM", "Description", ""] {
                    ui.label(RichText::new(h).strong());
                }
                ui.end_row();
                for s in list.iter().rev() {
                    ui.label(RichText::new(&s.name).strong());
                    ui.label(fmt_epoch(s.snaptime));
                    ui.label(if s.vmstate { "yes" } else { "" });
                    ui.label(s.description.trim());
                    ui.horizontal(|ui| {
                        if ui.small_button("Rollback").clicked() {
                            pending = Some(
                                Confirm::new(
                                    "Rollback",
                                    format!("Roll back to '{}'? All changes since {} are lost.", s.name, fmt_epoch(s.snaptime)),
                                    Act::Rollback(s.name.clone(), start_after),
                                )
                                .danger(),
                            );
                        }
                        if ui.small_button(RichText::new("Delete").color(theme::err())).clicked() {
                            pending = Some(Confirm::new(
                                "Delete snapshot",
                                format!("Delete snapshot '{}'?", s.name),
                                Act::Delete(s.name.clone()),
                            ));
                        }
                    });
                    ui.end_row();
                }
            });
        });
        if pending.is_some() {
            self.confirm = pending;
        }
    }
}
