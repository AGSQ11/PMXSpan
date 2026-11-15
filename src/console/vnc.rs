//! VM display consoles: `vncproxy` + `vncwebsocket` + RFB.
//!
//! RFB is spoken by the `vnc-rs` crate (pure Rust) over the WebSocket byte
//! stream. Proxmox's QEMU asks for VNC authentication (DES over the first 8
//! bytes of the password); the password is the `password` the vncproxy call
//! returns on current nodes, or the ticket itself on older ones. `vnc-rs`
//! implements the challenge.

use super::{ConsoleTarget, ws};
use crate::api::PveClient;
use anyhow::{Context, anyhow};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;
use tokio::sync::mpsc;
use vnc::{PixelFormat, VncConnector, VncEncoding, VncError, VncEvent, X11Event};

/// How long the RFB handshake may take.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
/// How often an incremental update is requested. QEMU answers only when the
/// screen changed and merges outstanding requests.
const REFRESH_EVERY: Duration = Duration::from_millis(33);

/// The encodings offered, in order of preference.
pub const ENCODINGS: &[VncEncoding] =
    &[VncEncoding::Zrle, VncEncoding::CopyRect, VncEncoding::Raw, VncEncoding::CursorPseudo, VncEncoding::DesktopSizePseudo];

/// Human-readable list of the offered encodings for the status line.
pub const ENCODINGS_LABEL: &str = "ZRLE, CopyRect, Raw (+ Cursor, DesktopSize)";

/// From the session to the window. Pixel data is BGRA (`PixelFormat::bgra`).
#[derive(Debug)]
pub enum VncUpdate {
    Status(String),
    Connected {
        width: u16,
        height: u16,
    },
    Resize {
        width: u16,
        height: u16,
    },
    Image {
        x: u16,
        y: u16,
        width: u16,
        height: u16,
        bgra: Vec<u8>,
    },
    Copy {
        dst: (u16, u16),
        src: (u16, u16),
        size: (u16, u16),
    },
    /// Cursor shape: hotspot, size and BGRA pixels whose alpha is the mask.
    Cursor {
        hot: (u16, u16),
        size: (u16, u16),
        bgra: Vec<u8>,
    },
    Bell,
    ClipboardText(String),
    /// The session ended; `Some` with the reason when it failed.
    Closed(Option<String>),
}

/// From the window to the session.
#[derive(Debug)]
pub enum VncInput {
    Key(u32, bool),
    /// Framebuffer coordinates and the RFB button mask.
    Pointer(u16, u16, u8),
    Clipboard(String),
    FullRefresh,
    Close,
}

fn vnc_error(e: VncError) -> anyhow::Error {
    match e {
        VncError::WrongPassword => anyhow!("the VNC server rejected the console ticket"),
        VncError::NoPassword => anyhow!("the VNC server asks for a password but none was available"),
        other => anyhow!("VNC: {other}"),
    }
}

/// Runs one VNC session until it ends. `received` counts WebSocket payload
/// bytes for the status line; `wake` is called after each batch of updates.
pub async fn run(
    client: PveClient,
    target: ConsoleTarget,
    events: std::sync::mpsc::Sender<VncUpdate>,
    mut input: mpsc::UnboundedReceiver<VncInput>,
    received: Arc<AtomicU64>,
    wake: impl Fn() + Send + 'static,
) {
    let result = session(&client, &target, &events, &mut input, received, &wake).await;
    let _ = events.send(VncUpdate::Closed(result.err().map(|e| format!("{e:#}"))));
    wake();
}

async fn session(
    client: &PveClient,
    target: &ConsoleTarget,
    events: &std::sync::mpsc::Sender<VncUpdate>,
    input: &mut mpsc::UnboundedReceiver<VncInput>,
    received: Arc<AtomicU64>,
    wake: &impl Fn(),
) -> anyhow::Result<()> {
    let send = |e: VncUpdate| {
        let _ = events.send(e);
    };
    send(VncUpdate::Status("Requesting a console ticket…".into()));
    wake();
    let ticket = super::request_proxy(client, target).await?;
    send(VncUpdate::Status("Opening the console socket…".into()));
    wake();
    let socket = ws::connect(client, &target.websocket_path(), ticket.port, &ticket.ticket).await?;
    let io = ws::WsIo::new(socket);
    let counter = io.received_counter();
    let (pipe, pump) = ws::bridge(io);
    // Mirror the transport's counter into the window's.
    let mirror = tokio::spawn({
        let counter = counter.clone();
        async move {
            let mut t = tokio::time::interval(Duration::from_millis(500));
            loop {
                t.tick().await;
                received.store(counter.load(std::sync::atomic::Ordering::Relaxed), std::sync::atomic::Ordering::Relaxed);
            }
        }
    });
    let _guard = AbortOnDrop(vec![pump.abort_handle(), mirror.abort_handle()]);

    send(VncUpdate::Status("VNC handshake…".into()));
    wake();
    let password = ticket.vnc_password();
    let mut connector = VncConnector::new(pipe).set_auth_method(async move { Ok(password) });
    for e in ENCODINGS {
        connector = connector.add_encoding(*e);
    }
    let vnc = tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
        connector
            .allow_shared(true)
            .set_pixel_format(PixelFormat::bgra())
            .build()
            .map_err(vnc_error)?
            .try_start()
            .await
            .map_err(vnc_error)?
            .finish()
            .map_err(vnc_error)
    })
    .await
    .map_err(|_| anyhow!("the VNC handshake did not finish within {}s", HANDSHAKE_TIMEOUT.as_secs()))?
    .context("VNC handshake failed")?;

    let mut refresh = tokio::time::interval(REFRESH_EVERY);
    refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut connected = false;
    let mut last_error: Option<String> = None;
    loop {
        tokio::select! {
            ev = vnc.recv_event() => {
                let ev = match ev {
                    Ok(ev) => ev,
                    Err(VncError::ClientNotRunning) => {
                        let _ = vnc.close().await;
                        return match last_error {
                            Some(e) => Err(anyhow!("VNC: {e}")),
                            None => Ok(()),
                        };
                    }
                    Err(e) => return Err(vnc_error(e)),
                };
                match ev {
                    VncEvent::SetResolution(s) => {
                        if connected {
                            send(VncUpdate::Resize { width: s.width, height: s.height });
                        } else {
                            connected = true;
                            send(VncUpdate::Connected { width: s.width, height: s.height });
                        }
                    }
                    VncEvent::RawImage(r, data) => {
                        send(VncUpdate::Image { x: r.x, y: r.y, width: r.width, height: r.height, bgra: data });
                    }
                    VncEvent::Copy(dst, src) => {
                        send(VncUpdate::Copy { dst: (dst.x, dst.y), src: (src.x, src.y), size: (dst.width, dst.height) });
                    }
                    VncEvent::SetCursor(r, data) => {
                        send(VncUpdate::Cursor { hot: (r.x, r.y), size: (r.width, r.height), bgra: data });
                    }
                    VncEvent::Bell => send(VncUpdate::Bell),
                    VncEvent::Text(t) => send(VncUpdate::ClipboardText(t)),
                    VncEvent::Error(e) => last_error = Some(e),
                    // Tight/JPEG is not offered; pixel format is set explicitly.
                    _ => {}
                }
                wake();
            }
            cmd = input.recv() => {
                let ev = match cmd {
                    Some(VncInput::Key(ks, down)) => X11Event::KeyEvent((ks, down).into()),
                    Some(VncInput::Pointer(x, y, buttons)) => X11Event::PointerEvent((x, y, buttons).into()),
                    Some(VncInput::Clipboard(text)) => X11Event::CopyText(text),
                    Some(VncInput::FullRefresh) => X11Event::FullRefresh,
                    Some(VncInput::Close) | None => {
                        let _ = vnc.close().await;
                        return Ok(());
                    }
                };
                if let Err(e) = vnc.input(ev).await {
                    let _ = vnc.close().await;
                    return match (e, last_error.take()) {
                        (_, Some(reason)) => Err(anyhow!("VNC: {reason}")),
                        (VncError::ClientNotRunning, None) => Ok(()),
                        (e, None) => Err(vnc_error(e)),
                    };
                }
            }
            _ = refresh.tick() => {
                if connected && vnc.input(X11Event::Refresh).await.is_err() {
                    // The receive arm reports why.
                }
            }
        }
    }
}

/// Aborts helper tasks when the session ends, however it ends.
struct AbortOnDrop(Vec<tokio::task::AbortHandle>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        for h in &self.0 {
            h.abort();
        }
    }
}

/// Characters RFB's ClientCutText can carry (Latin-1); others are replaced.
pub fn latin1_clipboard(text: &str) -> (String, bool) {
    let mut lossy = false;
    let s = text
        .chars()
        .map(|c| {
            if (c as u32) < 0x100 {
                c
            } else {
                lossy = true;
                '?'
            }
        })
        .collect();
    (s, lossy)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_is_latin1() {
        assert_eq!(latin1_clipboard("héllo"), ("héllo".to_string(), false));
        assert_eq!(latin1_clipboard("a€b"), ("a?b".to_string(), true));
    }

    #[test]
    fn offers_zrle_first_and_no_tight() {
        assert_eq!(ENCODINGS[0], VncEncoding::Zrle);
        assert!(!ENCODINGS.contains(&VncEncoding::Tight));
    }
}
