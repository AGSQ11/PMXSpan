//! Cluster task log with per-task log viewer and stop button.

use super::{Actions, Shared, theme, widgets};
use crate::api::Params;
use crate::api::types::TaskInfo;
use crate::rt::Fetch;
use crate::util::{ValueExt, fmt_epoch};
use egui::RichText;

#[derive(Default)]
pub struct TasksView {
    list: Fetch<Vec<TaskInfo>>,
    filter: String,
    errors_only: bool,
    running_only: bool,
}

impl TasksView {
    pub fn ui(&mut self, ui: &mut egui::Ui, sh: &Shared, _actions: &mut Actions) {
        if self.list.is_idle() || (self.list.age_secs() > 5.0 && !self.list.is_loading()) {
            self.list = sh.fetch(|c| async move {
                let mut v: Vec<TaskInfo> = c.get_list("/cluster/tasks").await?.iter().map(TaskInfo::from_value).collect();
                v.sort_by_key(|t| std::cmp::Reverse(t.starttime));
                Ok(v)
            });
        }
        ui.horizontal(|ui| {
            widgets::heading(ui, "Tasks");
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("filter by node, user, type, VMID…"));
            ui.checkbox(&mut self.errors_only, "errors only");
            ui.checkbox(&mut self.running_only, "running only");
        });
        let f = self.filter.to_lowercase();
        let (eo, ro) = (self.errors_only, self.running_only);
        widgets::fetch_ui(ui, &mut self.list, |ui, tasks| {
            let rows: Vec<TaskInfo> = tasks
                .iter()
                .filter(|t| !eo || (!t.running() && !t.ok()))
                .filter(|t| !ro || t.running())
                .filter(|t| {
                    f.is_empty() || format!("{} {} {} {} {}", t.node, t.user, t.task_type, t.id, t.describe()).to_lowercase().contains(&f)
                })
                .cloned()
                .collect();
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| task_table(ui, sh, &rows, "cluster-tasks"));
        });
    }
}

/// Table of tasks; each row expands to show its log.
pub fn task_table(ui: &mut egui::Ui, sh: &Shared, tasks: &[TaskInfo], id: &str) {
    if tasks.is_empty() {
        widgets::muted(ui, "No tasks.");
        return;
    }
    for t in tasks {
        let (icon, color) = if t.running() {
            ("⏳", theme::INFO)
        } else if t.ok() {
            ("✔", theme::OK)
        } else {
            ("✖", theme::ERR)
        };
        let header = RichText::new(format!(
            "{icon}  {}   {}   {}   {}   {}",
            fmt_epoch(t.starttime),
            t.node,
            t.describe(),
            t.user,
            if t.running() { "running".to_string() } else { t.status.clone() }
        ))
        .color(color);
        egui::CollapsingHeader::new(header).id_salt(format!("{id}-{}", t.upid)).show(ui, |ui| {
            task_log(ui, sh, t);
        });
    }
}

fn task_log(ui: &mut egui::Ui, sh: &Shared, t: &TaskInfo) {
    let mem_id = egui::Id::unique(("tasklog", t.upid.clone()));
    let mut fetch: Fetch<Vec<String>> = ui
        .data_mut(|d| d.remove_temp::<std::sync::Arc<parking_lot::Mutex<Fetch<Vec<String>>>>>(mem_id))
        .map(|a| std::mem::take(&mut *a.lock()))
        .unwrap_or_default();
    if fetch.is_idle() || (t.running() && fetch.age_secs() > 2.0 && !fetch.is_loading()) {
        let (node, upid) = (t.node.clone(), t.upid.clone());
        fetch = sh.fetch(move |c| async move {
            let data = c
                .get_q(
                    &format!("/nodes/{node}/tasks/{}/log", urlencoding::encode(&upid)),
                    &Params::new().set("start", 0).set("limit", 5000),
                )
                .await?;
            Ok(data.arr().iter().map(|l| l.s("t")).collect())
        });
    }
    ui.horizontal(|ui| {
        ui.label(RichText::new(&t.upid).monospace().small().color(theme::MUTED));
        if t.running() && ui.small_button(RichText::new("Stop task").color(theme::ERR)).clicked() {
            let (c, node, upid) = (sh.client.clone(), t.node.clone(), t.upid.clone());
            sh.spawn(format!("Stop task {}", t.describe()), move |_ctx| async move {
                c.delete(&format!("/nodes/{node}/tasks/{}", urlencoding::encode(&upid)), &Params::new()).await?;
                Ok("task stop requested".into())
            });
        }
    });
    match fetch.poll() {
        Some(Ok(lines)) => {
            egui::ScrollArea::vertical().id_salt(("tl", &t.upid)).max_height(260.0).stick_to_bottom(true).show(ui, |ui| {
                for l in lines {
                    ui.label(RichText::new(l).monospace().size(11.5));
                }
            });
        }
        Some(Err(e)) => widgets::error_box(ui, e),
        None => {
            ui.spinner();
        }
    }
    ui.data_mut(|d| d.insert_temp(mem_id, std::sync::Arc::new(parking_lot::Mutex::new(fetch))));
}
