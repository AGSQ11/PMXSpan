//! File + in-memory logging with credential redaction.
//!
//! Every log line passes through [`crate::util::redact`] before it is written.
//! The most recent lines
//! are kept in memory for the in-app log viewer.

use crate::config::AppPaths;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;

const MEMORY_LINES: usize = 2000;
/// Retention is bounded in bytes too: one event can no longer be arbitrarily large.
const MEMORY_BYTES: usize = 4 * 1024 * 1024;
/// Longest event kept, in bytes; the rest is cut off before redaction.
const MAX_EVENT_BYTES: usize = 16 * 1024;
/// A day's log file stops growing here (the next day starts a new file).
const MAX_LOG_FILE_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Default)]
struct Ring {
    lines: VecDeque<String>,
    bytes: usize,
}

impl Ring {
    fn push(&mut self, line: String) {
        self.bytes += line.len();
        self.lines.push_back(line);
        while self.lines.len() > MEMORY_LINES || (self.bytes > MEMORY_BYTES && self.lines.len() > 1) {
            if let Some(old) = self.lines.pop_front() {
                self.bytes -= old.len();
            }
        }
    }
}

static MEMORY: OnceLock<Arc<Mutex<Ring>>> = OnceLock::new();

pub fn recent_lines() -> Vec<String> {
    MEMORY.get().map(|m| m.lock().lines.iter().cloned().collect()).unwrap_or_default()
}

pub fn clear_memory() {
    if let Some(m) = MEMORY.get() {
        *m.lock() = Ring::default();
    }
}

#[derive(Clone)]
struct RedactingWriter {
    file: Option<Arc<Mutex<File>>>,
    /// Bytes written to the file so far (starting at its length).
    file_bytes: Arc<AtomicU64>,
    memory: Arc<Mutex<Ring>>,
}

impl RedactingWriter {
    /// Clips, redacts and stores one event.
    fn emit(&self, raw: &[u8]) {
        let mut raw = String::from_utf8_lossy(raw).into_owned();
        if raw.len() > MAX_EVENT_BYTES {
            let mut cut = MAX_EVENT_BYTES;
            while !raw.is_char_boundary(cut) {
                cut -= 1;
            }
            raw.truncate(cut);
            raw.push_str("... (event truncated)\n");
        }
        let text = crate::util::redact(&raw);
        if let Some(f) = &self.file
            && self.file_bytes.load(Ordering::Relaxed) < MAX_LOG_FILE_BYTES
            && f.lock().write_all(text.as_bytes()).is_ok()
        {
            self.file_bytes.fetch_add(text.len() as u64, Ordering::Relaxed);
        }
        let mut mem = self.memory.lock();
        for line in text.lines() {
            mem.push(line.to_string());
        }
    }
}

struct LineWriter {
    inner: RedactingWriter,
    buf: Vec<u8>,
}

impl Write for LineWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for LineWriter {
    fn drop(&mut self) {
        if !self.buf.is_empty() {
            self.inner.emit(&self.buf);
        }
    }
}

impl<'a> MakeWriter<'a> for RedactingWriter {
    type Writer = LineWriter;
    fn make_writer(&'a self) -> Self::Writer {
        LineWriter { inner: self.clone(), buf: Vec::with_capacity(256) }
    }
}

pub fn init(paths: &AppPaths) {
    let memory = MEMORY.get_or_init(|| Arc::new(Mutex::new(Ring::default()))).clone();
    let day = chrono::Local::now().format("%Y-%m-%d");
    let path = paths.log_dir.join(format!("pmxspan-{day}.log"));
    let mut open = OpenOptions::new();
    open.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        open.mode(0o600);
        // Logs from older versions were created with the default umask.
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    let file = open.open(&path).ok().map(|f| Arc::new(Mutex::new(f)));
    let file_bytes = Arc::new(AtomicU64::new(std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0)));
    prune_old_logs(paths, 14);

    // PXP_LOG is the variable's name from before the rename; still honoured.
    let filter = EnvFilter::try_from_env("PMXSPAN_LOG")
        .or_else(|_| EnvFilter::try_from_env("PXP_LOG"))
        .unwrap_or_else(|_| EnvFilter::new("info,pmxspan=debug,wgpu=warn,eframe=warn,egui=warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_target(false)
        .with_writer(RedactingWriter { file, file_bytes, memory })
        .try_init();
}

fn prune_old_logs(paths: &AppPaths, keep_days: u64) {
    let Ok(rd) = std::fs::read_dir(&paths.log_dir) else { return };
    let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(keep_days * 86400);
    for entry in rd.flatten() {
        if let Ok(meta) = entry.metadata()
            && meta.modified().map(|m| m < cutoff).unwrap_or(false)
        {
            let _ = std::fs::remove_file(entry.path());
        } else {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(entry.path(), std::fs::Permissions::from_mode(0o600));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn writer(file: Option<Arc<Mutex<File>>>, start: u64) -> RedactingWriter {
        RedactingWriter { file, file_bytes: Arc::new(AtomicU64::new(start)), memory: Arc::new(Mutex::new(Ring::default())) }
    }

    #[test]
    fn huge_events_are_clipped_and_the_ring_is_bounded_in_bytes() {
        let w = writer(None, 0);
        w.emit(format!("MCP action x requested, arguments {}\n", "A".repeat(1 << 20)).as_bytes());
        {
            let m = w.memory.lock();
            assert!(m.bytes <= MAX_EVENT_BYTES + 64, "{} bytes retained for one event", m.bytes);
            assert!(m.lines[0].ends_with("(event truncated)"), "{}", &m.lines[0][m.lines[0].len() - 40..]);
        }
        // Many large lines stay under the byte budget.
        for _ in 0..2000 {
            w.emit(format!("{}\n", "B".repeat(MAX_EVENT_BYTES - 100)).as_bytes());
        }
        let m = w.memory.lock();
        assert!(m.bytes <= MEMORY_BYTES, "{} > {MEMORY_BYTES}", m.bytes);
        assert!(m.lines.len() < MEMORY_LINES);
    }

    #[test]
    fn log_file_stops_at_its_cap() {
        let path = std::env::temp_dir().join(format!("pmxspan-log-{}.log", crate::config::new_id()));
        let f = OpenOptions::new().create(true).append(true).open(&path).unwrap();
        let w = writer(Some(Arc::new(Mutex::new(f))), MAX_LOG_FILE_BYTES - 10);
        w.emit(b"first line that crosses the cap\n");
        let len = std::fs::metadata(&path).unwrap().len();
        w.emit(b"second line must be dropped\n");
        assert_eq!(std::fs::metadata(&path).unwrap().len(), len);
        let _ = std::fs::remove_file(&path);
    }
}
