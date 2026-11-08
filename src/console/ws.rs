//! WebSocket transport over PMXSpan's own API client.
//!
//! The `vncwebsocket` upgrade is an ordinary authenticated API request, so it
//! goes through [`PveClient`]: the same reqwest client (TLS pinning included)
//! and the same `Authorization: PVEAPIToken=..` header or `PVEAuthCookie`.
//! After the `101 Switching Protocols` the upgraded connection is handed to
//! tungstenite without a second handshake and without a second TLS stack.

use crate::api::PveClient;
use anyhow::{anyhow, bail};
use base64::Engine;
use futures_util::{Sink, Stream};
use sha1::{Digest, Sha1};
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll, ready};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

/// The magic GUID of RFC 6455, section 1.3.
const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// How long the server gets to answer the upgrade request.
const UPGRADE_TIMEOUT: Duration = Duration::from_secs(15);

pub type WsStream = WebSocketStream<reqwest::Upgraded>;

/// A random `Sec-WebSocket-Key` (16 random bytes, base64).
pub fn new_key() -> String {
    let bytes: [u8; 16] = rand::random();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// The `Sec-WebSocket-Accept` the server must answer for `key`.
pub fn accept_key(key: &str) -> String {
    let mut h = Sha1::new();
    h.update(key.as_bytes());
    h.update(WS_GUID.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(h.finalize())
}

/// Explains a failed upgrade in terms of what the user can do about it.
pub fn upgrade_error(status: u16, reason: &str, body: &str) -> String {
    let mut msg = format!("console WebSocket refused: HTTP {status}");
    let reason = reason.trim();
    if !reason.is_empty() {
        msg.push(' ');
        msg.push_str(reason);
    }
    let body = body.trim();
    if !body.is_empty() && !reason.contains(body) {
        let short: String = body.chars().take(300).collect();
        msg.push_str(&format!(" ({short})"));
    }
    let hint = match status {
        401 => "the session or the console ticket was not accepted; console tickets must be used within ~10 seconds, try again",
        403 => "permission denied: the user needs VM.Console on the guest (Sys.Console for node shells)",
        500 if reason.contains("does not exist") || body.contains("does not exist") => "the guest no longer exists on that node",
        595 | 596 => "the node that runs the guest could not be reached from the node PMXSpan is connected to",
        _ => "",
    };
    if !hint.is_empty() {
        msg.push_str(" - ");
        msg.push_str(hint);
    }
    msg
}

/// Opens the console WebSocket at `ws_path` (a `.../vncwebsocket` path) for a
/// freshly issued proxy `port` and `ticket`.
pub async fn connect(client: &PveClient, ws_path: &str, port: u16, ticket: &str) -> anyhow::Result<WsStream> {
    let key = new_key();
    let req = client
        .authorized_get(ws_path)
        .await?
        // reqwest form-encodes the query: the ticket's ':', '=', '+' and '/' are escaped.
        .query(&[("port", port.to_string()), ("vncticket", ticket.to_string())])
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", &key)
        .header("Sec-WebSocket-Protocol", "binary");
    let resp = tokio::time::timeout(UPGRADE_TIMEOUT, req.send())
        .await
        .map_err(|_| anyhow!("the server did not answer the console WebSocket request within {}s", UPGRADE_TIMEOUT.as_secs()))?
        .map_err(|e| anyhow!("console WebSocket request failed: {e}"))?;
    let status = resp.status();
    if status != reqwest::StatusCode::SWITCHING_PROTOCOLS {
        let reason = resp
            .extensions()
            .get::<hyper::ext::ReasonPhrase>()
            .map(|r| String::from_utf8_lossy(r.as_bytes()).to_string())
            .or_else(|| status.canonical_reason().map(str::to_string))
            .unwrap_or_default();
        let body = resp.text().await.unwrap_or_default();
        bail!(upgrade_error(status.as_u16(), &reason, &body));
    }
    let accept = resp.headers().get("sec-websocket-accept").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
    if accept != accept_key(&key) {
        bail!("the server answered the console WebSocket upgrade with a wrong Sec-WebSocket-Accept");
    }
    let upgraded = resp.upgrade().await.map_err(|e| anyhow!("upgrading the console connection failed: {e}"))?;
    Ok(WebSocketStream::from_raw_socket(upgraded, Role::Client, None).await)
}

/// A WebSocket as a byte stream: binary (and text) messages are concatenated
/// on read, every write becomes one binary message, a close frame or a closed
/// connection reads as end of file. Pings are answered by tungstenite.
pub struct WsIo<S> {
    ws: WebSocketStream<S>,
    buf: Vec<u8>,
    pos: usize,
    eof: bool,
    received: Arc<AtomicU64>,
}

impl<S> WsIo<S> {
    pub fn new(ws: WebSocketStream<S>) -> Self {
        Self { ws, buf: Vec::new(), pos: 0, eof: false, received: Arc::new(AtomicU64::new(0)) }
    }

    /// Payload bytes received so far (shared counter for status lines).
    pub fn received_counter(&self) -> Arc<AtomicU64> {
        self.received.clone()
    }
}

fn io_err(e: WsError) -> io::Error {
    match e {
        WsError::Io(e) => e,
        other => io::Error::other(other),
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for WsIo<S> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, out: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        loop {
            if this.pos < this.buf.len() {
                let n = (this.buf.len() - this.pos).min(out.remaining());
                out.put_slice(&this.buf[this.pos..this.pos + n]);
                this.pos += n;
                return Poll::Ready(Ok(()));
            }
            if this.eof || out.remaining() == 0 {
                return Poll::Ready(Ok(()));
            }
            match ready!(Pin::new(&mut this.ws).poll_next(cx)) {
                Some(Ok(Message::Binary(b))) => {
                    this.received.fetch_add(b.len() as u64, Ordering::Relaxed);
                    this.buf = b.to_vec();
                    this.pos = 0;
                }
                Some(Ok(Message::Text(t))) => {
                    this.received.fetch_add(t.len() as u64, Ordering::Relaxed);
                    this.buf = t.as_bytes().to_vec();
                    this.pos = 0;
                }
                Some(Ok(Message::Close(_))) | None => this.eof = true,
                Some(Ok(_)) => {} // ping / pong / raw frame
                Some(Err(WsError::ConnectionClosed | WsError::AlreadyClosed)) => this.eof = true,
                Some(Err(e)) => return Poll::Ready(Err(io_err(e))),
            }
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for WsIo<S> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, data: &[u8]) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if data.is_empty() {
            return Poll::Ready(Ok(0));
        }
        ready!(Pin::new(&mut this.ws).poll_ready(cx)).map_err(io_err)?;
        Pin::new(&mut this.ws).start_send(Message::Binary(data.to_vec().into())).map_err(io_err)?;
        // Push it out now if the socket takes it; otherwise the next flush does.
        if let Poll::Ready(Err(e)) = Pin::new(&mut this.ws).poll_flush(cx) {
            return Poll::Ready(Err(io_err(e)));
        }
        Poll::Ready(Ok(data.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().ws).poll_flush(cx).map_err(io_err)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match ready!(Pin::new(&mut self.get_mut().ws).poll_close(cx)) {
            Ok(()) | Err(WsError::ConnectionClosed | WsError::AlreadyClosed) => Poll::Ready(Ok(())),
            Err(e) => Poll::Ready(Err(io_err(e))),
        }
    }
}

/// Bridges a [`WsIo`] to an in-memory duplex pipe and returns the other end.
///
/// The VNC engine wants a `Send + Sync` stream; the upgraded HTTP connection
/// is only `Send`. The pipe end is both, and the pump coalesces the many
/// small writes of one RFB message into a single WebSocket message.
pub fn bridge<S>(io: WsIo<S>) -> (tokio::io::DuplexStream, tokio::task::JoinHandle<()>)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (near, mut far) = tokio::io::duplex(256 * 1024);
    let pump = tokio::spawn(async move {
        let mut io = io;
        if let Err(e) = tokio::io::copy_bidirectional(&mut io, &mut far).await {
            tracing::debug!("console stream ended: {e}");
        }
        use tokio::io::AsyncWriteExt;
        let _ = io.shutdown().await;
    });
    (near, pump)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn accept_key_matches_rfc_example() {
        // RFC 6455, section 1.3.
        assert_eq!(accept_key("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
        let k = new_key();
        assert_eq!(base64::engine::general_purpose::STANDARD.decode(&k).unwrap().len(), 16);
        assert_ne!(k, new_key());
    }

    #[test]
    fn upgrade_errors_explain_the_status() {
        let e = upgrade_error(401, "No ticket", "");
        assert!(e.contains("HTTP 401 No ticket") && e.contains("10 seconds"), "{e}");
        let e = upgrade_error(403, "Permission check failed", "{\"data\":null}");
        assert!(e.contains("403") && e.contains("VM.Console"), "{e}");
        assert!(upgrade_error(595, "Connection refused", "").contains("could not be reached"));
        assert_eq!(upgrade_error(418, "", ""), "console WebSocket refused: HTTP 418");
    }

    async fn pair() -> (WebSocketStream<tokio::io::DuplexStream>, WebSocketStream<tokio::io::DuplexStream>) {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let client = WebSocketStream::from_raw_socket(a, Role::Client, None).await;
        let server = WebSocketStream::from_raw_socket(b, Role::Server, None).await;
        (client, server)
    }

    #[tokio::test]
    async fn adapter_concatenates_messages_and_writes_one_message_per_write() {
        let (client, mut server) = pair().await;
        let mut io = WsIo::new(client);
        let counter = io.received_counter();
        // Server sends one logical record split over three messages (plus a ping).
        server.send(Message::Binary(b"RFB 003".to_vec().into())).await.unwrap();
        server.send(Message::Ping(b"p".to_vec().into())).await.unwrap();
        server.send(Message::Binary(b".008".to_vec().into())).await.unwrap();
        server.send(Message::Text("\n".into())).await.unwrap();
        let mut got = [0u8; 12];
        io.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"RFB 003.008\n");
        assert_eq!(counter.load(Ordering::Relaxed), 12);

        // A short read buffer drains one message over several reads.
        server.send(Message::Binary(b"abcdef".to_vec().into())).await.unwrap();
        let mut two = [0u8; 2];
        io.read_exact(&mut two).await.unwrap();
        assert_eq!(&two, b"ab");
        let mut four = [0u8; 4];
        io.read_exact(&mut four).await.unwrap();
        assert_eq!(&four, b"cdef");

        io.write_all(b"hello").await.unwrap();
        io.flush().await.unwrap();
        io.write_all(b"world").await.unwrap();
        io.flush().await.unwrap();
        // (The client answered the ping with a pong first.)
        let mut data = Vec::new();
        while data.len() < 2 {
            match server.next().await.unwrap().unwrap() {
                Message::Pong(_) => {}
                m => data.push(m),
            }
        }
        let (m1, m2) = (data[0].clone(), data[1].clone());
        assert_eq!(m1, Message::Binary(b"hello".to_vec().into()));
        assert_eq!(m2, Message::Binary(b"world".to_vec().into()));

        // A close frame reads as end of file.
        server.close(None).await.unwrap();
        let mut rest = Vec::new();
        io.read_to_end(&mut rest).await.unwrap();
        assert!(rest.is_empty());
    }

    #[tokio::test]
    async fn bridge_carries_both_directions() {
        let (client, mut server) = pair().await;
        let (mut pipe, pump) = bridge(WsIo::new(client));
        server.send(Message::Binary(b"frag".to_vec().into())).await.unwrap();
        server.send(Message::Binary(b"ment".to_vec().into())).await.unwrap();
        let mut got = [0u8; 8];
        pipe.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"fragment");
        pipe.write_all(b"\x04\x01\x00\x00\x00\x00\xff\x0d").await.unwrap();
        let mut collected = Vec::new();
        while collected.len() < 8 {
            if let Message::Binary(b) = server.next().await.unwrap().unwrap() {
                collected.extend_from_slice(&b);
            }
        }
        assert_eq!(collected, b"\x04\x01\x00\x00\x00\x00\xff\x0d");
        drop(pipe);
        // Closing the pipe closes the WebSocket.
        let end = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match server.next().await {
                    None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                    _ => {}
                }
            }
        })
        .await;
        assert!(end.is_ok());
        drop(server);
        assert!(tokio::time::timeout(Duration::from_secs(5), pump).await.is_ok(), "the pump ends with the socket");
    }
}
