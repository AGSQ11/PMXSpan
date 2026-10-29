//! Create wizard for VMs and containers.

use super::guest::value_str;
use super::{Actions, Shared, theme, widgets};
use crate::api::Params;
use crate::api::types::{GuestKind, Resource};
use crate::ops::catalog::{self, CatalogEntry};
use crate::ops::create::{self, CreateSpec, IpMode, NetSpec, Source};
use crate::rt::Fetch;
use crate::util::ValueExt;
use egui::RichText;

#[derive(Clone, Default)]
pub struct Prefill {
    pub kind: Option<GuestKind>,
    pub template: Option<(String, u64)>,
}

impl Prefill {
    pub fn from_template(r: &Resource) -> Self {
        Self { kind: r.kind(), template: Some((r.node.clone(), r.vmid)) }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum QemuMode {
    #[default]
    Template,
    Iso,
    Catalog,
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum V4 {
    #[default]
    Dhcp,
    Static,
    Ipam,
    None,
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum V6 {
    #[default]
    None,
    Slaac,
    Dhcp,
    Static,
    Ipam,
}

pub struct CreateView {
    kind: GuestKind,
    qmode: QemuMode,
    template: Option<(String, u64)>,
    iso: Option<String>,
    vztmpl: String,
    lxc_catalog: bool,
    catalog_id: Option<String>,
    node: Option<String>,
    storage: Option<String>,
    auto_vmid: bool,
    vmid: u64,
    hostname: String,
    cores: u32,
    sockets: u32,
    memory: u64,
    swap: u64,
    disk: u64,
    disk_bus: String,
    cpu_type: String,
    ostype: String,
    bridge: String,
    vlan: Option<u16>,
    rate: f64,
    firewall: bool,
    nic_model: String,
    mac: String,
    v4: V4,
    v4_cidr: String,
    v4_gw: String,
    v6: V6,
    v6_cidr: String,
    v6_gw: String,
    ipam_pool4: Option<String>,
    ipam_pool6: Option<String>,
    preferred4: String,
    preferred6: String,
    dns: String,
    search: String,
    ciuser: String,
    password: String,
    show_pw: bool,
    sshkeys: String,
    pool: Option<String>,
    tags: String,
    description: String,
    unprivileged: bool,
    nesting: bool,
    agent: bool,
    onboot: bool,
    start: bool,
    media: Fetch<(Vec<String>, Vec<String>)>,
    media_node: Option<String>,
    bridges: Fetch<Vec<String>>,
    bridges_node: Option<String>,
    template_cfg: Fetch<serde_json::Map<String, serde_json::Value>>,
    template_cfg_for: Option<u64>,
    last_job: Option<u64>,
}

impl Default for CreateView {
    fn default() -> Self {
        Self {
            kind: GuestKind::Qemu,
            qmode: QemuMode::Template,
            template: None,
            iso: None,
            vztmpl: String::new(),
            lxc_catalog: false,
            catalog_id: None,
            node: None,
            storage: None,
            auto_vmid: true,
            vmid: 100,
            hostname: String::new(),
            cores: 2,
            sockets: 1,
            memory: 2048,
            swap: 512,
            disk: 20,
            disk_bus: "scsi".into(),
            cpu_type: "x86-64-v2-AES".into(),
            ostype: "l26".into(),
            bridge: "vmbr0".into(),
            vlan: None,
            rate: 0.0,
            firewall: true,
            nic_model: "virtio".into(),
            mac: String::new(),
            v4: V4::Dhcp,
            v4_cidr: String::new(),
            v4_gw: String::new(),
            v6: V6::None,
            v6_cidr: String::new(),
            v6_gw: String::new(),
            ipam_pool4: None,
            ipam_pool6: None,
            preferred4: String::new(),
            preferred6: String::new(),
            dns: String::new(),
            search: String::new(),
            ciuser: String::new(),
            password: crate::util::generate_password(16),
            show_pw: false,
            sshkeys: String::new(),
            pool: None,
            tags: String::new(),
            description: String::new(),
            unprivileged: true,
            nesting: true,
            agent: true,
            onboot: true,
            start: true,
            media: Fetch::default(),
            media_node: None,
            bridges: Fetch::default(),
            bridges_node: None,
            template_cfg: Fetch::default(),
            template_cfg_for: None,
            last_job: None,
        }
    }
}

impl CreateView {
    pub fn with_prefill(p: Prefill, sh: &Shared) -> Self {
        let mut v =
            Self { bridge: sh.settings.default_bridge.clone(), sshkeys: sh.settings.default_ssh_keys.clone(), ..Default::default() };
        if let Some(k) = p.kind {
            v.kind = k;
        }
        if let Some((node, vmid)) = p.template
            && v.kind == GuestKind::Qemu
        {
            v.qmode = QemuMode::Template;
            v.template = Some((node, vmid));
        }
        v
    }

    /// Node used to list media (ISOs / templates) and bridges.
    fn media_node(&self, sh: &Shared) -> Option<String> {
        self.node.clone().or_else(|| self.template.as_ref().map(|t| t.0.clone())).or_else(|| sh.online_nodes().into_iter().next())
    }

    fn load_media(&mut self, sh: &Shared) {
        let node = self.media_node(sh);
        if node.is_none() || (self.media_node == node && !self.media.is_idle()) {
            return;
        }
        self.media_node = node.clone();
        let node = node.expect("node");
        let iso_st = sh.storages_with(&node, "iso");
        let tpl_st = sh.storages_with(&node, "vztmpl");
        let n = node.clone();
        self.media = sh.fetch(move |c| async move {
            let mut isos = Vec::new();
            let mut tpls = Vec::new();
            for s in iso_st {
                if let Ok(v) = c.get_q(&format!("/nodes/{n}/storage/{s}/content"), &Params::new().set("content", "iso")).await {
                    isos.extend(v.arr().iter().map(|i| i.s("volid")));
                }
            }
            for s in tpl_st {
                if let Ok(v) = c.get_q(&format!("/nodes/{n}/storage/{s}/content"), &Params::new().set("content", "vztmpl")).await {
                    tpls.extend(v.arr().iter().map(|i| i.s("volid")));
                }
            }
            isos.sort();
            tpls.sort();
            Ok((isos, tpls))
        });
        let n = node.clone();
        if self.bridges_node.as_ref() != Some(&node) {
            self.bridges_node = Some(node);
            self.bridges = sh.fetch(move |c| async move {
                let mut v: Vec<String> =
                    c.get_list(&format!("/nodes/{n}/network?type=any_bridge")).await?.iter().map(|i| i.s("iface")).collect();
                if let Ok(vnets) = c.get_list("/cluster/sdn/vnets").await {
                    v.extend(vnets.iter().map(|x| x.s("vnet")));
                }
                v.sort();
                v.dedup();
                Ok(v)
            });
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, sh: &Shared, actions: &mut Actions) {
        self.load_media(sh);
        // Pull defaults from the chosen template once.
        if let (GuestKind::Qemu, QemuMode::Template, Some((tnode, tid))) = (self.kind, self.qmode, self.template.clone()) {
            if self.template_cfg_for != Some(tid) {
                self.template_cfg_for = Some(tid);
                self.template_cfg = sh.fetch(move |c| async move {
                    let v = c.get(&format!("/nodes/{tnode}/qemu/{tid}/config")).await?;
                    Ok(v.as_object().cloned().unwrap_or_default())
                });
            }
            if let Some(cfg) = self.template_cfg.take().and_then(Result::ok) {
                if let Some(c) = cfg.get("cores").map(value_str).and_then(|s| s.parse().ok()) {
                    self.cores = c;
                }
                if let Some(m) = cfg.get("memory").map(value_str).and_then(|s| s.parse().ok()) {
                    self.memory = m;
                }
                if let Some(o) = cfg.get("ostype").map(value_str) {
                    self.ostype = o;
                }
                if let Some(boot) = crate::ops::guest::boot_disk(GuestKind::Qemu, &cfg)
                    && let Some(sz) = crate::ops::guest::disk_size_bytes(&cfg, &boot)
                {
                    self.disk = self.disk.max(sz.div_ceil(1024 * 1024 * 1024));
                }
                self.template_cfg = Fetch::ready(cfg);
            }
        }

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            widgets::heading(ui, "Create guest");
            if let Some(id) = self.last_job {
                ui.horizontal(|ui| {
                    widgets::muted(ui, format!("Job #{id} started — progress is in the Jobs panel."));
                    if ui.small_button("Create another").clicked() {
                        self.last_job = None;
                        self.hostname.clear();
                        self.password = crate::util::generate_password(16);
                    }
                });
            }

            widgets::section(ui, "1. Type & source");
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.kind, GuestKind::Qemu, RichText::new("Virtual machine (QEMU)").strong());
                ui.selectable_value(&mut self.kind, GuestKind::Lxc, RichText::new("Container (LXC)").strong());
            });
            let (isos, vztmpls) = self.media.ok().cloned().unwrap_or_default();
            match self.kind {
                GuestKind::Qemu => {
                    ui.horizontal(|ui| {
                        ui.radio_value(&mut self.qmode, QemuMode::Template, "Clone a template (cloud-init ready)");
                        ui.radio_value(&mut self.qmode, QemuMode::Iso, "Blank VM + installer ISO");
                        ui.radio_value(&mut self.qmode, QemuMode::Catalog, "Image from the OS catalog");
                    });
                    match self.qmode {
                        QemuMode::Template => {
                            let templates = sh.templates(GuestKind::Qemu);
                            if templates.is_empty() {
                                widgets::warn_box(ui, "No QEMU templates found. Convert a prepared VM to a template (Manage → Convert to template), or use the ISO mode.");
                            }
                            let label = self
                                .template
                                .as_ref()
                                .and_then(|(_, id)| templates.iter().find(|t| t.vmid == *id))
                                .map(|t| format!("{} {} ({})", t.vmid, t.display_name(), t.node))
                                .unwrap_or_else(|| "(choose template)".into());
                            egui::ComboBox::from_id_salt("cr-tpl").selected_text(label).width(360.0).show_ui(ui, |ui| {
                                for t in &templates {
                                    let sel = self.template.as_ref().map(|x| x.1) == Some(t.vmid);
                                    if ui.selectable_label(sel, format!("{} {} ({})", t.vmid, t.display_name(), t.node)).clicked() {
                                        self.template = Some((t.node.clone(), t.vmid));
                                    }
                                }
                            });
                        }
                        QemuMode::Catalog => {
                            self.catalog_picker(ui, sh, actions, false);
                            ui.horizontal(|ui| {
                                ui.label("Disk bus");
                                egui::ComboBox::from_id_salt("cr-bus").selected_text(self.disk_bus.clone()).show_ui(ui, |ui| {
                                    for b in ["scsi", "virtio", "sata"] {
                                        ui.selectable_value(&mut self.disk_bus, b.to_string(), b);
                                    }
                                });
                                ui.label("CPU type");
                                ui.add(egui::TextEdit::singleline(&mut self.cpu_type).desired_width(130.0));
                            });
                        }
                        QemuMode::Iso => {
                            ui.horizontal(|ui| {
                                widgets::combo_opt(ui, "cr-iso", &mut self.iso, &isos, "no ISO (empty drive)", 360.0);
                                if self.media.is_loading() {
                                    ui.spinner();
                                }
                            });
                            ui.horizontal(|ui| {
                                ui.label("OS type");
                                egui::ComboBox::from_id_salt("cr-ostype").selected_text(super::guest::ostype_label(&self.ostype)).show_ui(ui, |ui| {
                                    for (v, l) in super::guest::ostypes() {
                                        ui.selectable_value(&mut self.ostype, v.to_string(), *l);
                                    }
                                });
                                ui.label("Disk bus");
                                egui::ComboBox::from_id_salt("cr-bus").selected_text(self.disk_bus.clone()).show_ui(ui, |ui| {
                                    for b in ["scsi", "virtio", "sata", "ide"] {
                                        ui.selectable_value(&mut self.disk_bus, b.to_string(), b);
                                    }
                                });
                                ui.label("CPU type");
                                ui.add(egui::TextEdit::singleline(&mut self.cpu_type).desired_width(130.0));
                            });
                        }
                    }
                }
                GuestKind::Lxc => {
                    ui.horizontal(|ui| {
                        ui.radio_value(&mut self.lxc_catalog, false, "Template on storage");
                        ui.radio_value(&mut self.lxc_catalog, true, "Template from the OS catalog");
                    });
                    if self.lxc_catalog {
                        self.catalog_picker(ui, sh, actions, true);
                    } else {
                    ui.horizontal(|ui| {
                        widgets::combo(ui, "cr-vztmpl", &mut self.vztmpl, &vztmpls, 420.0);
                        if self.media.is_loading() {
                            ui.spinner();
                        }
                    });
                    if vztmpls.is_empty() && !self.media.is_loading() {
                        ui.horizontal(|ui| {
                            widgets::muted(ui, "No container templates on this node's storages.");
                            if ui.link("Download templates in Storage →").clicked() {
                                actions.nav(super::Nav::Storage);
                            }
                        });
                    }
                    }
                }
            }

            widgets::section(ui, "2. Placement");
            let nodes = sh.online_nodes();
            ui.horizontal_wrapped(|ui| {
                ui.label("Node");
                let before = self.node.clone();
                widgets::combo_opt(ui, "cr-node", &mut self.node, &nodes, "auto (placement policy)", 200.0);
                if before != self.node {
                    self.storage = None;
                }
                ui.label("Storage");
                let content = if self.kind == GuestKind::Lxc { "rootdir" } else { "images" };
                let storages: Vec<String> = match &self.node {
                    Some(n) => sh.storages_with(n, content),
                    None => {
                        let mut all: Vec<String> = nodes.iter().flat_map(|n| sh.storages_with(n, content)).collect();
                        all.sort();
                        all.dedup();
                        all
                    }
                };
                widgets::combo_opt(ui, "cr-storage", &mut self.storage, &storages, "auto (most suitable)", 180.0);
                ui.label("VMID");
                ui.checkbox(&mut self.auto_vmid, "next free");
                if !self.auto_vmid {
                    ui.add(egui::DragValue::new(&mut self.vmid).range(100..=999_999_999));
                }
            });
            if self.node.is_none() {
                widgets::muted(
                    ui,
                    format!(
                        "Auto-placement: {:?}, priority {}. Change in Settings.",
                        sh.settings.placement,
                        sh.settings.resource_priority.iter().map(|r| r.label()).collect::<Vec<_>>().join(" > ")
                    ),
                );
            }

            widgets::section(ui, "3. Identity & resources");
            egui::Grid::new("cr-res").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                ui.label("Hostname");
                ui.add(egui::TextEdit::singleline(&mut self.hostname).hint_text("web01.example.com"));
                ui.end_row();
                ui.label("CPU");
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(&mut self.cores).range(1..=512).prefix("cores "));
                    if self.kind == GuestKind::Qemu {
                        ui.add(egui::DragValue::new(&mut self.sockets).range(1..=8).prefix("sockets "));
                    }
                });
                ui.end_row();
                ui.label("Memory");
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(&mut self.memory).range(64..=4_194_304).speed(128).suffix(" MiB"));
                    for m in [512, 1024, 2048, 4096, 8192, 16384] {
                        if ui.small_button(if m >= 1024 { format!("{}G", m / 1024) } else { format!("{m}M") }).clicked() {
                            self.memory = m;
                        }
                    }
                });
                ui.end_row();
                if self.kind == GuestKind::Lxc {
                    ui.label("Swap");
                    ui.add(egui::DragValue::new(&mut self.swap).range(0..=65536).speed(64).suffix(" MiB"));
                    ui.end_row();
                }
                ui.label(if self.kind == GuestKind::Lxc { "Root disk" } else { "Boot disk" });
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(&mut self.disk).range(1..=65536).suffix(" GiB"));
                    if self.kind == GuestKind::Qemu && self.qmode == QemuMode::Template {
                        widgets::muted(ui, "grown to this size after cloning (never shrunk)");
                    } else if self.kind == GuestKind::Qemu && self.qmode == QemuMode::Catalog {
                        widgets::muted(ui, "grown to this size after importing the image (never shrunk)");
                    }
                });
                ui.end_row();
            });

            widgets::section(ui, "4. Network");
            let bridges = self.bridges.ok().cloned().unwrap_or_default();
            egui::Grid::new("cr-net").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                ui.label("Bridge");
                ui.horizontal(|ui| {
                    widgets::combo(ui, "cr-bridge", &mut self.bridge, &bridges, 140.0);
                    ui.label("VLAN");
                    let mut has = self.vlan.is_some();
                    ui.checkbox(&mut has, "");
                    if has {
                        let mut v = self.vlan.unwrap_or(1);
                        ui.add(egui::DragValue::new(&mut v).range(1..=4094));
                        self.vlan = Some(v);
                    } else {
                        self.vlan = None;
                    }
                    ui.label("Rate");
                    ui.add(egui::DragValue::new(&mut self.rate).range(0.0..=100_000.0).suffix(" Mbit/s")).on_hover_text("0 = unlimited");
                    ui.checkbox(&mut self.firewall, "firewall");
                });
                ui.end_row();
                if self.kind == GuestKind::Qemu {
                    ui.label("NIC model");
                    egui::ComboBox::from_id_salt("cr-nic").selected_text(self.nic_model.clone()).show_ui(ui, |ui| {
                        for m in ["virtio", "e1000", "e1000e", "vmxnet3", "rtl8139"] {
                            ui.selectable_value(&mut self.nic_model, m.to_string(), m);
                        }
                    });
                    ui.end_row();
                }
                ui.label("MAC");
                ui.add(egui::TextEdit::singleline(&mut self.mac).hint_text(format!("auto ({}:…)", sh.settings.mac_prefix)).desired_width(180.0));
                ui.end_row();
                let addressing = self.kind == GuestKind::Lxc || self.qmode != QemuMode::Iso;
                if addressing {
                    let pools4: Vec<(String, String)> = sh.ipam.lock().pools.iter().filter(|p| !p.is_v6()).map(|p| (p.id.clone(), format!("{} ({})", p.name, p.cidr))).collect();
                    let pools6: Vec<(String, String)> = sh.ipam.lock().pools.iter().filter(|p| p.is_v6()).map(|p| (p.id.clone(), format!("{} ({})", p.name, p.cidr))).collect();
                    ui.label("IPv4");
                    ui.vertical(|ui| {
                        ui.horizontal(|ui| {
                            ui.radio_value(&mut self.v4, V4::Dhcp, "DHCP");
                            ui.radio_value(&mut self.v4, V4::Static, "Static");
                            ui.radio_value(&mut self.v4, V4::Ipam, "From IPAM");
                            ui.radio_value(&mut self.v4, V4::None, "None");
                        });
                        match self.v4 {
                            V4::Static => {
                                ui.horizontal(|ui| {
                                    ui.add(egui::TextEdit::singleline(&mut self.v4_cidr).hint_text("192.0.2.10/24").desired_width(160.0));
                                    ui.label("gateway");
                                    ui.add(egui::TextEdit::singleline(&mut self.v4_gw).desired_width(130.0));
                                });
                            }
                            V4::Ipam => pool_picker(ui, "cr-pool4", &mut self.ipam_pool4, &pools4, &mut self.preferred4),
                            _ => {}
                        }
                    });
                    ui.end_row();
                    ui.label("IPv6");
                    ui.vertical(|ui| {
                        ui.horizontal(|ui| {
                            ui.radio_value(&mut self.v6, V6::None, "None");
                            ui.radio_value(&mut self.v6, V6::Slaac, "SLAAC");
                            ui.radio_value(&mut self.v6, V6::Dhcp, "DHCPv6");
                            ui.radio_value(&mut self.v6, V6::Static, "Static");
                            ui.radio_value(&mut self.v6, V6::Ipam, "From IPAM");
                        });
                        match self.v6 {
                            V6::Static => {
                                ui.horizontal(|ui| {
                                    ui.add(egui::TextEdit::singleline(&mut self.v6_cidr).hint_text("2001:db8::10/64").desired_width(200.0));
                                    ui.label("gateway");
                                    ui.add(egui::TextEdit::singleline(&mut self.v6_gw).desired_width(160.0));
                                });
                            }
                            V6::Ipam => pool_picker(ui, "cr-pool6", &mut self.ipam_pool6, &pools6, &mut self.preferred6),
                            _ => {}
                        }
                    });
                    ui.end_row();
                    ui.label("DNS");
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut self.dns).hint_text(if sh.settings.default_dns.is_empty() { "host default".to_string() } else { sh.settings.default_dns.clone() }).desired_width(200.0));
                        ui.label("search");
                        ui.add(egui::TextEdit::singleline(&mut self.search).desired_width(160.0));
                    });
                    ui.end_row();
                } else {
                    ui.label("");
                    widgets::muted(ui, "Addressing is configured by the OS installer for ISO installs.");
                    ui.end_row();
                }
            });

            widgets::section(ui, "5. Access");
            egui::Grid::new("cr-access").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                if self.kind == GuestKind::Qemu && self.qmode != QemuMode::Iso {
                    let hint = match (self.qmode, self.catalog_entry(sh, false)) {
                        (QemuMode::Catalog, Some(e)) if !e.cloud_user.is_empty() => format!("{} (catalog default)", e.cloud_user),
                        _ => "image default".to_string(),
                    };
                    ui.label("Cloud-init user");
                    ui.add(egui::TextEdit::singleline(&mut self.ciuser).hint_text(hint));
                    ui.end_row();
                }
                if !(self.kind == GuestKind::Qemu && self.qmode == QemuMode::Iso) {
                    ui.label(if self.kind == GuestKind::Lxc { "Root password" } else { "Password" });
                    widgets::password_field(ui, &mut self.password, &mut self.show_pw, "cr-pw");
                    ui.end_row();
                    ui.label("SSH public keys");
                    ui.add(egui::TextEdit::multiline(&mut self.sshkeys).desired_rows(3).desired_width(560.0).font(egui::TextStyle::Monospace).hint_text("ssh-ed25519 AAAA… user@host"));
                    ui.end_row();
                }
            });

            widgets::section(ui, "6. Options");
            ui.horizontal_wrapped(|ui| {
                ui.checkbox(&mut self.start, "Start after creation");
                ui.checkbox(&mut self.onboot, "Start at boot");
                if self.kind == GuestKind::Qemu {
                    ui.checkbox(&mut self.agent, "QEMU guest agent");
                } else {
                    ui.checkbox(&mut self.unprivileged, "Unprivileged");
                    ui.checkbox(&mut self.nesting, "Nesting (systemd, docker)");
                }
            });
            egui::Grid::new("cr-opts").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                ui.label("Resource pool");
                let pools = sh.pools();
                widgets::combo_opt(ui, "cr-pool", &mut self.pool, &pools, "none", 180.0);
                ui.end_row();
                ui.label("Tags");
                ui.add(egui::TextEdit::singleline(&mut self.tags).hint_text("web;production"));
                ui.end_row();
                ui.label("Notes");
                ui.add(egui::TextEdit::multiline(&mut self.description).desired_rows(2).desired_width(420.0));
                ui.end_row();
            });

            ui.add_space(10.0);
            let spec = self.spec(sh);
            let valid = create::validate(&spec);
            let source_ok = match &spec.source {
                Source::QemuTemplate { .. } => self.template.is_some(),
                Source::QemuIso { .. } => true,
                Source::LxcTemplate { volid } => !volid.is_empty(),
                Source::Catalog { entry } => !entry.id.is_empty(),
            };
            if let Err(e) = &valid
                && !self.hostname.is_empty() {
                    widgets::warn_box(ui, &format!("{e:#}"));
                }
            if !source_ok {
                ui.label(RichText::new("Choose a source above.").color(theme::WARN));
            }
            ui.horizontal(|ui| {
                let can = valid.is_ok() && source_ok;
                if ui.add_enabled(can, egui::Button::new(RichText::new(format!("Create {}", self.kind.label())).strong().size(16.0))).clicked() {
                    let env = sh.env();
                    let title = format!("Create {} '{}'", self.kind.label(), crate::util::sanitize_hostname(&self.hostname));
                    self.last_job = Some(sh.spawn(title, move |ctx| create::run(env, spec, ctx)));
                    actions.toast("Create job started — see the Jobs panel");
                }
                if ui.button("Reset form").clicked() {
                    *self = Self::with_prefill(Prefill::default(), sh);
                }
            });
        });
    }

    fn catalog_entry(&self, sh: &Shared, lxc: bool) -> Option<CatalogEntry> {
        let id = self.catalog_id.as_ref()?;
        sh.catalog.lock().for_virt(lxc).into_iter().find(|e| &e.id == id)
    }

    fn catalog_picker(&mut self, ui: &mut egui::Ui, sh: &Shared, actions: &mut Actions, lxc: bool) {
        let entries = sh.catalog.lock().for_virt(lxc);
        if self.catalog_id.as_ref().is_some_and(|id| !entries.iter().any(|e| &e.id == id)) {
            self.catalog_id = None;
        }
        if entries.is_empty() {
            ui.horizontal(|ui| {
                widgets::warn_box(
                    ui,
                    if lxc { "The catalog has no container (lxc) entries." } else { "The catalog has no VM (qemu) entries." },
                );
                if ui.link("Edit the OS image catalog →").clicked() {
                    actions.nav(super::Nav::Catalog);
                }
            });
            return;
        }
        let label = self
            .catalog_id
            .as_ref()
            .and_then(|id| entries.iter().find(|e| &e.id == id))
            .map(|e| e.label.clone())
            .unwrap_or_else(|| "(choose image)".into());
        egui::ComboBox::from_id_salt("cr-catalog").selected_text(label).width(360.0).show_ui(ui, |ui| {
            for e in &entries {
                ui.selectable_value(&mut self.catalog_id, Some(e.id.clone()), &e.label).on_hover_text(&e.url);
            }
        });
        if let Some(e) = self.catalog_entry(sh, lxc) {
            widgets::muted(ui, format!("{} · {}", e.url, if e.nodes.is_empty() { "any node".to_string() } else { e.nodes.join(", ") }));
        }
        if catalog::configured_storages(&sh.profile, lxc).is_empty() {
            widgets::warn_box(
                ui,
                &format!(
                    "No catalog download storage is set for {} images on this server. Set one in Servers → Edit → Catalog download storage.",
                    if lxc { "container" } else { "VM" }
                ),
            );
        } else {
            widgets::muted(ui, "The image is downloaded to the node first and removed again once the guest is created.");
        }
    }

    fn spec(&self, sh: &Shared) -> CreateSpec {
        let catalog_source = |lxc: bool| Source::Catalog { entry: Box::new(self.catalog_entry(sh, lxc).unwrap_or_default()) };
        let source = match (self.kind, self.qmode) {
            (GuestKind::Lxc, _) if self.lxc_catalog => catalog_source(true),
            (GuestKind::Qemu, QemuMode::Catalog) => catalog_source(false),
            (GuestKind::Lxc, _) => Source::LxcTemplate { volid: self.vztmpl.clone() },
            (GuestKind::Qemu, QemuMode::Iso) => Source::QemuIso { iso: self.iso.clone() },
            (GuestKind::Qemu, _) => {
                let (node, vmid) = self.template.clone().unwrap_or_default();
                Source::QemuTemplate { node, vmid, full: true }
            }
        };
        let ipv4 = match self.v4 {
            V4::Dhcp => IpMode::Dhcp,
            V4::None => IpMode::None,
            V4::Static => IpMode::Static { cidr: self.v4_cidr.clone(), gateway: self.v4_gw.clone() },
            V4::Ipam => {
                IpMode::Ipam { pool_id: self.ipam_pool4.clone(), preferred: Some(self.preferred4.clone()).filter(|s| !s.trim().is_empty()) }
            }
        };
        let ipv6 = match self.v6 {
            V6::None => IpMode::None,
            V6::Slaac => IpMode::Auto,
            V6::Dhcp => IpMode::Dhcp,
            V6::Static => IpMode::Static { cidr: self.v6_cidr.clone(), gateway: self.v6_gw.clone() },
            V6::Ipam => {
                IpMode::Ipam { pool_id: self.ipam_pool6.clone(), preferred: Some(self.preferred6.clone()).filter(|s| !s.trim().is_empty()) }
            }
        };
        let iso_mode = self.kind == GuestKind::Qemu && self.qmode == QemuMode::Iso;
        CreateSpec {
            source,
            node: self.node.clone(),
            vmid: (!self.auto_vmid).then_some(self.vmid),
            hostname: self.hostname.clone(),
            cores: self.cores,
            sockets: self.sockets,
            memory_mb: self.memory,
            swap_mb: self.swap,
            disk_gb: self.disk,
            storage: self.storage.clone(),
            disk_bus: self.disk_bus.clone(),
            cpu_type: self.cpu_type.clone(),
            ostype: self.ostype.clone(),
            cpulimit: None,
            net: NetSpec {
                bridge: self.bridge.clone(),
                vlan: self.vlan,
                rate_mbps: (self.rate > 0.0).then_some(self.rate),
                firewall: self.firewall,
                model: if self.qmode == QemuMode::Template && self.kind == GuestKind::Qemu && self.nic_model == "virtio" {
                    String::new()
                } else {
                    self.nic_model.clone()
                },
                mac: Some(self.mac.trim().to_string()).filter(|m| !m.is_empty()),
                ipv4: if iso_mode { IpMode::None } else { ipv4 },
                ipv6: if iso_mode { IpMode::None } else { ipv6 },
            },
            nameserver: self.dns.clone(),
            searchdomain: self.search.clone(),
            ciuser: self.ciuser.clone(),
            password: if iso_mode { String::new() } else { self.password.clone() },
            ssh_keys: if iso_mode { String::new() } else { self.sshkeys.clone() },
            pool: self.pool.clone(),
            tags: self.tags.clone(),
            description: self.description.clone(),
            unprivileged: self.unprivileged,
            nesting: self.nesting,
            agent: self.agent,
            onboot: self.onboot,
            start: self.start,
        }
    }
}

fn pool_picker(ui: &mut egui::Ui, id: &str, pool: &mut Option<String>, pools: &[(String, String)], preferred: &mut String) {
    ui.horizontal(|ui| {
        if pools.is_empty() {
            ui.label(RichText::new("No pools of this family — add one under IP address management.").color(theme::WARN));
            return;
        }
        egui::ComboBox::from_id_salt(id)
            .selected_text(
                pool.as_ref().and_then(|p| pools.iter().find(|x| &x.0 == p)).map(|x| x.1.clone()).unwrap_or("any matching pool".into()),
            )
            .show_ui(ui, |ui| {
                ui.selectable_value(pool, None, "any matching pool");
                for (pid, label) in pools {
                    ui.selectable_value(pool, Some(pid.clone()), label);
                }
            });
        ui.add(egui::TextEdit::singleline(preferred).hint_text("preferred IP (exact, optional)").desired_width(190.0));
    });
}
