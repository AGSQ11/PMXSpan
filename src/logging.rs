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
use std::sync::{Arc, OnceLock};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;

const MEMORY_LINES: usize = 2000;

static MEMORY: OnceLock<Arc<Mutex<VecDeque<String>>>> = OnceLock::new();

pub fn recent_lines() -> Vec<String> {
    MEMORY.get().map(|m| m.lock().iter().cloned().collect()).unwrap_or_default()
}

pub fn clear_memory() {
    if let Some(m) = MEMORY.get() {
        m.lock().clear();
    }
}

#[derive(Clone)]
struct RedactingWriter {
    file: Option<Arc<Mutex<File>>>,
    memory: Arc<Mutex<VecDeque<String>>>,
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
        if self.buf.is_empty() {
            return;
        }
        let text = crate::util::redact(&String::from_utf8_lossy(&self.buf));
        if let Some(f) = &self.inner.file {
            let _ = f.lock().write_all(text.as_bytes());
        }
        let mut mem = self.inner.memory.lock();
        for line in text.lines() {
            if mem.len() >= MEMORY_LINES {
                mem.pop_front();
            }
            mem.push_back(line.to_string());
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
    let memory = MEMORY.get_or_init(|| Arc::new(Mutex::new(VecDeque::new()))).clone();
    let day = chrono::Local::now().format("%Y-%m-%d");
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.log_dir.join(format!("pmxspan-{day}.log")))
        .ok()
        .map(|f| Arc::new(Mutex::new(f)));
    prune_old_logs(paths, 14);

    // PXP_LOG is the variable's name from before the rename; still honoured.
    let filter = EnvFilter::try_from_env("PMXSPAN_LOG")
        .or_else(|_| EnvFilter::try_from_env("PXP_LOG"))
        .unwrap_or_else(|_| EnvFilter::new("info,pmxspan=debug,wgpu=warn,eframe=warn,egui=warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_target(false)
        .with_writer(RedactingWriter { file, memory })
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
        }
    }
}
