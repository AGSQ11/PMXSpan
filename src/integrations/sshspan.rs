//! Finding and launching SSHSpan (contract section 1).
//!
//! Discovery never blocks the UI: it runs on a background thread when an
//! SSHSpan action is first drawn, and again after a minute or when the path
//! setting changes. Nothing here runs at startup or in `--run-schedules`.

use parking_lot::Mutex;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Shown on every disabled SSHSpan action.
pub const NOT_FOUND_HINT: &str = "SSHSpan was not found on this computer. Install it or set its path in Settings.";

/// How a SSHSpan executable was found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// The path set in Settings.
    UserSet,
    /// Found automatically (needs a one-time confirmation before launch).
    Detected(&'static str),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Discovery {
    pub exe: Option<(PathBuf, Source)>,
    /// `sshspan://` has an OS handler.
    pub scheme_registered: bool,
    /// The path set in Settings is unusable (then discovery falls back to the automatic search).
    pub user_path_error: Option<String>,
}

impl Discovery {
    pub fn available(&self) -> bool {
        self.exe.is_some() || self.scheme_registered
    }

    /// One line for Settings.
    pub fn describe(&self) -> String {
        match &self.exe {
            Some((p, Source::UserSet)) => format!("Found: {} (path set below)", p.display()),
            Some((p, Source::Detected(how))) => format!("Found: {} ({how})", p.display()),
            None if self.scheme_registered => "Found: the sshspan:// link handler registered with the system".into(),
            None => "Not found on this computer.".into(),
        }
    }
}

/// What the environment looks like; injected in tests.
#[derive(Clone, Debug, Default)]
pub struct Env {
    pub path_var: Option<OsString>,
    pub home: Option<PathBuf>,
    pub program_files: Option<PathBuf>,
    pub local_app_data: Option<PathBuf>,
    /// Install locations from the Windows uninstall registry.
    pub registry_dirs: Vec<PathBuf>,
    /// Fixed system locations (`/usr/bin`, ...); tests point these at a temp dir.
    pub system_dirs: Vec<PathBuf>,
    pub opt_dir: PathBuf,
    pub mac_apps: PathBuf,
}

impl Env {
    pub fn current() -> Self {
        let home = directories::UserDirs::new().map(|d| d.home_dir().to_path_buf());
        Self {
            path_var: std::env::var_os("PATH"),
            home,
            program_files: std::env::var_os("ProgramFiles").map(PathBuf::from),
            local_app_data: std::env::var_os("LOCALAPPDATA").map(PathBuf::from),
            registry_dirs: registry_install_dirs("SSHSpan"),
            system_dirs: vec![PathBuf::from("/usr/bin"), PathBuf::from("/usr/local/bin")],
            opt_dir: PathBuf::from("/opt"),
            mac_apps: PathBuf::from("/Applications"),
        }
    }
}

const EXE: &str = if cfg!(windows) { "sshspan.exe" } else { "sshspan" };

/// Checks a user-entered path: absolute, exists, regular file, `.exe` on Windows.
pub fn validate_user_path(raw: &str) -> Result<PathBuf, String> {
    let p = PathBuf::from(raw.trim());
    if !p.is_absolute() {
        return Err("use an absolute path".into());
    }
    let meta = std::fs::metadata(&p).map_err(|e| format!("cannot read it: {e}"))?;
    if !meta.is_file() {
        return Err("it is not a file".into());
    }
    if cfg!(windows) && !p.extension().is_some_and(|e| e.eq_ignore_ascii_case("exe")) {
        return Err("it must be an .exe file".into());
    }
    if !is_executable(&meta) {
        return Err("it is not executable".into());
    }
    Ok(p)
}

#[cfg(unix)]
fn is_executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_meta: &std::fs::Metadata) -> bool {
    true
}

fn usable(p: &Path) -> bool {
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && is_executable(&m))
}

/// The automatic search, in the contract's order.
pub fn candidates(env: &Env) -> Vec<(PathBuf, &'static str)> {
    let mut out: Vec<(PathBuf, &'static str)> = Vec::new();
    if cfg!(windows) {
        for d in &env.registry_dirs {
            out.push((d.join(EXE), "installed"));
        }
        if let Some(pf) = &env.program_files {
            out.push((pf.join("SSHSpan").join(EXE), "Program Files"));
        }
        if let Some(la) = &env.local_app_data {
            out.push((la.join("Programs").join("SSHSpan").join(EXE), "per-user install"));
        }
        if let Some(path) = &env.path_var {
            out.extend(std::env::split_paths(path).map(|d| (d.join(EXE), "on PATH")));
        }
    } else {
        if let Some(path) = &env.path_var {
            out.extend(std::env::split_paths(path).filter(|d| d.is_absolute()).map(|d| (d.join(EXE), "on PATH")));
        }
        for d in &env.system_dirs {
            out.push((d.join(EXE), "system location"));
        }
        if let Some(h) = &env.home {
            out.push((h.join(".local").join("bin").join(EXE), "~/.local/bin"));
        }
        out.push((env.opt_dir.join("SSHSpan").join(EXE), "/opt"));
        if let Some(h) = &env.home {
            // ~/Applications/SSHSpan*.AppImage, newest name last so it wins on sort.
            let dir = h.join("Applications");
            let mut images: Vec<PathBuf> = std::fs::read_dir(&dir)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("SSHSpan") && n.ends_with(".AppImage")))
                .collect();
            images.sort();
            images.reverse();
            out.extend(images.into_iter().map(|p| (p, "AppImage")));
        }
        out.push((env.mac_apps.join("SSHSpan.app").join("Contents").join("MacOS").join(EXE), "application bundle"));
    }
    out
}

/// Runs discovery against `env` (no scheme probe: see [`scheme_registered`]).
pub fn discover_in(user_path: &str, env: &Env) -> Discovery {
    let mut d = Discovery::default();
    if !user_path.trim().is_empty() {
        match validate_user_path(user_path) {
            Ok(p) => {
                d.exe = Some((p, Source::UserSet));
                return d;
            }
            Err(e) => d.user_path_error = Some(e),
        }
    }
    d.exe = candidates(env).into_iter().find(|(p, _)| usable(p)).map(|(p, how)| (p, Source::Detected(how)));
    d
}

// ---- scheme probe -----------------------------------------------------------

/// Whether the OS has a handler for `sshspan://`.
pub fn scheme_registered() -> bool {
    probe_scheme("sshspan")
}

#[cfg(all(unix, not(target_os = "macos")))]
fn probe_scheme(scheme: &str) -> bool {
    std::process::Command::new("xdg-mime")
        .args(["query", "default", &format!("x-scheme-handler/{scheme}")])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .is_ok_and(|o| o.status.success() && !String::from_utf8_lossy(&o.stdout).trim().is_empty())
}

#[cfg(target_os = "macos")]
fn probe_scheme(_scheme: &str) -> bool {
    // No cheap, reliable probe: rely on finding the application bundle.
    false
}

#[cfg(windows)]
fn probe_scheme(scheme: &str) -> bool {
    use windows_sys::Win32::System::Registry::{HKEY_CLASSES_ROOT, HKEY_CURRENT_USER};
    let user = format!("Software\\Classes\\{scheme}");
    winreg::has_value(HKEY_CURRENT_USER, &user, "URL Protocol") || winreg::has_value(HKEY_CLASSES_ROOT, scheme, "URL Protocol")
}

/// Install directories of `display_name` from the uninstall registry keys.
#[cfg(windows)]
pub fn registry_install_dirs(display_name: &str) -> Vec<PathBuf> {
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    let base = "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall";
    let mut out = Vec::new();
    for root in [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
        for sub in winreg::subkeys(root, base) {
            let key = format!("{base}\\{sub}");
            if winreg::string(root, &key, "DisplayName").as_deref() != Some(display_name) {
                continue;
            }
            if let Some(loc) = winreg::string(root, &key, "InstallLocation").filter(|s| !s.trim().is_empty()) {
                out.push(PathBuf::from(loc.trim().trim_matches('"')));
            } else if let Some(icon) = winreg::string(root, &key, "DisplayIcon") {
                // "C:\...\sshspan.exe,0"
                let icon = icon.split(',').next().unwrap_or("").trim().trim_matches('"').to_string();
                if let Some(dir) = Path::new(&icon).parent() {
                    out.push(dir.to_path_buf());
                }
            }
        }
    }
    out
}

#[cfg(not(windows))]
pub fn registry_install_dirs(_display_name: &str) -> Vec<PathBuf> {
    Vec::new()
}

/// Minimal registry reads.
#[cfg(windows)]
mod winreg {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        HKEY, KEY_READ, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ, RegCloseKey, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW,
    };

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn string(root: HKEY, sub: &str, value: &str) -> Option<String> {
        let (sub, value) = (wide(sub), wide(value));
        let mut buf = vec![0u16; 1024];
        let mut len = (buf.len() * 2) as u32;
        // SAFETY: valid NUL-terminated strings and a buffer of `len` bytes.
        let rc = unsafe {
            RegGetValueW(
                root,
                sub.as_ptr(),
                value.as_ptr(),
                RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ,
                std::ptr::null_mut(),
                buf.as_mut_ptr().cast(),
                &mut len,
            )
        };
        if rc != ERROR_SUCCESS {
            return None;
        }
        let n = (len as usize / 2).min(buf.len());
        let s = String::from_utf16_lossy(&buf[..n]);
        Some(s.trim_end_matches('\0').to_string())
    }

    pub fn has_value(root: HKEY, sub: &str, value: &str) -> bool {
        let (sub, value) = (wide(sub), wide(value));
        let mut len = 0u32;
        // SAFETY: valid NUL-terminated strings; a null buffer only queries the size.
        let rc = unsafe {
            RegGetValueW(
                root,
                sub.as_ptr(),
                value.as_ptr(),
                windows_sys::Win32::System::Registry::RRF_RT_ANY,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut len,
            )
        };
        rc == ERROR_SUCCESS
    }

    pub fn subkeys(root: HKEY, sub: &str) -> Vec<String> {
        let sub = wide(sub);
        let mut key: HKEY = std::ptr::null_mut();
        // SAFETY: valid string, out pointer to a HKEY.
        if unsafe { RegOpenKeyExW(root, sub.as_ptr(), 0, KEY_READ, &mut key) } != ERROR_SUCCESS {
            return Vec::new();
        }
        let mut out = Vec::new();
        for i in 0..4096u32 {
            let mut name = vec![0u16; 256];
            let mut len = name.len() as u32;
            // SAFETY: `key` is open; `name` holds `len` UTF-16 units.
            let rc = unsafe {
                RegEnumKeyExW(
                    key,
                    i,
                    name.as_mut_ptr(),
                    &mut len,
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            if rc != ERROR_SUCCESS {
                break;
            }
            out.push(String::from_utf16_lossy(&name[..len as usize]));
        }
        // SAFETY: `key` was opened above.
        unsafe { RegCloseKey(key) };
        out
    }
}

// ---- shared status ------------------------------------------------------------

#[derive(Default)]
struct State {
    discovery: Option<Discovery>,
    checked: Option<Instant>,
    for_path: String,
    running: bool,
}

fn state() -> &'static Mutex<State> {
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(Default::default)
}

const RECHECK: Duration = Duration::from_secs(60);

/// The latest discovery result (None until the first check finished).
/// Starts a background check when there is none, it is a minute old, or the
/// path setting changed.
pub fn status(user_path: &str) -> Option<Discovery> {
    let mut st = state().lock();
    let stale = st.checked.is_none_or(|t| t.elapsed() >= RECHECK) || st.for_path != user_path;
    if stale && !st.running {
        st.running = true;
        st.for_path = user_path.to_string();
        let path = user_path.to_string();
        let spawned = std::thread::Builder::new().name("sshspan-discovery".into()).spawn(move || {
            let mut d = discover_in(&path, &Env::current());
            d.scheme_registered = scheme_registered();
            let mut st = state().lock();
            st.discovery = Some(d);
            st.checked = Some(Instant::now());
            st.running = false;
            drop(st);
            crate::rt::repaint();
        });
        if spawned.is_err() {
            st.running = false;
        }
    }
    st.discovery.clone()
}

/// Forces the next [`status`] call to search again.
pub fn recheck() {
    state().lock().checked = None;
}

// ---- launching ----------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Plan {
    /// Spawn this executable with the URL as its single argument.
    Exe(PathBuf),
    /// Found automatically and not confirmed yet: ask, then remember it.
    Confirm(PathBuf),
    /// Ask the OS to open the URL (a handler is registered).
    OsOpen,
    Unavailable,
}

/// Picks how to deliver a URL, in the contract's order.
pub fn plan(d: &Discovery, approved_path: &str) -> Plan {
    match &d.exe {
        Some((p, Source::UserSet)) => Plan::Exe(p.clone()),
        Some((p, Source::Detected(_))) if !approved_path.is_empty() && Path::new(approved_path) == p => Plan::Exe(p.clone()),
        Some((p, Source::Detected(_))) => Plan::Confirm(p.clone()),
        None if d.scheme_registered => Plan::OsOpen,
        None => Plan::Unavailable,
    }
}

/// Starts SSHSpan with `url` as its single argument (no shell involved).
pub fn spawn_exe(exe: &Path, url: &str) -> std::io::Result<()> {
    let mut child = std::process::Command::new(exe)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    // Reap it when it exits so no zombie is left behind.
    let _ = std::thread::Builder::new().name("sshspan-child".into()).spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Opens `url` with the OS handler for its scheme.
pub fn os_open(url: &str) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::Shell::ShellExecuteW;
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        let op: Vec<u16> = "open\0".encode_utf16().collect();
        let target: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: NUL-terminated wide strings; no window handle needed.
        let rc =
            unsafe { ShellExecuteW(std::ptr::null_mut(), op.as_ptr(), target.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL) };
        // ShellExecute reports success with a value above 32.
        if rc as isize <= 32 {
            return Err(std::io::Error::other(format!("ShellExecute failed ({})", rc as isize)));
        }
        Ok(())
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(url).spawn().map(|_| ())
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let mut child = std::process::Command::new("xdg-open")
            .arg(url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        let _ = std::thread::Builder::new().spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pmxspan-disc-{tag}-{}", crate::config::new_id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn fake_exe(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    fn env_in(root: &Path, path_dirs: &[PathBuf]) -> Env {
        Env {
            path_var: Some(std::env::join_paths(path_dirs).unwrap()),
            home: Some(root.join("home")),
            program_files: Some(root.join("pf")),
            local_app_data: Some(root.join("lad")),
            registry_dirs: vec![],
            system_dirs: vec![root.join("usr-bin")],
            opt_dir: root.join("opt"),
            mac_apps: root.join("Applications"),
        }
    }

    #[test]
    fn finds_sshspan_on_an_injected_path() {
        let root = temp("path");
        let (a, b) = (root.join("a"), root.join("b"));
        let env = env_in(&root, &[a.clone(), b.clone()]);
        assert_eq!(discover_in("", &env).exe, None);
        assert!(!discover_in("", &env).available());

        fake_exe(&b.join(EXE));
        let d = discover_in("", &env);
        assert_eq!(d.exe, Some((b.join(EXE), Source::Detected("on PATH"))));
        // PATH order wins.
        fake_exe(&a.join(EXE));
        assert_eq!(discover_in("", &env).exe.unwrap().0, a.join(EXE));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn checks_fixed_locations_and_appimages() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp("fixed");
        let env = env_in(&root, &[]);
        // A non-executable file is skipped.
        let local = root.join("home/.local/bin").join(EXE);
        fake_exe(&local);
        std::fs::set_permissions(&local, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(discover_in("", &env).exe, None);
        let img = root.join("home/Applications/SSHSpan-1.2.0-x86_64.AppImage");
        fake_exe(&img);
        assert_eq!(discover_in("", &env).exe, Some((img.clone(), Source::Detected("AppImage"))));
        fake_exe(&root.join("opt/SSHSpan").join(EXE));
        assert_eq!(discover_in("", &env).exe.unwrap().1, Source::Detected("/opt"));
        fake_exe(&root.join("usr-bin").join(EXE));
        assert_eq!(discover_in("", &env).exe.unwrap().1, Source::Detected("system location"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn user_path_wins_and_is_validated() {
        let root = temp("user");
        let on_path = root.join("p");
        fake_exe(&on_path.join(EXE));
        let env = env_in(&root, std::slice::from_ref(&on_path));
        let mine = root.join("custom").join(if cfg!(windows) { "ssh-span.exe" } else { "ssh-span" });
        fake_exe(&mine);
        let d = discover_in(mine.to_str().unwrap(), &env);
        assert_eq!(d.exe, Some((mine.clone(), Source::UserSet)));
        assert_eq!(plan(&d, ""), Plan::Exe(mine.clone()));

        // Unusable user paths are reported and the search falls back.
        for bad in ["relative/sshspan", root.join("missing").to_str().unwrap(), root.to_str().unwrap()] {
            let d = discover_in(bad, &env);
            assert!(d.user_path_error.is_some(), "{bad}");
            assert_eq!(d.exe.as_ref().unwrap().0, on_path.join(EXE));
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn plans_launches_in_order() {
        let p = PathBuf::from(if cfg!(windows) { r"C:\x\sshspan.exe" } else { "/usr/bin/sshspan" });
        let found = Discovery { exe: Some((p.clone(), Source::Detected("on PATH"))), scheme_registered: true, user_path_error: None };
        // A detected path needs confirmation once, then launches directly.
        assert_eq!(plan(&found, ""), Plan::Confirm(p.clone()));
        assert_eq!(plan(&found, "/somewhere/else"), Plan::Confirm(p.clone()));
        assert_eq!(plan(&found, p.to_str().unwrap()), Plan::Exe(p.clone()));
        let scheme_only = Discovery { exe: None, scheme_registered: true, user_path_error: None };
        assert_eq!(plan(&scheme_only, ""), Plan::OsOpen);
        assert_eq!(plan(&Discovery::default(), ""), Plan::Unavailable);
        assert!(scheme_only.available() && !Discovery::default().available());
    }
}
