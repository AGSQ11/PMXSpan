//! Visual styling: the Graphite (dark) and Daylight (light) palettes, the
//! bundled fonts and icon font, and the egui visuals built from them.

use egui::epaint::Shadow;
use egui::{Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, RichText, Stroke, TextStyle, Visuals};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Every colour the UI uses, per theme.
pub struct Palette {
    pub dark: bool,
    /// Window background behind the cards.
    pub bg: Color32,
    /// Sidebar and top bar.
    pub side: Color32,
    pub surface: Color32,
    /// Buttons, inputs and inset areas.
    pub surface2: Color32,
    pub border: Color32,
    pub border2: Color32,
    pub chip: Color32,
    pub text: Color32,
    pub text2: Color32,
    pub muted: Color32,
    pub track: Color32,
    pub accent: Color32,
    pub on_accent: Color32,
    pub link: Color32,
    pub ok: Color32,
    pub warn: Color32,
    pub err: Color32,
    pub info: Color32,
    pub cpu: Color32,
    pub mem: Color32,
    pub disk: Color32,
    pub stopped: Color32,
    pub nav_on_bg: Color32,
    pub nav_on_fg: Color32,
    /// Tinted backgrounds for pills: (ok, info, err, warn).
    pub pill_ok: (Color32, Color32),
    pub pill_info: (Color32, Color32),
    pub pill_err: (Color32, Color32),
    pub pill_warn: (Color32, Color32),
}

const fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

const fn rgba(hex: u32, a: u8) -> Color32 {
    // Premultiplied alpha, as egui expects.
    let r = ((hex >> 16) & 0xff) * a as u32 / 255;
    let g = ((hex >> 8) & 0xff) * a as u32 / 255;
    let b = (hex & 0xff) * a as u32 / 255;
    Color32::from_rgba_premultiplied(r as u8, g as u8, b as u8, a)
}

pub static GRAPHITE: Palette = Palette {
    dark: true,
    bg: rgb(0x0F1115),
    side: rgb(0x14171D),
    surface: rgb(0x171A21),
    surface2: rgb(0x1B1F27),
    border: rgb(0x232833),
    border2: rgb(0x2A303C),
    chip: rgb(0x222733),
    text: rgb(0xE6E8EC),
    text2: rgb(0xC3C9D4),
    muted: rgb(0x9AA3B2),
    track: rgb(0x232833),
    accent: rgb(0xF07B1F),
    on_accent: rgb(0x14171D),
    link: rgb(0xF59E4C),
    ok: rgb(0x3FB950),
    warn: rgb(0xD29922),
    err: rgb(0xF0605D),
    info: rgb(0x58A6FF),
    cpu: rgb(0x58A6FF),
    mem: rgb(0xBC8CFF),
    disk: rgb(0x8B93A3),
    stopped: rgb(0x4B5363),
    nav_on_bg: rgb(0x222733),
    nav_on_fg: rgb(0xFFFFFF),
    pill_ok: (rgba(0x3FB950, 36), rgb(0x56D364)),
    pill_info: (rgba(0x58A6FF, 36), rgb(0x79B8FF)),
    pill_err: (rgba(0xF0605D, 36), rgb(0xFF7B78)),
    pill_warn: (rgba(0xD29922, 36), rgb(0xE3B341)),
};

pub static DAYLIGHT: Palette = Palette {
    dark: false,
    bg: rgb(0xF5F6F8),
    side: rgb(0xFFFFFF),
    surface: rgb(0xFFFFFF),
    surface2: rgb(0xF5F6F8),
    border: rgb(0xE3E6EB),
    border2: rgb(0xD5D9E0),
    chip: rgb(0xF1F3F6),
    text: rgb(0x1F2430),
    text2: rgb(0x3E4757),
    muted: rgb(0x5B6474),
    track: rgb(0xEEF0F3),
    accent: rgb(0x2563EB),
    on_accent: rgb(0xFFFFFF),
    link: rgb(0x1D4ED8),
    ok: rgb(0x16A34A),
    warn: rgb(0xB45309),
    err: rgb(0xDC2626),
    info: rgb(0x2563EB),
    cpu: rgb(0x16A34A),
    mem: rgb(0x7C3AED),
    disk: rgb(0x94A3B8),
    stopped: rgb(0xB8BFCA),
    nav_on_bg: rgb(0xEEF2FF),
    nav_on_fg: rgb(0x1D4ED8),
    pill_ok: (rgb(0xDCFCE7), rgb(0x166534)),
    pill_info: (rgb(0xDBEAFE), rgb(0x1E40AF)),
    pill_err: (rgb(0xFEE2E2), rgb(0x991B1B)),
    pill_warn: (rgb(0xFEF3C7), rgb(0x92400E)),
};

static DARK: AtomicBool = AtomicBool::new(true);

/// The active palette.
pub fn p() -> &'static Palette {
    if DARK.load(Ordering::Relaxed) { &GRAPHITE } else { &DAYLIGHT }
}

pub fn is_dark() -> bool {
    DARK.load(Ordering::Relaxed)
}

pub fn ok() -> Color32 {
    p().ok
}
pub fn warn() -> Color32 {
    p().warn
}
pub fn err() -> Color32 {
    p().err
}
pub fn info() -> Color32 {
    p().info
}
pub fn muted() -> Color32 {
    p().muted
}
pub fn accent() -> Color32 {
    p().accent
}

/// Phosphor icon glyphs, remapped to U+F700.. by `tools/make_icon_font.py` so
/// they can be mixed into any label: `format!("{} Refresh", icon::REFRESH)`.
#[allow(dead_code)]
pub mod icon {
    pub const ARCHIVE: &str = "\u{f700}";
    pub const REFRESH: &str = "\u{f701}";
    pub const EXTERNAL: &str = "\u{f702}";
    pub const CAMERA: &str = "\u{f703}";
    pub const CARET_DOWN: &str = "\u{f704}";
    pub const CARET_RIGHT: &str = "\u{f705}";
    pub const CHECK: &str = "\u{f706}";
    pub const CLOCK: &str = "\u{f707}";
    pub const DOWNLOAD: &str = "\u{f708}";
    pub const SERVER: &str = "\u{f709}";
    pub const COPY: &str = "\u{f70a}";
    pub const CPU: &str = "\u{f70b}";
    pub const CUBE: &str = "\u{f70c}";
    pub const DATABASE: &str = "\u{f70d}";
    pub const DESKTOP: &str = "\u{f70e}";
    pub const DISC: &str = "\u{f70f}";
    pub const DOTS: &str = "\u{f710}";
    pub const SAVE: &str = "\u{f711}";
    pub const FUNNEL: &str = "\u{f712}";
    pub const GEAR: &str = "\u{f713}";
    pub const GLOBE: &str = "\u{f714}";
    pub const HARD_DRIVE: &str = "\u{f715}";
    pub const HARD_DRIVES: &str = "\u{f716}";
    pub const HOUSE: &str = "\u{f717}";
    pub const INFO: &str = "\u{f718}";
    pub const KEY: &str = "\u{f719}";
    pub const LIST: &str = "\u{f71a}";
    pub const TASKS: &str = "\u{f71b}";
    pub const SEARCH: &str = "\u{f71c}";
    pub const MOON: &str = "\u{f71d}";
    pub const NETWORK: &str = "\u{f71e}";
    pub const PACKAGE: &str = "\u{f71f}";
    pub const PAUSE: &str = "\u{f720}";
    pub const PLAY: &str = "\u{f721}";
    pub const PLUGS: &str = "\u{f722}";
    pub const PLUS: &str = "\u{f723}";
    pub const POWER: &str = "\u{f724}";
    pub const SHIELD: &str = "\u{f725}";
    pub const SIGN_OUT: &str = "\u{f726}";
    pub const SLIDERS: &str = "\u{f727}";
    pub const DASHBOARD: &str = "\u{f728}";
    pub const STACK: &str = "\u{f729}";
    pub const STOP: &str = "\u{f72a}";
    pub const SUN: &str = "\u{f72b}";
    pub const SWAP: &str = "\u{f72c}";
    pub const TERMINAL: &str = "\u{f72d}";
    pub const TRASH: &str = "\u{f72e}";
    pub const TREE: &str = "\u{f72f}";
    pub const USERS: &str = "\u{f730}";
    pub const WARNING: &str = "\u{f731}";
    pub const WRENCH: &str = "\u{f732}";
    pub const CLOSE: &str = "\u{f733}";
    pub const LOCK: &str = "\u{f734}";
    pub const ARROW_RIGHT: &str = "\u{f735}";
    pub const PENCIL: &str = "\u{f736}";
    pub const EYE: &str = "\u{f737}";
    pub const CHART: &str = "\u{f738}";
    pub const UPLOAD: &str = "\u{f739}";
    pub const CLOUD: &str = "\u{f73a}";
    pub const CIRCLE_NOTCH: &str = "\u{f73b}";
    pub const LIGHTNING: &str = "\u{f73c}";
    pub const ROCKET: &str = "\u{f73d}";
    pub const IMAGE: &str = "\u{f73e}";
}

/// Font family for semibold text (headings, emphasis).
pub fn semibold() -> FontFamily {
    FontFamily::Name("semibold".into())
}

/// A semibold label at the given size.
pub fn bold(text: impl Into<String>, size: f32) -> RichText {
    RichText::new(text.into()).family(semibold()).size(size).color(p().text)
}

/// Bundles Inter (UI), JetBrains Mono (code, IDs) and Phosphor (icons).
/// egui's default fonts stay as fallbacks for emoji and symbols.
pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let add = |fonts: &mut FontDefinitions, name: &str, bytes: &'static [u8]| {
        fonts.font_data.insert(name.into(), Arc::new(FontData::from_static(bytes)));
    };
    add(&mut fonts, "Inter", include_bytes!("../../assets/fonts/Inter-Regular.ttf"));
    add(&mut fonts, "Inter-SemiBold", include_bytes!("../../assets/fonts/Inter-SemiBold.ttf"));
    add(&mut fonts, "JetBrainsMono", include_bytes!("../../assets/fonts/JetBrainsMono-Regular.ttf"));
    add(&mut fonts, "Phosphor", include_bytes!("../../assets/fonts/PhosphorSubset.ttf"));

    let defaults = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let mut prop = vec!["Inter".to_string(), "Phosphor".to_string()];
    prop.extend(defaults.iter().cloned());
    fonts.families.insert(FontFamily::Proportional, prop);
    let mut strong = vec!["Inter-SemiBold".to_string(), "Phosphor".to_string()];
    strong.extend(defaults.iter().cloned());
    fonts.families.insert(semibold(), strong);
    let mono_defaults = fonts.families.get(&FontFamily::Monospace).cloned().unwrap_or_default();
    let mut mono = vec!["JetBrainsMono".to_string(), "Phosphor".to_string()];
    mono.extend(mono_defaults);
    fonts.families.insert(FontFamily::Monospace, mono);
    ctx.set_fonts(fonts);
}

pub fn apply(ctx: &egui::Context) {
    install_fonts(ctx);
    set_dark(ctx, true);
}

pub fn set_dark(ctx: &egui::Context, dark: bool) {
    DARK.store(dark, Ordering::Relaxed);
    let p = p();
    let mut v = if dark { Visuals::dark() } else { Visuals::light() };
    v.override_text_color = Some(p.text);
    v.panel_fill = p.bg;
    v.window_fill = p.surface;
    v.window_stroke = Stroke::new(1.0, p.border2);
    v.faint_bg_color = p.surface2;
    v.extreme_bg_color = p.surface2;
    v.text_edit_bg_color = Some(p.surface2);
    v.code_bg_color = p.chip;
    v.hyperlink_color = p.link;
    v.warn_fg_color = p.warn;
    v.error_fg_color = p.err;
    v.window_corner_radius = CornerRadius::same(14);
    v.menu_corner_radius = CornerRadius::same(10);
    let shadow_color = if dark { Color32::from_black_alpha(110) } else { Color32::from_black_alpha(28) };
    v.window_shadow = Shadow { offset: [0, 8], blur: 24, spread: 0, color: shadow_color };
    v.popup_shadow = Shadow { offset: [0, 4], blur: 14, spread: 0, color: shadow_color };
    // Selected labels/tabs read as a soft accent tint with accent text.
    v.selection.bg_fill = if dark { rgb(0x3A2716) } else { rgb(0xDBEAFE) };
    v.selection.stroke = Stroke::new(1.0, if dark { rgb(0xFFB877) } else { rgb(0x1D4ED8) });
    v.striped = false;
    v.indent_has_left_vline = false;

    let radius = CornerRadius::same(8);
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = p.surface;
    w.noninteractive.weak_bg_fill = p.surface;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    w.noninteractive.corner_radius = radius;
    w.inactive.bg_fill = p.surface2;
    w.inactive.weak_bg_fill = p.surface2;
    w.inactive.bg_stroke = Stroke::new(1.0, p.border2);
    w.inactive.fg_stroke = Stroke::new(1.0, p.text);
    w.inactive.corner_radius = radius;
    w.hovered.bg_fill = p.chip;
    w.hovered.weak_bg_fill = p.chip;
    w.hovered.bg_stroke = Stroke::new(1.0, if dark { rgb(0x3A4150) } else { rgb(0xB8BFCA) });
    w.hovered.fg_stroke = Stroke::new(1.5, p.text);
    w.hovered.corner_radius = radius;
    w.hovered.expansion = 0.0;
    w.active.bg_fill = p.nav_on_bg;
    w.active.weak_bg_fill = p.nav_on_bg;
    w.active.bg_stroke = Stroke::new(1.0, p.accent);
    w.active.fg_stroke = Stroke::new(1.5, p.text);
    w.active.corner_radius = radius;
    w.active.expansion = 0.0;
    w.open.bg_fill = p.surface2;
    w.open.weak_bg_fill = p.surface2;
    w.open.bg_stroke = Stroke::new(1.0, p.border2);
    w.open.fg_stroke = Stroke::new(1.0, p.text);
    w.open.corner_radius = radius;
    ctx.set_visuals(v);

    ctx.all_styles_mut(|style| {
        style.text_styles = [
            (TextStyle::Small, FontId::new(11.5, FontFamily::Proportional)),
            (TextStyle::Body, FontId::new(13.5, FontFamily::Proportional)),
            (TextStyle::Button, FontId::new(13.5, FontFamily::Proportional)),
            (TextStyle::Heading, FontId::new(20.0, semibold())),
            (TextStyle::Monospace, FontId::new(12.5, FontFamily::Monospace)),
        ]
        .into();
        let s = &mut style.spacing;
        s.item_spacing = egui::vec2(8.0, 7.0);
        s.button_padding = egui::vec2(10.0, 5.0);
        s.interact_size.y = 28.0;
        s.menu_margin = Margin::same(8);
        s.window_margin = Margin::same(16);
    });
}

/// Frame for a content card.
pub fn card_frame() -> egui::Frame {
    let p = p();
    let mut f = egui::Frame::new()
        .fill(p.surface)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(14))
        .inner_margin(Margin::symmetric(18, 16));
    if !p.dark {
        f = f.shadow(Shadow { offset: [0, 1], blur: 3, spread: 0, color: Color32::from_black_alpha(14) });
    }
    f
}

pub fn status_color(status: &str) -> Color32 {
    let p = p();
    match status {
        "running" | "online" | "available" | "OK" | "active" => p.ok,
        "paused" | "suspended" | "prelaunch" => p.warn,
        "stopped" | "" | "unknown" => p.muted,
        _ => p.err,
    }
}

pub fn usage_color(frac: f32) -> Color32 {
    let p = p();
    if frac >= 0.9 {
        p.err
    } else if frac >= 0.75 {
        p.warn
    } else {
        p.ok
    }
}
