//! Settings → MCP server: the switch, port, status, bearer token and the
//! setup steps for SSHSpan (or any MCP client).

use super::widgets::{self, Tone};
use super::{Action, Actions, theme};
use crate::config::{AppConfig, AppPaths};
use crate::mcp::{self, McpStatus};
use crate::vault::Vault;
use egui::RichText;

#[derive(Default)]
pub struct McpPanel {
    /// Port being edited; applied with the button (so a half-typed number
    /// never restarts the server).
    port_text: Option<String>,
}

impl McpPanel {
    pub fn ui(&mut self, ui: &mut egui::Ui, config: &mut AppConfig, vault: &mut Vault, paths: &AppPaths, actions: &mut Actions) {
        egui::ScrollArea::vertical().id_salt("mcp-settings").show(ui, |ui| {
            widgets::info_box(
                ui,
                "A local MCP (Model Context Protocol) server lets an AI assistant on this computer, such as SSHSpan's, read this \
                 cluster through PMXSpan: guests, nodes, IPs, tasks, backups. It listens on loopback only, needs the bearer \
                 token below and runs only while PMXSpan is open and the vault is unlocked.",
            );
            ui.add_space(8.0);
            let s = &mut config.settings;
            egui::Grid::new("mcp-grid").num_columns(2).spacing([16.0, 10.0]).show(ui, |ui| {
                ui.label("MCP server");
                let mut on = s.mcp_enabled;
                if ui.checkbox(&mut on, "Enabled").on_hover_text("Off by default. Nothing listens while this is off.").changed() {
                    s.mcp_enabled = on;
                    if on
                        && !mcp::has_token(vault)
                        && let Err(e) = mcp::ensure_token(vault)
                    {
                        actions.toast_err(format!("Could not store the MCP token in the vault: {e:#}"));
                    }
                    actions.0.push(Action::ConfigChanged);
                }
                ui.end_row();

                ui.label("Port");
                ui.horizontal(|ui| {
                    let text = self.port_text.get_or_insert_with(|| s.mcp_port.to_string());
                    ui.add(egui::TextEdit::singleline(text).desired_width(70.0));
                    let parsed = text.trim().parse::<u16>().ok().filter(|p| *p >= 1024);
                    let changed = parsed.is_some_and(|p| p != s.mcp_port);
                    if ui.add_enabled(changed, egui::Button::new("Apply")).clicked()
                        && let Some(p) = parsed
                    {
                        s.mcp_port = p;
                        actions.0.push(Action::ConfigChanged);
                    }
                    if parsed.is_none() {
                        ui.label(RichText::new("1024–65535").color(theme::err()));
                    }
                    widgets::muted(ui, format!("default {}", mcp::DEFAULT_PORT));
                });
                ui.end_row();

                ui.label("Status");
                ui.horizontal(|ui| match mcp::status() {
                    McpStatus::Listening(addrs) if s.mcp_enabled => {
                        widgets::pill(ui, "Listening", Tone::Ok);
                        let n = mcp::session_count();
                        ui.label(format!(
                            "on {}  ·  {n} client session{}",
                            mcp::fmt_addrs(&addrs),
                            if n == 1 { "" } else { "s" }
                        ));
                    }
                    McpStatus::Error(e) if s.mcp_enabled => {
                        widgets::pill(ui, "Error", Tone::Err);
                        ui.label(RichText::new(e).color(theme::err()));
                    }
                    _ if s.mcp_enabled => {
                        widgets::pill(ui, "Starting", Tone::Info);
                    }
                    _ => {
                        widgets::pill(ui, "Stopped", Tone::Neutral);
                    }
                });
                ui.end_row();

                ui.label("Endpoint");
                widgets::copyable(ui, &mcp::endpoint_url(s.mcp_port));
                ui.end_row();

                ui.label("Bearer token");
                ui.vertical(|ui| {
                    if let Some(t) = mcp::revealed_token() {
                        widgets::warn_box(ui, "Copy this token into your MCP client now. It is shown only this once; afterwards only \"Regenerate\" gives you a new one.");
                        ui.horizontal(|ui| {
                            let mut shown: &str = t.as_str();
                            ui.add(egui::TextEdit::singleline(&mut shown).font(egui::TextStyle::Monospace).desired_width(380.0));
                            if ui.button("Copy").clicked() {
                                ui.ctx().copy_text(t.to_string());
                                actions.toast_ok("Token copied");
                            }
                            if ui.button("Hide").on_hover_text("Done copying: hide the token").clicked() {
                                mcp::hide_token();
                            }
                        });
                    } else if mcp::has_token(vault) {
                        widgets::muted(ui, "Stored encrypted in the vault (hidden).");
                    } else {
                        widgets::muted(ui, "None yet: one is generated when the server is first enabled.");
                    }
                    if ui
                        .button("Regenerate")
                        .on_hover_text("Make a new token. The old one stops working at once and connected clients must be updated.")
                        .clicked()
                    {
                        match mcp::regenerate_token(vault) {
                            Ok(_) => actions.toast_ok("New MCP token generated; update it in your MCP client"),
                            Err(e) => actions.toast_err(format!("Could not store the MCP token in the vault: {e:#}")),
                        }
                    }
                });
                ui.end_row();

                ui.label("Actions");
                ui.vertical(|ui| {
                    if ui.checkbox(&mut s.mcp_allow_actions, "Allow actions over MCP").changed() {
                        actions.0.push(Action::ConfigChanged);
                    }
                    widgets::muted(
                        ui,
                        "Adds power (start/shutdown/reboot/stop/suspend/resume), snapshot_create, agent_exec and set_tags. \
                         Destroy, rebuild, rollback, deleting snapshots or backups, firewall and access changes are always refused.",
                    );
                });
                ui.end_row();

                ui.label("Audit log");
                widgets::copyable(ui, &paths.data_dir.join(mcp::AUDIT_FILE).display().to_string());
                ui.end_row();
            });

            ui.add_space(14.0);
            widgets::section(ui, "Connect SSHSpan");
            let port = config.settings.mcp_port;
            ui.label("In SSHSpan: Settings → AI assistant → MCP servers → Add MCP server, then:");
            egui::Grid::new("mcp-sshspan").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                widgets::kv(ui, "Name", "pmxspan");
                ui.label(RichText::new("URL").color(theme::muted()));
                widgets::copyable(ui, &mcp::endpoint_url(port));
                ui.end_row();
                widgets::kv(ui, "Auth", "Bearer token (stored secret)");
                widgets::kv(ui, "Secret", "the token above");
            });
            widgets::muted(
                ui,
                "SSHSpan warns that plain http is only allowed for loopback addresses; that is expected here, since the server \
                 never leaves this computer. Then enable the PMXSpan tools you want in SSHSpan's tool list.",
            );
            widgets::muted(ui, "Other MCP clients: Streamable HTTP transport, the URL above, header 'Authorization: Bearer <token>'.");
        });
    }
}
