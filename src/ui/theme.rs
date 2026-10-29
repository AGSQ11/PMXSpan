//! Visual styling and the status colour palette.

use egui::{Color32, CornerRadius, Visuals};

pub const OK: Color32 = Color32::from_rgb(0x3f, 0xb9, 0x50);
pub const WARN: Color32 = Color32::from_rgb(0xe3, 0xa0, 0x08);
pub const ERR: Color32 = Color32::from_rgb(0xe5, 0x48, 0x4d);
pub const INFO: Color32 = Color32::from_rgb(0x3b, 0x8e, 0xea);
pub const MUTED: Color32 = Color32::from_rgb(0x8b, 0x94, 0x9e);
pub const ACCENT: Color32 = Color32::from_rgb(0xe5, 0x70, 0x00);

pub fn apply(ctx: &egui::Context) {
    set_dark(ctx, true);
}

pub fn set_dark(ctx: &egui::Context, dark: bool) {
    let mut v = if dark { Visuals::dark() } else { Visuals::light() };
    v.selection.bg_fill = if dark { Color32::from_rgb(0x8a, 0x45, 0x05) } else { Color32::from_rgb(0xff, 0xc8, 0x8a) };
    v.selection.stroke.color = if dark { Color32::WHITE } else { Color32::BLACK };
    v.hyperlink_color = INFO;
    v.window_corner_radius = CornerRadius::same(8);
    v.menu_corner_radius = CornerRadius::same(6);
    ctx.set_visuals(v);
}

pub fn status_color(status: &str) -> Color32 {
    match status {
        "running" | "online" | "available" | "OK" | "active" => OK,
        "paused" | "suspended" | "prelaunch" => WARN,
        "stopped" => MUTED,
        "" | "unknown" => MUTED,
        _ => ERR,
    }
}

pub fn usage_color(frac: f32) -> Color32 {
    if frac >= 0.9 {
        ERR
    } else if frac >= 0.75 {
        WARN
    } else {
        OK
    }
}
