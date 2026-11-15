//! End-to-end console tests against `tools/mock_pve.py`: the real API client,
//! the real WebSocket upgrade over TLS, a VNC session and a terminal session.
//! Skipped (with a note) when `python3` or `openssl` is not available.

use super::term::{self, TermEvent, TermInput};
use super::vnc::{self, VncInput, VncUpdate};
use super::{ConsoleTarget, ws};
use crate::api::types::{GuestKind, GuestRef};
use crate::api::{LoginOutcome, PveClient};
use crate::config::{AuthMode, ServerProfile, TlsMode};
use std::io::BufRead;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

struct Mock {
    child: Child,
    port: u16,
    certdir: std::path::PathBuf,
    lines: Arc<Mutex<Vec<String>>>,
}

impl Drop for Mock {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.certdir);
    }
}

fn available(cmd: &str, arg: &str) -> bool {
    Command::new(cmd).arg(arg).stdout(Stdio::null()).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false)
}

impl Mock {
    fn start() -> Option<Mock> {
        if !available("python3", "--version") || !available("openssl", "version") {
            eprintln!("skipping console end-to-end test: python3 or openssl not found");
            return None;
        }
        let port = std::net::TcpListener::bind("127.0.0.1:0").ok()?.local_addr().ok()?.port();
        let certdir = std::env::temp_dir().join(format!("pmxspan-mock-{}-{port}", std::process::id()));
        let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tools/mock_pve.py");
        let mut child = Command::new("python3")
            .arg(&script)
            .args(["--port", &port.to_string(), "--certdir"])
            .arg(&certdir)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .ok()?;
        let lines = Arc::new(Mutex::new(Vec::new()));
        let stdout = child.stdout.take()?;
        let sink = lines.clone();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout).lines().map_while(Result::ok) {
                sink.lock().unwrap().push(line);
            }
        });
        let mock = Mock { child, port, certdir, lines };
        assert!(mock.wait_line(|l| l.contains("mock Proxmox VE API"), Duration::from_secs(30)), "mock server did not start");
        Some(mock)
    }

    fn wait_line(&self, f: impl Fn(&str) -> bool, timeout: Duration) -> bool {
        let end = Instant::now() + timeout;
        while Instant::now() < end {
            if self.lines.lock().unwrap().iter().any(|l| f(l)) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    async fn client(&self, auth: AuthMode) -> PveClient {
        let profile = ServerProfile {
            host: "127.0.0.1".into(),
            port: self.port,
            auth,
            username: "root".into(),
            realm: "pam".into(),
            token_id: "pmxspan".into(),
            tls: TlsMode::Insecure,
            // The shortest client timeout: sessions must outlive it.
            request_timeout_secs: 5,
            ..Default::default()
        };
        let secret = if auth == AuthMode::ApiToken { "mocktoken" } else { "mockpass" };
        match PveClient::login(&profile, secret, None).await.expect("login") {
            LoginOutcome::Ok(c) => c,
            LoginOutcome::NeedTotp => panic!("unexpected TOTP"),
        }
    }
}

fn vm(vmid: u64) -> GuestRef {
    GuestRef { node: "pve1".into(), kind: GuestKind::Qemu, vmid }
}

fn recv<T>(rx: &mpsc::Receiver<T>, until: Instant) -> Option<T> {
    let left = until.checked_duration_since(Instant::now())?;
    rx.recv_timeout(left).ok()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn vnc_session_end_to_end_with_api_token() {
    let Some(mock) = Mock::start() else { return };
    // VNC consoles work with API tokens.
    let client = mock.client(AuthMode::ApiToken).await;
    let (etx, erx) = mpsc::channel();
    let (itx, irx) = tokio::sync::mpsc::unbounded_channel();
    let received = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let task = tokio::spawn(vnc::run(client, ConsoleTarget::VmDisplay(vm(100)), etx, irx, received.clone(), || {}));

    let erx = Arc::new(Mutex::new(erx));
    let wait_for = |pred: fn(&VncUpdate) -> bool, secs: u64| {
        let erx = erx.clone();
        tokio::task::spawn_blocking(move || {
            let until = Instant::now() + Duration::from_secs(secs);
            let rx = erx.lock().unwrap();
            while let Some(ev) = recv(&rx, until) {
                if let VncUpdate::Closed(Some(e)) = &ev {
                    panic!("session failed: {e}");
                }
                if pred(&ev) {
                    return Some(ev);
                }
            }
            None
        })
    };
    let connected = wait_for(|e| matches!(e, VncUpdate::Connected { .. }), 20).await.unwrap();
    assert!(matches!(connected, Some(VncUpdate::Connected { width: 320, height: 200 })), "{connected:?}");
    let image = wait_for(|e| matches!(e, VncUpdate::Image { .. }), 20).await.unwrap();
    match image {
        Some(VncUpdate::Image { width, height, bgra, .. }) => {
            assert_eq!((width, height), (320, 200));
            assert_eq!(bgra.len(), 320 * 200 * 4);
            // The first stripe is (200, 40, 40) red, as BGRA.
            assert_eq!(&bgra[4..7], &[40, 40, 200]);
        }
        other => panic!("no framebuffer update: {other:?}"),
    }
    // A second (incremental) update arrives too: the refresh loop works.
    assert!(wait_for(|e| matches!(e, VncUpdate::Image { .. }), 10).await.unwrap().is_some());

    itx.send(VncInput::Key(0x61, true)).unwrap();
    itx.send(VncInput::Key(0x61, false)).unwrap();
    itx.send(VncInput::Pointer(10, 20, 1)).unwrap();
    assert!(mock.wait_line(|l| l == "MOCK-VNC key 0x61 down", Duration::from_secs(10)), "key event not seen by the server");
    assert!(mock.wait_line(|l| l == "MOCK-VNC key 0x61 up", Duration::from_secs(10)));
    assert!(mock.wait_line(|l| l == "MOCK-VNC pointer 10,20 buttons 1", Duration::from_secs(10)));
    assert!(mock.wait_line(|l| l.starts_with("MOCK-VNC encodings [16, 1, 0"), Duration::from_secs(1)), "ZRLE first");

    itx.send(VncInput::Close).unwrap();
    let closed = wait_for(|e| matches!(e, VncUpdate::Closed(_)), 10).await.unwrap();
    assert!(matches!(closed, Some(VncUpdate::Closed(None))), "{closed:?}");
    assert!(mock.wait_line(|l| l == "MOCK-VNC closed", Duration::from_secs(10)), "the server saw no close");
    tokio::time::timeout(Duration::from_secs(5), task).await.expect("session task ended").unwrap();
}

async fn collect_output(rx: Arc<Mutex<mpsc::Receiver<TermEvent>>>, want: &'static str, secs: u64) -> (bool, String) {
    tokio::task::spawn_blocking(move || {
        let until = Instant::now() + Duration::from_secs(secs);
        let rx = rx.lock().unwrap();
        let mut all = Vec::new();
        while let Some(ev) = recv(&rx, until) {
            match ev {
                TermEvent::Output(b) => {
                    all.extend_from_slice(&b);
                    if String::from_utf8_lossy(&all).contains(want) {
                        return (true, String::from_utf8_lossy(&all).into_owned());
                    }
                }
                TermEvent::Closed(e) => return (false, format!("closed: {e:?}; output: {}", String::from_utf8_lossy(&all))),
                _ => {}
            }
        }
        (false, String::from_utf8_lossy(&all).into_owned())
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_session_end_to_end() {
    let Some(mock) = Mock::start() else { return };
    let client = mock.client(AuthMode::Password).await;
    let (etx, erx) = mpsc::channel();
    let (itx, irx) = tokio::sync::mpsc::unbounded_channel();
    let target = ConsoleTarget::Container(GuestRef { node: "pve2".into(), kind: GuestKind::Lxc, vmid: 102 });
    let task = tokio::spawn(term::run(client, target, (100, 30), etx, irx, || {}));
    let erx = Arc::new(Mutex::new(erx));

    let connected = {
        let rx = erx.clone();
        tokio::task::spawn_blocking(move || {
            let until = Instant::now() + Duration::from_secs(20);
            let rx = rx.lock().unwrap();
            while let Some(ev) = recv(&rx, until) {
                match ev {
                    TermEvent::Connected => return Ok(()),
                    TermEvent::Closed(e) => return Err(format!("{e:?}")),
                    _ => {}
                }
            }
            Err("timeout".into())
        })
        .await
        .unwrap()
    };
    if let Err(e) = connected {
        panic!("terminal login (OK) failed: {e}\nmock: {:#?}", mock.lines.lock().unwrap());
    }
    assert!(mock.wait_line(|l| l == "MOCK-TERM resize 100x30", Duration::from_secs(10)), "initial resize frame");

    // Outlive the client's 5 s request timeout: the upgraded socket must stay up.
    tokio::time::sleep(Duration::from_secs(6)).await;
    itx.send(TermInput::Data("echo pmx-$((6*7)) ä\r".as_bytes().to_vec())).unwrap();
    let shell = cfg!(target_os = "linux");
    let want = if shell { "pmx-42 ä" } else { "pmx-$((6*7))" };
    let (ok, out) = collect_output(erx.clone(), want, 20).await;
    assert!(ok, "command output not seen: {out}");
    assert!(mock.wait_line(|l| l.starts_with("MOCK-TERM input b'echo pmx-"), Duration::from_secs(5)));

    itx.send(TermInput::Resize { cols: 132, rows: 43 }).unwrap();
    assert!(mock.wait_line(|l| l == "MOCK-TERM resize 132x43", Duration::from_secs(10)));
    itx.send(TermInput::Close).unwrap();
    tokio::time::timeout(Duration::from_secs(10), task).await.expect("session task ended").unwrap();
    assert!(mock.wait_line(|l| l == "MOCK-TERM closed", Duration::from_secs(10)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_with_api_token_explains_password_requirement() {
    let Some(mock) = Mock::start() else { return };
    let client = mock.client(AuthMode::ApiToken).await;
    let (etx, erx) = mpsc::channel();
    let (_itx, irx) = tokio::sync::mpsc::unbounded_channel();
    term::run(client, ConsoleTarget::NodeShell("pve1".into()), (80, 24), etx, irx, || {}).await;
    let last = erx.try_iter().last();
    match last {
        Some(TermEvent::Closed(Some(e))) => {
            assert!(e.contains("closed the connection during login"), "{e}");
            assert!(e.contains("password login"), "{e}");
        }
        other => panic!("expected a failed session, got {other:?}"),
    }
    assert!(mock.wait_line(|l| l.starts_with("MOCK-TERM auth rejected for 'root@pam!pmxspan'"), Duration::from_secs(5)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn errors_are_specific() {
    let Some(mock) = Mock::start() else { return };
    let client = mock.client(AuthMode::Password).await;
    // A ticket that was never issued: the upgrade is refused with the status.
    let Err(err) = ws::connect(&client, "/nodes/pve1/qemu/100/vncwebsocket", 5900, "PVEVNC:bogus").await else {
        panic!("a bogus ticket was accepted")
    };
    let msg = format!("{err:#}");
    assert!(msg.contains("HTTP 401") && msg.contains("invalid vnc ticket"), "{msg}");
    // A VM without a serial port.
    let err = super::request_proxy(&client, &ConsoleTarget::VmSerial(vm(101), 0)).await.expect_err("no serial port");
    assert!(format!("{err:#}").contains("no serial port"), "{err:#}");
    // A stopped guest.
    let ct = GuestRef { node: "pve2".into(), kind: GuestKind::Lxc, vmid: 103 };
    let err = super::request_proxy(&client, &ConsoleTarget::Container(ct)).await.expect_err("not running");
    assert!(format!("{err:#}").contains("not running"), "{err:#}");
    // A VM serial console works where the port exists.
    let t = super::request_proxy(&client, &ConsoleTarget::VmSerial(vm(100), 0)).await.expect("serial0");
    assert!(t.ticket.starts_with("PVETERM:"));
}
