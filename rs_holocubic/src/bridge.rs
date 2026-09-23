//! 后台 JSON 桥接：全部 GUI 动作在进程内 Rust 完成，不再派生 Python。

use serde_json::{json, Value};
use std::path::PathBuf;

/// 分发一条 GUI 后台请求。
pub fn call(request: Value) -> Result<Value, String> {
    match request.get("action").and_then(Value::as_str) {
        Some("ports") => crate::usb::list_ports_result().map_err(|e| e.0),
        Some("status") => {
            let port = request.get("port").and_then(Value::as_str);
            let labels_crc = request.get("labels_crc").and_then(Value::as_str);
            let radar_crc = request.get("radar_crc").and_then(Value::as_str);
            let calendar_signature = request.get("calendar_signature").and_then(Value::as_str);
            let home_crcs = request.get("home_crcs");
            crate::usb::status_result_with_hints(
                port,
                labels_crc,
                radar_crc,
                calendar_signature,
                home_crcs,
            )
            .map_err(|e| e.0)
        }
        Some("ping") | Some("hello") => {
            let port = request.get("port").and_then(Value::as_str);
            crate::usb::ping_result(port).map_err(|e| e.0)
        }
        Some("browse") => browse_image(),
        Some("radar") => {
            let cache = request
                .get("cache_dir")
                .and_then(Value::as_str)
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    std::env::current_exe()
                        .ok()
                        .and_then(|p| p.parent().map(|d| d.join("radar_cache")))
                        .unwrap_or_else(|| PathBuf::from("radar_cache"))
                });
            crate::radar_data::collect(&cache, None)
        }
        Some("calendar_data") => crate::calendar_data::collect(None, None),
        Some(_) => crate::actions::execute_device(&request).map_err(|e| e.0),
        None => Err("缺少 action".into()),
    }
}

fn browse_image() -> Result<Value, String> {
    let path = rfd::FileDialog::new()
        .set_title("选择显示图片")
        .add_filter("图片", &["png", "jpg", "jpeg", "bmp", "webp"])
        .add_filter("所有文件", &["*"])
        .pick_file();
    Ok(json!({
        "path": path.map(|p| p.to_string_lossy().to_string()).unwrap_or_default(),
    }))
}
