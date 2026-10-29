//! Summary tab: live status, IPs, traffic for the period, and graphs.

use super::GCtx;
use crate::api::types::GuestKind;
use crate::ops::bandwidth::{self, Traffic};
use crate::rt::Fetch;
use crate::ui::charts::{self, Timeframe};
use crate::ui::{Actions, theme, widgets};
use crate::util::{ValueExt, fmt_bytes, fmt_bytes_si, fmt_duration, fmt_epoch, pct};
use egui::RichText;
use serde_json::Value;

#[derive(Default)]
pub struct SummaryTab {
    status: Fetch<Value>,
    rrd: Fetch<Vec<Value>>,
    tf: Timeframe,
    traffic: Fetch<Traffic>,
    traffic_since: Option<i64>,
    ips: Fetch<Vec<(String, Vec<String>)>>,
}

impl SummaryTab {
    pub fn ui(&mut self, ui: &mut egui::Ui, gc: &GCtx, _actions: &mut Actions) {
        let g = gc.g.clone();
        if self.status.is_idle() || (self.status.age_secs() > 5.0 && !self.status.is_loading()) {
            let gr = g.clone();
            self.status = gc.sh.fetch(move |c| async move { crate::ops::guest::status(&c, &gr).await });
        }
        if self.rrd.is_idle() || (self.rrd.age_secs() > 60.0 && !self.rrd.is_loading()) {
            let (gr, tf) = (g.clone(), self.tf);
            self.rrd =
                gc.sh.fetch(move |c| async move { c.get_list(&format!("{}/rrddata?timeframe={}&cf=AVERAGE", gr.path(), tf.api())).await });
        }
        if self.traffic.is_idle() {
            let since = *self.traffic_since.get_or_insert_with(bandwidth::month_start);
            let gr = g.clone();
            self.traffic = gc.sh.fetch(move |c| async move { bandwidth::guest_traffic(&c, &gr, since).await });
        }
        if gc.running() && (self.ips.is_idle() || (self.ips.age_secs() > 30.0 && !self.ips.is_loading())) {
            let gr = g.clone();
            self.ips = gc.sh.fetch(move |c| async move { guest_ips(&c, &gr).await });
        }

        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(420.0);
                widgets::section(ui, "Status");
                let st = self.status.ok().cloned();
                egui::Grid::new("sum-grid").num_columns(2).spacing([12.0, 5.0]).show(ui, |ui| {
                    let r = gc.res;
                    widgets::kv(ui, "Node", &gc.g.node);
                    if let Some(st) = &st {
                        ui.label(RichText::new("CPU").color(theme::MUTED));
                        widgets::usage_bar(
                            ui,
                            (st.f("cpu") as f32).min(1.0),
                            format!("{:.1}% of {} cores", st.f("cpu") * 100.0, st.f("cpus")),
                            260.0,
                        );
                        ui.end_row();
                        ui.label(RichText::new("Memory").color(theme::MUTED));
                        widgets::usage_bar(
                            ui,
                            pct(st.f("mem"), st.f("maxmem")),
                            format!("{} / {}", fmt_bytes(st.f("mem")), fmt_bytes(st.f("maxmem"))),
                            260.0,
                        );
                        ui.end_row();
                        if gc.g.kind == GuestKind::Lxc {
                            ui.label(RichText::new("Root disk").color(theme::MUTED));
                            widgets::usage_bar(
                                ui,
                                pct(st.f("disk"), st.f("maxdisk")),
                                format!("{} / {}", fmt_bytes(st.f("disk")), fmt_bytes(st.f("maxdisk"))),
                                260.0,
                            );
                            ui.end_row();
                            if st.f("maxswap") > 0.0 {
                                ui.label(RichText::new("Swap").color(theme::MUTED));
                                widgets::usage_bar(
                                    ui,
                                    pct(st.f("swap"), st.f("maxswap")),
                                    format!("{} / {}", fmt_bytes(st.f("swap")), fmt_bytes(st.f("maxswap"))),
                                    260.0,
                                );
                                ui.end_row();
                            }
                        } else {
                            widgets::kv(ui, "Boot disk size", fmt_bytes(st.f("maxdisk")));
                        }
                        widgets::kv(ui, "Uptime", fmt_duration(st.u("uptime")));
                        if gc.g.kind == GuestKind::Qemu {
                            let qmp = st.s("qmpstatus");
                            if !qmp.is_empty() {
                                widgets::kv(ui, "QMP status", qmp);
                            }
                            let agent = st.get("agent").map(|_| st.b("agent")).unwrap_or(false);
                            widgets::kv(ui, "Guest agent", if agent { "enabled" } else { "disabled" });
                        }
                        if !st.s("ha").is_empty() || st.get("ha").is_some() {
                            let ha = st.get("ha").map(|h| h.s("state")).unwrap_or_default();
                            if !ha.is_empty() {
                                widgets::kv(ui, "HA state", ha);
                            }
                        }
                    } else {
                        widgets::kv(ui, "Max CPU", format!("{:.0}", r.maxcpu));
                        widgets::kv(ui, "Max memory", fmt_bytes(r.maxmem));
                    }
                    if !r.pool.is_empty() {
                        widgets::kv(ui, "Pool", &r.pool);
                    }
                    let desc = gc.cfg_str("description");
                    if !desc.is_empty() {
                        widgets::kv(ui, "Notes", desc.lines().next().unwrap_or("").to_string());
                    }
                });

                widgets::section(ui, "IP addresses");
                if !gc.running() {
                    widgets::muted(ui, "Guest is not running.");
                } else {
                    match self.ips.poll() {
                        Some(Ok(list)) if !list.is_empty() => {
                            egui::Grid::new("ips").num_columns(2).show(ui, |ui| {
                                for (iface, addrs) in list {
                                    ui.label(RichText::new(iface).color(theme::MUTED));
                                    ui.vertical(|ui| {
                                        for a in addrs {
                                            widgets::copyable(ui, a);
                                        }
                                    });
                                    ui.end_row();
                                }
                            });
                        }
                        Some(Ok(_)) => widgets::muted(ui, "No addresses reported."),
                        Some(Err(e)) => widgets::muted(ui, format!("unavailable: {}", e.lines().next().unwrap_or(""))),
                        None => {
                            ui.spinner();
                        }
                    }
                }
                let ipam: Vec<String> = gc.sh.ipam.lock().guest_ips(&gc.sh.server_id, gc.g.vmid).iter().map(|a| a.ip.clone()).collect();
                if !ipam.is_empty() {
                    widgets::muted(ui, format!("IPAM: {}", ipam.join(", ")));
                }
            });
            ui.add_space(24.0);
            ui.vertical(|ui| {
                widgets::section(ui, "Traffic");
                ui.horizontal(|ui| {
                    ui.label("Since");
                    let since = self.traffic_since.unwrap_or_else(bandwidth::month_start);
                    ui.label(RichText::new(fmt_epoch(since)).strong());
                    for (label, ts) in [
                        ("this month", bandwidth::month_start()),
                        ("24 h", chrono::Utc::now().timestamp() - 86_400),
                        ("7 days", chrono::Utc::now().timestamp() - 7 * 86_400),
                        ("30 days", chrono::Utc::now().timestamp() - 30 * 86_400),
                    ] {
                        if ui.small_button(label).clicked() {
                            self.traffic_since = Some(ts);
                            self.traffic = Fetch::default();
                        }
                    }
                });
                match self.traffic.poll() {
                    Some(Ok(t)) => {
                        egui::Grid::new("traffic").num_columns(2).show(ui, |ui| {
                            widgets::kv(ui, "Received", fmt_bytes_si(t.received));
                            widgets::kv(ui, "Sent", fmt_bytes_si(t.sent));
                            ui.label(RichText::new("Total").color(theme::MUTED));
                            ui.label(RichText::new(fmt_bytes_si(t.total())).strong());
                            ui.end_row();
                        });
                        widgets::muted(
                            ui,
                            format!(
                                "from {} RRD samples ({} resolution){}",
                                t.samples,
                                t.timeframe,
                                if t.covered_from > 0 { format!(", first {}", fmt_epoch(t.covered_from)) } else { String::new() }
                            ),
                        );
                    }
                    Some(Err(e)) => widgets::muted(ui, format!("unavailable: {e}")),
                    None => {
                        ui.spinner();
                    }
                }
            });
        });

        widgets::section(ui, "Performance");
        if charts::timeframe_picker(ui, &mut self.tf) {
            self.rrd = Fetch::default();
        }
        let id = format!("g{}", gc.g.vmid);
        widgets::fetch_ui(ui, &mut self.rrd, |ui, data| charts::rrd_charts(ui, &id, data, false));
    }
}

/// Interface addresses: guest agent for VMs, `/interfaces` for containers.
async fn guest_ips(c: &crate::api::PveClient, g: &crate::api::types::GuestRef) -> anyhow::Result<Vec<(String, Vec<String>)>> {
    let mut out = Vec::new();
    match g.kind {
        GuestKind::Qemu => {
            let r = crate::ops::guest::agent_get(c, g, "network-get-interfaces").await?;
            for iface in r.arr() {
                let name = iface.s("name");
                if name == "lo" {
                    continue;
                }
                let addrs: Vec<String> = iface
                    .get("ip-addresses")
                    .map(|a| a.arr())
                    .unwrap_or_default()
                    .iter()
                    .filter(|a| !a.s("ip-address").starts_with("fe80"))
                    .map(|a| format!("{}/{}", a.s("ip-address"), a.s("prefix")))
                    .collect();
                if !addrs.is_empty() {
                    out.push((name, addrs));
                }
            }
        }
        GuestKind::Lxc => {
            let r = c.get_list(&format!("{}/interfaces", g.path())).await?;
            for iface in r {
                let name = iface.s("name");
                if name == "lo" {
                    continue;
                }
                let mut addrs = Vec::new();
                for k in ["inet", "inet6"] {
                    let a = iface.s(k);
                    if !a.is_empty() && !a.starts_with("fe80") {
                        addrs.push(a);
                    }
                }
                if !addrs.is_empty() {
                    out.push((name, addrs));
                }
            }
        }
    }
    Ok(out)
}
