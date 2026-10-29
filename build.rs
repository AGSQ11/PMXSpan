//! Embeds the application icon and version info into the Windows executable.

fn main() {
    println!("cargo:rerun-if-changed=packaging/assets/proxmoxpanel.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("packaging/assets/proxmoxpanel.ico");
        res.set("ProductName", "ProxmoxPanel");
        res.set("FileDescription", "ProxmoxPanel - Proxmox VE manager");
        if let Err(e) = res.compile() {
            // A missing resource compiler should not break the build; the exe just has no icon.
            println!("cargo:warning=could not embed Windows icon: {e}");
        }
    }
}
