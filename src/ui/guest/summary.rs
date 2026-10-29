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

        let wide = ui.available_width() > 860.0;
        if wide {
            ui.columns(2, |c| {
                self.resources_card(&mut c[0], gc);
                c[1].vertical(|ui| {
                    self.ips_card(ui, gc);
                    ui.add_space(14.0);
                    self.traffic_card(ui);
                });
            });
        } else {
            self.resources_card(ui, gc);
            ui.add_space(14.0);
            self.ips_card(ui, gc);
            ui.add_space(14.0);
            self.traffic_card(ui);
        }
        ui.add_space(14.0);
        widgets::card(ui, |ui| {
            ui.horizontal(|ui| {
                widgets::card_title(ui, "Performance");
                ui.add_space(12.0);
                if charts::timeframe_picker(ui, &mut self.tf) {
                    self.rrd = Fetch::default();
                }
            });
            let id = format!("g{}", gc.g.vmid);
            widgets::fetch_ui(ui, &mut self.rrd, |ui, data| charts::rrd_charts(ui, &id, data, false));
        });
    }

    fn resources_card(&mut self, ui: &mut egui::Ui, gc: &GCtx) {
        let p = theme::p();
        let st = self.status.ok().cloned();
        let r = gc.res;
        widgets::card(ui, |ui| {
            widgets::card_title(ui, "Resources");
            if let Some(st) = &st {
                widgets::meter_row(
                    ui,
                    "CPU",
                    (st.f("cpu") as f32).min(1.0),
                    p.cpu,
                    format!("{:.1}% of {} vCPU", st.f("cpu") * 100.0, st.f("cpus")),
                );
                widgets::meter_row(
                    ui,
                    "Memory",
                    pct(st.f("mem"), st.f("maxmem")),
                    p.mem,
                    format!("{} / {}", fmt_bytes(st.f("mem")), fmt_bytes(st.f("maxmem"))),
                );
                if gc.g.kind == GuestKind::Lxc {
                    widgets::meter_row(
                        ui,
                        "Root disk",
                        pct(st.f("disk"), st.f("maxdisk")),
                        p.disk,
                        format!("{} / {}", fmt_bytes(st.f("disk")), fmt_bytes(st.f("maxdisk"))),
                    );
                    if st.f("maxswap") > 0.0 {
                        widgets::meter_row(
                            ui,
                            "Swap",
                            pct(st.f("swap"), st.f("maxswap")),
                            p.disk,
                            format!("{} / {}", fmt_bytes(st.f("swap")), fmt_bytes(st.f("maxswap"))),
                        );
                    }
                }
            }
            ui.add_space(4.0);
            ui.separator();
            let mut facts: Vec<(&str, String)> = vec![("Node", gc.g.node.clone())];
            if let Some(st) = &st {
                if gc.g.kind == GuestKind::Qemu {
                    facts.push(("Boot disk", fmt_bytes(st.f("maxdisk"))));
                }
                facts.push(("Uptime", if st.u("uptime") > 0 { fmt_duration(st.u("uptime")) } else { "—".into() }));
                if gc.g.kind == GuestKind::Qemu {
                    let agent = st.get("agent").map(|_| st.b("agent")).unwrap_or(false);
                    facts.push(("Guest agent", if agent { "enabled".into() } else { "disabled".into() }));
                    let qmp = st.s("qmpstatus");
                    if !qmp.is_empty() {
                        facts.push(("QMP status", qmp));
                    }
                }
                let ha = st.get("ha").map(|h| h.s("state")).unwrap_or_default();
                facts.push(("HA", if ha.is_empty() { "not managed".into() } else { ha }));
            } else {
                facts.push(("Max CPU", format!("{:.0}", r.maxcpu)));
                facts.push(("Max memory", fmt_bytes(r.maxmem)));
            }
            facts.push(("Pool", if r.pool.is_empty() { "none".into() } else { r.pool.clone() }));
            ui.add_space(2.0);
            for pair in facts.chunks(2) {
                ui.columns(2, |c| {
                    for (i, (k, v)) in pair.iter().enumerate() {
                        c[i].spacing_mut().item_spacing.y = 1.0;
                        c[i].label(RichText::new(*k).size(12.0).color(p.muted));
                        c[i].label(v);
                    }
                });
                ui.add_space(6.0);
            }
            let desc = gc.cfg_str("description");
            if !desc.is_empty() {
                ui.add_space(4.0);
                ui.label(RichText::new("Notes").size(12.0).color(p.muted));
                ui.label(desc.lines().take(3).collect::<Vec<_>>().join("\n"));
            }
        });
    }

    fn ips_card(&mut self, ui: &mut egui::Ui, gc: &GCtx) {
        let p = theme::p();
        widgets::card(ui, |ui| {
            widgets::card_title(ui, "IP addresses");
            let ipam: Vec<(String, bool, String)> = {
                let db = gc.sh.ipam.lock();
                db.guest_ips(&gc.sh.server_id, gc.g.vmid).iter().map(|a| (a.ip.clone(), a.main, format!("net{}", a.iface()))).collect()
            };
            for (ip, main, iface) in &ipam {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(ip).monospace());
                    widgets::pill(
                        ui,
                        if *main { "main" } else { "additional" },
                        if *main { widgets::Tone::Info } else { widgets::Tone::Neutral },
                    );
                    ui.label(RichText::new(iface).size(12.0).color(p.muted));
                });
            }
            if !ipam.is_empty() {
                ui.add_space(4.0);
            }
            ui.label(RichText::new("Reported by the guest").size(12.0).color(p.muted));
            if !gc.running() {
                widgets::muted(ui, "Guest is not running.");
                return;
            }
            match self.ips.poll() {
                Some(Ok(list)) if !list.is_empty() => {
                    for (iface, addrs) in list {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(RichText::new(iface).color(p.muted));
                            for a in addrs {
                                widgets::copyable(ui, a);
                            }
                        });
                    }
                }
                Some(Ok(_)) => widgets::muted(ui, "No addresses reported."),
                Some(Err(e)) => widgets::muted(ui, format!("unavailable: {}", e.lines().next().unwrap_or(""))),
                None => {
                    ui.spinner();
                }
            }
        });
    }

    fn traffic_card(&mut self, ui: &mut egui::Ui) {
        let p = theme::p();
        widgets::card(ui, |ui| {
            widgets::card_title(ui, "Traffic");
            ui.horizontal_wrapped(|ui| {
                let since = self.traffic_since.unwrap_or_else(bandwidth::month_start);
                ui.label(RichText::new(format!("since {}", fmt_epoch(since))).color(p.muted));
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
            ui.add_space(4.0);
            match self.traffic.poll() {
                Some(Ok(t)) => {
                    ui.horizontal(|ui| {
                        for (label, value, color) in [
                            ("Received", fmt_bytes_si(t.received), p.ok),
                            ("Sent", fmt_bytes_si(t.sent), p.accent),
                            ("Total", fmt_bytes_si(t.total()), p.text),
                        ] {
                            ui.vertical(|ui| {
                                ui.spacing_mut().item_spacing.y = 1.0;
                                ui.label(RichText::new(label).size(12.0).color(p.muted));
                                ui.label(RichText::new(value).family(theme::semibold()).size(20.0).color(color));
                            });
                            ui.add_space(18.0);
                        }
                    });
                    ui.add_space(2.0);
                    let total = t.total().max(1.0);
                    widgets::meter(ui, (t.received / total) as f32, p.ok, ui.available_width());
                    ui.add_space(4.0);
                    widgets::muted(
                        ui,
                        format!(
                            "From {} RRD samples ({} resolution){}",
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
