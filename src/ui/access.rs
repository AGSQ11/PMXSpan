//! Users, API tokens, resource pools, ACLs, and the least-privilege tenant wizard.

use super::widgets::{self, Confirm};
use super::{Actions, Shared, theme};
use crate::api::Params;
use crate::ops::tenant::{self, TenantSpec};
use crate::rt::Fetch;
use crate::util::{ValueExt, fmt_epoch};
use egui::RichText;
use serde_json::Value;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Tab {
    #[default]
    Tenant,
    Users,
    Pools,
    Acl,
}

#[derive(Clone)]
enum Del {
    User(String),
    Pool(String),
    Token(String, String),
    Acl(String, String, String),
}

#[derive(Default)]
pub struct AccessView {
    tab: Tab,
    users: Fetch<Vec<Value>>,
    pools: Fetch<Vec<Value>>,
    acl: Fetch<Vec<Value>>,
    roles: Fetch<Vec<String>>,
    tokens: Fetch<Vec<Value>>,
    tokens_for: String,
    tenant: Option<TenantSpec>,
    tenant_vmids: String,
    show_pw: bool,
    new_user: String,
    new_user_pw: String,
    new_pool: String,
    new_token: String,
    new_token_privsep: bool,
    token_secret: Fetch<(String, String)>,
    pw_user: String,
    pw_new: String,
    acl_path: String,
    acl_who: String,
    acl_role: String,
    confirm: Option<Confirm<Del>>,
}

impl AccessView {
    pub fn ui(&mut self, ui: &mut egui::Ui, sh: &Shared, _actions: &mut Actions) {
        if self.users.is_idle() || (self.users.is_stale() && !self.users.is_loading()) {
            self.users = sh.fetch(|c| async move { c.get_list("/access/users").await });
        }
        if self.pools.is_idle() || (self.pools.is_stale() && !self.pools.is_loading()) {
            self.pools = sh.fetch(|c| async move { c.get_list("/pools").await });
        }
        if self.acl.is_idle() || (self.acl.is_stale() && !self.acl.is_loading()) {
            self.acl = sh.fetch(|c| async move { c.get_list("/access/acl").await });
        }
        if self.roles.is_idle() {
            self.roles = sh.fetch(|c| async move {
                let mut v: Vec<String> = c.get_list("/access/roles").await?.iter().map(|r| r.s("roleid")).collect();
                v.sort();
                Ok(v)
            });
        }
        if let Some(d) = widgets::confirm_ui(ui.ctx(), &mut self.confirm) {
            let c = sh.client.clone();
            match d {
                Del::User(u) => {
                    sh.spawn(format!("Delete user {u}"), move |_ctx| async move {
                        c.delete(&format!("/access/users/{}", urlencoding::encode(&u)), &Params::new()).await?;
                        Ok(format!("user {u} deleted"))
                    });
                }
                Del::Pool(p) => {
                    sh.spawn(format!("Delete pool {p}"), move |_ctx| async move {
                        c.delete(&format!("/pools/{p}"), &Params::new()).await?;
                        Ok(format!("pool {p} deleted"))
                    });
                }
                Del::Token(u, t) => {
                    sh.spawn(format!("Revoke token {u}!{t}"), move |_ctx| async move {
                        c.delete(&format!("/access/users/{}/token/{t}", urlencoding::encode(&u)), &Params::new()).await?;
                        Ok(format!("token {u}!{t} revoked"))
                    });
                    self.tokens = Fetch::default();
                }
                Del::Acl(path, who, role) => {
                    sh.spawn(format!("Remove ACL {path}"), move |_ctx| async move {
                        let key = if who.contains('!') {
                            "tokens"
                        } else if who.contains('@') {
                            "users"
                        } else {
                            "groups"
                        };
                        c.put("/access/acl", &Params::new().set("path", &path).set(key, &who).set("roles", &role).set("delete", 1)).await?;
                        Ok(format!("removed {role} on {path} for {who}"))
                    });
                }
            }
        }
        widgets::heading(ui, "Users · Pools · Tenants");
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.tab, Tab::Tenant, "Tenant wizard");
            ui.selectable_value(&mut self.tab, Tab::Users, "Users & tokens");
            ui.selectable_value(&mut self.tab, Tab::Pools, "Pools");
            ui.selectable_value(&mut self.tab, Tab::Acl, "Permissions");
        });
        ui.separator();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| match self.tab {
            Tab::Tenant => self.tenant_ui(ui, sh),
            Tab::Users => self.users_ui(ui, sh),
            Tab::Pools => self.pools_ui(ui, sh),
            Tab::Acl => self.acl_ui(ui, sh),
        });
    }

    fn tenant_ui(&mut self, ui: &mut egui::Ui, sh: &Shared) {
        widgets::info_box(
            ui,
            "Creates a resource pool, a Proxmox user, and grants a role on /pool/<pool> only, so the user can manage \
             nothing outside that pool. Give the user's credentials to the customer; they will see only guests in their pool.",
        );
        let t = self.tenant.get_or_insert_with(TenantSpec::default);
        let roles = self.roles.ok().cloned().unwrap_or_default();
        egui::Grid::new("tenant").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
            ui.label("Pool ID");
            ui.add(egui::TextEdit::singleline(&mut t.pool).hint_text("customer-acme"));
            ui.end_row();
            ui.label("User");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut t.username).hint_text("acme").desired_width(140.0));
                ui.label("@");
                ui.add(egui::TextEdit::singleline(&mut t.realm).desired_width(60.0));
            });
            ui.end_row();
            ui.label("Password");
            widgets::password_field(ui, &mut t.password, &mut self.show_pw, "tenant-pw");
            ui.end_row();
            ui.label("E-mail");
            ui.text_edit_singleline(&mut t.email);
            ui.end_row();
            ui.label("Role on pool");
            widgets::combo(ui, "tenant-role", &mut t.role, &roles, 200.0);
            ui.end_row();
            ui.label("Guests to add");
            ui.add(egui::TextEdit::singleline(&mut self.tenant_vmids).hint_text("100,101 (optional)"));
            ui.end_row();
            ui.label("Comment");
            ui.text_edit_singleline(&mut t.comment);
            ui.end_row();
        });
        widgets::muted(ui, "PVEVMUser: console, power, backup, snapshots. PVEVMAdmin adds config changes. Avoid Administrator.");
        if ui.button(RichText::new("Create tenant").strong()).clicked() {
            let mut spec = t.clone();
            spec.vmids = self.tenant_vmids.split([',', ' ']).filter_map(|v| v.trim().parse().ok()).collect();
            let c = sh.client.clone();
            sh.spawn(format!("Create tenant {}", spec.pool), move |ctx| async move { tenant::create(&c, &ctx, spec).await });
        }
    }

    fn users_ui(&mut self, ui: &mut egui::Ui, sh: &Shared) {
        ui.horizontal_wrapped(|ui| {
            ui.label("New user");
            ui.add(egui::TextEdit::singleline(&mut self.new_user).hint_text("name@pve").desired_width(160.0));
            ui.add(egui::TextEdit::singleline(&mut self.new_user_pw).password(true).hint_text("password (pve realm)").desired_width(160.0));
            if ui.add_enabled(self.new_user.contains('@'), egui::Button::new("Create")).clicked() {
                let p = Params::new().set("userid", self.new_user.trim()).opt("password", Some(&self.new_user_pw));
                let c = sh.client.clone();
                let u = self.new_user.clone();
                sh.spawn(format!("Create user {u}"), move |_ctx| async move {
                    c.post("/access/users", &p).await?;
                    Ok(format!("user {u} created"))
                });
                self.new_user.clear();
                self.new_user_pw.clear();
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.label("Change password");
            let users: Vec<String> = self.users.ok().map(|l| l.iter().map(|u| u.s("userid")).collect()).unwrap_or_default();
            widgets::combo(ui, "pw-user", &mut self.pw_user, &users, 180.0);
            ui.add(egui::TextEdit::singleline(&mut self.pw_new).password(true).desired_width(160.0));
            if ui.add_enabled(!self.pw_user.is_empty() && self.pw_new.len() >= 8, egui::Button::new("Set")).clicked() {
                let p = Params::new().set("userid", &self.pw_user).set("password", &self.pw_new);
                let (c, u) = (sh.client.clone(), self.pw_user.clone());
                sh.spawn(format!("Change password of {u}"), move |_ctx| async move {
                    c.put("/access/password", &p).await?;
                    Ok(format!("password of {u} changed"))
                });
                self.pw_new.clear();
            }
        });
        if let Some(res) = self.token_secret.poll() {
            match res {
                Ok((id, secret)) => {
                    widgets::warn_box(ui, "Copy the token secret now — Proxmox will never show it again.");
                    widgets::copyable(ui, &id.to_string());
                    widgets::copyable(ui, secret);
                    if ui.small_button("Dismiss").clicked() {
                        self.token_secret = Fetch::default();
                    }
                }
                Err(e) => widgets::error_box(ui, e),
            }
        }
        let mut del = None;
        let mut show_tokens = None;
        widgets::fetch_ui(ui, &mut self.users, |ui, users| {
            egui::Grid::new("users").striped(true).num_columns(6).spacing([12.0, 4.0]).show(ui, |ui| {
                for h in ["User", "Enabled", "Expires", "E-mail", "Comment", ""] {
                    ui.label(RichText::new(h).strong());
                }
                ui.end_row();
                for u in users.iter() {
                    let id = u.s("userid");
                    ui.label(&id);
                    ui.label(if u.get("enable").map(|_| u.b("enable")).unwrap_or(true) { "yes" } else { "no" });
                    let exp = u.f("expire") as i64;
                    ui.label(if exp > 0 { fmt_epoch(exp) } else { "never".into() });
                    ui.label(u.s("email"));
                    ui.label(u.s("comment"));
                    ui.horizontal(|ui| {
                        if ui.small_button("Tokens").clicked() {
                            show_tokens = Some(id.clone());
                        }
                        if id != "root@pam" && ui.small_button(RichText::new("Delete").color(theme::ERR)).clicked() {
                            del = Some(id.clone());
                        }
                    });
                    ui.end_row();
                }
            });
        });
        if let Some(u) = del {
            self.confirm = Some(Confirm::new("Delete user", format!("Delete {u}?"), Del::User(u)).danger());
        }
        if let Some(u) = show_tokens {
            self.tokens_for = u.clone();
            self.tokens = sh.fetch(move |c| async move { c.get_list(&format!("/access/users/{}/token", urlencoding::encode(&u))).await });
        }
        if !self.tokens_for.is_empty() {
            widgets::section(ui, &format!("API tokens of {}", self.tokens_for));
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.new_token).hint_text("token id").desired_width(140.0));
                ui.checkbox(&mut self.new_token_privsep, "privilege separation")
                    .on_hover_text("Token permissions = intersection of user ACLs and token ACLs");
                if ui.add_enabled(!self.new_token.is_empty(), egui::Button::new("Create token")).clicked() {
                    let (u, t, ps) = (self.tokens_for.clone(), self.new_token.clone(), self.new_token_privsep);
                    self.token_secret = sh.fetch(move |c| async move {
                        let r = c
                            .post(&format!("/access/users/{}/token/{t}", urlencoding::encode(&u)), &Params::new().flag("privsep", ps))
                            .await?;
                        Ok((r.s("full-tokenid"), r.s("value")))
                    });
                    self.new_token.clear();
                    self.tokens = Fetch::default();
                }
            });
            let user = self.tokens_for.clone();
            let mut revoke = None;
            if self.tokens.is_idle() {
                let u = user.clone();
                self.tokens =
                    sh.fetch(move |c| async move { c.get_list(&format!("/access/users/{}/token", urlencoding::encode(&u))).await });
            }
            widgets::fetch_ui(ui, &mut self.tokens, |ui, toks| {
                if toks.is_empty() {
                    widgets::muted(ui, "No tokens.");
                }
                for t in toks.iter() {
                    ui.horizontal(|ui| {
                        ui.label(format!("{user}!{}", t.s("tokenid")));
                        ui.label(RichText::new(if t.b("privsep") { "privsep" } else { "full privileges" }).small().color(theme::MUTED));
                        if ui.small_button(RichText::new("Revoke").color(theme::ERR)).clicked() {
                            revoke = Some(t.s("tokenid"));
                        }
                    });
                }
            });
            if let Some(t) = revoke {
                self.confirm = Some(
                    Confirm::new("Revoke token", format!("Revoke {user}!{t}? Anything using it stops working."), Del::Token(user, t))
                        .danger(),
                );
            }
        }
    }

    fn pools_ui(&mut self, ui: &mut egui::Ui, sh: &Shared) {
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.new_pool).hint_text("pool id").desired_width(160.0));
            if ui.add_enabled(!self.new_pool.is_empty(), egui::Button::new("Create pool")).clicked() {
                let (c, p) = (sh.client.clone(), self.new_pool.trim().to_string());
                sh.spawn(format!("Create pool {p}"), move |_ctx| async move {
                    c.post("/pools", &Params::new().set("poolid", &p)).await?;
                    Ok(format!("pool {p} created"))
                });
                self.new_pool.clear();
            }
        });
        let mut del = None;
        widgets::fetch_ui(ui, &mut self.pools, |ui, pools| {
            egui::Grid::new("pools").striped(true).num_columns(3).spacing([12.0, 4.0]).show(ui, |ui| {
                for p in pools.iter() {
                    let id = p.s("poolid");
                    ui.label(RichText::new(&id).strong());
                    let members: Vec<String> =
                        sh.guests().filter(|g| g.pool == id).map(|g| format!("{} {}", g.vmid, g.display_name())).collect();
                    ui.label(if members.is_empty() { p.s("comment") } else { members.join(", ") });
                    if ui.small_button(RichText::new("Delete").color(theme::ERR)).clicked() {
                        del = Some(id.clone());
                    }
                    ui.end_row();
                }
            });
        });
        if let Some(p) = del {
            self.confirm = Some(Confirm::new("Delete pool", format!("Delete pool {p}? (It must be empty.)"), Del::Pool(p)));
        }
    }

    fn acl_ui(&mut self, ui: &mut egui::Ui, sh: &Shared) {
        let roles = self.roles.ok().cloned().unwrap_or_default();
        ui.horizontal_wrapped(|ui| {
            ui.label("Grant");
            widgets::combo(ui, "acl-role", &mut self.acl_role, &roles, 160.0);
            ui.label("on");
            ui.add(egui::TextEdit::singleline(&mut self.acl_path).hint_text("/pool/acme or /vms/100").desired_width(180.0));
            ui.label("to");
            ui.add(egui::TextEdit::singleline(&mut self.acl_who).hint_text("user@pve, user@pve!token or group").desired_width(220.0));
            if ui
                .add_enabled(!self.acl_role.is_empty() && !self.acl_path.is_empty() && !self.acl_who.is_empty(), egui::Button::new("Grant"))
                .clicked()
            {
                let who = self.acl_who.trim().to_string();
                let key = if who.contains('!') {
                    "tokens"
                } else if who.contains('@') {
                    "users"
                } else {
                    "groups"
                };
                let p = Params::new().set("path", self.acl_path.trim()).set(key, &who).set("roles", &self.acl_role).set("propagate", 1);
                let c = sh.client.clone();
                sh.spawn(format!("Grant {} to {who}", self.acl_role), move |_ctx| async move {
                    c.put("/access/acl", &p).await?;
                    Ok("permission granted".into())
                });
            }
        });
        if self.acl_path.starts_with('/') && self.acl_path.trim() == "/" {
            widgets::warn_box(ui, "Granting on '/' gives cluster-wide rights. Prefer /pool/<pool> for tenants.");
        }
        let mut del = None;
        widgets::fetch_ui(ui, &mut self.acl, |ui, acl| {
            egui::Grid::new("acl").striped(true).num_columns(5).spacing([12.0, 4.0]).show(ui, |ui| {
                for h in ["Path", "Type", "User/group/token", "Role", ""] {
                    ui.label(RichText::new(h).strong());
                }
                ui.end_row();
                for a in acl.iter() {
                    ui.label(a.s("path"));
                    ui.label(a.s("type"));
                    ui.label(a.s("ugid"));
                    ui.label(a.s("roleid"));
                    if ui.small_button(RichText::new("Remove").color(theme::ERR)).clicked() {
                        del = Some((a.s("path"), a.s("ugid"), a.s("roleid")));
                    }
                    ui.end_row();
                }
            });
        });
        if let Some((p, w, r)) = del {
            self.confirm = Some(Confirm::new("Remove permission", format!("Remove {r} on {p} for {w}?"), Del::Acl(p, w, r)));
        }
    }
}
