//! Reusable UI pieces.

use super::theme;
use crate::rt::Fetch;
use egui::{Color32, RichText, Ui};

pub fn status_dot(ui: &mut Ui, status: &str) {
    let color = theme::status_color(status);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.5, color);
}

/// Horizontal usage bar with a label, coloured by fill level.
pub fn usage_bar(ui: &mut Ui, frac: f32, text: impl Into<String>, width: f32) {
    let bar = egui::ProgressBar::new(frac)
        .desired_width(width)
        .fill(theme::usage_color(frac))
        .text(RichText::new(text.into()).color(Color32::WHITE).small());
    ui.add(bar);
}

pub fn heading(ui: &mut Ui, text: &str) {
    ui.add_space(4.0);
    ui.label(RichText::new(text).heading().strong());
    ui.add_space(2.0);
}

pub fn section(ui: &mut Ui, text: &str) {
    ui.add_space(6.0);
    ui.label(RichText::new(text).strong().size(15.0));
    ui.separator();
}

pub fn muted(ui: &mut Ui, text: impl Into<String>) {
    ui.label(RichText::new(text.into()).color(theme::MUTED));
}

pub fn error_box(ui: &mut Ui, err: &str) {
    egui::Frame::new()
        .fill(theme::ERR.linear_multiply(0.15))
        .stroke(egui::Stroke::new(1.0, theme::ERR))
        .corner_radius(4.0)
        .inner_margin(8.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(err).color(theme::ERR));
        });
}

pub fn warn_box(ui: &mut Ui, text: &str) {
    egui::Frame::new()
        .fill(theme::WARN.linear_multiply(0.12))
        .stroke(egui::Stroke::new(1.0, theme::WARN))
        .corner_radius(4.0)
        .inner_margin(8.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(text).color(theme::WARN));
        });
}

pub fn info_box(ui: &mut Ui, text: &str) {
    egui::Frame::new()
        .fill(theme::INFO.linear_multiply(0.10))
        .stroke(egui::Stroke::new(1.0, theme::INFO))
        .corner_radius(4.0)
        .inner_margin(8.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(text);
        });
}

/// Renders a fetch: spinner while loading, error box on failure, content when ready.
/// Returns true if the user clicked "Retry".
pub fn fetch_ui<T: Send + 'static>(ui: &mut Ui, f: &mut Fetch<T>, content: impl FnOnce(&mut Ui, &mut T)) -> bool {
    let mut retry = false;
    if f.is_loading() {
        ui.horizontal(|ui| {
            ui.spinner();
            muted(ui, "Loading…");
        });
        return false;
    }
    match f.poll_mut() {
        None => {
            muted(ui, "Not loaded.");
            retry = ui.button("Load").clicked();
        }
        Some(Err(e)) => {
            let e = e.clone();
            error_box(ui, &e);
            retry = ui.button("Retry").clicked();
        }
        Some(Ok(v)) => content(ui, v),
    }
    retry
}

/// Password entry with show/hide, generate and copy - like the module's admin reinstall form.
pub fn password_field(ui: &mut Ui, value: &mut String, show: &mut bool, id: &str) {
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(value).password(!*show).desired_width(220.0).id_salt(id));
        if ui.small_button(if *show { "Hide" } else { "Show" }).clicked() {
            *show = !*show;
        }
        if ui.small_button("Generate").clicked() {
            *value = crate::util::generate_password(16);
            *show = true;
        }
        if ui.small_button("Copy").clicked() {
            ui.ctx().copy_text(value.clone());
        }
    });
}

pub fn copyable(ui: &mut Ui, text: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(text).monospace());
        if ui.small_button("Copy").on_hover_text("Copy to clipboard").clicked() {
            ui.ctx().copy_text(text.to_string());
        }
    });
}

pub fn kv(ui: &mut Ui, k: &str, v: impl Into<String>) {
    ui.label(RichText::new(k).color(theme::MUTED));
    ui.label(v.into());
    ui.end_row();
}

/// A modal confirmation carrying a payload. When `require` is set the user must
/// type it exactly (used for destroy / rebuild, mirroring "type the VMID").
pub struct Confirm<T> {
    pub title: String,
    pub body: String,
    pub payload: T,
    pub require: Option<String>,
    pub typed: String,
    pub danger: bool,
}

impl<T: Clone> Confirm<T> {
    pub fn new(title: impl Into<String>, body: impl Into<String>, payload: T) -> Self {
        Self { title: title.into(), body: body.into(), payload, require: None, typed: String::new(), danger: false }
    }
    pub fn danger(mut self) -> Self {
        self.danger = true;
        self
    }
    pub fn require(mut self, text: impl Into<String>) -> Self {
        self.require = Some(text.into());
        self
    }
}

/// Shows a pending confirmation. Returns the payload once confirmed.
pub fn confirm_ui<T: Clone>(ctx: &egui::Context, slot: &mut Option<Confirm<T>>) -> Option<T> {
    let mut result = None;
    let mut close = false;
    if let Some(c) = slot.as_mut() {
        egui::Window::new(RichText::new(&c.title).strong())
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_max_width(460.0);
                ui.label(&c.body);
                if let Some(req) = &c.require {
                    ui.add_space(6.0);
                    ui.label(format!("Type {req} to confirm:"));
                    ui.text_edit_singleline(&mut c.typed);
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let ok_enabled = c.require.as_ref().map(|r| r == c.typed.trim()).unwrap_or(true);
                    let label =
                        if c.danger { RichText::new("Confirm").color(theme::ERR).strong() } else { RichText::new("Confirm").strong() };
                    if ui.add_enabled(ok_enabled, egui::Button::new(label)).clicked() {
                        result = Some(c.payload.clone());
                        close = true;
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });
    }
    if close {
        *slot = None;
    }
    result
}

/// Combo box over a list of strings; returns true if changed.
pub fn combo(ui: &mut Ui, id: &str, value: &mut String, options: &[String], width: f32) -> bool {
    let mut changed = false;
    egui::ComboBox::from_id_salt(id)
        .width(width)
        .selected_text(if value.is_empty() { "(choose)".to_string() } else { value.clone() })
        .show_ui(ui, |ui| {
            for o in options {
                if ui.selectable_label(value == o, o).clicked() {
                    *value = o.clone();
                    changed = true;
                }
            }
        });
    changed
}

/// Optional combo: `None` entry labelled `none_label`.
pub fn combo_opt(ui: &mut Ui, id: &str, value: &mut Option<String>, options: &[String], none_label: &str, width: f32) {
    egui::ComboBox::from_id_salt(id).width(width).selected_text(value.clone().unwrap_or_else(|| none_label.to_string())).show_ui(
        ui,
        |ui| {
            if ui.selectable_label(value.is_none(), none_label).clicked() {
                *value = None;
            }
            for o in options {
                if ui.selectable_label(value.as_deref() == Some(o.as_str()), o).clicked() {
                    *value = Some(o.clone());
                }
            }
        },
    );
}
