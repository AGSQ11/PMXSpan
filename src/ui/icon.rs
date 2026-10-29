//! The window icon: the same PMXSpan mark as the installer and desktop entry
//! (packaging/assets/pmxspan.png, drawn by packaging/assets/make_icon.py).

pub fn app_icon() -> egui::IconData {
    eframe::icon_data::from_png_bytes(include_bytes!("../../packaging/assets/pmxspan.png")).unwrap_or_default()
}
