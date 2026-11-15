//! Console windows: each console runs in its own OS window (an egui deferred
//! viewport) with its state behind `Arc<Mutex<..>>`. The session socket is
//! held by a tokio task on PMXSpan's runtime; updates reach the window
//! through a channel and a repaint request for that viewport.
//!
//! Views open a console with [`open`]; the app shell calls [`show_windows`]
//! once per frame and [`close_all`] when it locks or disconnects.

mod term_view;
mod vnc_view;

use super::Shared;
use crate::api::PveClient;
use crate::api::types::{GuestKind, GuestRef, Resource};
use crate::config::Settings;
use crate::console::{ConsoleTarget, TOKEN_TERMINAL_NOTE, has_display, primary_target, serial_ports};
use crate::ui::{theme, widgets};
use egui::{RichText, ViewportBuilder, ViewportId};
use parking_lot::Mutex;
use serde_json::{Map, Value};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// Connection state shown by both views.
pub(crate) enum Phase {
    Connecting(String),
    Connected,
    /// Ended; `Some` with the error when it failed.
    Ended(Option<String>),
}

/// The channels and task of one running session.
pub(crate) struct Link<E, I> {
    pub events: std::sync::mpsc::Receiver<E>,
    pub input: tokio::sync::mpsc::UnboundedSender<I>,
    pub task: tokio::task::JoinHandle<()>,
}

impl<E, I: Send + 'static> Link<E, I> {
    /// Asks the session to close (it sends a WebSocket close frame) and
    /// aborts it if it has not ended two seconds later.
    pub fn shutdown(self, close: I) {
        let _ = self.input.send(close);
        let abort = self.task.abort_handle();
        crate::rt::runtime().spawn(async move {
            tokio::time::sleep(Duration::from_secs(2)).await;
            abort.abort();
        });
    }
}

/// The console preferences, copied from [`Settings`] every frame.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ConsoleSettings {
    pub font_size: f32,
    pub scrollback: usize,
    pub vnc_fit: bool,
    pub confirm_paste: bool,
}

impl From<&Settings> for ConsoleSettings {
    fn from(s: &Settings) -> Self {
        Self {
            font_size: s.console_font_size,
            scrollback: s.console_scrollback,
            vnc_fit: s.console_vnc_fit,
            confirm_paste: s.console_confirm_paste,
        }
    }
}

/// A request to open a console window.
pub struct OpenRequest {
    pub client: PveClient,
    pub target: ConsoleTarget,
    /// Window title, e.g. "web01 (100) — Console".
    pub title: String,
    /// The Proxmox web console for the "Open in browser" fallback.
    pub browser_url: Option<String>,
}

impl OpenRequest {
    /// Builds the request with the standard title.
    pub fn new(client: PveClient, target: ConsoleTarget, name: &str, browser_url: Option<String>) -> Self {
        let title = match &target {
            ConsoleTarget::VmDisplay(g) | ConsoleTarget::VmSerial(g, _) | ConsoleTarget::Container(g) => {
                format!("{name} ({}) — {}", g.vmid, target.kind_label())
            }
            ConsoleTarget::NodeShell(node) => format!("{node} — Shell"),
        };
        Self { client, target, title, browser_url }
    }
}

enum View {
    Vnc(Box<vnc_view::VncView>),
    Term(Box<term_view::TermView>),
}

struct ConsoleState {
    client: PveClient,
    target: ConsoleTarget,
    browser_url: Option<String>,
    settings: ConsoleSettings,
    view: View,
    close: bool,
    started: bool,
}

impl ConsoleState {
    fn ui(&mut self, ui: &mut egui::Ui, vid: ViewportId) {
        if !self.started {
            self.started = true;
            self.connect(ui.ctx(), vid);
        }
        let mut reconnect = false;
        let mut browser = false;
        let has_browser = self.browser_url.is_some();
        match &mut self.view {
            View::Vnc(v) => v.ui(ui, &mut reconnect, &mut browser, has_browser),
            View::Term(t) => t.ui(ui, &self.settings, &mut reconnect, &mut browser, has_browser),
        }
        if reconnect {
            self.connect(ui.ctx(), vid);
        }
        if browser && let Some(url) = &self.browser_url {
            let _ = webbrowser::open(url);
        }
    }

    fn connect(&mut self, ctx: &egui::Context, vid: ViewportId) {
        match &mut self.view {
            View::Vnc(v) => v.connect(&self.client, &self.target, ctx, vid),
            View::Term(t) => t.connect(&self.client, &self.target, ctx, vid),
        }
    }

    fn shutdown(&mut self) {
        match &mut self.view {
            View::Vnc(v) => v.shutdown(),
            View::Term(t) => t.shutdown(),
        }
    }
}

struct Window {
    vid: ViewportId,
    title: String,
    target: ConsoleTarget,
    server: String,
    terminal: bool,
    state: Arc<Mutex<ConsoleState>>,
}

struct Consoles {
    queue: Vec<OpenRequest>,
    windows: Vec<Window>,
    /// A terminal console asked for on an API-token profile, waiting for the
    /// user to read the explanation.
    token_prompt: Option<OpenRequest>,
    serial: u64,
}

static CONSOLES: Mutex<Consoles> = Mutex::new(Consoles { queue: Vec::new(), windows: Vec::new(), token_prompt: None, serial: 0 });

/// Opens a console window (shown from the next frame).
pub fn open(req: OpenRequest) {
    CONSOLES.lock().queue.push(req);
    crate::rt::repaint();
}

/// Closes every console window and ends its session.
pub fn close_all() {
    let windows = std::mem::take(&mut CONSOLES.lock().windows);
    for w in windows {
        w.state.lock().shutdown();
    }
}

static ACTIVITY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// True once after the user typed or clicked in a console window (input
/// there counts as activity for the vault's auto-lock).
pub fn take_activity() -> bool {
    ACTIVITY.swap(false, std::sync::atomic::Ordering::Relaxed)
}

fn icon() -> Arc<egui::IconData> {
    static ICON: OnceLock<Arc<egui::IconData>> = OnceLock::new();
    ICON.get_or_init(|| Arc::new(crate::ui::icon::app_icon())).clone()
}

/// Shows the console windows; call once per frame of the main window.
pub fn show_windows(ctx: &egui::Context, settings: &Settings) {
    let cs = ConsoleSettings::from(settings);
    // Take what is needed and release the lock: a viewport may run its
    // callback synchronously (embedded mode).
    let requests = {
        let mut c = CONSOLES.lock();
        // Windows the user closed: end their sessions.
        let (closed, open): (Vec<Window>, Vec<Window>) = std::mem::take(&mut c.windows).into_iter().partition(|w| w.state.lock().close);
        c.windows = open;
        for w in closed {
            w.state.lock().shutdown();
        }
        std::mem::take(&mut c.queue)
    };
    for req in requests {
        if req.target.is_terminal() && req.client.uses_token() {
            CONSOLES.lock().token_prompt = Some(req);
        } else {
            add_window(ctx, req, &cs);
        }
    }
    token_prompt(ctx, &cs);

    let list: Vec<(ViewportId, String, Arc<Mutex<ConsoleState>>, bool)> =
        CONSOLES.lock().windows.iter().map(|w| (w.vid, w.title.clone(), w.state.clone(), w.terminal)).collect();
    for (vid, title, state, terminal) in list {
        {
            let mut st = state.lock();
            if st.settings != cs {
                st.settings = cs.clone();
            }
        }
        let size = if terminal { [900.0, 560.0] } else { [1060.0, 860.0] };
        let builder = ViewportBuilder::default()
            .with_title(title)
            .with_app_id("pmxspan-console")
            .with_inner_size(size)
            .with_min_inner_size([480.0, 320.0])
            .with_icon(icon());
        ctx.show_viewport_deferred(vid, builder, move |ui, _class| {
            let ctx = ui.ctx().clone();
            if ctx.input(|i| !i.events.is_empty()) {
                ACTIVITY.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            let mut st = state.lock();
            if ctx.input(|i| i.viewport().close_requested()) {
                st.close = true;
                ctx.request_repaint_of(ViewportId::ROOT);
            }
            st.ui(ui, vid);
        });
    }
}

fn add_window(ctx: &egui::Context, req: OpenRequest, cs: &ConsoleSettings) {
    let mut c = CONSOLES.lock();
    let server = format!("{}:{}", req.client.host(), req.client.port());
    // One window per target: bring an existing one to the front.
    if let Some(w) = c.windows.iter().find(|w| w.target == req.target && w.server == server) {
        ctx.send_viewport_cmd_to(w.vid, egui::ViewportCommand::Focus);
        return;
    }
    c.serial += 1;
    let vid = ViewportId::from_hash_of(("pmxspan-console", c.serial));
    let terminal = req.target.is_terminal();
    let view = if terminal { View::Term(Box::new(term_view::TermView::new(cs))) } else { View::Vnc(Box::new(vnc_view::VncView::new(cs))) };
    let state = ConsoleState {
        client: req.client,
        target: req.target.clone(),
        browser_url: req.browser_url,
        settings: cs.clone(),
        view,
        close: false,
        started: false,
    };
    c.windows.push(Window { vid, title: req.title, target: req.target, server, terminal, state: Arc::new(Mutex::new(state)) });
}

fn token_prompt(ctx: &egui::Context, cs: &ConsoleSettings) {
    let (title, has_browser) = {
        let c = CONSOLES.lock();
        let Some(req) = c.token_prompt.as_ref() else { return };
        (req.title.clone(), req.browser_url.is_some())
    };
    let mut choice = None;
    egui::Window::new("Terminal console").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(
        ctx,
        |ui| {
            ui.set_max_width(460.0);
            ui.label(RichText::new(&title).strong());
            ui.add_space(4.0);
            ui.label(TOKEN_TERMINAL_NOTE);
            ui.add_space(4.0);
            widgets::muted(
                ui,
                "Add a server profile that logs in with a password (TOTP supported) to use terminal consoles inside PMXSpan.",
            );
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if has_browser && widgets::primary_button(ui, format!("{}  Open in browser", theme::icon::EXTERNAL)).clicked() {
                    choice = Some(1);
                }
                if ui.button("Try anyway").on_hover_text("Asks the server; it will most likely refuse the token").clicked() {
                    choice = Some(2);
                }
                if ui.button("Cancel").clicked() {
                    choice = Some(0);
                }
            });
        },
    );
    if let Some(choice) = choice {
        let req = CONSOLES.lock().token_prompt.take();
        if let Some(req) = req {
            match choice {
                1 => {
                    if let Some(url) = &req.browser_url {
                        let _ = webbrowser::open(url);
                    }
                }
                2 => add_window(ctx, req, cs),
                _ => {}
            }
        }
    }
}

/// The web console URL of a target (the "Open in browser" fallback).
pub fn browser_url(client: &PveClient, target: &ConsoleTarget, name: &str) -> String {
    use crate::ops::guest::{console_url, node_shell_url};
    match target {
        ConsoleTarget::VmDisplay(g) | ConsoleTarget::Container(g) => console_url(client, g, name),
        // The web UI shows a VM's serial port with xterm.js.
        ConsoleTarget::VmSerial(g, _) => console_url(client, g, name).replace("novnc=1", "xtermjs=1"),
        ConsoleTarget::NodeShell(node) => node_shell_url(client, node),
    }
}

/// Opens an in-app console for `target` on the connected server.
pub fn open_target(sh: &Shared, target: ConsoleTarget, name: &str) {
    let url = browser_url(&sh.client, &target, name);
    open(OpenRequest::new(sh.client.clone(), target, name, Some(url)));
}

/// The console a guest's primary action opens: VNC for VMs with a display,
/// the first serial port for VMs without one, the console for containers.
pub fn primary_for(g: &GuestRef, cfg: Option<&Map<String, Value>>) -> ConsoleTarget {
    if g.kind == GuestKind::Qemu
        && let Some(cfg) = cfg
        && !has_display(cfg)
        && let Some(&n) = serial_ports(cfg).first()
    {
        return ConsoleTarget::VmSerial(g.clone(), n);
    }
    primary_target(g)
}

/// The guest page's console controls: "Console" (in-app) and a menu with
/// the serial ports, the browser console and SPICE. For right-to-left
/// layouts (the menu button is added first so it ends up on the right).
pub fn guest_console_buttons(ui: &mut egui::Ui, sh: &Shared, g: &GuestRef, res: &Resource, cfg: Option<&Map<String, Value>>) {
    let running = res.status == "running";
    let name = res.display_name();
    let r = widgets::icon_button(ui, theme::icon::CARET_DOWN, "").on_hover_text("More console options");
    egui::Popup::menu(&r).show(|ui| {
        if g.kind == GuestKind::Qemu {
            match cfg.map(serial_ports) {
                Some(ports) if !ports.is_empty() => {
                    for n in ports {
                        if ui
                            .add_enabled(running, egui::Button::new(format!("{}  Serial console (serial{n})", theme::icon::TERMINAL)))
                            .clicked()
                        {
                            open_target(sh, ConsoleTarget::VmSerial(g.clone(), n), &name);
                        }
                    }
                }
                Some(_) => {
                    ui.add_enabled(false, egui::Button::new(format!("{}  Serial console", theme::icon::TERMINAL))).on_disabled_hover_text(
                        "This VM has no serial port. Add one (Hardware: serial0 = socket) and restart the VM to use a serial console.",
                    );
                }
                None => {
                    ui.add_enabled(false, egui::Button::new("Serial console (reading config…)"));
                }
            }
            if !cfg.map(has_display).unwrap_or(true) {
                ui.add_enabled(false, egui::Button::new(format!("{}  Display console", theme::icon::DESKTOP)))
                    .on_disabled_hover_text("The VM has no display (vga: none / serial).");
            } else if ui.add_enabled(running, egui::Button::new(format!("{}  Display console (VNC)", theme::icon::DESKTOP))).clicked() {
                open_target(sh, ConsoleTarget::VmDisplay(g.clone()), &name);
            }
            ui.separator();
        }
        if ui
            .button(format!("{}  Open in browser", theme::icon::EXTERNAL))
            .on_hover_text("The Proxmox web console (log in to the web UI there if asked)")
            .clicked()
        {
            let _ = webbrowser::open(&crate::ops::guest::console_url(&sh.client, g, &name));
        }
        if g.kind == GuestKind::Qemu
            && ui
                .add_enabled(running, egui::Button::new(format!("{}  SPICE (virt-viewer)", theme::icon::DESKTOP)))
                .on_hover_text("Requires a SPICE display and virt-viewer")
                .clicked()
        {
            let (c, gr) = (sh.client.clone(), g.clone());
            sh.spawn(format!("SPICE console {}", g.vmid), move |_ctx| async move { crate::ops::guest::open_spice(&c, &gr).await });
        }
    });
    let hover = if !running {
        "Start the guest to open its console"
    } else if g.kind == GuestKind::Qemu {
        "Opens the VM console in a PMXSpan window"
    } else {
        "Opens the container console in a PMXSpan window"
    };
    if ui
        .add_enabled(running, egui::Button::new(format!("{}  Console", theme::icon::TERMINAL)))
        .on_hover_text(hover)
        .on_disabled_hover_text(hover)
        .clicked()
    {
        open_target(sh, primary_for(g, cfg), &name);
    }
}

/// The "Consoles" section of the settings view.
pub fn settings_section(ui: &mut egui::Ui, s: &mut Settings) {
    ui.add_space(12.0);
    ui.label(RichText::new("Consoles").strong());
    egui::Grid::new("console-prefs").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
        ui.label("Terminal font size");
        ui.add(egui::DragValue::new(&mut s.console_font_size).range(8.0..=32.0).speed(0.25).suffix(" pt"));
        ui.end_row();
        ui.label("Terminal scrollback");
        ui.add(egui::DragValue::new(&mut s.console_scrollback).range(0..=100_000).speed(50).suffix(" lines"))
            .on_hover_text("Applies to consoles opened (or reconnected) after the change.");
        ui.end_row();
        ui.label("VNC scaling");
        ui.horizontal(|ui| {
            ui.radio_value(&mut s.console_vnc_fit, true, "Fit to window");
            ui.radio_value(&mut s.console_vnc_fit, false, "1:1");
        });
        ui.end_row();
        ui.label("Multi-line paste");
        ui.checkbox(&mut s.console_confirm_paste, "ask before pasting several lines into a terminal");
        ui.end_row();
    });
}
