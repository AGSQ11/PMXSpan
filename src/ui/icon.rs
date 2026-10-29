//! Procedurally drawn window icon (no binary assets in the repo).

pub fn app_icon() -> egui::IconData {
    const N: usize = 64;
    let mut rgba = vec![0u8; N * N * 4];
    let c = (N as f32 - 1.0) / 2.0;
    for y in 0..N {
        for x in 0..N {
            let i = (y * N + x) * 4;
            let (fx, fy) = (x as f32 - c, y as f32 - c);
            // Rounded square background.
            let edge = (fx.abs().max(fy.abs())) / c;
            if edge > 0.97 {
                continue;
            }
            let (mut r, mut g, mut b) = (0x1d, 0x23, 0x2b);
            // Stylised "X" made of two thick strokes, Proxmox orange.
            let d1 = (fx - fy).abs() / std::f32::consts::SQRT_2;
            let d2 = (fx + fy).abs() / std::f32::consts::SQRT_2;
            if (d1 < 5.5 || d2 < 5.5) && edge < 0.72 {
                (r, g, b) = (0xe5, 0x70, 0x00);
            }
            rgba[i] = r;
            rgba[i + 1] = g;
            rgba[i + 2] = b;
            rgba[i + 3] = 255;
        }
    }
    egui::IconData { rgba, width: N as u32, height: N as u32 }
}
