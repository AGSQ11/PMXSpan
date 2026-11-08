//! Terminal consoles over Proxmox's termproxy (the pve-xtermjs protocol).
//!
//! After the WebSocket is open:
//! 1. the client sends `"<user>:<ticket>\n"` (both from the termproxy reply);
//! 2. the server answers `OK` (anything else, or a closed socket, is a failed
//!    login);
//! 3. client -> server frames: `0:<byte length>:<bytes>` for input,
//!    `1:<cols>:<rows>:` for a resize and `2` as a keep-alive ping;
//! 4. server -> client: raw terminal output.

use super::{ConsoleTarget, ws};
use crate::api::PveClient;
use anyhow::{anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

/// The web client pings every 30 seconds; so do we.
pub const PING_INTERVAL: Duration = Duration::from_secs(30);
/// How long the server gets to answer the login line.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(10);

/// The keep-alive frame.
pub const PING: &[u8] = b"2";

/// The login line.
pub fn auth_line(user: &str, ticket: &str) -> Vec<u8> {
    format!("{user}:{ticket}\n").into_bytes()
}

/// An input frame: `0:<length of data in bytes>:<data>`.
pub fn encode_input(data: &[u8]) -> Vec<u8> {
    let mut out = format!("0:{}:", data.len()).into_bytes();
    out.extend_from_slice(data);
    out
}

/// A resize frame: `1:<cols>:<rows>:`.
pub fn encode_resize(cols: u16, rows: u16) -> Vec<u8> {
    format!("1:{cols}:{rows}:").into_bytes()
}

/// A decoded client frame (what the server side sees; used by tests and to
/// keep the encoder honest).
#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientFrame {
    Input(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    Ping,
}

#[cfg(test)]
impl ClientFrame {
    /// Parses one frame from the start of `buf`. Returns the frame and the
    /// bytes it used, `Ok(None)` when more bytes are needed.
    pub fn parse(buf: &[u8]) -> anyhow::Result<Option<(ClientFrame, usize)>> {
        let Some(&kind) = buf.first() else { return Ok(None) };
        match kind {
            b'2' => Ok(Some((ClientFrame::Ping, 1))),
            b'0' => {
                let Some((len, head)) = number_field(buf, 2)? else { return Ok(None) };
                let len = usize::try_from(len).map_err(|_| anyhow!("frame too long"))?;
                if buf.len() < head + len {
                    return Ok(None);
                }
                Ok(Some((ClientFrame::Input(buf[head..head + len].to_vec()), head + len)))
            }
            b'1' => {
                let Some((cols, after_cols)) = number_field(buf, 2)? else { return Ok(None) };
                let Some((rows, end)) = number_field(buf, after_cols)? else { return Ok(None) };
                let cols = u16::try_from(cols).map_err(|_| anyhow!("bad column count"))?;
                let rows = u16::try_from(rows).map_err(|_| anyhow!("bad row count"))?;
                Ok(Some((ClientFrame::Resize { cols, rows }, end)))
            }
            other => bail!("unknown frame type {:?}", other as char),
        }
    }
}

/// Reads `<digits>:` starting at `start` (which must follow a ':' at
/// `start - 1`). Returns the number and the index after its ':'.
#[cfg(test)]
fn number_field(buf: &[u8], start: usize) -> anyhow::Result<Option<(u64, usize)>> {
    if buf.len() < start {
        return Ok(None);
    }
    if buf[start - 1] != b':' {
        bail!("malformed frame");
    }
    let mut n: u64 = 0;
    for (i, &b) in buf[start..].iter().enumerate() {
        match b {
            b'0'..=b'9' => {
                n = n.checked_mul(10).and_then(|n| n.checked_add(u64::from(b - b'0'))).ok_or_else(|| anyhow!("number too large"))?;
                if i > 10 {
                    bail!("number too large");
                }
            }
            b':' if i > 0 => return Ok(Some((n, start + i + 1))),
            _ => bail!("malformed frame"),
        }
    }
    Ok(None)
}

/// Result of looking at the bytes received after the login line.
#[derive(Debug, PartialEq, Eq)]
pub enum LoginReply {
    /// Need more bytes.
    Pending,
    /// Logged in; the bytes after `OK` are already terminal output.
    Ok(Vec<u8>),
    /// Anything else: the proxy's complaint, if it said something.
    Rejected(String),
}

pub fn check_login_reply(buf: &[u8]) -> LoginReply {
    if buf.len() < 2 {
        return if b"OK".starts_with(buf) { LoginReply::Pending } else { LoginReply::Rejected(String::from_utf8_lossy(buf).into()) };
    }
    if &buf[..2] == b"OK" {
        LoginReply::Ok(buf[2..].to_vec())
    } else {
        LoginReply::Rejected(String::from_utf8_lossy(buf).trim().chars().take(200).collect())
    }
}

/// A connection that ended without a WebSocket close frame.
fn closed_abruptly(e: &tokio_tungstenite::tungstenite::Error) -> bool {
    use tokio_tungstenite::tungstenite::Error as E;
    use tokio_tungstenite::tungstenite::error::ProtocolError;
    match e {
        E::ConnectionClosed | E::AlreadyClosed | E::Protocol(ProtocolError::ResetWithoutClosingHandshake) => true,
        E::Io(io) => {
            matches!(io.kind(), std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe)
        }
        _ => false,
    }
}

/// From the session to the window.
#[derive(Debug)]
pub enum TermEvent {
    Status(String),
    Connected,
    Output(Vec<u8>),
    /// The session ended; `Some` with the reason when it failed.
    Closed(Option<String>),
}

/// From the window to the session.
#[derive(Debug)]
pub enum TermInput {
    Data(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    Close,
}

/// Runs one terminal session until it ends. Events go to `events`; `wake`
/// is called after each batch so the window repaints.
pub async fn run(
    client: PveClient,
    target: ConsoleTarget,
    size: (u16, u16),
    events: std::sync::mpsc::Sender<TermEvent>,
    mut input: mpsc::UnboundedReceiver<TermInput>,
    wake: impl Fn() + Send + 'static,
) {
    let result = session(&client, &target, size, &events, &mut input, &wake).await;
    let reason = match result {
        Ok(()) => None,
        Err(e) => {
            let mut msg = format!("{e:#}");
            if client.uses_token() && !msg.contains("password login") {
                msg.push_str("\n\n");
                msg.push_str(super::TOKEN_TERMINAL_NOTE);
            }
            Some(msg)
        }
    };
    let _ = events.send(TermEvent::Closed(reason));
    wake();
}

async fn session(
    client: &PveClient,
    target: &ConsoleTarget,
    (cols, rows): (u16, u16),
    events: &std::sync::mpsc::Sender<TermEvent>,
    input: &mut mpsc::UnboundedReceiver<TermInput>,
    wake: &impl Fn(),
) -> anyhow::Result<()> {
    let send = |e: TermEvent| {
        let _ = events.send(e);
        wake();
    };
    send(TermEvent::Status("Requesting a console ticket…".into()));
    let ticket = super::request_proxy(client, target).await?;
    send(TermEvent::Status("Opening the console socket…".into()));
    let mut ws = ws::connect(client, &target.websocket_path(), ticket.port, &ticket.ticket).await?;
    ws.send(Message::Binary(auth_line(&ticket.user, &ticket.ticket).into())).await?;

    let mut buf = Vec::new();
    let login = tokio::time::timeout(LOGIN_TIMEOUT, async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Binary(b))) => buf.extend_from_slice(&b),
                Some(Ok(Message::Text(t))) => buf.extend_from_slice(t.as_bytes()),
                Some(Ok(Message::Close(_))) | None => {
                    bail!("the terminal proxy closed the connection during login (user '{}' was not accepted)", ticket.user)
                }
                Some(Ok(_)) => continue,
                Some(Err(e)) if buf.is_empty() && closed_abruptly(&e) => {
                    bail!("the terminal proxy closed the connection during login (user '{}' was not accepted)", ticket.user)
                }
                Some(Err(e)) => bail!("the console connection failed during login: {e}"),
            }
            match check_login_reply(&buf) {
                LoginReply::Pending => continue,
                LoginReply::Ok(rest) => return Ok(rest),
                LoginReply::Rejected(why) => bail!("the terminal proxy rejected the login: {why}"),
            }
        }
    })
    .await
    .map_err(|_| anyhow!("the terminal proxy did not confirm the login within {}s", LOGIN_TIMEOUT.as_secs()))??;

    send(TermEvent::Connected);
    ws.send(Message::Binary(encode_resize(cols, rows).into())).await?;
    if !login.is_empty() {
        send(TermEvent::Output(login));
    }

    let mut ping = tokio::time::interval_at(tokio::time::Instant::now() + PING_INTERVAL, PING_INTERVAL);
    loop {
        tokio::select! {
            msg = ws.next() => match msg {
                Some(Ok(Message::Binary(b))) => send(TermEvent::Output(b.to_vec())),
                Some(Ok(Message::Text(t))) => send(TermEvent::Output(t.as_bytes().to_vec())),
                Some(Ok(Message::Close(_))) | None => return Ok(()),
                Some(Ok(_)) => {}
                Some(Err(e)) if closed_abruptly(&e) => return Ok(()),
                Some(Err(e)) => bail!("the console connection failed: {e}"),
            },
            cmd = input.recv() => match cmd {
                Some(TermInput::Data(d)) => {
                    if !d.is_empty() {
                        ws.send(Message::Binary(encode_input(&d).into())).await?;
                    }
                }
                Some(TermInput::Resize { cols, rows }) => ws.send(Message::Binary(encode_resize(cols, rows).into())).await?,
                Some(TermInput::Close) | None => {
                    let _ = tokio::time::timeout(Duration::from_secs(2), ws.close(None)).await;
                    return Ok(());
                }
            },
            _ = ping.tick() => ws.send(Message::Binary(PING.to_vec().into())).await?,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_all(mut buf: &[u8]) -> Vec<ClientFrame> {
        let mut out = Vec::new();
        while let Some((f, n)) = ClientFrame::parse(buf).unwrap() {
            out.push(f);
            buf = &buf[n..];
        }
        assert!(buf.is_empty(), "left over: {buf:?}");
        out
    }

    #[test]
    fn input_frames_count_utf8_bytes() {
        assert_eq!(encode_input(b"ls\r"), b"0:3:ls\r");
        // "ä" is two bytes, "€" three, the emoji four.
        let s = "ä€😀";
        assert_eq!(encode_input(s.as_bytes()), format!("0:9:{s}").into_bytes());
        assert_eq!(encode_input(b""), b"0:0:");
        assert_eq!(encode_resize(80, 24), b"1:80:24:");
        assert_eq!(auth_line("root@pam", "PVETERM:x"), b"root@pam:PVETERM:x\n");
    }

    #[test]
    fn frames_round_trip() {
        let mut stream = Vec::new();
        stream.extend(encode_resize(132, 43));
        stream.extend(encode_input("héllo:1:2".as_bytes()));
        stream.extend_from_slice(PING);
        stream.extend(encode_input(b"0:5:x"));
        assert_eq!(
            decode_all(&stream),
            vec![
                ClientFrame::Resize { cols: 132, rows: 43 },
                ClientFrame::Input("héllo:1:2".as_bytes().to_vec()),
                ClientFrame::Ping,
                ClientFrame::Input(b"0:5:x".to_vec()),
            ]
        );
    }

    #[test]
    fn partial_and_malformed_frames() {
        assert_eq!(ClientFrame::parse(b"").unwrap(), None);
        assert_eq!(ClientFrame::parse(b"0:5:ab").unwrap(), None);
        assert_eq!(ClientFrame::parse(b"0:1").unwrap(), None);
        assert_eq!(ClientFrame::parse(b"1:80:").unwrap(), None);
        assert!(ClientFrame::parse(b"9").is_err());
        assert!(ClientFrame::parse(b"0x").is_err());
        assert!(ClientFrame::parse(b"0::").is_err());
        assert!(ClientFrame::parse(b"1:99999:1:").is_err());
    }

    #[test]
    fn login_reply() {
        assert_eq!(check_login_reply(b""), LoginReply::Pending);
        assert_eq!(check_login_reply(b"O"), LoginReply::Pending);
        assert_eq!(check_login_reply(b"OK"), LoginReply::Ok(vec![]));
        assert_eq!(check_login_reply(b"OKroot@pve1:~# "), LoginReply::Ok(b"root@pve1:~# ".to_vec()));
        assert!(matches!(check_login_reply(b"authentication failed\n"), LoginReply::Rejected(s) if s == "authentication failed"));
        assert!(matches!(check_login_reply(b"X"), LoginReply::Rejected(_)));
    }
}
