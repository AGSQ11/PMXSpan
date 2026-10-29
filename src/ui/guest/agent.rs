//! QEMU guest agent: health, OS info, filesystems, password reset, command execution.

use super::GCtx;
use crate::ops::guest;
use crate::rt::Fetch;
use crate::ui::{Actions, theme, widgets};
use crate::util::{ValueExt, fmt_bytes};
use egui::RichText;
use serde_json::Value;

#[derive(Default)]
pub struct AgentTab {
    ping: Fetch<()>,
    osinfo: Fetch<Value>,
    fsinfo: Fetch<Value>,
    user: String,
    password: String,
    show_pw: bool,
    cmd: String,
    input: String,
    exec: Fetch<Value>,
}

impl AgentTab {
    pub fn ui(&mut self, ui: &mut egui::Ui, gc: &GCtx, _actions: &mut Actions) {
        if !gc.running() {
            widgets::muted(ui, "The VM is not running.");
            return;
        }
        let agent_on = gc.cfg_str("agent").split(',').next().map(|v| v == "1" || v == "enabled=1").unwrap_or(false);
        if !agent_on {
            widgets::warn_box(
                ui,
                "QEMU guest agent is not enabled in the VM options (agent: 1). Enable it in Hardware → apply, install qemu-guest-agent in the guest, and restart the VM.",
            );
        }
        let g = gc.g.clone();
        if self.ping.is_idle() {
            let gr = g.clone();
            self.ping = gc.sh.fetch(move |c| async move { guest::agent_ping(&c, &gr).await });
        }
        ui.horizontal(|ui| {
            widgets::section(ui, "Agent");
            match self.ping.poll() {
                Some(Ok(())) => {
                    ui.label(RichText::new("responding").color(theme::ok()));
                }
                Some(Err(e)) => {
                    ui.label(RichText::new("not responding").color(theme::err())).on_hover_text(e.as_str());
                }
                None => {
                    ui.spinner();
                }
            }
            if ui.small_button("⟳").clicked() {
                self.ping = Fetch::default();
                self.osinfo = Fetch::default();
                self.fsinfo = Fetch::default();
            }
        });
        if !matches!(self.ping.poll(), Some(Ok(()))) {
            return;
        }
        if self.osinfo.is_idle() {
            let gr = g.clone();
            self.osinfo = gc.sh.fetch(move |c| async move { guest::agent_get(&c, &gr, "get-osinfo").await });
        }
        if self.fsinfo.is_idle() {
            let gr = g.clone();
            self.fsinfo = gc.sh.fetch(move |c| async move { guest::agent_get(&c, &gr, "get-fsinfo").await });
        }
        if let Some(Ok(os)) = self.osinfo.poll() {
            egui::Grid::new("osinfo").num_columns(2).show(ui, |ui| {
                widgets::kv(ui, "OS", os.s("pretty-name"));
                widgets::kv(ui, "Kernel", format!("{} {}", os.s("kernel-release"), os.s("machine")));
            });
        }
        if let Some(Ok(fs)) = self.fsinfo.poll() {
            widgets::section(ui, "Filesystems");
            egui::Grid::new("fsinfo").striped(true).num_columns(3).show(ui, |ui| {
                for f in fs.arr() {
                    let total = f.f("total-bytes");
                    if total <= 0.0 {
                        continue;
                    }
                    let used = f.f("used-bytes");
                    ui.label(f.s("mountpoint"));
                    ui.label(f.s("type"));
                    widgets::usage_bar(ui, crate::util::pct(used, total), format!("{} / {}", fmt_bytes(used), fmt_bytes(total)), 240.0);
                    ui.end_row();
                }
            });
        }

        widgets::section(ui, "Set user password");
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.user).hint_text("root / Administrator").desired_width(140.0));
            widgets::password_field(ui, &mut self.password, &mut self.show_pw, "agent-pw");
            if ui.add_enabled(!self.user.is_empty() && self.password.len() >= 5, egui::Button::new("Set")).clicked() {
                let (c, gr, u, p) = (gc.sh.client.clone(), g.clone(), self.user.clone(), self.password.clone());
                gc.sh.spawn(gc.title(&format!("Set password for {u}")), move |_ctx| async move {
                    guest::agent_set_password(&c, &gr, &u, &p).await?;
                    Ok(format!("password for '{u}' changed"))
                });
            }
        });

        widgets::section(ui, "Run command");
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.cmd)
                    .hint_text("e.g. systemctl status nginx")
                    .desired_width(420.0)
                    .font(egui::TextStyle::Monospace),
            );
            let busy = self.exec.is_loading();
            if ui.add_enabled(!busy && !self.cmd.trim().is_empty(), egui::Button::new("Run")).clicked() {
                let argv = split_args(&self.cmd);
                let (gr, input) = (g.clone(), self.input.clone());
                self.exec = gc.sh.fetch(move |c| async move { guest::agent_exec(&c, &gr, &argv, Some(&input)).await });
            }
            if busy {
                ui.spinner();
            }
        });
        ui.add(egui::TextEdit::multiline(&mut self.input).hint_text("stdin (optional)").desired_rows(1).desired_width(420.0));
        widgets::muted(ui, "Runs directly (no shell). Use: sh -c \"…\" for pipes and redirects.");
        if let Some(r) = self.exec.poll() {
            match r {
                Ok(v) => {
                    ui.label(format!("exit code: {}", v.s("exitcode")));
                    for (k, label) in [("out-data", "stdout"), ("err-data", "stderr")] {
                        let t = v.s(k);
                        if !t.is_empty() {
                            ui.label(RichText::new(label).color(theme::muted()));
                            let mut t = t;
                            ui.add(egui::TextEdit::multiline(&mut t).font(egui::TextStyle::Monospace).desired_width(f32::INFINITY));
                        }
                    }
                }
                Err(e) => widgets::error_box(ui, e),
            }
        }
    }
}

/// Splits a command line honouring simple single/double quotes.
pub fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut has = false;
    for c in s.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                has = true;
            }
            (None, c) if c.is_whitespace() => {
                if has || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    has = false;
                }
            }
            (None, c) => cur.push(c),
        }
    }
    if has || !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn split() {
        assert_eq!(super::split_args(r#"sh -c "echo 'hi there' | wc -c""#), vec!["sh", "-c", "echo 'hi there' | wc -c"]);
        assert_eq!(super::split_args("ls   -la /"), vec!["ls", "-la", "/"]);
        assert_eq!(super::split_args("echo ''"), vec!["echo", ""]);
    }
}
