//! The VNC console window: the remote framebuffer as a texture updated per
//! rectangle, keyboard/pointer input, Ctrl+Alt+Del, scaling and clipboard.

use super::{ConsoleSettings, Link, Phase};
use crate::api::PveClient;
use crate::console::ConsoleTarget;
use crate::console::keysym::{self, KeyMapper};
use crate::console::vnc::{self, VncInput, VncUpdate};
use crate::ui::{theme, widgets};
use egui::{Color32, ColorImage, Event, Pos2, Rect, RichText, Sense, TextureHandle, TextureOptions, Vec2};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

struct Cursor {
    tex: TextureHandle,
    hot: Vec2,
    size: Vec2,
}

pub struct VncView {
    link: Option<Link<VncUpdate, VncInput>>,
    phase: Phase,
    width: usize,
    height: usize,
    fb: Vec<Color32>,
    texture: Option<TextureHandle>,
    /// Union of the rectangles changed since the last upload: x0, y0, x1, y1.
    dirty: Option<[usize; 4]>,
    full_upload: bool,
    /// `None` = the server's cursor is drawn into the framebuffer (or hidden).
    cursor: Option<Cursor>,
    cursor_hidden: bool,
    fit: bool,
    keys: KeyMapper,
    buttons: u8,
    last_pointer: Option<(u16, u16)>,
    wheel_acc: Vec2,
    received: Arc<AtomicU64>,
    resolution_changes: u32,
    bell_until: Option<Instant>,
    clipboard_open: bool,
    clipboard_draft: String,
    remote_clipboard: Option<String>,
    focus_once: bool,
}

impl VncView {
    pub fn new(settings: &ConsoleSettings) -> Self {
        Self {
            link: None,
            phase: Phase::Connecting("Starting…".into()),
            width: 0,
            height: 0,
            fb: Vec::new(),
            texture: None,
            dirty: None,
            full_upload: true,
            cursor: None,
            cursor_hidden: false,
            fit: settings.vnc_fit,
            keys: KeyMapper::default(),
            buttons: 0,
            last_pointer: None,
            wheel_acc: Vec2::ZERO,
            received: Arc::new(AtomicU64::new(0)),
            resolution_changes: 0,
            bell_until: None,
            clipboard_open: false,
            clipboard_draft: String::new(),
            remote_clipboard: None,
            focus_once: true,
        }
    }

    pub fn connect(&mut self, client: &PveClient, target: &ConsoleTarget, ctx: &egui::Context, vid: egui::ViewportId) {
        self.shutdown();
        self.phase = Phase::Connecting("Starting…".into());
        self.keys = KeyMapper::default();
        self.buttons = 0;
        self.cursor = None;
        self.cursor_hidden = false;
        self.received = Arc::new(AtomicU64::new(0));
        let (etx, erx) = std::sync::mpsc::channel();
        let (itx, irx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = ctx.clone();
        let task = crate::rt::runtime()
            .spawn(vnc::run(client.clone(), target.clone(), etx, irx, self.received.clone(), move || ctx.request_repaint_of(vid)));
        self.link = Some(Link { events: erx, input: itx, task });
        self.focus_once = true;
    }

    pub fn shutdown(&mut self) {
        if let Some(link) = self.link.take() {
            // Let go of anything the guest thinks is still held.
            let mut out = Vec::new();
            self.keys.release_all(&mut out);
            for (k, down) in out {
                let _ = link.input.send(VncInput::Key(k, down));
            }
            link.shutdown(VncInput::Close);
        }
    }

    fn send(&self, input: VncInput) {
        if let (Some(l), Phase::Connected) = (&self.link, &self.phase) {
            let _ = l.input.send(input);
        }
    }

    fn resize(&mut self, w: u16, h: u16) {
        self.width = usize::from(w);
        self.height = usize::from(h);
        self.fb = vec![Color32::BLACK; self.width * self.height];
        self.full_upload = true;
        self.dirty = None;
    }

    fn mark(&mut self, x: usize, y: usize, w: usize, h: usize) {
        let r = [x, y, (x + w).min(self.width), (y + h).min(self.height)];
        self.dirty = Some(match self.dirty {
            None => r,
            Some(d) => [d[0].min(r[0]), d[1].min(r[1]), d[2].max(r[2]), d[3].max(r[3])],
        });
    }

    fn pump(&mut self, ctx: &egui::Context) {
        let Some(link) = &self.link else { return };
        let mut updates = Vec::new();
        while let Ok(u) = link.events.try_recv() {
            updates.push(u);
        }
        for u in updates {
            match u {
                VncUpdate::Status(s) => self.phase = Phase::Connecting(s),
                VncUpdate::Connected { width, height } => {
                    self.phase = Phase::Connected;
                    self.resize(width, height);
                }
                VncUpdate::Resize { width, height } => {
                    self.resolution_changes += 1;
                    self.resize(width, height);
                }
                VncUpdate::Image { x, y, width, height, bgra } => {
                    let (x, y, w, h) = (usize::from(x), usize::from(y), usize::from(width), usize::from(height));
                    if x + w > self.width || y + h > self.height || bgra.len() < w * h * 4 {
                        continue;
                    }
                    for row in 0..h {
                        let src = &bgra[row * w * 4..(row + 1) * w * 4];
                        let dst = &mut self.fb[(y + row) * self.width + x..(y + row) * self.width + x + w];
                        for (d, s) in dst.iter_mut().zip(src.chunks_exact(4)) {
                            *d = Color32::from_rgb(s[2], s[1], s[0]);
                        }
                    }
                    self.mark(x, y, w, h);
                }
                VncUpdate::Copy { dst, src, size } => {
                    let (w, h) = (usize::from(size.0), usize::from(size.1));
                    let (sx, sy, dx, dy) = (usize::from(src.0), usize::from(src.1), usize::from(dst.0), usize::from(dst.1));
                    if sx + w > self.width || sy + h > self.height || dx + w > self.width || dy + h > self.height {
                        continue;
                    }
                    let mut tmp = Vec::with_capacity(w * h);
                    for row in 0..h {
                        tmp.extend_from_slice(&self.fb[(sy + row) * self.width + sx..(sy + row) * self.width + sx + w]);
                    }
                    for row in 0..h {
                        self.fb[(dy + row) * self.width + dx..(dy + row) * self.width + dx + w]
                            .copy_from_slice(&tmp[row * w..(row + 1) * w]);
                    }
                    self.mark(dx, dy, w, h);
                }
                VncUpdate::Cursor { hot, size, bgra } => {
                    let (w, h) = (usize::from(size.0), usize::from(size.1));
                    if w == 0 || h == 0 || bgra.len() < w * h * 4 {
                        // An empty cursor: the guest hides the pointer.
                        self.cursor = None;
                        self.cursor_hidden = true;
                        continue;
                    }
                    let pixels: Vec<Color32> =
                        bgra.chunks_exact(4).take(w * h).map(|s| Color32::from_rgba_unmultiplied(s[2], s[1], s[0], s[3])).collect();
                    let img = ColorImage::new([w, h], pixels);
                    let tex = ctx.load_texture("vnc-cursor", img, TextureOptions::NEAREST);
                    self.cursor =
                        Some(Cursor { tex, hot: Vec2::new(f32::from(hot.0), f32::from(hot.1)), size: Vec2::new(w as f32, h as f32) });
                    self.cursor_hidden = false;
                }
                VncUpdate::Bell => {
                    self.bell_until = Some(Instant::now() + Duration::from_millis(1200));
                    if !ctx.input(|i| i.viewport().focused.unwrap_or(true)) {
                        ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(egui::UserAttentionType::Informational));
                    }
                }
                VncUpdate::ClipboardText(t) => self.remote_clipboard = Some(t),
                VncUpdate::Closed(reason) => {
                    self.phase = Phase::Ended(reason);
                    self.link = None;
                    break;
                }
            }
        }
        self.upload(ctx);
    }

    fn upload(&mut self, ctx: &egui::Context) {
        if self.width == 0 || self.height == 0 {
            return;
        }
        let opts = TextureOptions::LINEAR;
        if self.full_upload || self.texture.as_ref().map(|t| t.size() != [self.width, self.height]).unwrap_or(true) {
            let img = ColorImage::new([self.width, self.height], self.fb.clone());
            match &mut self.texture {
                Some(t) => t.set(img, opts),
                None => self.texture = Some(ctx.load_texture("vnc-framebuffer", img, opts)),
            }
            self.full_upload = false;
            self.dirty = None;
        } else if let (Some([x0, y0, x1, y1]), Some(tex)) = (self.dirty.take(), self.texture.as_mut()) {
            if x1 <= x0 || y1 <= y0 {
                return;
            }
            let w = x1 - x0;
            let mut pixels = Vec::with_capacity(w * (y1 - y0));
            for row in y0..y1 {
                pixels.extend_from_slice(&self.fb[row * self.width + x0..row * self.width + x1]);
            }
            tex.set_partial([x0, y0], ColorImage::new([w, y1 - y0], pixels), opts);
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, reconnect: &mut bool, browser: &mut bool, has_browser: bool) {
        let ctx = ui.ctx().clone();
        self.pump(&ctx);
        let p = theme::p();
        let connected = matches!(self.phase, Phase::Connected);
        egui::Panel::top("vnc-toolbar").frame(egui::Frame::new().fill(p.side).inner_margin(egui::Margin::symmetric(10, 6))).show(
            ui,
            |ui| {
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(connected, egui::Button::new("Ctrl+Alt+Del"))
                        .on_hover_text("Send Ctrl+Alt+Delete to the guest")
                        .clicked()
                    {
                        for (k, d) in keysym::ctrl_alt_del() {
                            self.send(VncInput::Key(k, d));
                        }
                    }
                    ui.separator();
                    ui.selectable_value(&mut self.fit, true, "Fit").on_hover_text("Scale the display to the window");
                    ui.selectable_value(&mut self.fit, false, "1:1").on_hover_text("One guest pixel per screen pixel (scroll if larger)");
                    ui.separator();
                    let r = ui.add_enabled(connected, egui::Button::new(format!("{}  Clipboard", theme::icon::COPY)));
                    if r.clicked() {
                        self.clipboard_open = !self.clipboard_open;
                    }
                    if ui
                        .add_enabled(connected, egui::Button::new(format!("{}  Refresh", theme::icon::REFRESH)))
                        .on_hover_text("Ask for a full screen update")
                        .clicked()
                    {
                        self.send(VncInput::FullRefresh);
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if has_browser
                            && ui
                                .button(format!("{}  Open in browser", theme::icon::EXTERNAL))
                                .on_hover_text("noVNC in the Proxmox web UI")
                                .clicked()
                        {
                            *browser = true;
                        }
                        if ui.button(format!("{}  Reconnect", theme::icon::PLUGS)).clicked() {
                            *reconnect = true;
                        }
                    });
                });
                if self.clipboard_open {
                    self.clipboard_panel(ui);
                }
            },
        );
        egui::Panel::bottom("vnc-status").frame(egui::Frame::new().fill(p.side).inner_margin(egui::Margin::symmetric(10, 4))).show(
            ui,
            |ui| {
                self.status_line(ui);
            },
        );
        egui::CentralPanel::default().frame(egui::Frame::new().fill(Color32::from_rgb(0x0A, 0x0B, 0x0E))).show(ui, |ui| {
            match &self.phase {
                Phase::Ended(Some(err)) => {
                    let err = err.clone();
                    ui.add_space(12.0);
                    egui::Frame::new().inner_margin(12).show(ui, |ui| {
                        widgets::error_box(ui, &err);
                        ui.horizontal(|ui| {
                            if widgets::primary_button(ui, format!("{}  Reconnect", theme::icon::REFRESH)).clicked() {
                                *reconnect = true;
                            }
                            if has_browser && ui.button(format!("{}  Open in browser instead", theme::icon::EXTERNAL)).clicked() {
                                *browser = true;
                            }
                        });
                    });
                }
                Phase::Ended(None) if self.texture.is_none() => {
                    ui.centered_and_justified(|ui| ui.label(RichText::new("The console session ended.").color(theme::muted())));
                }
                Phase::Connecting(s) if self.texture.is_none() => {
                    let s = s.clone();
                    ui.centered_and_justified(|ui| {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(RichText::new(s).color(Color32::from_gray(200)));
                        });
                    });
                }
                _ => {}
            }
            if self.texture.is_some() {
                if self.fit {
                    let avail = ui.available_rect_before_wrap();
                    self.screen(ui, avail);
                } else {
                    egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
                        let ppp = ui.ctx().pixels_per_point();
                        let size = Vec2::new(self.width as f32, self.height as f32) / ppp;
                        let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
                        self.screen(ui, rect);
                    });
                }
            }
        });
        if let Some(t) = self.bell_until {
            if Instant::now() < t {
                ctx.request_repaint_after(Duration::from_millis(200));
            } else {
                self.bell_until = None;
            }
        }
    }

    fn clipboard_panel(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::multiline(&mut self.clipboard_draft).desired_rows(2).desired_width(420.0).hint_text("Text for the guest…"),
            );
            ui.vertical(|ui| {
                if ui
                    .button("Type it")
                    .on_hover_text("Types the text as key presses (works with any guest; line breaks press Enter)")
                    .clicked()
                {
                    for c in self.clipboard_draft.replace("\r\n", "\n").chars() {
                        let ks = keysym::char_keysym(c);
                        self.send(VncInput::Key(ks, true));
                        self.send(VncInput::Key(ks, false));
                    }
                }
                let (latin1, lossy) = vnc::latin1_clipboard(&self.clipboard_draft);
                if ui
                    .button("Send as clipboard")
                    .on_hover_text(if lossy {
                        "RFB clipboards carry Latin-1 only; other characters become '?'. Needs a guest clipboard agent (QEMU vdagent)."
                    } else {
                        "Sets the guest clipboard (RFB ClientCutText). Needs a guest clipboard agent (QEMU vdagent)."
                    })
                    .clicked()
                {
                    self.send(VncInput::Clipboard(latin1));
                }
            });
        });
        if let Some(remote) = self.remote_clipboard.clone() {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Guest clipboard:").color(theme::muted()));
                let preview: String = remote.chars().take(80).collect();
                ui.label(RichText::new(preview).monospace());
                if ui.small_button("Copy").clicked() {
                    ui.ctx().copy_text(remote);
                }
            });
        }
    }

    fn status_line(&self, ui: &mut egui::Ui) {
        let p = theme::p();
        ui.horizontal(|ui| {
            let (dot, text) = match &self.phase {
                Phase::Connecting(s) => (p.warn, s.clone()),
                Phase::Connected => (p.ok, "Connected".to_string()),
                Phase::Ended(None) => (p.muted, "Session ended".to_string()),
                Phase::Ended(Some(_)) => (p.err, "Disconnected (error)".to_string()),
            };
            widgets::dot(ui, dot);
            ui.label(RichText::new(text).size(12.0));
            let mut facts = Vec::new();
            if self.width > 0 {
                facts.push(format!("{}×{}", self.width, self.height));
            }
            facts.push(format!("encodings {}", vnc::ENCODINGS_LABEL));
            facts.push(format!("{} received", crate::util::fmt_bytes(self.received.load(Ordering::Relaxed) as f64)));
            if self.resolution_changes > 0 {
                facts.push(format!("{} resolution change(s)", self.resolution_changes));
            }
            ui.label(RichText::new(facts.join("  ·  ")).size(12.0).color(p.muted));
            if self.bell_until.is_some() {
                widgets::pill(ui, "Bell", widgets::Tone::Warn);
            }
        });
        if matches!(self.phase, Phase::Connected) {
            // The byte counter is sampled; keep the line fresh while connected.
            ui.ctx().request_repaint_after(Duration::from_millis(500));
        }
    }

    /// Paints the framebuffer into `area` (scaled to fit, or at its own size)
    /// and handles input over it.
    fn screen(&mut self, ui: &mut egui::Ui, area: Rect) {
        let Some(tex) = &self.texture else { return };
        let (fw, fh) = (self.width as f32, self.height as f32);
        let img_rect = if self.fit {
            let scale = (area.width() / fw).min(area.height() / fh).max(0.01);
            Rect::from_center_size(area.center(), Vec2::new(fw * scale, fh * scale))
        } else {
            area
        };
        let resp = ui.interact(img_rect, ui.scope_id().with("vnc-screen"), Sense::click_and_drag());
        ui.painter().image(tex.id(), img_rect, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);

        if self.focus_once || resp.clicked() || resp.drag_started() || resp.secondary_clicked() || resp.middle_clicked() {
            resp.request_focus();
            self.focus_once = false;
        }
        let focused = resp.has_focus();
        if focused {
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    resp.id,
                    egui::EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: true },
                )
            });
        }
        let to_fb = |pos: Pos2| -> (u16, u16) {
            let x = ((pos.x - img_rect.left()) / img_rect.width() * fw).clamp(0.0, fw - 1.0);
            let y = ((pos.y - img_rect.top()) / img_rect.height() * fh).clamp(0.0, fh - 1.0);
            (x as u16, y as u16)
        };
        let hovered = resp.hovered() || resp.dragged();
        if hovered && matches!(self.phase, Phase::Connected) && (self.cursor.is_some() || self.cursor_hidden) {
            ui.ctx().set_cursor_icon(egui::CursorIcon::None);
        }

        let events = ui.input(|i| i.events.clone());
        let connected = matches!(self.phase, Phase::Connected);
        let mut keys = Vec::new();
        for ev in &events {
            match ev {
                Event::PointerMoved(pos) if connected && (hovered || self.buttons != 0) => {
                    let p = to_fb(*pos);
                    if self.last_pointer != Some(p) {
                        self.last_pointer = Some(p);
                        self.send(VncInput::Pointer(p.0, p.1, self.buttons));
                    }
                }
                Event::PointerButton { pos, button, pressed, .. } if connected && (img_rect.contains(*pos) || self.buttons != 0) => {
                    let bit = match button {
                        egui::PointerButton::Primary => 1,
                        egui::PointerButton::Middle => 2,
                        egui::PointerButton::Secondary => 4,
                        _ => 0,
                    };
                    if *pressed {
                        self.buttons |= bit;
                    } else {
                        self.buttons &= !bit;
                    }
                    let p = to_fb(*pos);
                    self.last_pointer = Some(p);
                    self.send(VncInput::Pointer(p.0, p.1, self.buttons));
                }
                Event::MouseWheel { delta, unit, .. } if connected && resp.hovered() => {
                    let notches = match unit {
                        egui::MouseWheelUnit::Point => *delta / 40.0,
                        egui::MouseWheelUnit::Line => *delta,
                        egui::MouseWheelUnit::Page => *delta * 3.0,
                    };
                    self.wheel_acc += notches;
                    let (x, y) = self.last_pointer.unwrap_or((0, 0));
                    let vy = self.wheel_acc.y.trunc() as i32;
                    let vx = self.wheel_acc.x.trunc() as i32;
                    self.wheel_acc.y -= vy as f32;
                    self.wheel_acc.x -= vx as f32;
                    // Content moving down = wheel up = button 4 (mask 8); 5 = 16; 6/7 horizontal.
                    for (bit, n) in [(if vy > 0 { 8u8 } else { 16 }, vy), (if vx > 0 { 32 } else { 64 }, vx)] {
                        for _ in 0..n.unsigned_abs().min(10) {
                            self.send(VncInput::Pointer(x, y, self.buttons | bit));
                            self.send(VncInput::Pointer(x, y, self.buttons));
                        }
                    }
                }
                ev if connected && focused => self.keys.event(ev, &mut keys),
                Event::WindowFocused(false) => self.keys.release_all(&mut keys),
                _ => {}
            }
        }
        if connected && focused {
            let mods = ui.input(|i| i.modifiers);
            self.keys.sync_modifiers(mods, &mut keys);
        } else if !focused {
            self.keys.release_all(&mut keys);
        }
        for (k, d) in keys {
            self.send(VncInput::Key(k, d));
        }

        // Local cursor (cursor pseudo-encoding).
        if hovered && let (Some(c), Some(pos)) = (&self.cursor, ui.input(|i| i.pointer.hover_pos())) {
            let scale = img_rect.width() / fw;
            let r = Rect::from_min_size(pos - c.hot * scale, c.size * scale);
            ui.painter().image(c.tex.id(), r, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
        }
        if !focused && connected {
            ui.painter().text(
                img_rect.left_top() + Vec2::new(8.0, 8.0),
                egui::Align2::LEFT_TOP,
                "Click to type into the console",
                egui::FontId::proportional(12.0),
                Color32::from_white_alpha(170),
            );
        }
    }
}

impl Drop for VncView {
    fn drop(&mut self) {
        self.shutdown();
    }
}
