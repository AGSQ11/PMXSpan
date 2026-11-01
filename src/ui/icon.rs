//! The PMXSpan mark: the window icon and the in-app logo are the same image
//! as the installer and desktop entry (packaging/assets/pmxspan.png, drawn by
//! packaging/assets/make_icon.py).

const PNG: &[u8] = include_bytes!("../../packaging/assets/pmxspan.png");

pub fn app_icon() -> egui::IconData {
    eframe::icon_data::from_png_bytes(PNG).unwrap_or_default()
}

/// Draws the logo `size` points wide.
pub fn logo(ui: &mut egui::Ui, size: f32) -> egui::Response {
    let id = egui::Id::unique("pmxspan-logo");
    let tex = ui.ctx().data(|d| d.get_temp::<egui::TextureHandle>(id)).unwrap_or_else(|| {
        let icon = app_icon();
        let img = egui::ColorImage::from_rgba_unmultiplied([icon.width as usize, icon.height as usize], &icon.rgba);
        // Mipmaps keep the 256 px image smooth at sidebar size.
        let options = egui::TextureOptions { mipmap_mode: Some(egui::TextureFilter::Linear), ..egui::TextureOptions::LINEAR };
        let tex = ui.ctx().load_texture("pmxspan-logo", img, options);
        ui.ctx().data_mut(|d| d.insert_temp(id, tex.clone()));
        tex
    });
    ui.add(egui::Image::new((tex.id(), egui::vec2(size, size))))
}
