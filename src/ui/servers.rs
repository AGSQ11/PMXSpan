//! Server profiles: add/edit/delete, certificate pinning, and connection diagnostics.

use super::widgets::{self, Confirm};
use super::{Action, Actions, theme};
use crate::api::{LoginOutcome, PveClient};
use crate::config::{AppConfig, AuthMode, ServerProfile, TlsMode};
use crate::ops::diagnostics::{self, Report};
use crate::rt::Fetch;
use crate::vault::Vault;
use egui::RichText;

#[derive(Default)]
pub struct ServersView {
    selected: Option<String>,
    draft: Option<ServerProfile>,
    secret: String,
    show_secret: bool,
    fingerprint: Fetch<String>,
    diag: Fetch<Report>,
    confirm_delete: Option<Confirm<String>>,
    pub last_error: Option<String>,
}

impl ServersView {
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        config: &mut AppConfig,
        vault: &mut Vault,
        connected: Option<&str>,
        connecting: bool,
        actions: &mut Actions,
    ) {
        widgets::heading(ui, "Servers");
        if let Some(id) = widgets::confirm_ui(ui.ctx(), &mut self.confirm_delete) {
            config.profiles.retain(|p| p.id != id);
            let _ = vault.remove(&format!("server:{id}"));
            if connected == Some(id.as_str()) {
                actions.0.push(Action::Disconnect);
            }
            self.selected = None;
            self.draft = None;
            actions.0.push(Action::ConfigChanged);
        }
        if config.profiles.is_empty() && self.draft.is_none() {
            widgets::info_box(
                ui,
                "Add your first Proxmox VE server. An API token (Datacenter → Permissions → API Tokens) is recommended; \
                 password login is needed for browser consoles and 2FA accounts.",
            );
            ui.add_space(6.0);
        }
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(230.0);
                if ui.button("＋ Add server").clicked() {
                    let p = ServerProfile::default();
                    self.selected = Some(p.id.clone());
                    self.draft = Some(p);
                    self.secret.clear();
                    self.diag = Fetch::default();
                    self.fingerprint = Fetch::default();
                }
                ui.add_space(4.0);
                for p in &config.profiles {
                    let is_conn = connected == Some(p.id.as_str());
                    let label = if is_conn {
                        RichText::new(format!("● {}", p.display_name())).color(theme::OK)
                    } else {
                        RichText::new(p.display_name())
                    };
                    if ui.selectable_label(self.selected.as_deref() == Some(p.id.as_str()), label).on_hover_text(p.web_url()).clicked() {
                        self.selected = Some(p.id.clone());
                        self.draft = Some(p.clone());
                        self.secret.clear();
                        self.diag = Fetch::default();
                        self.fingerprint = Fetch::default();
                    }
                }
            });
            ui.separator();
            ui.vertical(|ui| {
                if let Some(e) = self.last_error.clone() {
                    widgets::error_box(ui, &format!("Last connection attempt failed: {e}"));
                    if ui.small_button("Dismiss").clicked() {
                        self.last_error = None;
                    }
                    ui.add_space(6.0);
                }
                if self.draft.is_some() {
                    egui::ScrollArea::vertical().show(ui, |ui| self.editor(ui, config, vault, connected, connecting, actions));
                } else {
                    widgets::muted(ui, "Select a server or add a new one.");
                }
            });
        });
    }

    fn editor(
        &mut self,
        ui: &mut egui::Ui,
        config: &mut AppConfig,
        vault: &mut Vault,
        connected: Option<&str>,
        connecting: bool,
        actions: &mut Actions,
    ) {
        let draft = self.draft.as_mut().expect("draft");
        let is_new = config.profile(&draft.id).is_none();
        let has_secret = vault.get(&draft.secret_key()).is_some();
        egui::Grid::new("server-form").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label("Name");
            ui.add(egui::TextEdit::singleline(&mut draft.name).hint_text("e.g. Production cluster"));
            ui.end_row();
            ui.label("Host");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut draft.host).hint_text("pve.example.com or 10.0.0.10"));
                ui.label("Port");
                ui.add(egui::DragValue::new(&mut draft.port).range(1..=65535));
            });
            ui.end_row();
            ui.label("Authentication");
            ui.horizontal(|ui| {
                ui.radio_value(&mut draft.auth, AuthMode::ApiToken, "API token");
                ui.radio_value(&mut draft.auth, AuthMode::Password, "Password (ticket)");
            });
            ui.end_row();
            ui.label("User");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut draft.username).desired_width(120.0));
                ui.label("@");
                ui.add(egui::TextEdit::singleline(&mut draft.realm).desired_width(80.0).hint_text("pam"));
            });
            ui.end_row();
            if draft.auth == AuthMode::ApiToken {
                ui.label("Token ID");
                ui.add(egui::TextEdit::singleline(&mut draft.token_id).hint_text("the part after '!' e.g. panel"));
                ui.end_row();
                ui.label("Token secret");
            } else {
                ui.label("Password");
            }
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.secret).password(!self.show_secret).hint_text(if has_secret {
                        "stored — leave empty to keep"
                    } else {
                        ""
                    }));
                    if ui.small_button(if self.show_secret { "Hide" } else { "Show" }).clicked() {
                        self.show_secret = !self.show_secret;
                    }
                });
                if draft.auth == AuthMode::ApiToken {
                    widgets::muted(ui, format!("Header: PVEAPIToken={}!{}=<secret>", draft.userid(), draft.token_id));
                }
            });
            ui.end_row();
            if draft.auth == AuthMode::Password {
                ui.label("Two-factor");
                ui.checkbox(&mut draft.totp, "Account uses TOTP (you will be prompted)");
                ui.end_row();
            }
            ui.label("TLS");
            ui.horizontal(|ui| {
                ui.radio_value(&mut draft.tls, TlsMode::Pinned, "Pin certificate");
                ui.radio_value(&mut draft.tls, TlsMode::System, "CA-verified");
                ui.radio_value(&mut draft.tls, TlsMode::Insecure, "Don't verify");
            });
            ui.end_row();
            if draft.tls == TlsMode::Pinned {
                ui.label("SHA-256 fingerprint");
                ui.vertical(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut draft.fingerprint).desired_width(520.0).font(egui::TextStyle::Monospace));
                    ui.horizontal(|ui| {
                        let loading = self.fingerprint.is_loading();
                        if ui.add_enabled(!loading && !draft.host.is_empty(), egui::Button::new("Fetch from server")).clicked() {
                            let (h, p) = (draft.host.clone(), draft.port);
                            self.fingerprint = Fetch::spawn(async move { crate::api::fetch_fingerprint(&h, p).await });
                        }
                        if loading {
                            ui.spinner();
                        }
                        widgets::muted(ui, "Compare with: Node → System → Certificates, or `pvenode cert info`.");
                    });
                    if let Some(res) = self.fingerprint.take() {
                        match res {
                            Ok(fp) => {
                                draft.fingerprint = fp;
                                actions.toast("Fingerprint fetched — verify it before saving");
                            }
                            Err(e) => actions.toast_err(format!("Fetching fingerprint failed: {e}")),
                        }
                    }
                });
                ui.end_row();
            }
            if draft.tls == TlsMode::Insecure {
                ui.label("");
                ui.label(RichText::new("Certificates are not checked: credentials can be intercepted.").color(theme::WARN));
                ui.end_row();
            }
            ui.label("Request timeout");
            ui.add(egui::DragValue::new(&mut draft.request_timeout_secs).range(5..=600).suffix(" s"));
            ui.end_row();
            ui.label("Notes");
            ui.add(egui::TextEdit::multiline(&mut draft.notes).desired_rows(2));
            ui.end_row();
        });

        ui.add_space(8.0);
        let mut save = false;
        let mut save_connect = false;
        ui.horizontal(|ui| {
            save = ui.button(RichText::new("Save").strong()).clicked();
            save_connect = ui.add_enabled(!connecting, egui::Button::new("Save & connect")).clicked();
            let diag_loading = self.diag.is_loading();
            if ui.add_enabled(!diag_loading, egui::Button::new("Test connection")).clicked() {
                let profile = draft.clone();
                let secret =
                    if self.secret.is_empty() { vault.get(&profile.secret_key()).unwrap_or_default() } else { self.secret.clone() };
                self.diag = Fetch::spawn(async move {
                    match PveClient::login(&profile, &secret, None).await? {
                        LoginOutcome::Ok(c) => diagnostics::run(&c).await,
                        LoginOutcome::NeedTotp => anyhow::bail!("this account requires a TOTP code; use 'Save & connect' instead"),
                    }
                });
            }
            if diag_loading {
                ui.spinner();
            }
            if !is_new && ui.button(RichText::new("Delete").color(theme::ERR)).clicked() {
                self.confirm_delete = Some(
                    Confirm::new(
                        "Delete server",
                        format!("Remove '{}' and its stored credentials?", draft.display_name()),
                        draft.id.clone(),
                    )
                    .danger(),
                );
            }
            if connected == Some(draft.id.as_str()) {
                ui.label(RichText::new("connected").color(theme::OK));
            }
        });
        if save || save_connect {
            let p = draft.clone();
            if p.host.trim().is_empty() {
                actions.toast_err("Host is required");
                return;
            }
            if !self.secret.is_empty() {
                if let Err(e) = vault.set(&p.secret_key(), &self.secret) {
                    actions.toast_err(format!("Saving secret failed: {e:#}"));
                    return;
                }
                self.secret.clear();
            }
            match config.profiles.iter_mut().find(|x| x.id == p.id) {
                Some(slot) => *slot = p.clone(),
                None => config.profiles.push(p.clone()),
            }
            actions.0.push(Action::ConfigChanged);
            actions.toast_ok(format!("Saved {}", p.display_name()));
            if save_connect {
                actions.0.push(Action::Connect(p.id));
            }
        }

        ui.add_space(10.0);
        if !self.diag.is_idle() {
            widgets::section(ui, "Diagnostics");
            widgets::fetch_ui(ui, &mut self.diag, |ui, r| report_ui(ui, r));
        }
    }
}

pub fn report_ui(ui: &mut egui::Ui, r: &Report) {
    egui::Grid::new("diag-grid").num_columns(2).spacing([12.0, 4.0]).show(ui, |ui| {
        widgets::kv(ui, "Authenticated as", &r.userid);
        widgets::kv(ui, "Proxmox VE", format!("{} (release {})", r.version, r.release));
        widgets::kv(ui, "Cluster", r.cluster_name.clone().unwrap_or_else(|| "standalone node".into()));
        if let Some(q) = r.quorate {
            ui.label(RichText::new("Quorum").color(theme::MUTED));
            ui.label(if q { RichText::new("quorate").color(theme::OK) } else { RichText::new("NOT quorate").color(theme::ERR) });
            ui.end_row();
        }
        ui.label(RichText::new("Nodes").color(theme::MUTED));
        ui.horizontal_wrapped(|ui| {
            for n in &r.nodes {
                widgets::status_dot(ui, if n.online { "online" } else { "offline" });
                ui.label(if n.ip.is_empty() { n.name.clone() } else { format!("{} ({})", n.name, n.ip) });
            }
        });
        ui.end_row();
    });
    ui.add_space(6.0);
    ui.label(RichText::new("Storage capabilities").strong());
    egui::Grid::new("diag-content").striped(true).num_columns(2).show(ui, |ui| {
        for (ct, what) in diagnostics::CONTENT_TYPES {
            let list = r.content_matrix.get(ct);
            ui.label(format!("{ct} ({what})"));
            match list {
                Some(l) => ui.label(RichText::new(l.join(", ")).color(theme::OK)),
                None => ui.label(RichText::new("none").color(theme::WARN)),
            };
            ui.end_row();
        }
    });
    egui::CollapsingHeader::new(format!("Storage definitions ({})", r.storages.len())).id_salt("diag-storages").show(ui, |ui| {
        egui::Grid::new("diag-st").striped(true).num_columns(5).show(ui, |ui| {
            for s in &r.storages {
                ui.label(RichText::new(&s.storage).strong());
                ui.label(&s.stype);
                ui.label(if s.shared { "shared" } else { "local" });
                ui.label(format!("nodes: {}", s.nodes));
                ui.label(if s.disabled { RichText::new("disabled").color(theme::WARN) } else { RichText::new(&s.content).small() });
                ui.end_row();
            }
        });
    });
    if !r.privileges.is_empty() {
        ui.add_space(6.0);
        ui.label(RichText::new("Effective privileges on /").strong());
        ui.horizontal_wrapped(|ui| {
            for (p, g) in &r.privileges {
                ui.label(RichText::new(format!("{} {p}", if *g { "✔" } else { "✖" })).color(if *g { theme::OK } else { theme::ERR }));
            }
        });
    }
    if !r.warnings.is_empty() {
        ui.add_space(6.0);
        for w in &r.warnings {
            widgets::warn_box(ui, w);
        }
    } else {
        widgets::info_box(ui, "No problems found.");
    }
}
