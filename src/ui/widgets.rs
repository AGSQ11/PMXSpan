//! Reusable UI pieces.

use super::theme;
use crate::rt::Fetch;
use egui::{Color32, CornerRadius, RichText, Ui};

pub fn status_dot(ui: &mut Ui, status: &str) {
    let color = theme::status_color(status);
    dot(ui, color);
}

/// A small coloured dot, sized to sit in a line of text.
pub fn dot(ui: &mut Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(8.0, 12.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 3.5, color);
}

/// A thin rounded bar filled to `frac`.
pub fn meter(ui: &mut Ui, frac: f32, color: Color32, width: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 6.0), egui::Sense::hover());
    let p = theme::p();
    ui.painter().rect_filled(rect, CornerRadius::same(3), p.track);
    let frac = frac.clamp(0.0, 1.0);
    if frac > 0.0 {
        let mut fill = rect;
        fill.set_width((rect.width() * frac).max(6.0));
        ui.painter().rect_filled(fill, CornerRadius::same(3), color);
    }
}

/// Usage bar with its value beside it; coloured by fill level.
pub fn usage_bar(ui: &mut Ui, frac: f32, text: impl Into<String>, width: f32) {
    ui.horizontal(|ui| {
        meter(ui, frac, theme::usage_color(frac), (width * 0.45).max(60.0));
        ui.label(RichText::new(text.into()).monospace().size(12.0).color(theme::p().text2));
    });
}

/// "Label ............ value" over a full-width meter, as in the resource cards.
pub fn meter_row(ui: &mut Ui, label: &str, frac: f32, color: Color32, value: impl Into<String>) {
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        ui.spacing_mut().interact_size.y = 18.0;
        ui.horizontal(|ui| {
            ui.label(RichText::new(label).size(12.5).color(theme::muted()));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(value.into()).monospace().size(12.0));
            });
        });
        meter(ui, frac, color, ui.available_width());
    });
    ui.add_space(3.0);
}

/// Vertical scroll area for a page, with room on the right for the scroll bar.
pub fn page<R>(ui: &mut Ui, id: &str, add: impl FnOnce(&mut Ui) -> R) -> R {
    egui::ScrollArea::vertical()
        .id_salt(id)
        .auto_shrink([false, false])
        .show(ui, |ui| egui::Frame::new().inner_margin(egui::Margin { left: 0, right: 14, top: 0, bottom: 8 }).show(ui, add).inner)
        .inner
}

/// Page title.
pub fn heading(ui: &mut Ui, text: &str) {
    ui.add_space(2.0);
    ui.label(theme::bold(text, 20.0));
    ui.add_space(4.0);
}

/// Section title inside a page.
pub fn section(ui: &mut Ui, text: &str) {
    ui.add_space(10.0);
    ui.label(theme::bold(text, 15.0));
    ui.add_space(2.0);
}

/// Title row of a card.
pub fn card_title(ui: &mut Ui, text: &str) {
    ui.label(theme::bold(text, 14.0));
    ui.add_space(2.0);
}

/// A content card spanning the available width.
pub fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    theme::card_frame()
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

pub fn muted(ui: &mut Ui, text: impl Into<String>) {
    ui.label(RichText::new(text.into()).color(theme::muted()));
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Ok,
    Info,
    Err,
    Warn,
    Neutral,
}

/// A rounded status label.
pub fn pill(ui: &mut Ui, text: impl Into<String>, tone: Tone) -> egui::Response {
    let p = theme::p();
    let (bg, fg) = match tone {
        Tone::Ok => p.pill_ok,
        Tone::Info => p.pill_info,
        Tone::Err => p.pill_err,
        Tone::Warn => p.pill_warn,
        Tone::Neutral => (p.chip, p.text2),
    };
    egui::Frame::new()
        .fill(bg)
        .corner_radius(CornerRadius::same(255))
        .inner_margin(egui::Margin::symmetric(8, 2))
        .show(ui, |ui| ui.label(RichText::new(text.into()).family(theme::semibold()).size(11.5).color(fg)))
        .response
}

/// Pill for a guest/node status string.
pub fn status_pill(ui: &mut Ui, status: &str) -> egui::Response {
    let tone = match status {
        "running" | "online" | "OK" => Tone::Ok,
        "paused" | "suspended" | "prelaunch" => Tone::Warn,
        "stopped" | "" | "unknown" | "offline" => Tone::Neutral,
        _ => Tone::Err,
    };
    let mut label = status.to_string();
    if let Some(c) = label.get_mut(0..1) {
        c.make_ascii_uppercase();
    }
    pill(ui, label, tone)
}

/// The accent-filled main action of a view.
pub fn primary_button(ui: &mut Ui, text: impl Into<String>) -> egui::Response {
    let p = theme::p();
    ui.add(
        egui::Button::new(RichText::new(text.into()).family(theme::semibold()).color(p.on_accent))
            .fill(p.accent)
            .stroke(egui::Stroke::NONE)
            .corner_radius(CornerRadius::same(9))
            .min_size(egui::vec2(0.0, 32.0)),
    )
}

/// A secondary button with an icon in front.
pub fn icon_button(ui: &mut Ui, glyph: &str, text: &str) -> egui::Response {
    let label = if text.is_empty() { glyph.to_string() } else { format!("{glyph}  {text}") };
    ui.add(egui::Button::new(RichText::new(label)).corner_radius(CornerRadius::same(9)).min_size(egui::vec2(32.0, 32.0)))
}

/// Underlined tab strip. Returns true when the selection changed.
pub fn tab_bar<T: PartialEq + Copy>(ui: &mut Ui, current: &mut T, tabs: &[(T, &str)]) -> bool {
    let p = theme::p();
    let mut changed = false;
    egui::ScrollArea::horizontal().id_salt("tabbar").show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 22.0;
            for (value, label) in tabs {
                let selected = *current == *value;
                let text = if selected {
                    RichText::new(*label).family(theme::semibold()).color(p.text)
                } else {
                    RichText::new(*label).color(p.muted)
                };
                let r = ui.add(egui::Button::new(text).frame(false).min_size(egui::vec2(0.0, 34.0)));
                let r = r.on_hover_cursor(egui::CursorIcon::PointingHand);
                if selected {
                    let rect = r.rect;
                    ui.painter().line_segment(
                        [egui::pos2(rect.left() - 2.0, rect.bottom() - 1.0), egui::pos2(rect.right() + 2.0, rect.bottom() - 1.0)],
                        egui::Stroke::new(2.0, p.accent),
                    );
                } else if r.hovered() {
                    let rect = r.rect;
                    ui.painter().line_segment(
                        [egui::pos2(rect.left() - 2.0, rect.bottom() - 1.0), egui::pos2(rect.right() + 2.0, rect.bottom() - 1.0)],
                        egui::Stroke::new(2.0, p.border2),
                    );
                }
                if r.clicked() && !selected {
                    *current = *value;
                    changed = true;
                }
            }
        });
    });
    changed
}

/// A full-width sidebar entry with an icon.
pub fn nav_item(ui: &mut Ui, glyph: &str, label: &str, selected: bool, enabled: bool) -> egui::Response {
    let p = theme::p();
    let fg = if !enabled {
        p.stopped
    } else if selected {
        p.nav_on_fg
    } else {
        p.text2
    };
    let width = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, 32.0), if enabled { egui::Sense::click() } else { egui::Sense::hover() });
    if selected {
        ui.painter().rect_filled(rect, CornerRadius::same(8), p.nav_on_bg);
    } else if resp.hovered() && enabled {
        ui.painter().rect_filled(rect, CornerRadius::same(8), p.surface2);
    }
    let painter = ui.painter();
    painter.text(egui::pos2(rect.left() + 12.0, rect.center().y), egui::Align2::LEFT_CENTER, glyph, egui::FontId::proportional(17.0), fg);
    let font = if selected { egui::FontId::new(13.5, theme::semibold()) } else { egui::FontId::proportional(13.5) };
    painter.text(egui::pos2(rect.left() + 40.0, rect.center().y), egui::Align2::LEFT_CENTER, label, font, fg);
    if enabled { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp }
}

fn notice(ui: &mut Ui, color: Color32, glyph: &str, text: &str, colored_text: bool) {
    egui::Frame::new()
        .fill(color.gamma_multiply(0.12))
        .stroke(egui::Stroke::new(1.0, color.gamma_multiply(0.55)))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(egui::Margin::symmetric(12, 9))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(glyph).color(color).size(15.0));
                let t = RichText::new(text);
                ui.label(if colored_text { t.color(color) } else { t });
            });
        });
}

pub fn error_box(ui: &mut Ui, err: &str) {
    notice(ui, theme::err(), theme::icon::WARNING, err, true);
}

pub fn warn_box(ui: &mut Ui, text: &str) {
    notice(ui, theme::warn(), theme::icon::WARNING, text, true);
}

pub fn info_box(ui: &mut Ui, text: &str) {
    notice(ui, theme::info(), theme::icon::INFO, text, false);
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

/// Password entry with show/hide, generate and copy.
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
    ui.label(RichText::new(k).color(theme::muted()));
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
                        if c.danger { RichText::new("Confirm").color(theme::err()).strong() } else { RichText::new("Confirm").strong() };
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
