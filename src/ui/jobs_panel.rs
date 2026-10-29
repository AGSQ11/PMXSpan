//! Bottom panel listing background jobs and their step-by-step logs.

use super::{theme, widgets};
use crate::jobs::{JobStatus, Jobs, Level};
use egui::RichText;

#[derive(Default)]
pub struct JobsPanel {
    pub open: bool,
    selected: Option<u64>,
    newest_seen: u64,
}

impl JobsPanel {
    pub fn ui(&mut self, ui: &mut egui::Ui, jobs: &Jobs) {
        let list = jobs.snapshot();
        ui.horizontal(|ui| {
            ui.label(theme::bold("Jobs", 14.0));
            if ui.small_button("Clear finished").clicked() {
                jobs.clear_finished();
                self.selected = None;
            }
            if ui.small_button("Close").clicked() {
                self.open = false;
            }
        });
        if list.is_empty() {
            widgets::muted(ui, "No jobs yet. Long operations (create, rebuild, backups, migrations…) appear here with a full step log.");
            return;
        }
        // Follow the newest job whenever one is started.
        let newest = list.last().map(|j| j.id).unwrap_or(0);
        if self.selected.is_none() || newest > self.newest_seen {
            self.selected = list.last().map(|j| j.id);
            self.newest_seen = newest;
        }
        ui.horizontal_top(|ui| {
            egui::ScrollArea::vertical().id_salt("jobs-list").max_width(360.0).auto_shrink([false, false]).show(ui, |ui| {
                ui.set_width(350.0);
                ui.vertical(|ui| {
                    for job in list.iter().rev() {
                        let st = job.state.lock();
                        let (icon, color) = match st.status {
                            JobStatus::Running => (theme::icon::CLOCK, theme::info()),
                            JobStatus::Done => (theme::icon::CHECK, theme::ok()),
                            JobStatus::Failed => (theme::icon::CLOSE, theme::err()),
                        };
                        let text = RichText::new(format!("{icon} {}  {}", job.started.format("%H:%M:%S"), job.title)).color(color);
                        if ui.selectable_label(self.selected == Some(job.id), text).clicked() {
                            self.selected = Some(job.id);
                        }
                        if st.status == JobStatus::Running
                            && let Some(p) = st.progress
                        {
                            ui.add(egui::ProgressBar::new(p).desired_height(4.0));
                        }
                    }
                });
            });
            ui.separator();
            egui::ScrollArea::vertical().id_salt("jobs-log").stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
                ui.vertical(|ui| {
                    let Some(job) = list.iter().find(|j| Some(j.id) == self.selected) else { return };
                    let st = job.state.lock();
                    ui.label(theme::bold(&job.title, 14.0));
                    for l in &st.lines {
                        let color = match l.level {
                            Level::Info => ui.visuals().text_color(),
                            Level::Warn => theme::warn(),
                            Level::Error => theme::err(),
                        };
                        ui.label(RichText::new(format!("{}  {}", l.at.format("%H:%M:%S"), l.text)).monospace().size(12.0).color(color));
                    }
                    if st.status == JobStatus::Running {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            widgets::muted(ui, "running…");
                        });
                    }
                });
            });
        });
    }
}
