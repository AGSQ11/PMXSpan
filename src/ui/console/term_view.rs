//! The terminal console window: a `vt100` screen painted with the bundled
//! JetBrains Mono, keyboard and mouse input, selection, clipboard and
//! scrollback.

use super::{ConsoleSettings, Link, Phase};
use crate::api::PveClient;
use crate::console::ConsoleTarget;
use crate::console::screen::{self, TermColors};
use crate::console::term::{self, TermEvent, TermInput};
use crate::console::termkeys;
use crate::ui::{theme, widgets};
use egui::{Color32, Event, FontId, Key, Pos2, Rect, RichText, Sense, Stroke, Vec2};
use std::num::NonZeroU16;
use std::time::{Duration, Instant};

/// Collects what the parser reports besides screen changes.
#[derive(Default)]
pub struct Callbacks {
    bells: u64,
    title: Option<String>,
}

impl vt100::Callbacks for Callbacks {
    fn audible_bell(&mut self, _: &mut vt100::Screen) {
        self.bells += 1;
    }
    fn visual_bell(&mut self, _: &mut vt100::Screen) {
        self.bells += 1;
    }
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.title = Some(String::from_utf8_lossy(title).chars().take(120).collect());
    }
}

const DEFAULT_COLS: u16 = 100;
const DEFAULT_ROWS: u16 = 32;

fn nz(v: u16) -> NonZeroU16 {
    NonZeroU16::new(v.max(1)).unwrap_or(NonZeroU16::MIN)
}

pub struct TermView {
    parser: vt100::Parser<Callbacks>,
    link: Option<Link<TermEvent, TermInput>>,
    phase: Phase,
    /// (cols, rows) of the grid.
    size: (u16, u16),
    scrollback_lines: usize,
    received: u64,
    seen_bells: u64,
    bell_until: Option<Instant>,
    /// Selection anchor and end, in visible (row, col) cells.
    selection: Option<((u16, u16), (u16, u16))>,
    pending_paste: Option<String>,
    /// A paste was requested from the clipboard (Ctrl+Shift+V / right click).
    expect_paste: bool,
    wheel_acc: f32,
    focus_once: bool,
}

impl TermView {
    pub fn new(settings: &ConsoleSettings) -> Self {
        Self {
            parser: vt100::Parser::new_with_callbacks(nz(DEFAULT_ROWS), nz(DEFAULT_COLS), settings.scrollback, Callbacks::default()),
            link: None,
            phase: Phase::Connecting("Starting…".into()),
            size: (DEFAULT_COLS, DEFAULT_ROWS),
            scrollback_lines: settings.scrollback,
            received: 0,
            seen_bells: 0,
            bell_until: None,
            selection: None,
            pending_paste: None,
            expect_paste: false,
            wheel_acc: 0.0,
            focus_once: true,
        }
    }

    pub fn connect(&mut self, client: &PveClient, target: &ConsoleTarget, ctx: &egui::Context, vid: egui::ViewportId) {
        self.shutdown();
        // A fresh screen for a fresh session.
        self.parser = vt100::Parser::new_with_callbacks(nz(self.size.1), nz(self.size.0), self.scrollback_lines, Callbacks::default());
        self.seen_bells = 0;
        self.selection = None;
        self.received = 0;
        self.phase = Phase::Connecting("Starting…".into());
        let (etx, erx) = std::sync::mpsc::channel();
        let (itx, irx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = ctx.clone();
        let task =
            crate::rt::runtime().spawn(term::run(client.clone(), target.clone(), self.size, etx, irx, move || ctx.request_repaint_of(vid)));
        self.link = Some(Link { events: erx, input: itx, task });
        self.focus_once = true;
    }

    pub fn shutdown(&mut self) {
        if let Some(link) = self.link.take() {
            link.shutdown(TermInput::Close);
        }
    }

    fn send(&mut self, data: Vec<u8>) {
        if !matches!(self.phase, Phase::Connected) || data.is_empty() {
            return;
        }
        if let Some(l) = &self.link {
            let _ = l.input.send(TermInput::Data(data));
        }
        // Typing returns to the live screen.
        if self.parser.screen().scrollback() > 0 {
            self.parser.screen_mut().set_scrollback(0);
        }
        self.selection = None;
    }

    fn pump(&mut self) {
        let Some(link) = &self.link else { return };
        let mut ended = None;
        while let Ok(ev) = link.events.try_recv() {
            match ev {
                TermEvent::Status(s) => self.phase = Phase::Connecting(s),
                TermEvent::Connected => self.phase = Phase::Connected,
                TermEvent::Output(bytes) => {
                    self.received += bytes.len() as u64;
                    self.parser.process(&bytes);
                }
                TermEvent::Closed(reason) => ended = Some(reason),
            }
        }
        if let Some(reason) = ended {
            self.phase = Phase::Ended(reason);
            self.link = None;
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, settings: &ConsoleSettings, reconnect: &mut bool, browser: &mut bool, has_browser: bool) {
        self.pump();
        if settings.scrollback != self.scrollback_lines {
            // Applies to the next session (vt100 fixes it at creation).
            self.scrollback_lines = settings.scrollback;
        }
        let bells = self.parser.callbacks().bells;
        if bells > self.seen_bells {
            self.seen_bells = bells;
            self.bell_until = Some(Instant::now() + Duration::from_millis(1200));
            if !ui.ctx().input(|i| i.viewport().focused.unwrap_or(true)) {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(egui::UserAttentionType::Informational));
            }
        }

        let p = theme::p();
        egui::Panel::top("term-toolbar").frame(egui::Frame::new().fill(p.side).inner_margin(egui::Margin::symmetric(10, 6))).show(
            ui,
            |ui| {
                ui.horizontal(|ui| {
                    let connected = matches!(self.phase, Phase::Connected);
                    if ui
                        .add_enabled(self.selection.is_some(), egui::Button::new(format!("{}  Copy", theme::icon::COPY)))
                        .on_hover_text("Copy the selection (Ctrl+Shift+C)")
                        .clicked()
                    {
                        self.copy_selection(ui.ctx());
                    }
                    if ui
                        .add_enabled(connected, egui::Button::new("Paste"))
                        .on_hover_text("Paste the clipboard (Ctrl+Shift+V, right click)")
                        .clicked()
                    {
                        self.request_paste(ui.ctx());
                    }
                    if ui.add_enabled(connected, egui::Button::new("Ctrl+C")).on_hover_text("Send an interrupt (0x03)").clicked() {
                        self.send(vec![0x03]);
                    }
                    if ui.add_enabled(connected, egui::Button::new("Ctrl+D")).on_hover_text("Send end-of-file (0x04)").clicked() {
                        self.send(vec![0x04]);
                    }
                    if ui.add_enabled(connected, egui::Button::new("Esc")).clicked() {
                        self.send(vec![0x1b]);
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if has_browser
                            && ui
                                .button(format!("{}  Open in browser", theme::icon::EXTERNAL))
                                .on_hover_text("The Proxmox web console")
                                .clicked()
                        {
                            *browser = true;
                        }
                        if ui.button(format!("{}  Reconnect", theme::icon::REFRESH)).clicked() {
                            *reconnect = true;
                        }
                    });
                });
            },
        );
        egui::Panel::bottom("term-status")
            .frame(egui::Frame::new().fill(p.side).inner_margin(egui::Margin::symmetric(10, 4)))
            .show(ui, |ui| self.status_line(ui));
        let colors = if theme::is_dark() { TermColors::dark() } else { TermColors::light() };
        egui::CentralPanel::default().frame(egui::Frame::new().fill(colors.bg).inner_margin(egui::Margin::same(4))).show(ui, |ui| {
            if let Phase::Ended(Some(err)) = &self.phase {
                let err = err.clone();
                widgets::error_box(ui, &err);
                ui.horizontal(|ui| {
                    if widgets::primary_button(ui, format!("{}  Reconnect", theme::icon::REFRESH)).clicked() {
                        *reconnect = true;
                    }
                    if has_browser && ui.button(format!("{}  Open in browser instead", theme::icon::EXTERNAL)).clicked() {
                        *browser = true;
                    }
                });
                ui.add_space(6.0);
            }
            self.paste_prompt(ui);
            self.canvas(ui, settings, &colors);
        });
        if let Some(t) = self.bell_until {
            if Instant::now() < t {
                ui.ctx().request_repaint_after(Duration::from_millis(200));
            } else {
                self.bell_until = None;
            }
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
            let s = self.parser.screen();
            let mut facts =
                vec![format!("{}×{}", self.size.0, self.size.1), format!("{} received", crate::util::fmt_bytes(self.received as f64))];
            if s.application_cursor() {
                facts.push("application cursor".into());
            }
            if s.alternate_screen() {
                facts.push("full-screen app".into());
            }
            if s.scrollback() > 0 {
                facts.push(format!("scrolled back {} lines", s.scrollback()));
            }
            if let Some(t) = &self.parser.callbacks().title
                && !t.is_empty()
            {
                facts.push(t.clone());
            }
            ui.label(RichText::new(facts.join("  ·  ")).size(12.0).color(p.muted));
            if self.bell_until.is_some() {
                widgets::pill(ui, "Bell", widgets::Tone::Warn);
            }
        });
    }

    fn paste_prompt(&mut self, ui: &mut egui::Ui) {
        let Some(text) = self.pending_paste.clone() else { return };
        let lines = text.lines().count();
        let mut decided = None;
        egui::Frame::new().fill(theme::p().surface2).stroke(Stroke::new(1.0, theme::warn())).inner_margin(8).corner_radius(6).show(
            ui,
            |ui| {
                ui.label(RichText::new(format!("Paste {lines} lines? Each line break runs like Enter.")).color(theme::warn()));
                let preview: String = text.lines().take(4).collect::<Vec<_>>().join("\n");
                ui.label(RichText::new(preview).monospace().size(11.5).color(theme::muted()));
                ui.horizontal(|ui| {
                    if widgets::primary_button(ui, "Paste").clicked() {
                        decided = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        decided = Some(false);
                    }
                });
            },
        );
        ui.add_space(4.0);
        if let Some(ok) = decided {
            self.pending_paste = None;
            if ok {
                self.paste_now(&text);
            }
        }
    }

    fn request_paste(&mut self, ctx: &egui::Context) {
        self.expect_paste = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::RequestPaste);
    }

    fn paste(&mut self, text: &str, settings: &ConsoleSettings) {
        if text.is_empty() {
            return;
        }
        if settings.confirm_paste && text.trim_end_matches(['\n', '\r']).contains(['\n', '\r']) {
            self.pending_paste = Some(text.to_string());
        } else {
            self.paste_now(text);
        }
    }

    fn paste_now(&mut self, text: &str) {
        let bracketed = self.parser.screen().bracketed_paste();
        self.send(termkeys::paste_bytes(text, bracketed));
    }

    fn copy_selection(&mut self, ctx: &egui::Context) {
        if let Some((a, b)) = self.selection {
            let text = screen::selection_text(self.parser.screen(), a, b);
            if !text.is_empty() {
                ctx.copy_text(text);
            }
        }
    }

    fn canvas(&mut self, ui: &mut egui::Ui, settings: &ConsoleSettings, colors: &TermColors) {
        let font = FontId::monospace(settings.font_size.clamp(6.0, 40.0));
        let (cw, ch) = ui.ctx().fonts_mut(|f| (f.glyph_width(&font, 'M'), f.row_height(&font)));
        let (cw, ch) = (cw.max(1.0), ch.max(1.0));
        let avail = ui.available_size();
        let cols = ((avail.x / cw).floor() as u16).clamp(10, 500);
        let rows = ((avail.y / ch).floor() as u16).clamp(3, 300);
        if (cols, rows) != self.size {
            self.size = (cols, rows);
            self.parser.screen_mut().set_size(nz(rows), nz(cols));
            self.selection = None;
            if let Some(l) = &self.link {
                let _ = l.input.send(TermInput::Resize { cols, rows });
            }
        }
        let (rect, resp) = ui.allocate_exact_size(avail, Sense::click_and_drag());
        if self.focus_once || resp.clicked() || resp.drag_started() {
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
        let origin = rect.min;
        let cell_at = |pos: Pos2| -> (u16, u16) {
            let c = ((pos.x - origin.x) / cw).floor().clamp(0.0, f32::from(cols - 1)) as u16;
            let r = ((pos.y - origin.y) / ch).floor().clamp(0.0, f32::from(rows - 1)) as u16;
            (r, c)
        };

        // Mouse: selection, right-click paste, wheel scrollback.
        if resp.drag_started_by(egui::PointerButton::Primary)
            && let Some(pos) = resp.interact_pointer_pos()
        {
            let c = cell_at(pos);
            self.selection = Some((c, c));
        } else if resp.dragged_by(egui::PointerButton::Primary)
            && let (Some(pos), Some((a, _))) = (resp.interact_pointer_pos(), self.selection)
        {
            self.selection = Some((a, cell_at(pos)));
        } else if resp.clicked() {
            self.selection = None;
        }
        if resp.secondary_clicked() && matches!(self.phase, Phase::Connected) {
            self.request_paste(ui.ctx());
        }
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Text);
        }

        // Keyboard and other input.
        let events = if focused || resp.hovered() { ui.input(|i| i.events.clone()) } else { Vec::new() };
        let mods_now = ui.input(|i| i.modifiers);
        let app_cursor = self.parser.screen().application_cursor();
        let mac = cfg!(target_os = "macos");
        for ev in events {
            match ev {
                Event::MouseWheel { delta, unit, .. } if resp.hovered() => {
                    let lines = match unit {
                        egui::MouseWheelUnit::Point => delta.y / ch,
                        egui::MouseWheelUnit::Line => delta.y,
                        egui::MouseWheelUnit::Page => delta.y * f32::from(rows),
                    };
                    self.wheel_acc += lines;
                    let steps = self.wheel_acc.trunc() as i32;
                    if steps != 0 {
                        self.wheel_acc -= steps as f32;
                        if self.parser.screen().alternate_screen() {
                            // Full-screen apps (less, vim) get arrow keys, like xterm's alternateScroll.
                            let key = if steps > 0 { Key::ArrowUp } else { Key::ArrowDown };
                            let seq = termkeys::key_bytes(key, egui::Modifiers::NONE, app_cursor).unwrap_or_default();
                            self.send(seq.repeat(steps.unsigned_abs().min(20) as usize));
                        } else {
                            let cur = self.parser.screen().scrollback() as i64;
                            let next = (cur + i64::from(steps) * 3).max(0) as usize;
                            self.parser.screen_mut().set_scrollback(next);
                        }
                    }
                }
                _ if !focused => {}
                Event::Key { key, pressed: true, modifiers, .. } => {
                    let ctrl_shift = modifiers.ctrl && modifiers.shift && !modifiers.alt;
                    if ctrl_shift && key == Key::C {
                        self.copy_selection(ui.ctx());
                    } else if ctrl_shift && key == Key::V {
                        self.request_paste(ui.ctx());
                    } else if modifiers.shift && !modifiers.ctrl && key == Key::PageUp {
                        let s = self.parser.screen().scrollback() + usize::from(rows.saturating_sub(1));
                        self.parser.screen_mut().set_scrollback(s);
                    } else if modifiers.shift && !modifiers.ctrl && key == Key::PageDown {
                        let s = self.parser.screen().scrollback().saturating_sub(usize::from(rows.saturating_sub(1)));
                        self.parser.screen_mut().set_scrollback(s);
                    } else if let Some(bytes) = termkeys::key_bytes(key, modifiers, app_cursor) {
                        self.send(bytes);
                    }
                }
                Event::Text(t) => {
                    let alt = mods_now.alt && !mac;
                    self.send(termkeys::text_bytes(&t, alt));
                }
                // egui turns Ctrl+C / Ctrl+X / Ctrl+V into clipboard events; in a
                // terminal (except Cmd on macOS) they are control characters.
                Event::Copy => {
                    if mac {
                        self.copy_selection(ui.ctx());
                    } else {
                        self.send(vec![0x03]);
                    }
                }
                Event::Cut => {
                    if !mac {
                        self.send(vec![0x18]);
                    }
                }
                Event::Paste(text) => {
                    let ctrl_v = !mac && mods_now.ctrl && !mods_now.shift && !self.expect_paste;
                    self.expect_paste = false;
                    if ctrl_v {
                        self.send(vec![0x16]);
                    } else {
                        self.paste(&text, settings);
                    }
                }
                _ => {}
            }
        }

        // Paint.
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, colors.bg);
        let cell_rect = |r: u16, c: u16, w: u16| {
            Rect::from_min_size(Pos2::new(origin.x + f32::from(c) * cw, origin.y + f32::from(r) * ch), Vec2::new(f32::from(w) * cw, ch))
        };
        let screen = self.parser.screen();
        for (r, runs) in screen::rows(screen, colors).iter().enumerate() {
            let r = r as u16;
            for run in runs {
                if run.style.bg != colors.bg {
                    painter.rect_filled(cell_rect(r, run.col, run.width), 0.0, run.style.bg);
                }
                if run.text.trim().is_empty() && !run.style.underline {
                    continue;
                }
                let mut job = egui::text::LayoutJob::default();
                job.append(
                    &run.text,
                    0.0,
                    egui::TextFormat {
                        font_id: font.clone(),
                        color: run.style.fg,
                        italics: run.style.italic,
                        underline: if run.style.underline { Stroke::new(1.0, run.style.fg) } else { Stroke::NONE },
                        ..Default::default()
                    },
                );
                let galley = painter.layout_job(job);
                let pos = cell_rect(r, run.col, run.width).min;
                if run.style.bold {
                    // No bold face is bundled: overstrike.
                    painter.galley(pos + Vec2::new(0.6, 0.0), galley.clone(), run.style.fg);
                }
                painter.galley(pos, galley, run.style.fg);
            }
        }
        if let Some((a, b)) = self.selection {
            let (s, e) = if a <= b { (a, b) } else { (b, a) };
            let fill = Color32::from_rgba_unmultiplied(0x58, 0xA6, 0xFF, 70);
            for r in s.0..=e.0 {
                let c0 = if r == s.0 { s.1 } else { 0 };
                let c1 = if r == e.0 { e.1 } else { cols - 1 };
                painter.rect_filled(cell_rect(r, c0, c1 - c0 + 1), 0.0, fill);
            }
        }
        if screen.scrollback() == 0 && !screen.hide_cursor() && matches!(self.phase, Phase::Connected) {
            let (cr, cc) = screen.cursor_position();
            let wide = screen.cell(cr, cc).map(|c| c.is_wide()).unwrap_or(false);
            let rc = cell_rect(cr, cc, if wide { 2 } else { 1 });
            if focused {
                painter.rect_filled(rc, 1.0, colors.fg.gamma_multiply(0.75));
                if let Some(cell) = screen.cell(cr, cc)
                    && cell.has_contents()
                {
                    painter.text(rc.min, egui::Align2::LEFT_TOP, cell.contents(), font.clone(), colors.bg);
                }
            } else {
                painter.rect_stroke(rc, 1.0, Stroke::new(1.0, colors.fg), egui::StrokeKind::Inside);
            }
        }
        // Scrollback position indicator.
        let total = screen.scrollback_len();
        if screen.scrollback() > 0 && total > 0 {
            let h = rect.height();
            let thumb = (h * f32::from(rows) / (total as f32 + f32::from(rows))).max(16.0);
            let frac = 1.0 - screen.scrollback() as f32 / total as f32;
            let y = rect.top() + (h - thumb) * frac;
            painter.rect_filled(Rect::from_min_size(Pos2::new(rect.right() - 5.0, y), Vec2::new(4.0, thumb)), 2.0, theme::muted());
        }
    }
}

impl Drop for TermView {
    fn drop(&mut self) {
        self.shutdown();
    }
}
