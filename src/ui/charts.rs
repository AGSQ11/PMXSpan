//! RRD performance graphs for guests and nodes.

use super::theme;
use crate::util::{ValueExt, fmt_bytes};
use egui_plot::{Legend, Line, Plot, PlotPoints};
use serde_json::Value;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum Timeframe {
    #[default]
    Hour,
    Day,
    Week,
    Month,
    Year,
}

impl Timeframe {
    pub const ALL: [Timeframe; 5] = [Timeframe::Hour, Timeframe::Day, Timeframe::Week, Timeframe::Month, Timeframe::Year];
    pub fn api(self) -> &'static str {
        match self {
            Timeframe::Hour => "hour",
            Timeframe::Day => "day",
            Timeframe::Week => "week",
            Timeframe::Month => "month",
            Timeframe::Year => "year",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Timeframe::Hour => "Hour",
            Timeframe::Day => "Day",
            Timeframe::Week => "Week",
            Timeframe::Month => "Month",
            Timeframe::Year => "Year",
        }
    }
}

pub fn timeframe_picker(ui: &mut egui::Ui, tf: &mut Timeframe) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        for t in Timeframe::ALL {
            if ui.selectable_label(*tf == t, t.label()).clicked() && *tf != t {
                *tf = t;
                changed = true;
            }
        }
    });
    changed
}

fn series(data: &[Value], key: &str, scale: f64) -> Vec<[f64; 2]> {
    data.iter().filter(|d| d.get(key).is_some()).map(|d| [d.f("time"), d.f(key) * scale]).collect()
}

fn time_fmt(x: f64) -> String {
    use chrono::TimeZone;
    match chrono::Local.timestamp_opt(x as i64, 0) {
        chrono::LocalResult::Single(t) => t.format("%m-%d %H:%M").to_string(),
        _ => String::new(),
    }
}

type Series<'a> = Vec<(&'a str, Vec<[f64; 2]>, egui::Color32)>;

fn chart(ui: &mut egui::Ui, id: &str, title: &str, lines: Series<'_>, bytes: bool) {
    ui.label(egui::RichText::new(title).strong());
    Plot::new(id)
        .height(150.0)
        .width(ui.available_width())
        .legend(Legend::default())
        .allow_scroll(false)
        .allow_zoom(false)
        .allow_drag(false)
        .include_y(0.0)
        // Reserve room for labels like "953.67 MiB" so they are never clipped.
        .y_axis_min_width(if bytes { 74.0 } else { 34.0 })
        // Power-of-two steps land on round KiB/MiB/GiB values and stay dense enough to label.
        .y_grid_spacer(egui_plot::log_grid_spacer(if bytes { 2 } else { 10 }))
        .x_axis_formatter(|m, _| time_fmt(m.value))
        .y_axis_formatter(move |m, _| if bytes { fmt_bytes(m.value) } else { format!("{:.0}", m.value) })
        .show(ui, |p| {
            for (name, pts, color) in lines {
                p.line(Line::new(name, PlotPoints::from(pts)).color(color));
            }
        });
}

/// Draws CPU, memory, network and disk IO (or load for nodes) charts from `rrddata`, two per row.
pub fn rrd_charts(ui: &mut egui::Ui, id: &str, data: &[Value], node: bool) {
    let (used, total) = if node { ("memused", "memtotal") } else { ("mem", "maxmem") };
    ui.columns(2, |cols| {
        chart(&mut cols[0], &format!("{id}-cpu"), "CPU usage (%)", vec![("CPU", series(data, "cpu", 100.0), theme::INFO)], false);
        chart(
            &mut cols[1],
            &format!("{id}-mem"),
            "Memory",
            vec![("used", series(data, used, 1.0), theme::OK), ("total", series(data, total, 1.0), theme::MUTED)],
            true,
        );
    });
    ui.add_space(6.0);
    ui.columns(2, |cols| {
        chart(
            &mut cols[0],
            &format!("{id}-net"),
            "Network (bytes/s)",
            vec![("in", series(data, "netin", 1.0), theme::OK), ("out", series(data, "netout", 1.0), theme::ACCENT)],
            true,
        );
        if node {
            chart(&mut cols[1], &format!("{id}-load"), "Load average", vec![("load", series(data, "loadavg", 1.0), theme::WARN)], false);
        } else {
            chart(
                &mut cols[1],
                &format!("{id}-disk"),
                "Disk IO (bytes/s)",
                vec![("read", series(data, "diskread", 1.0), theme::INFO), ("write", series(data, "diskwrite", 1.0), theme::WARN)],
                true,
            );
        }
    });
}
