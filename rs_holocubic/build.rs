//! 从用户选定的棱镜小电视 PNG 生成窗口图标和多尺寸 Windows 资源。

use image::{
    ExtendedColorType,
    codecs::ico::{IcoEncoder, IcoFrame},
    imageops::FilterType,
};
use std::{env, fs::File, path::PathBuf};

/// 保留源图透明通道，构建 16～256 像素图标；Windows 下嵌入 exe 资源。
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/holocubic-icon.png");
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR 未设置"));
    let source = image::open("assets/holocubic-icon.png")
        .expect("无法读取棱镜小电视图标")
        .to_rgba8();
    assert_eq!(source.width(), source.height(), "应用图标必须为正方形");
    assert!(
        source.pixels().any(|pixel| pixel[3] == 0),
        "图标背景必须透明"
    );
    let mut frames = Vec::new();
    for size in [16, 24, 32, 48, 64, 128, 256] {
        let pixels = image::imageops::resize(&source, size, size, FilterType::Lanczos3);
        frames.push(
            IcoFrame::as_png(pixels.as_raw(), size, size, ExtendedColorType::Rgba8)
                .expect("无法编码 ICO 图层"),
        );
        if size == 256 {
            pixels
                .save(output.join("holocubic-window.png"))
                .expect("无法保存窗口图标");
        }
    }
    let icon_path = output.join("holocubic.ico");
    IcoEncoder::new(File::create(&icon_path).expect("无法创建 ICO"))
        .encode_images(&frames)
        .expect("无法保存多尺寸 ICO");
    if env::var_os("CARGO_CFG_WINDOWS").is_some() {
        winres::WindowsResource::new()
            .set_icon(icon_path.to_str().expect("图标路径不是 UTF-8"))
            .compile()
            .expect("无法编译 Windows 图标资源");
    }
}
