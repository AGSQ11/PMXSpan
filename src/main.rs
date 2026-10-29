//! ProxmoxPanel — a standalone desktop manager for Proxmox VE clusters.
//!
//! This is a port of the `proxmoxopen` HostBill provisioning module into a
//! self-contained GUI application: no billing system, no PHP, no web server.
//! It talks directly to the Proxmox VE API.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod api;
mod config;
mod ipam;
mod jobs;
mod logging;
mod ops;
mod rt;
mod ui;
mod util;
mod vault;

fn main() -> eframe::Result {
    // reqwest is built without a bundled crypto provider; install ring as the
    // process-wide default before any TLS client is constructed.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let paths = config::AppPaths::resolve();
    logging::init(&paths);
    tracing::info!("ProxmoxPanel {} starting", env!("CARGO_PKG_VERSION"));

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ProxmoxPanel")
            .with_inner_size([1360.0, 860.0])
            .with_min_inner_size([960.0, 600.0])
            .with_icon(ui::icon::app_icon()),
        ..Default::default()
    };

    eframe::run_native(
        "ProxmoxPanel",
        options,
        Box::new(move |cc| {
            rt::init(cc.egui_ctx.clone());
            ui::theme::apply(&cc.egui_ctx);
            Ok(Box::new(ui::App::new(paths)))
        }),
    )
}
