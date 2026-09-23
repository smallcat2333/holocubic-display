#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! HoloCubic 原生桌面控制器入口。

mod actions;
mod app;
mod bridge;
mod calendar;
mod calendar_data;
mod home;
mod mirror;
mod model;
mod protocol;
mod render;
mod radar;
mod radar_data;
mod usb;

/// 加载构建时生成的透明 PNG，替换 eframe 默认的窗口及任务栏图标。
fn app_icon() -> eframe::egui::IconData {
    eframe::icon_data::from_png_bytes(include_bytes!(concat!(
        env!("OUT_DIR"),
        "/holocubic-window.png"
    )))
    .expect("内嵌应用图标无效")
}

/// 启动中文 egui 窗口；截图参数只用于可重复的布局验收。
fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().collect();
    let small = args.iter().any(|arg| arg == "--small");
    let radar = args.iter().any(|arg| arg == "--radar");
    let home = args.iter().any(|arg| arg == "--home");
    let calendar = args.iter().any(|arg| arg == "--calendar");
    let screenshot = args
        .windows(2)
        .find(|args| args[0] == "--screenshot")
        .map(|args| args[1].clone());
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_icon(app_icon())
            .with_inner_size(if small {
                [860.0, 640.0]
            } else if radar {
                [1440.0, 900.0]
            } else {
                [1100.0, 760.0]
            })
            .with_min_inner_size([860.0, 620.0]),
        ..Default::default()
    };
    eframe::run_native(
        "HoloCubic 控制台",
        options,
        Box::new(move |cc| {
            Ok(Box::new(app::HoloApp::new(
                cc, screenshot, radar, home, calendar,
            )))
        }),
    )
}

#[cfg(test)]
mod icon_tests {
    use super::*;

    /// 窗口使用 256 像素 RGBA 图标，透明背景与可见主体都必须存在。
    #[test]
    fn window_icon_has_transparency() {
        let icon = app_icon();
        assert_eq!((icon.width, icon.height), (256, 256));
        assert_eq!(icon.rgba.len(), 256 * 256 * 4);
        assert!(icon.rgba.chunks_exact(4).any(|pixel| pixel[3] == 0));
        assert!(icon.rgba.chunks_exact(4).any(|pixel| pixel[3] > 0));
    }

    /// 检查 exe 图标包包含全部常用 Windows 尺寸及有效 PNG 图层。
    #[test]
    fn executable_icon_has_seven_sizes() {
        let bytes = include_bytes!(concat!(env!("OUT_DIR"), "/holocubic.ico"));
        assert_eq!(&bytes[..6], &[0, 0, 1, 0, 7, 0]);
        for (index, size) in [16u32, 24, 32, 48, 64, 128, 256].into_iter().enumerate() {
            let entry = 6 + index * 16;
            assert_eq!(bytes[entry], size as u8);
            assert_eq!(bytes[entry + 1], size as u8);
            let offset =
                u32::from_le_bytes(bytes[entry + 12..entry + 16].try_into().unwrap()) as usize;
            assert_eq!(&bytes[offset..offset + 8], b"\x89PNG\r\n\x1a\n");
            assert_eq!(
                u32::from_be_bytes(bytes[offset + 16..offset + 20].try_into().unwrap()),
                size
            );
        }
    }
}
