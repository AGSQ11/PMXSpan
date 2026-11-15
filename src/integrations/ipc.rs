//! Single instance: a second PMXSpan process hands its `pmxspan://` link (or
//! just "raise the window") to the running one over a local socket and exits.
//!
//! Unix: a domain socket at `$XDG_RUNTIME_DIR/pmxspan/ipc.sock` (fallback:
//! the data directory; portable mode always uses the data directory beside
//! the executable). Windows: the named pipe `\\.\pipe\pmxspan-<hash of the
//! data dir>`. The socket doubles as the instance lock. `--run-schedules`
//! never gets here.
//!
//! Frames are a 4-byte big-endian length and a JSON body, at most 64 KiB.

use serde::{Deserialize, Serialize};
use std::sync::mpsc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const MAX_MSG: usize = 64 * 1024;
/// More arguments than this are not a link hand-over.
const MAX_ARGS: usize = 16;
const IO_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    /// `pmxspan://` links from the second process's command line (may be empty).
    #[serde(default)]
    pub args: Vec<String>,
}

pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, body: &[u8]) -> std::io::Result<()> {
    if body.len() > MAX_MSG {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "message over 64 KiB"));
    }
    w.write_all(&(body.len() as u32).to_be_bytes()).await?;
    w.write_all(body).await?;
    w.flush().await
}

pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_MSG {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "message over 64 KiB refused"));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    Ok(body)
}

/// Decodes and sanity-checks a received message.
pub fn decode(body: &[u8]) -> std::io::Result<Message> {
    let m: Message = serde_json::from_slice(body).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    if m.args.len() > MAX_ARGS {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "too many arguments"));
    }
    Ok(m)
}

/// Serves one connection: read a message, pass it on, acknowledge.
async fn handle<S: AsyncRead + AsyncWrite + Unpin>(mut s: S, tx: &mpsc::Sender<Message>) -> std::io::Result<()> {
    let body = tokio::time::timeout(IO_TIMEOUT, read_frame(&mut s)).await.map_err(|_| std::io::ErrorKind::TimedOut)??;
    let msg = decode(&body)?;
    let _ = tx.send(msg);
    crate::rt::repaint();
    tokio::time::timeout(IO_TIMEOUT, write_frame(&mut s, b"ok")).await.map_err(|_| std::io::ErrorKind::TimedOut)?
}

/// Sends a message over a connected stream and waits for the acknowledgement.
async fn send<S: AsyncRead + AsyncWrite + Unpin>(mut s: S, msg: &Message) -> std::io::Result<()> {
    let body = serde_json::to_vec(msg)?;
    tokio::time::timeout(IO_TIMEOUT, async {
        write_frame(&mut s, &body).await?;
        let ack = read_frame(&mut s).await?;
        if ack != b"ok" {
            return Err(std::io::Error::other("unexpected reply"));
        }
        Ok(())
    })
    .await
    .map_err(|_| std::io::Error::from(std::io::ErrorKind::TimedOut))?
}

/// The `pmxspan://` links on this process's command line.
pub fn link_args() -> Vec<String> {
    std::env::args().skip(1).filter(|a| a.get(..8).is_some_and(|p| p.eq_ignore_ascii_case("pmxspan:"))).take(MAX_ARGS).collect()
}

pub enum Instance {
    /// This process is the primary instance (or runs without IPC).
    Primary,
    /// The link was handed to the running instance: exit now.
    Forwarded,
}

static RECEIVER: parking_lot::Mutex<Option<mpsc::Receiver<Message>>> = parking_lot::Mutex::new(None);

/// Messages for the UI: the start-up links first, then anything forwarded.
pub fn take_receiver() -> Option<mpsc::Receiver<Message>> {
    RECEIVER.lock().take()
}

/// Becomes the primary instance or forwards `args` to the running one.
pub fn start(paths: &crate::config::AppPaths, args: Vec<String>) -> Instance {
    let (tx, rx) = mpsc::channel();
    let msg = Message { args };
    let outcome = crate::rt::runtime().block_on(platform::claim(paths, &msg, tx.clone()));
    match outcome {
        Ok(true) => {
            tracing::info!("handed over to the running PMXSpan");
            return Instance::Forwarded;
        }
        Ok(false) => {}
        Err(e) => tracing::warn!("single-instance check unavailable, running on our own: {e}"),
    }
    if !msg.args.is_empty() {
        let _ = tx.send(msg);
    }
    *RECEIVER.lock() = Some(rx);
    Instance::Primary
}

#[cfg(unix)]
pub mod platform {
    use super::*;
    use std::path::{Path, PathBuf};
    use tokio::net::{UnixListener, UnixStream};

    pub fn socket_path(paths: &crate::config::AppPaths) -> PathBuf {
        if !paths.is_portable()
            && let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|p| p.is_absolute() && p.is_dir())
        {
            return rt.join("pmxspan").join("ipc.sock");
        }
        paths.data_dir.join("ipc.sock")
    }

    /// Ok(true): forwarded; Ok(false): we are primary now and serve the socket.
    pub async fn claim(paths: &crate::config::AppPaths, msg: &Message, tx: mpsc::Sender<Message>) -> std::io::Result<bool> {
        claim_at(&socket_path(paths), msg, tx).await
    }

    pub async fn claim_at(path: &Path, msg: &Message, tx: mpsc::Sender<Message>) -> std::io::Result<bool> {
        for _ in 0..2 {
            if try_forward(path, msg).await? {
                return Ok(true);
            }
            match bind(path) {
                Ok(listener) => {
                    tokio::spawn(serve(listener, tx));
                    return Ok(false);
                }
                // Another instance bound it in between: forward to it.
                Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => continue,
                Err(e) => return Err(e),
            }
        }
        Err(std::io::Error::other("could not claim the instance socket"))
    }

    /// Ok(false) when nobody listens (no socket, or a stale one).
    pub async fn try_forward(path: &Path, msg: &Message) -> std::io::Result<bool> {
        match tokio::time::timeout(Duration::from_secs(2), UnixStream::connect(path)).await {
            Ok(Ok(s)) => send(s, msg).await.map(|()| true),
            Ok(Err(e)) if matches!(e.kind(), std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound) => Ok(false),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(std::io::ErrorKind::TimedOut.into()),
        }
    }

    pub fn bind(path: &Path) -> std::io::Result<UnixListener> {
        use std::os::unix::fs::{FileTypeExt, PermissionsExt};
        if let Some(dir) = path.parent() {
            crate::config::private_dir(dir);
        }
        // Remove a stale socket (nobody answered on it); never anything else.
        if let Ok(meta) = std::fs::symlink_metadata(path) {
            if !meta.file_type().is_socket() {
                return Err(std::io::Error::other(format!("{} exists and is not a socket", path.display())));
            }
            std::fs::remove_file(path)?;
        }
        let l = UnixListener::bind(path)?;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        Ok(l)
    }

    pub async fn serve(listener: UnixListener, tx: mpsc::Sender<Message>) {
        use std::os::unix::fs::MetadataExt;
        let owner = listener.local_addr().ok().and_then(|a| a.as_pathname().and_then(|p| std::fs::metadata(p).ok())).map(|m| m.uid());
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            };
            // Only our own user may talk to us (the folder is private too).
            if let (Some(uid), Ok(cred)) = (owner, stream.peer_cred())
                && cred.uid() != uid
            {
                tracing::warn!("refused an instance message from uid {}", cred.uid());
                continue;
            }
            let tx = tx.clone();
            tokio::spawn(async move {
                if let Err(e) = handle(stream, &tx).await {
                    tracing::warn!("instance message refused: {e}");
                }
            });
        }
    }
}

#[cfg(windows)]
pub mod platform {
    use super::*;
    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};

    const ERROR_FILE_NOT_FOUND: i32 = 2;
    const ERROR_PIPE_BUSY: i32 = 231;

    pub fn pipe_name(paths: &crate::config::AppPaths) -> String {
        let key = paths.data_dir.to_string_lossy().to_lowercase();
        let hash = crate::integrations::bundle::sha256_hex(key.as_bytes());
        format!(r"\\.\pipe\pmxspan-{}", &hash[..16])
    }

    pub async fn claim(paths: &crate::config::AppPaths, msg: &Message, tx: mpsc::Sender<Message>) -> std::io::Result<bool> {
        let name = pipe_name(paths);
        for _ in 0..2 {
            if try_forward(&name, msg).await? {
                return Ok(true);
            }
            match ServerOptions::new().first_pipe_instance(true).reject_remote_clients(true).create(&name) {
                Ok(server) => {
                    tokio::spawn(serve(name, server, tx));
                    return Ok(false);
                }
                // Someone else created it in between: forward to it.
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => continue,
                Err(e) => return Err(e),
            }
        }
        Err(std::io::Error::other("could not claim the instance pipe"))
    }

    async fn try_forward(name: &str, msg: &Message) -> std::io::Result<bool> {
        for _ in 0..20 {
            match ClientOptions::new().open(name) {
                Ok(client) => return send(client, msg).await.map(|()| true),
                Err(e) if e.raw_os_error() == Some(ERROR_FILE_NOT_FOUND) => return Ok(false),
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => tokio::time::sleep(Duration::from_millis(50)).await,
                Err(e) => return Err(e),
            }
        }
        Err(std::io::ErrorKind::TimedOut.into())
    }

    async fn serve(name: String, mut server: NamedPipeServer, tx: mpsc::Sender<Message>) {
        loop {
            if server.connect().await.is_err() {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            let next = match ServerOptions::new().reject_remote_clients(true).create(&name) {
                Ok(n) => n,
                Err(e) => {
                    tracing::warn!("instance pipe stopped: {e}");
                    return;
                }
            };
            let conn = std::mem::replace(&mut server, next);
            let tx = tx.clone();
            tokio::spawn(async move {
                if let Err(e) = handle(conn, &tx).await {
                    tracing::warn!("instance message refused: {e}");
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_round_trip_and_oversize_is_refused() {
        let (mut a, mut b) = tokio::io::duplex(256 * 1024);
        write_frame(&mut a, b"hello").await.unwrap();
        assert_eq!(read_frame(&mut b).await.unwrap(), b"hello");
        assert!(write_frame(&mut a, &vec![0u8; MAX_MSG + 1]).await.is_err());
        // A peer announcing more than 64 KiB is refused before reading it.
        a.write_all(&((MAX_MSG as u32) + 1).to_be_bytes()).await.unwrap();
        assert_eq!(read_frame(&mut b).await.unwrap_err().kind(), std::io::ErrorKind::InvalidData);
        assert!(decode(b"not json").is_err());
        assert!(decode(serde_json::to_vec(&Message { args: vec!["x".into(); 17] }).unwrap().as_slice()).is_err());
        assert_eq!(decode(br#"{"args":["pmxspan://guest?v=1"],"future":1}"#).unwrap().args, ["pmxspan://guest?v=1"]);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn second_instance_forwards_over_the_socket() {
        let dir = std::env::temp_dir().join(format!("pmxspan-ipc-{}", &crate::config::new_id()[..8]));
        let path = dir.join("ipc.sock");
        // First: nobody listens, so we become primary.
        let (tx, rx) = mpsc::channel();
        assert!(!platform::claim_at(&path, &Message::default(), tx).await.unwrap());
        // Second: forwards and the primary receives it.
        let msg = Message { args: vec!["pmxspan://guest?v=1&cluster=a&vmid=100".into()] };
        let (tx2, _rx2) = mpsc::channel();
        assert!(platform::claim_at(&path, &msg, tx2).await.unwrap());
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), msg);
        // An empty hand-over (start without a link) raises the window.
        assert!(platform::try_forward(&path, &Message::default()).await.unwrap());
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), Message::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stale_socket_is_replaced() {
        let dir = std::env::temp_dir().join(format!("pmxspan-ipc-stale-{}", &crate::config::new_id()[..8]));
        let path = dir.join("ipc.sock");
        // A socket nobody listens on any more.
        drop(platform::bind(&path).unwrap());
        assert!(path.exists());
        assert!(!platform::try_forward(&path, &Message::default()).await.unwrap());
        let (tx, _rx) = mpsc::channel();
        assert!(!platform::claim_at(&path, &Message::default(), tx).await.unwrap());
        // A regular file in its place is never removed.
        let other = dir.join("not-a-socket");
        std::fs::write(&other, b"x").unwrap();
        assert!(platform::bind(&other).is_err());
        assert!(other.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
