//! Bridge between the immediate-mode UI thread and the async tokio runtime.
//!
//! The UI never blocks on the network: every API call is spawned onto the
//! runtime and its result is delivered back through a [`Fetch`] handle that the
//! UI polls each frame. Finished work requests a repaint so results show up
//! without user input.

use std::future::Future;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::Instant;

static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
static EGUI_CTX: OnceLock<egui::Context> = OnceLock::new();

/// Bumped whenever a background job finishes, so views know cached data may be stale.
static GENERATION: AtomicU64 = AtomicU64::new(0);

pub fn init(ctx: egui::Context) {
    let _ = EGUI_CTX.set(ctx);
    runtime();
}

pub fn runtime() -> &'static tokio::runtime::Runtime {
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .thread_name("pxp-worker")
            .build()
            .expect("failed to start tokio runtime")
    })
}

pub fn repaint() {
    if let Some(ctx) = EGUI_CTX.get() {
        ctx.request_repaint();
    }
}

pub fn generation() -> u64 {
    GENERATION.load(Ordering::Relaxed)
}

pub fn bump_generation() {
    GENERATION.fetch_add(1, Ordering::Relaxed);
    repaint();
}

/// Fire-and-forget spawn.
pub fn spawn<F>(fut: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    runtime().spawn(fut);
}

/// A value being produced in the background.
pub struct Fetch<T> {
    rx: Option<mpsc::Receiver<Result<T, String>>>,
    value: Option<Result<T, String>>,
    started: Instant,
    generation: u64,
}

impl<T> Default for Fetch<T> {
    fn default() -> Self {
        Self { rx: None, value: None, started: Instant::now(), generation: 0 }
    }
}

impl<T: Send + 'static> Fetch<T> {
    pub fn spawn<F>(fut: F) -> Self
    where
        F: Future<Output = anyhow::Result<T>> + Send + 'static,
    {
        let (tx, rx) = mpsc::channel();
        runtime().spawn(async move {
            let res = fut.await.map_err(|e| format!("{e:#}"));
            let _ = tx.send(res);
            repaint();
        });
        Self { rx: Some(rx), value: None, started: Instant::now(), generation: generation() }
    }

    /// A fetch that is already resolved.
    pub fn ready(value: T) -> Self {
        Self { rx: None, value: Some(Ok(value)), started: Instant::now(), generation: generation() }
    }

    fn pump(&mut self) {
        if let Some(rx) = &self.rx {
            match rx.try_recv() {
                Ok(v) => {
                    self.value = Some(v);
                    self.rx = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.value = Some(Err("background task was cancelled".into()));
                    self.rx = None;
                }
            }
        }
    }

    pub fn poll(&mut self) -> Option<&Result<T, String>> {
        self.pump();
        self.value.as_ref()
    }

    pub fn poll_mut(&mut self) -> Option<&mut Result<T, String>> {
        self.pump();
        self.value.as_mut()
    }

    /// Takes the finished value out, leaving the fetch idle.
    pub fn take(&mut self) -> Option<Result<T, String>> {
        self.pump();
        self.value.take()
    }

    pub fn ok(&mut self) -> Option<&T> {
        self.poll().and_then(|r| r.as_ref().ok())
    }

    pub fn is_loading(&mut self) -> bool {
        self.pump();
        self.rx.is_some()
    }

    pub fn is_idle(&mut self) -> bool {
        self.pump();
        self.rx.is_none() && self.value.is_none()
    }

    pub fn age_secs(&self) -> f32 {
        self.started.elapsed().as_secs_f32()
    }

    /// True when a job finished after this fetch started (data may be stale).
    pub fn is_stale(&self) -> bool {
        self.generation != generation()
    }
}
