//! Embeds the app icon in the Windows executable (taskbar, Alt+Tab, Explorer).

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.png");
    #[cfg(windows)]
    windows_icon();
}

#[cfg(windows)]
fn windows_icon() {
    use image::ImageEncoder as _;
    use image::codecs::ico::{IcoEncoder, IcoFrame};
    use image::codecs::png::PngEncoder;
    use std::{fs, path::PathBuf};

    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    let source = image::open("assets/icon.png").expect("read assets/icon.png");

    // Every size Windows asks for, each resampled from the large source.
    let encoded: Vec<(u32, Vec<u8>)> = [16, 20, 24, 32, 40, 48, 64, 128, 256]
        .into_iter()
        .map(|size| {
            let rgba = source
                .resize_exact(size, size, image::imageops::FilterType::Lanczos3)
                .to_rgba8();
            let mut png = Vec::new();
            PngEncoder::new(&mut png)
                .write_image(&rgba, size, size, image::ExtendedColorType::Rgba8)
                .expect("encode icon frame");
            (size, png)
        })
        .collect();
    let frames: Vec<IcoFrame> = encoded
        .iter()
        .map(|(size, png)| {
            IcoFrame::with_encoded(
                png.as_slice(),
                *size,
                *size,
                image::ExtendedColorType::Rgba8,
            )
            .expect("icon frame")
        })
        .collect();

    let icon = out.join("fuji.ico");
    IcoEncoder::new(fs::File::create(&icon).expect("create fuji.ico"))
        .encode_images(&frames)
        .expect("write fuji.ico");

    // Resource id 1 is the icon GPUI and Explorer pick up.
    let rc = out.join("fuji.rc");
    fs::write(
        &rc,
        format!("1 ICON \"{}\"\n", icon.to_string_lossy().replace('\\', "/")),
    )
    .expect("write fuji.rc");
    embed_resource::compile(rc, embed_resource::NONE)
        .manifest_optional()
        .expect("compile icon resource");
}
