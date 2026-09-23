//! In-process bridge actions that talk to the device over USB.

use crate::calendar_data;
use crate::protocol::{crc32_hex, ProtocolError};
use crate::radar_data;
use crate::render;
use crate::usb::{self, HoloCubicUsb};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;

fn err(msg: impl Into<String>) -> ProtocolError {
    ProtocolError(msg.into())
}

fn bytes_from_json_array(value: &Value) -> Option<Vec<u8>> {
    value.as_array().map(|arr| {
        arr.iter()
            .filter_map(|v| v.as_u64().map(|n| n as u8))
            .collect()
    })
}

fn task_values(settings: &Value) -> Result<(String, String, u32, u32, [u8; 3]), ProtocolError> {
    let title = settings
        .get("task_name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let footer = settings
        .get("footer")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if !(1..=24).contains(&title.chars().count()) || title.contains(['\n', '\r']) {
        return Err(err("任务名称需要 1～24 个字符且不能换行"));
    }
    if footer.chars().count() > 36 || footer.contains(['\n', '\r']) {
        return Err(err("顶部说明最多 36 个字符且不能换行"));
    }
    let hours = settings.get("hours").and_then(Value::as_u64).ok_or_else(|| err("无效的时间字段"))?;
    let minutes = settings.get("minutes").and_then(Value::as_u64).ok_or_else(|| err("无效的时间字段"))?;
    let seconds = settings.get("seconds").and_then(Value::as_u64).ok_or_else(|| err("无效的时间字段"))?;
    if hours > 168 || minutes >= 60 || seconds >= 60 {
        return Err(err("无效的时间字段"));
    }
    let duration = (hours * 3600 + minutes * 60 + seconds) as u32;
    let remaining = settings
        .get("remaining")
        .and_then(Value::as_f64)
        .ok_or_else(|| err("时间或百分比超出范围"))?;
    if !(1..=604800).contains(&duration) || !remaining.is_finite() || !(0.0..=100.0).contains(&remaining)
    {
        return Err(err("时间或百分比超出范围"));
    }
    let timed = settings
        .get("timed")
        .and_then(Value::as_bool)
        .ok_or_else(|| err("timed 必须是布尔值"))?;
    let looping = settings
        .get("looping")
        .and_then(Value::as_bool)
        .ok_or_else(|| err("looping 必须是布尔值"))?;
    let _ = (timed, looping);
    let accent = settings
        .get("accent")
        .and_then(Value::as_array)
        .ok_or_else(|| err("无效的 RGB 颜色"))?;
    if accent.len() != 3 {
        return Err(err("无效的 RGB 颜色"));
    }
    let mut rgb = [0u8; 3];
    for (i, v) in accent.iter().enumerate() {
        let n = v.as_u64().ok_or_else(|| err("无效的 RGB 颜色"))?;
        if n > 255 {
            return Err(err("无效的 RGB 颜色"));
        }
        rgb[i] = n as u8;
    }
    let bp = ((remaining * 100.0) + 0.5).floor() as u32;
    Ok((title, footer, duration, bp, rgb))
}

fn attach_port(mut result: Value, port: &str) -> Value {
    if let Some(obj) = result.as_object_mut() {
        obj.insert("port".into(), json!(port));
    }
    result
}

fn mirror_after(
    device: &mut HoloCubicUsb,
    mut result: Value,
    request: &Value,
    uploaded_labels: Option<&[u8]>,
) -> Result<Value, ProtocolError> {
    let labels_crc = request.get("labels_crc").and_then(Value::as_str);
    let radar_crc = request.get("radar_crc").and_then(Value::as_str);
    let calendar_signature = request.get("calendar_signature").and_then(Value::as_str);
    let home_crcs = request.get("home_crcs");
    result = match result.get("mode").and_then(Value::as_str) {
        Some("home") => usb::mirror_home_public(
            device,
            result,
            labels_crc,
            radar_crc,
            calendar_signature,
            home_crcs,
        )?,
        Some("radar") => usb::mirror_radar_public(device, result, radar_crc)?,
        Some("calendar") => usb::mirror_calendar_public(device, result, calendar_signature)?,
        _ => device.mirror_labels(result, labels_crc, uploaded_labels)?,
    };
    Ok(result)
}

pub fn send_radar(
    device: &mut HoloCubicUsb,
    snapshot: &Value,
    standalone: bool,
) -> Result<Value, ProtocolError> {
    let payload = radar_data::compact(snapshot, None).map_err(err)?;
    // serde_json::to_string uses compact separators matching Python separators=(",", ":").
    let data = serde_json::to_string(&payload)
        .map_err(|e| err(e.to_string()))?
        .into_bytes();
    if data.len() > 8192 {
        return Err(err("雷达数据超过设备 8KB 上限"));
    }
    let crc = crc32_hex(&data);
    device.request(
        json!({"cmd":"radar_begin","size": data.len(),"crc32": crc}),
        Duration::from_secs(4),
    )?;
    for (sequence, chunk) in data.chunks(96).enumerate() {
        let reply = device.request(
            json!({
                "cmd":"radar_chunk",
                "seq": sequence,
                "data": B64.encode(chunk),
            }),
            Duration::from_secs(3),
        )?;
        if reply.get("next_seq").and_then(Value::as_u64) != Some((sequence + 1) as u64) {
            return Err(err("雷达数据分块回执不匹配"));
        }
    }
    let mut result = device.request(
        json!({"cmd":"radar_end","standalone": standalone}),
        Duration::from_secs(8),
    )?;
    let scene = if result.get("mode").and_then(Value::as_str) == Some("home") {
        result
            .get("radar")
            .cloned()
            .ok_or_else(|| err("设备未确认雷达画面"))?
    } else {
        result.clone()
    };
    if scene.get("mode").and_then(Value::as_str) != Some("radar")
        || scene.get("crc32").and_then(Value::as_str) != Some(crc.as_str())
    {
        return Err(err("设备未确认雷达画面"));
    }
    if result.get("mode").and_then(Value::as_str) == Some("home") {
        if let Some(obj) = result.get_mut("radar").and_then(Value::as_object_mut) {
            obj.insert("radar_data".into(), payload);
        }
    } else if let Some(obj) = result.as_object_mut() {
        obj.insert("radar_data".into(), payload);
    }
    Ok(result)
}

pub fn send_calendar(
    device: &mut HoloCubicUsb,
    snapshot: &Value,
    pages: &[Vec<u8>],
    standalone: bool,
) -> Result<Value, ProtocolError> {
    let signature = snapshot
        .get("signature")
        .and_then(Value::as_str)
        .ok_or_else(|| err("日历缺少签名"))?;
    let prepared = device.request(
        json!({
            "cmd":"calendar_begin",
            "pages": pages.len(),
            "signature": signature,
            "date": snapshot.get("date"),
            "total": snapshot.get("total"),
            "done": snapshot.get("done"),
        }),
        Duration::from_secs(4),
    )?;
    let bank = prepared
        .get("bank")
        .and_then(Value::as_str)
        .ok_or_else(|| err("设备返回了无效日历槽"))?;
    if bank != "a" && bank != "b" {
        return Err(err("设备返回了无效日历槽"));
    }
    for (index, data) in pages.iter().enumerate() {
        device.upload_jpeg(data, &format!("calendar_{bank}_{:02}", index + 1))?;
    }
    let mut result = device.request(
        json!({
            "cmd":"calendar_commit",
            "signature": signature,
            "standalone": standalone,
        }),
        Duration::from_secs(8),
    )?;
    let scene = if result.get("mode").and_then(Value::as_str) == Some("home") {
        result
            .get_mut("calendar")
            .ok_or_else(|| err("设备未确认日历内容"))?
    } else {
        &mut result
    };
    if scene.get("mode").and_then(Value::as_str) != Some("calendar")
        || scene.get("calendar_signature").and_then(Value::as_str) != Some(signature)
    {
        return Err(err("设备未确认日历内容"));
    }
    let page_metas = scene
        .get_mut("calendar_pages")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| err("设备未确认日历内容"))?;
    if page_metas.len() != pages.len() {
        return Err(err("设备未确认日历内容"));
    }
    for (meta, raw) in page_metas.iter_mut().zip(pages.iter()) {
        if meta.get("size").and_then(Value::as_u64) != Some(raw.len() as u64)
            || meta.get("crc32").and_then(Value::as_str) != Some(crc32_hex(raw).as_str())
        {
            return Err(err("日历页面 CRC 回执不匹配"));
        }
        if let Some(obj) = meta.as_object_mut() {
            obj.insert(
                "data".into(),
                Value::Array(raw.iter().map(|b| json!(*b)).collect()),
            );
        }
    }
    Ok(result)
}

struct PreparedHome {
    entries: Value,
    images: std::collections::HashMap<String, Vec<u8>>,
    task: Option<Value>,
    labels: Option<Vec<u8>>,
    radar: Option<Value>,
    calendar: Option<Value>,
    calendar_pages: Option<Vec<Vec<u8>>>,
}

fn prepare_home(request: &Value) -> Result<PreparedHome, ProtocolError> {
    let entries = request
        .get("home_entries")
        .and_then(Value::as_array)
        .ok_or_else(|| err("至少勾选一个轮播画面"))?;
    if !(1..=5).contains(&entries.len()) {
        return Err(err("至少勾选一个轮播画面"));
    }
    let mut seen = std::collections::HashSet::new();
    for row in entries {
        let mode = row.get("mode").and_then(Value::as_str).unwrap_or("");
        if !matches!(mode, "task" | "radar" | "text" | "image" | "calendar") || !seen.insert(mode) {
            return Err(err("轮播模式无效或重复"));
        }
        let seconds = row.get("seconds").and_then(Value::as_u64).unwrap_or(0);
        if !(1..=3600).contains(&seconds) {
            return Err(err("每个画面的停留时间应为 1～3600 秒"));
        }
    }
    let settings = request
        .get("settings")
        .ok_or_else(|| err("缺少 settings"))?;
    let mut prepared = PreparedHome {
        entries: Value::Array(entries.clone()),
        images: std::collections::HashMap::new(),
        task: None,
        labels: None,
        radar: None,
        calendar: None,
        calendar_pages: None,
    };
    if seen.contains("task") {
        if let Some(saved) = request.get("resume_task") {
            let labels = bytes_from_json_array(&saved["labels"])
                .ok_or_else(|| err("任务恢复标签 CRC 不匹配"))?;
            if crc32_hex(&labels) != saved.get("labels_crc").and_then(Value::as_str).unwrap_or("")
            {
                return Err(err("任务恢复标签 CRC 不匹配"));
            }
            prepared.labels = Some(labels);
            prepared.task = Some(json!({
                "cmd":"task",
                "seconds": saved["duration"],
                "bp": saved["initial_bp"],
                "timed": saved["timed"],
                "looping": saved["looping"],
                "accent": saved["accent"],
                "restore": {
                    "counter": saved["counter"],
                    "paused": saved["paused"],
                }
            }));
        } else {
            let (title, footer, duration, bp, accent) = task_values(settings)?;
            let labels = render::task_labels(&title, &footer).map_err(|e| err(e.0))?;
            prepared.labels = Some(labels);
            prepared.task = Some(json!({
                "cmd":"task",
                "seconds": duration,
                "bp": bp,
                "timed": settings["timed"],
                "looping": settings["looping"],
                "accent": accent[0] as u32 * 65536 + accent[1] as u32 * 256 + accent[2] as u32,
            }));
        }
    }
    if seen.contains("text") {
        let body = settings
            .get("text_body")
            .and_then(Value::as_str)
            .unwrap_or("");
        if body.trim().is_empty() {
            return Err(err("文字正文不能为空，请先在文字页面编辑"));
        }
        let accent = settings
            .get("accent")
            .and_then(Value::as_array)
            .ok_or_else(|| err("无效的 RGB 颜色"))?;
        let accent_hex = format!(
            "#{:02x}{:02x}{:02x}",
            accent[0].as_u64().unwrap_or(0),
            accent[1].as_u64().unwrap_or(0),
            accent[2].as_u64().unwrap_or(0)
        );
        let frame = render::render_text_frame(
            settings.get("text_title").and_then(Value::as_str).unwrap_or(""),
            body,
            settings
                .get("text_footer")
                .and_then(Value::as_str)
                .unwrap_or("USB DIRECT"),
            &accent_hex,
        )
        .map_err(|e| err(e.0))?;
        prepared
            .images
            .insert("text".into(), render::encode_jpeg(&frame).map_err(|e| err(e.0))?);
    }
    if seen.contains("image") {
        let path = settings
            .get("image_path")
            .and_then(Value::as_str)
            .unwrap_or("");
        if path.is_empty() {
            return Err(err("请先在图片页面选择图片"));
        }
        let frame = render::render_image_frame(Path::new(path), "#000000").map_err(|e| err(e.0))?;
        prepared
            .images
            .insert("image".into(), render::encode_jpeg(&frame).map_err(|e| err(e.0))?);
    }
    if seen.contains("radar") {
        let snapshot = request
            .get("snapshot")
            .ok_or_else(|| err("请等待雷达首次采集完成"))?;
        let _ = radar_data::compact(snapshot, None).map_err(err)?;
        prepared.radar = Some(snapshot.clone());
    }
    if seen.contains("calendar") {
        let snapshot = request
            .get("calendar_snapshot")
            .ok_or_else(|| err("请等待首次日历读取完成"))?;
        let pages = calendar_data::render(snapshot).map_err(|e| err(e.0))?;
        prepared.calendar = Some(snapshot.clone());
        prepared.calendar_pages = Some(pages);
    }
    Ok(prepared)
}

fn start_home(device: &mut HoloCubicUsb, prepared: PreparedHome) -> Result<Value, ProtocolError> {
    device.request(json!({"cmd":"home_begin"}), Duration::from_secs(4))?;
    let result = (|| -> Result<Value, ProtocolError> {
        for (kind, data) in &prepared.images {
            device.upload_jpeg(data, &format!("home_{kind}"))?;
        }
        let calendar = if let (Some(snapshot), Some(pages)) =
            (prepared.calendar.as_ref(), prepared.calendar_pages.as_ref())
        {
            Some(send_calendar(device, snapshot, pages, false)?)
        } else {
            None
        };
        if let Some(task_cmd) = &prepared.task {
            let labels = prepared.labels.as_ref().unwrap();
            device.upload_jpeg(labels, "task_labels")?;
            let task = device.request(task_cmd.clone(), Duration::from_secs(8))?;
            if task.get("mode").and_then(Value::as_str) != Some("task") {
                return Err(err("沙漏预加载未确认"));
            }
            if task_cmd.get("restore").is_none()
                && task.get("bp") != task_cmd.get("bp")
            {
                return Err(err("沙漏预加载未确认"));
            }
            if let Some(restore) = task_cmd.get("restore") {
                if task.get("duration") != task_cmd.get("seconds")
                    || task.get("counter") != restore.get("counter")
                    || task.get("paused") != restore.get("paused")
                {
                    return Err(err("沙漏恢复状态未确认"));
                }
            }
        }
        let radar = if let Some(snapshot) = &prepared.radar {
            Some(send_radar(device, snapshot, false)?)
        } else {
            None
        };
        let session = { let s = uuid::Uuid::new_v4().simple().to_string(); s[..8].to_string() };
        let mut result = device.request(
            json!({
                "cmd":"home_apply",
                "entries": prepared.entries,
                "session": session,
            }),
            Duration::from_secs(8),
        )?;
        if result.get("mode").and_then(Value::as_str) != Some("home")
            || result.get("home_id").and_then(Value::as_str) != Some(session.as_str())
            || result.get("home_entries") != Some(&prepared.entries)
        {
            return Err(err("设备未确认轮播配置"));
        }
        if let Some(labels) = &prepared.labels {
            if let Some(obj) = result.get_mut("task").and_then(Value::as_object_mut) {
                obj.insert(
                    "labels".into(),
                    Value::Array(labels.iter().map(|b| json!(*b)).collect()),
                );
            }
        }
        if let Some(radar_result) = radar {
            let radar_crc = if radar_result.get("mode").and_then(Value::as_str) == Some("home") {
                radar_result.pointer("/radar/crc32").cloned()
            } else {
                radar_result.get("crc32").cloned()
            };
            let radar_data = if radar_result.get("mode").and_then(Value::as_str) == Some("home") {
                radar_result.pointer("/radar/radar_data").cloned()
            } else {
                radar_result.get("radar_data").cloned()
            };
            if result.pointer("/radar/crc32") != radar_crc.as_ref() {
                return Err(err("预加载雷达在启动期间已变化"));
            }
            if let (Some(data), Some(obj)) = (
                radar_data,
                result.get_mut("radar").and_then(Value::as_object_mut),
            ) {
                obj.insert("radar_data".into(), data);
            }
        }
        if let Some(calendar_result) = calendar {
            let sig = if calendar_result.get("mode").and_then(Value::as_str) == Some("home") {
                calendar_result.pointer("/calendar/calendar_signature").cloned()
            } else {
                calendar_result.get("calendar_signature").cloned()
            };
            if result.pointer("/calendar/calendar_signature") != sig.as_ref() {
                return Err(err("日历预加载在启动期间已变化"));
            }
            let old_pages = if calendar_result.get("mode").and_then(Value::as_str) == Some("home") {
                calendar_result.pointer("/calendar/calendar_pages")
            } else {
                calendar_result.get("calendar_pages")
            };
            if let (Some(Value::Array(old)), Some(Value::Array(new))) = (
                old_pages,
                result
                    .pointer_mut("/calendar/calendar_pages")
                    .map(|v| v as &mut Value),
            ) {
                for (new_page, old_page) in new.iter_mut().zip(old) {
                    if let Some(data) = old_page.get("data") {
                        if let Some(obj) = new_page.as_object_mut() {
                            obj.insert("data".into(), data.clone());
                        }
                    }
                }
            }
        }
        for (kind, data) in &prepared.images {
            let crc = crc32_hex(data);
            if result.pointer(&format!("/home_images/{kind}/crc32")).and_then(Value::as_str)
                != Some(crc.as_str())
            {
                return Err(err("轮播图片校验值不匹配"));
            }
            if let Some(obj) = result
                .pointer_mut(&format!("/home_images/{kind}"))
                .and_then(Value::as_object_mut)
            {
                obj.insert(
                    "data".into(),
                    Value::Array(data.iter().map(|b| json!(*b)).collect()),
                );
            }
        }
        Ok(result)
    })();
    if result.is_err() {
        let _ = device.request(json!({"cmd":"clear"}), Duration::from_secs(3));
    }
    result
}

/// Run one device-facing GUI action (everything that needs the serial port).
pub fn execute_device(request: &Value) -> Result<Value, ProtocolError> {
    let action = request
        .get("action")
        .and_then(Value::as_str)
        .ok_or_else(|| err("缺少 action"))?;
    let settings = request.get("settings");
    let port = usb::find_holocubic_port(request.get("port").and_then(Value::as_str))?;
    let mut device = HoloCubicUsb::new(&port);
    device.open()?;
    let state = device.wait_until_ready(Duration::from_secs(6))?;
    let mut uploaded: Option<Vec<u8>> = None;
    let mut result = match action {
        "clear" => {
            let mut r = device.request(json!({"cmd":"clear"}), Duration::from_secs(3))?;
            if let Some(obj) = r.as_object_mut() {
                obj.insert("mode".into(), json!("image"));
            }
            r
        }
        "pause" | "resume" | "reset" => device.request(
            json!({"cmd":"task_control","action": action}),
            Duration::from_secs(4),
        )?,
        "home_stop" => device.request(json!({"cmd":"home_stop"}), Duration::from_secs(4))?,
        "text" => {
            let settings = settings.ok_or_else(|| err("缺少 settings"))?;
            let body = settings
                .get("text_body")
                .and_then(Value::as_str)
                .unwrap_or("");
            if body.trim().is_empty() {
                return Err(err("正文不能为空"));
            }
            let accent = settings.get("accent").and_then(Value::as_array).ok_or_else(|| err("无效的 RGB 颜色"))?;
            let accent_hex = format!(
                "#{:02x}{:02x}{:02x}",
                accent[0].as_u64().unwrap_or(0),
                accent[1].as_u64().unwrap_or(0),
                accent[2].as_u64().unwrap_or(0)
            );
            let frame = render::render_text_frame(
                settings.get("text_title").and_then(Value::as_str).unwrap_or(""),
                body,
                settings
                    .get("text_footer")
                    .and_then(Value::as_str)
                    .unwrap_or("USB DIRECT"),
                &accent_hex,
            )
            .map_err(|e| err(e.0))?;
            let mut r = device.upload_jpeg(&render::encode_jpeg(&frame).map_err(|e| err(e.0))?, "frame")?;
            if let Some(obj) = r.as_object_mut() {
                obj.insert("mode".into(), json!("image"));
            }
            r
        }
        "image" => {
            let settings = settings.ok_or_else(|| err("缺少 settings"))?;
            let path = settings
                .get("image_path")
                .and_then(Value::as_str)
                .unwrap_or("");
            let frame =
                render::render_image_frame(Path::new(path), "#000000").map_err(|e| err(e.0))?;
            let mut r = device.upload_jpeg(&render::encode_jpeg(&frame).map_err(|e| err(e.0))?, "frame")?;
            if let Some(obj) = r.as_object_mut() {
                obj.insert("mode".into(), json!("image"));
            }
            r
        }
        "task" => {
            if state.get("task_protocol").and_then(Value::as_u64) != Some(1)
                || state.get("mirror_protocol").and_then(Value::as_u64) != Some(1)
                || state.get("loop_protocol").and_then(Value::as_u64) != Some(1)
            {
                return Err(err(
                    "设备程序需要更新：请通过热点上传 main.lua 和 quota_scene.lua",
                ));
            }
            let settings = settings.ok_or_else(|| err("缺少 settings"))?;
            let (title, footer, duration, bp, accent) = task_values(settings)?;
            let labels = render::task_labels(&title, &footer).map_err(|e| err(e.0))?;
            device.upload_jpeg(&labels, "task_labels")?;
            let looping = settings.get("looping").and_then(Value::as_bool).unwrap_or(false);
            let timed = settings.get("timed").and_then(Value::as_bool).unwrap_or(true);
            let result = device.request(
                json!({
                    "cmd":"task",
                    "seconds": duration,
                    "bp": bp,
                    "timed": timed,
                    "looping": looping,
                    "accent": accent[0] as u32 * 65536 + accent[1] as u32 * 256 + accent[2] as u32,
                }),
                Duration::from_secs(8),
            )?;
            if result.get("mode").and_then(Value::as_str) != Some("task")
                || result.get("bp").and_then(Value::as_u64) != Some(bp as u64)
                || result.get("looping").and_then(Value::as_bool) != Some(looping)
            {
                return Err(err("设备没有正确应用任务参数"));
            }
            uploaded = Some(labels);
            result
        }
        "radar_usb" | "radar_only" => {
            if state.get("radar_protocol").and_then(Value::as_u64) != Some(1)
                || state.get("radar_mirror_protocol").and_then(Value::as_u64) != Some(1)
            {
                return Err(err(
                    "设备程序需要更新：请上传新版 main.lua 和 radar_scene.lua",
                ));
            }
            let snapshot = request
                .get("snapshot")
                .ok_or_else(|| err("请等待雷达首次采集完成"))?;
            send_radar(&mut device, snapshot, action == "radar_only")?
        }
        "calendar_update" | "calendar_only" => {
            if state.get("calendar_protocol").and_then(Value::as_u64) != Some(1) {
                return Err(err("设备程序需要更新：请上传支持日历的 Lua 程序"));
            }
            let snapshot = request
                .get("calendar_snapshot")
                .ok_or_else(|| err("请等待首次日历读取完成"))?;
            let pages = calendar_data::render(snapshot).map_err(|e| err(e.0))?;
            send_calendar(&mut device, snapshot, &pages, action == "calendar_only")?
        }
        "home" => {
            if state.get("home_protocol").and_then(Value::as_u64) != Some(1) {
                return Err(err("设备程序需要更新：请上传主页轮播 Lua 程序"));
            }
            let prepared = prepare_home(request)?;
            start_home(&mut device, prepared)?
        }
        other => return Err(err(format!("不支持的操作: {other}"))),
    };
    result = mirror_after(&mut device, result, request, uploaded.as_deref())?;
    Ok(attach_port(result, &port))
}
