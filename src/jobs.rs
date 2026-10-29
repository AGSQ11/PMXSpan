//! Background jobs: multi-step operations (create, rebuild, migrate, ...) that
//! run on the runtime and report progress lines to the Jobs panel.
//!
//! Every job records each step it performs and the exact error it stopped on.

use parking_lot::Mutex;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobStatus {
    Running,
    Done,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

#[derive(Clone, Debug)]
pub struct JobLine {
    pub at: chrono::DateTime<chrono::Local>,
    pub level: Level,
    pub text: String,
}

pub struct JobState {
    pub status: JobStatus,
    pub lines: Vec<JobLine>,
    pub progress: Option<f32>,
    pub result: Option<String>,
    pub finished: Option<chrono::DateTime<chrono::Local>>,
}

pub struct Job {
    pub id: u64,
    pub title: String,
    pub started: chrono::DateTime<chrono::Local>,
    pub state: Mutex<JobState>,
}

/// Handle given to a running job for reporting.
#[derive(Clone)]
pub struct JobCtx {
    job: Arc<Job>,
}

impl JobCtx {
    fn push(&self, level: Level, text: impl Into<String>) {
        let text = crate::util::redact(&text.into());
        match level {
            Level::Info => tracing::info!("[job {}] {}", self.job.id, text),
            Level::Warn => tracing::warn!("[job {}] {}", self.job.id, text),
            Level::Error => tracing::error!("[job {}] {}", self.job.id, text),
        }
        self.job.state.lock().lines.push(JobLine { at: chrono::Local::now(), level, text });
        crate::rt::repaint();
    }
    pub fn info(&self, text: impl Into<String>) {
        self.push(Level::Info, text)
    }
    pub fn warn(&self, text: impl Into<String>) {
        self.push(Level::Warn, text)
    }
    pub fn progress(&self, p: f32) {
        self.job.state.lock().progress = Some(p.clamp(0.0, 1.0));
        crate::rt::repaint();
    }
}

#[derive(Clone, Default)]
pub struct Jobs {
    list: Arc<Mutex<Vec<Arc<Job>>>>,
}

impl Jobs {
    /// Starts a job. The future's `Ok` string is shown as the outcome.
    pub fn spawn<F, Fut>(&self, title: impl Into<String>, f: F) -> u64
    where
        F: FnOnce(JobCtx) -> Fut + Send + 'static,
        Fut: Future<Output = anyhow::Result<String>> + Send + 'static,
    {
        let job = Arc::new(Job {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            title: title.into(),
            started: chrono::Local::now(),
            state: Mutex::new(JobState { status: JobStatus::Running, lines: Vec::new(), progress: None, result: None, finished: None }),
        });
        let id = job.id;
        {
            let mut list = self.list.lock();
            list.push(job.clone());
            // Keep the panel bounded.
            if list.len() > 200 {
                let drop_n = list.len() - 200;
                let mut removed = 0;
                list.retain(|j| {
                    if removed < drop_n && j.state.lock().status != JobStatus::Running {
                        removed += 1;
                        false
                    } else {
                        true
                    }
                });
            }
        }
        tracing::info!("[job {}] started: {}", id, job.title);
        let ctx = JobCtx { job: job.clone() };
        crate::rt::spawn(async move {
            let res = f(ctx.clone()).await;
            {
                let mut st = job.state.lock();
                st.finished = Some(chrono::Local::now());
                match res {
                    Ok(msg) => {
                        st.status = JobStatus::Done;
                        st.progress = Some(1.0);
                        if !msg.is_empty() {
                            st.lines.push(JobLine { at: chrono::Local::now(), level: Level::Info, text: msg.clone() });
                        }
                        st.result = Some(msg);
                    }
                    Err(e) => {
                        let msg = crate::util::redact(&format!("{e:#}"));
                        tracing::error!("[job {}] failed: {}", job.id, msg);
                        st.status = JobStatus::Failed;
                        st.lines.push(JobLine { at: chrono::Local::now(), level: Level::Error, text: msg.clone() });
                        st.result = Some(msg);
                    }
                }
            }
            crate::rt::bump_generation();
        });
        id
    }

    pub fn snapshot(&self) -> Vec<Arc<Job>> {
        self.list.lock().clone()
    }

    pub fn running_count(&self) -> usize {
        self.list.lock().iter().filter(|j| j.state.lock().status == JobStatus::Running).count()
    }

    pub fn clear_finished(&self) {
        self.list.lock().retain(|j| j.state.lock().status == JobStatus::Running);
    }
}
