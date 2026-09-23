//! In-process ESP32-S3 USB CDC client for `@HCUSB/1` commands.
//!
//! Mirrors `holo_usb_display.HoloCubicUSB`: DTR/RTS stay low, 115200 baud,
//! request/response matching by `id`, and multipart `response_read` assembly.

use crate::protocol::{
    assemble_multipart_body, crc32_hex, decode_line, decode_response_part, encode_frame,
    is_valid_image_target, jpeg_chunks, validate_multipart_meta, ProtocolError, READBACK_CHUNK,
    USB_PID, USB_VID,
};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde_json::{json, Value};
use serialport::SerialPort;
use std::io::{Read, Write};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// One enumerated serial port with identifiers used to find HoloCubic.
#[derive(Debug, Clone)]
pub struct PortInfo {
    pub device: String,
    pub description: String,
    pub vid: Option<u16>,
    pub pid: Option<u16>,
    pub serial_number: Option<String>,
}

impl PortInfo {
    pub fn to_json(&self) -> Value {
        json!({
            "device": self.device,
            "description": self.description,
            "vid": self.vid,
            "pid": self.pid,
            "serial_number": self.serial_number,
        })
    }
}

/// List serial ports (includes VID/PID when the OS exposes them).
pub fn available_ports() -> Result<Vec<PortInfo>, ProtocolError> {
    let ports = serialport::available_ports().map_err(|e| ProtocolError(e.to_string()))?;
    Ok(ports
        .into_iter()
        .map(|port| {
            let (vid, pid, serial_number, description) = match port.port_type {
                serialport::SerialPortType::UsbPort(info) => (
                    Some(info.vid),
                    Some(info.pid),
                    info.serial_number,
                    info.product
                        .or(info.manufacturer)
                        .unwrap_or_else(|| port.port_name.clone()),
                ),
                _ => (None, None, None, port.port_name.clone()),
            };
            PortInfo {
                device: port.port_name,
                description,
                vid,
                pid,
                serial_number,
            }
        })
        .collect())
}

/// Use an explicit port or auto-detect Espressif USB Serial/JTAG (303A:1001).
pub fn find_holocubic_port(requested: Option<&str>) -> Result<String, ProtocolError> {
    if let Some(port) = requested.filter(|p| !p.is_empty()) {
        return Ok(port.to_owned());
    }
    let matches: Vec<String> = available_ports()?
        .into_iter()
        .filter(|p| p.vid == Some(USB_VID) && p.pid == Some(USB_PID))
        .map(|p| p.device)
        .collect();
    match matches.as_slice() {
        [] => Err(ProtocolError(
            "未找到 Espressif 303A:1001 串口，请检查 USB 线或用 --port COMx 指定。".into(),
        )),
        [only] => Ok(only.clone()),
        many => Err(ProtocolError(format!(
            "找到多个 ESP32-S3 串口：{}，请用 --port 指定。",
            many.join(", ")
        ))),
    }
}

/// Framed USB session against one CDC port.
pub struct HoloCubicUsb {
    port_name: String,
    baudrate: u32,
    serial: Option<Box<dyn SerialPort>>,
    request_number: u64,
}

impl HoloCubicUsb {
    pub fn new(port: impl Into<String>) -> Self {
        Self {
            port_name: port.into(),
            baudrate: 115_200,
            serial: None,
            request_number: 0,
        }
    }

    #[allow(dead_code)]
    pub fn port_name(&self) -> &str {
        &self.port_name
    }

    /// Open USB CDC with DTR and RTS deasserted to avoid bootloader entry.
    pub fn open(&mut self) -> Result<(), ProtocolError> {
        let builder = serialport::new(&self.port_name, self.baudrate)
            .timeout(Duration::from_millis(150))
            .dtr_on_open(false)
            .flow_control(serialport::FlowControl::None)
            .data_bits(serialport::DataBits::Eight)
            .parity(serialport::Parity::None)
            .stop_bits(serialport::StopBits::One);
        let mut serial = builder
            .open()
            .map_err(|e| ProtocolError(format!("无法打开串口：{e}")))?;
        // Keep both control lines low (Python sets dtr/rts False before/after open).
        let _ = serial.write_data_terminal_ready(false);
        let _ = serial.write_request_to_send(false);
        std::thread::sleep(Duration::from_millis(200));
        let _ = serial.clear(serialport::ClearBuffer::Input);
        self.serial = Some(serial);
        Ok(())
    }

    pub fn close(&mut self) {
        if let Some(mut serial) = self.serial.take() {
            let _ = serial.write_data_terminal_ready(false);
            let _ = serial.write_request_to_send(false);
            // Drop closes the port.
        }
    }

    fn next_id(&mut self) -> String {
        self.request_number += 1;
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        format!("{ms}-{}", self.request_number)
    }

    fn serial_mut(&mut self) -> Result<&mut Box<dyn SerialPort>, ProtocolError> {
        self.serial
            .as_mut()
            .ok_or_else(|| ProtocolError("串口尚未打开。".into()))
    }

    /// Send one JSON command and wait for its matching prefixed response.
    pub fn request(&mut self, command: Value, timeout: Duration) -> Result<Value, ProtocolError> {
        let request_id = self.next_id();
        let mut payload = command;
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("id".into(), Value::String(request_id.clone()));
        } else {
            return Err(ProtocolError("命令必须是 JSON 对象。".into()));
        }
        let raw = encode_frame(&payload)?;
        {
            let serial = self.serial_mut()?;
            serial
                .write_all(&raw)
                .map_err(|e| ProtocolError(format!("USB 写入失败：{e}")))?;
            serial
                .flush()
                .map_err(|e| ProtocolError(format!("USB 刷新失败：{e}")))?;
        }

        let deadline = Instant::now() + timeout;
        let mut pending = Vec::new();
        loop {
            if Instant::now() >= deadline {
                return Err(ProtocolError(
                    "设备未响应：请先在 HoloCubic Launcher 中启动“USB直连显示”应用。".into(),
                ));
            }
            let mut buf = [0u8; 1024];
            let read = {
                let serial = self.serial_mut()?;
                match serial.read(&mut buf) {
                    Ok(n) => n,
                    Err(err) if err.kind() == std::io::ErrorKind::TimedOut => 0,
                    Err(err) => return Err(ProtocolError(format!("USB 读取失败：{err}"))),
                }
            };
            if read > 0 {
                pending.extend_from_slice(&buf[..read]);
            }
            if pending.len() > 65536 {
                return Err(ProtocolError("USB 回执超过 64KB 上限。".into()));
            }
            while let Some(pos) = pending.iter().position(|&b| b == b'\n') {
                let line = pending.drain(..=pos).collect::<Vec<u8>>();
                let Some(response) = decode_line(&line)? else {
                    continue;
                };
                if response.get("id").and_then(Value::as_str) != Some(request_id.as_str()) {
                    continue;
                }
                let response = if response.get("status").and_then(Value::as_str) == Some("multipart")
                {
                    self.read_multipart(response, &request_id, deadline)?
                } else {
                    response
                };
                if response.get("status").and_then(Value::as_str) == Some("error") {
                    let message = response
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("设备返回未知错误");
                    return Err(ProtocolError(message.to_owned()));
                }
                return Ok(response);
            }
            // Avoid busy-spin when the read timed out with no newline yet.
            if read == 0 {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    fn read_multipart(
        &mut self,
        header: Value,
        request_id: &str,
        deadline: Instant,
    ) -> Result<Value, ProtocolError> {
        let sampled_at = Instant::now();
        let (size, checksum) = validate_multipart_meta(&header)?;
        let mut data = Vec::with_capacity(size);
        while data.len() < size {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(ProtocolError("USB 分块回执超时。".into()));
            }
            let part = self.request(
                json!({
                    "cmd": "response_read",
                    "token": request_id,
                    "offset": data.len(),
                }),
                remaining,
            )?;
            let chunk = decode_response_part(&part, data.len(), size - data.len())?;
            data.extend_from_slice(&chunk);
        }
        let mut response = assemble_multipart_body(&data, &checksum, request_id)?;
        if let Some(obj) = response.as_object_mut() {
            obj.insert(
                "_transport_ms".into(),
                json!(sampled_at.elapsed().as_millis() as u64),
            );
        }
        Ok(response)
    }

    /// Retry the hello handshake while the foreground Lua app starts.
    pub fn wait_until_ready(&mut self, timeout: Duration) -> Result<Value, ProtocolError> {
        let deadline = Instant::now() + timeout;
        let mut last_error: Option<ProtocolError> = None;
        while Instant::now() < deadline {
            match self.request(json!({"cmd": "hello"}), Duration::from_millis(1500)) {
                Ok(value) => return Ok(value),
                Err(err) => {
                    last_error = Some(err);
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        }
        Err(last_error.unwrap_or_else(|| ProtocolError("设备未就绪。".into())))
    }

    /// Convenience: hello after open (used by bridge `status` / `ping`).
    pub fn hello(&mut self) -> Result<Value, ProtocolError> {
        self.wait_until_ready(Duration::from_secs(6))
    }

    /// Query device status command (distinct from hello; used after asset readback).
    pub fn status(&mut self) -> Result<Value, ProtocolError> {
        self.request(json!({"cmd": "status"}), Duration::from_secs(3))
    }

    /// Upload a complete JPEG with ordered chunks and CRC32 validation.
    pub fn upload_jpeg(&mut self, data: &[u8], target: &str) -> Result<Value, ProtocolError> {
        if !is_valid_image_target(target) {
            return Err(ProtocolError("不支持的图片目标。".into()));
        }
        let checksum = crc32_hex(data);
        self.request(
            json!({
                "cmd": "image_begin",
                "size": data.len(),
                "crc32": checksum,
                "target": target,
            }),
            Duration::from_secs(4),
        )?;
        for (seq, encoded) in jpeg_chunks(data) {
            self.request(
                json!({
                    "cmd": "image_chunk",
                    "seq": seq,
                    "data": encoded,
                }),
                Duration::from_secs(3),
            )?;
        }
        let mut response = self.request(json!({"cmd": "image_end"}), Duration::from_secs(5))?;
        let expected_status = if target == "frame" {
            "displayed"
        } else {
            "stored"
        };
        let status_ok = response.get("status").and_then(Value::as_str) == Some(expected_status);
        let bytes_ok = response.get("bytes").and_then(Value::as_u64) == Some(data.len() as u64);
        let crc_ok = response.get("crc32").and_then(Value::as_str) == Some(checksum.as_str());
        if !(status_ok && bytes_ok && crc_ok) {
            return Err(ProtocolError(
                "设备完成回执与发送画面的大小或 CRC32 不一致。".into(),
            ));
        }
        // Hex avoids the firmware's 32-bit floating-point JSON precision loss.
        if let Some(obj) = response.as_object_mut() {
            let value = u32::from_str_radix(&checksum, 16).unwrap_or(0);
            obj.insert("crc32".into(), json!(value));
        }
        Ok(response)
    }

    /// Read task label JPEG bytes when CRC changed (mirrors `bridge.mirror_labels`).
    pub fn mirror_labels(
        &mut self,
        mut state: Value,
        known_crc: Option<&str>,
        uploaded: Option<&[u8]>,
    ) -> Result<Value, ProtocolError> {
        if state.get("mode").and_then(Value::as_str) != Some("task") {
            return Ok(state);
        }
        if state.get("mirror_protocol").and_then(Value::as_u64) != Some(1) {
            return Err(ProtocolError(
                "设备程序需要更新：请上传新版 main.lua 和 quota_scene.lua 以回读实际画面".into(),
            ));
        }
        let expected_crc = state
            .get("labels_crc")
            .and_then(Value::as_str)
            .ok_or_else(|| ProtocolError("设备标签元数据无效".into()))?
            .to_owned();
        let size = state
            .get("labels_size")
            .and_then(Value::as_u64)
            .ok_or_else(|| ProtocolError("设备标签元数据无效".into()))? as usize;
        if expected_crc.len() != 8 || !(1..=512 * 1024).contains(&size) {
            return Err(ProtocolError("设备标签元数据无效".into()));
        }
        let data = if let Some(uploaded) = uploaded {
            uploaded.to_vec()
        } else if known_crc == Some(expected_crc.as_str()) {
            return Ok(state);
        } else {
            let mut data = Vec::with_capacity(size);
            while data.len() < size {
                let part = self.request(
                    json!({
                        "cmd": "task_labels_read",
                        "crc32": expected_crc,
                        "offset": data.len(),
                    }),
                    Duration::from_secs(3),
                )?;
                let encoded = part
                    .get("data")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ProtocolError("设备标签分块不匹配".into()))?;
                let chunk = B64
                    .decode(encoded)
                    .map_err(|_| ProtocolError("设备标签分块不匹配".into()))?;
                let part_crc = part.get("crc32").and_then(Value::as_str);
                let part_offset = part.get("offset").and_then(Value::as_u64).map(|v| v as usize);
                let max = READBACK_CHUNK.min(size - data.len());
                if part_crc != Some(expected_crc.as_str())
                    || part_offset != Some(data.len())
                    || chunk.is_empty()
                    || chunk.len() > max
                {
                    return Err(ProtocolError("设备标签分块不匹配".into()));
                }
                data.extend_from_slice(&chunk);
            }
            // File transfer takes time: sample again so the visual values are current.
            let mut refreshed = self.status()?;
            if refreshed.get("mode").and_then(Value::as_str) == Some("home") {
                refreshed = refreshed
                    .get("task")
                    .cloned()
                    .ok_or_else(|| ProtocolError("回读期间设备任务已变化，请重新检测连接".into()))?;
            }
            if refreshed.get("mode").and_then(Value::as_str) != Some("task")
                || refreshed.get("labels_crc").and_then(Value::as_str) != Some(expected_crc.as_str())
            {
                return Err(ProtocolError(
                    "回读期间设备任务已变化，请重新检测连接".into(),
                ));
            }
            state = refreshed;
            data
        };
        if data.len() != size || crc32_hex(&data) != expected_crc {
            return Err(ProtocolError("设备标签 CRC32 校验失败".into()));
        }
        if let Some(obj) = state.as_object_mut() {
            obj.insert(
                "labels".into(),
                Value::Array(data.into_iter().map(|b| json!(b)).collect()),
            );
        }
        Ok(state)
    }
}

impl Drop for HoloCubicUsb {
    fn drop(&mut self) {
        self.close();
    }
}

/// Bridge-facing helpers: list ports or run hello/status on a device.
pub fn list_ports_result() -> Result<Value, ProtocolError> {
    let ports = available_ports()?;
    Ok(json!({
        "ports": ports.iter().map(PortInfo::to_json).collect::<Vec<_>>(),
    }))
}

/// Ping/hello path: open + handshake, attach resolved `port`.
pub fn ping_result(port: Option<&str>) -> Result<Value, ProtocolError> {
    let port = find_holocubic_port(port)?;
    let mut device = HoloCubicUsb::new(&port);
    device.open()?;
    let mut result = device.hello()?;
    if let Some(obj) = result.as_object_mut() {
        obj.insert("port".into(), Value::String(port));
    }
    Ok(result)
}

/// Full status path with optional CRC hints from the GUI.
pub fn status_result_with_hints(
    port: Option<&str>,
    labels_crc: Option<&str>,
    radar_crc: Option<&str>,
    calendar_signature: Option<&str>,
    home_crcs: Option<&Value>,
) -> Result<Value, ProtocolError> {
    let port = find_holocubic_port(port)?;
    let mut device = HoloCubicUsb::new(&port);
    device.open()?;
    let mut result = device.hello()?;
    result = match result.get("mode").and_then(Value::as_str) {
        Some("home") => mirror_home_public(
            &mut device,
            result,
            labels_crc,
            radar_crc,
            calendar_signature,
            home_crcs,
        )?,
        Some("radar") => mirror_radar_public(&mut device, result, radar_crc)?,
        Some("calendar") => mirror_calendar_public(&mut device, result, calendar_signature)?,
        _ => device.mirror_labels(result, labels_crc, None)?,
    };
    if let Some(obj) = result.as_object_mut() {
        obj.insert("port".into(), Value::String(port));
    }
    Ok(result)
}

pub(crate) fn mirror_radar_public(
    device: &mut HoloCubicUsb,
    state: Value,
    known_crc: Option<&str>,
) -> Result<Value, ProtocolError> {
    if state.get("radar_mirror_protocol").and_then(Value::as_u64) != Some(1) {
        return Err(ProtocolError(
            "设备程序需要更新：请上传支持雷达预览的 main.lua 和 radar_scene.lua".into(),
        ));
    }
    let crc = state
        .get("crc32")
        .and_then(Value::as_str)
        .ok_or_else(|| ProtocolError("设备雷达元数据无效".into()))?
        .to_owned();
    let size = state
        .get("radar_size")
        .and_then(Value::as_u64)
        .ok_or_else(|| ProtocolError("设备雷达元数据无效".into()))? as usize;
    if crc.len() != 8 || !(1..=8192).contains(&size) {
        return Err(ProtocolError("设备雷达元数据无效".into()));
    }
    if state.get("radar_data").is_some() || known_crc == Some(crc.as_str()) {
        return Ok(state);
    }
    let mut raw = Vec::with_capacity(size);
    while raw.len() < size {
        let part = device.request(
            json!({
                "cmd": "radar_read",
                "crc32": crc,
                "offset": raw.len(),
            }),
            Duration::from_secs(3),
        )?;
        let encoded = part
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| ProtocolError("设备雷达分块不匹配".into()))?;
        let chunk = B64
            .decode(encoded)
            .map_err(|_| ProtocolError("设备雷达分块不匹配".into()))?;
        let max = READBACK_CHUNK.min(size - raw.len());
        if part.get("crc32").and_then(Value::as_str) != Some(crc.as_str())
            || part.get("offset").and_then(Value::as_u64).map(|v| v as usize) != Some(raw.len())
            || chunk.is_empty()
            || chunk.len() > max
        {
            return Err(ProtocolError("设备雷达分块不匹配".into()));
        }
        raw.extend_from_slice(&chunk);
    }
    if crc32_hex(&raw) != crc {
        return Err(ProtocolError("设备雷达 CRC 校验失败".into()));
    }
    let mut result = device.status()?;
    if result.get("mode").and_then(Value::as_str) == Some("home") {
        result = result
            .get("radar")
            .cloned()
            .ok_or_else(|| ProtocolError("回读期间设备画面已变化，请重新检测连接".into()))?;
    }
    if result.get("mode").and_then(Value::as_str) != Some("radar")
        || result.get("crc32").and_then(Value::as_str) != Some(crc.as_str())
    {
        return Err(ProtocolError(
            "回读期间设备画面已变化，请重新检测连接".into(),
        ));
    }
    let parsed: Value = serde_json::from_slice(&raw)
        .map_err(|_| ProtocolError("设备雷达数据不是合法 JSON".into()))?;
    if let Some(obj) = result.as_object_mut() {
        obj.insert("radar_data".into(), parsed);
    }
    Ok(result)
}

pub(crate) fn mirror_calendar_public(
    device: &mut HoloCubicUsb,
    state: Value,
    known_signature: Option<&str>,
) -> Result<Value, ProtocolError> {
    let signature = state
        .get("calendar_signature")
        .and_then(Value::as_str)
        .ok_or_else(|| ProtocolError("日历缺少签名".into()))?
        .to_owned();
    let pages = state
        .get("calendar_pages")
        .and_then(Value::as_array)
        .ok_or_else(|| ProtocolError("设备日历页数无效".into()))?;
    if !(1..=32).contains(&pages.len()) {
        return Err(ProtocolError("设备日历页数无效".into()));
    }
    if known_signature == Some(signature.as_str()) || pages.iter().all(|p| p.get("data").is_some())
    {
        return Ok(state);
    }
    let page_metas: Vec<(usize, String, usize)> = pages
        .iter()
        .enumerate()
        .map(|(i, page)| {
            let size = page
                .get("size")
                .and_then(Value::as_u64)
                .ok_or_else(|| ProtocolError("设备日历页面大小无效".into()))? as usize;
            let crc = page
                .get("crc32")
                .and_then(Value::as_str)
                .ok_or_else(|| ProtocolError("设备日历页面大小无效".into()))?
                .to_owned();
            if !(1..=512 * 1024).contains(&size) {
                return Err(ProtocolError("设备日历页面大小无效".into()));
            }
            Ok((i + 1, crc, size))
        })
        .collect::<Result<_, _>>()?;

    let mut page_data: Vec<Vec<u8>> = Vec::with_capacity(page_metas.len());
    for (index, crc, size) in &page_metas {
        let mut raw = Vec::with_capacity(*size);
        while raw.len() < *size {
            let part = device.request(
                json!({
                    "cmd": "calendar_read",
                    "signature": signature,
                    "page": index,
                    "offset": raw.len(),
                }),
                Duration::from_secs(3),
            )?;
            let encoded = part
                .get("data")
                .and_then(Value::as_str)
                .ok_or_else(|| ProtocolError("日历回读分块不匹配".into()))?;
            let chunk = B64
                .decode(encoded)
                .map_err(|_| ProtocolError("日历回读分块不匹配".into()))?;
            let max = READBACK_CHUNK.min(*size - raw.len());
            if part.get("page").and_then(Value::as_u64).map(|v| v as usize) != Some(*index)
                || part.get("offset").and_then(Value::as_u64).map(|v| v as usize) != Some(raw.len())
                || part.get("crc32").and_then(Value::as_str) != Some(crc.as_str())
                || chunk.is_empty()
                || chunk.len() > max
            {
                return Err(ProtocolError("日历回读分块不匹配".into()));
            }
            raw.extend_from_slice(&chunk);
        }
        if crc32_hex(&raw) != *crc {
            return Err(ProtocolError("日历回读 CRC 不匹配".into()));
        }
        page_data.push(raw);
    }

    let mut final_state = device.status()?;
    if final_state.get("mode").and_then(Value::as_str) == Some("home") {
        final_state = final_state
            .get("calendar")
            .cloned()
            .ok_or_else(|| ProtocolError("回读期间日历已变化，请重新检测连接".into()))?;
    }
    if final_state.get("mode").and_then(Value::as_str) != Some("calendar")
        || final_state.get("calendar_signature").and_then(Value::as_str) != Some(signature.as_str())
    {
        return Err(ProtocolError(
            "回读期间日历已变化，请重新检测连接".into(),
        ));
    }
    let latest_pages = final_state
        .get_mut("calendar_pages")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| ProtocolError("回读期间日历已变化，请重新检测连接".into()))?;
    if latest_pages.len() != page_data.len() {
        return Err(ProtocolError("回读期间日历页已变化".into()));
    }
    for (latest, (old_data, (_, old_crc, _))) in latest_pages
        .iter_mut()
        .zip(page_data.into_iter().zip(page_metas))
    {
        if latest.get("crc32").and_then(Value::as_str) != Some(old_crc.as_str()) {
            return Err(ProtocolError("回读期间日历页已变化".into()));
        }
        if let Some(obj) = latest.as_object_mut() {
            obj.insert(
                "data".into(),
                Value::Array(old_data.into_iter().map(|b| json!(b)).collect()),
            );
        }
    }
    Ok(final_state)
}

pub(crate) fn mirror_home_public(
    device: &mut HoloCubicUsb,
    mut state: Value,
    labels_crc: Option<&str>,
    radar_crc: Option<&str>,
    calendar_signature: Option<&str>,
    home_crcs: Option<&Value>,
) -> Result<Value, ProtocolError> {
    if state.get("home_protocol").and_then(Value::as_u64) != Some(1) {
        return Err(ProtocolError("设备不支持主页轮播回读".into()));
    }
    let mut transferred = false;
    if let Some(task) = state.get("task").cloned() {
        let uploaded = task.get("labels").and_then(Value::as_array).map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_u64().map(|n| n as u8))
                .collect::<Vec<u8>>()
        });
        let need =
            uploaded.is_none() && task.get("labels_crc").and_then(Value::as_str) != labels_crc;
        transferred |= need;
        let mut task = task;
        if let Some(obj) = task.as_object_mut() {
            obj.remove("labels");
        }
        let mirrored = device.mirror_labels(task, labels_crc, uploaded.as_deref())?;
        if let Some(obj) = state.as_object_mut() {
            obj.insert("task".into(), mirrored);
        }
    }
    if let Some(radar) = state.get("radar").cloned() {
        let need = radar.get("radar_data").is_none()
            && radar.get("crc32").and_then(Value::as_str) != radar_crc;
        transferred |= need;
        let mirrored = mirror_radar_public(device, radar, radar_crc)?;
        if let Some(obj) = state.as_object_mut() {
            obj.insert("radar".into(), mirrored);
        }
    }
    if let Some(calendar) = state.get("calendar").cloned() {
        let pages_complete = calendar
            .get("calendar_pages")
            .and_then(Value::as_array)
            .map(|p| p.iter().all(|page| page.get("data").is_some()))
            .unwrap_or(false);
        let need = calendar.get("calendar_signature").and_then(Value::as_str) != calendar_signature
            && !pages_complete;
        transferred |= need;
        let mirrored = mirror_calendar_public(device, calendar, calendar_signature)?;
        if let Some(obj) = state.as_object_mut() {
            obj.insert("calendar".into(), mirrored);
        }
    }

    let kinds: Vec<(String, usize, String)> = {
        let mut out = Vec::new();
        if let Some(images) = state.get("home_images").and_then(Value::as_object) {
            for (kind, picture) in images {
                if kind != "text" && kind != "image" {
                    return Err(ProtocolError("轮播图片元数据无效".into()));
                }
                let size = picture
                    .get("size")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| ProtocolError("轮播图片元数据无效".into()))?
                    as usize;
                let crc = picture
                    .get("crc32")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ProtocolError("轮播图片元数据无效".into()))?
                    .to_owned();
                if !(1..=512 * 1024).contains(&size) {
                    return Err(ProtocolError("轮播图片元数据无效".into()));
                }
                if picture.get("data").is_some() {
                    continue;
                }
                if home_crcs
                    .and_then(Value::as_object)
                    .and_then(|m| m.get(kind))
                    .and_then(Value::as_str)
                    == Some(crc.as_str())
                {
                    continue;
                }
                out.push((kind.clone(), size, crc));
            }
        }
        out
    };

    for (kind, size, crc) in &kinds {
        transferred = true;
        let mut raw = Vec::with_capacity(*size);
        while raw.len() < *size {
            let part = device.request(
                json!({
                    "cmd": "home_asset_read",
                    "kind": kind,
                    "crc32": crc,
                    "offset": raw.len(),
                }),
                Duration::from_secs(3),
            )?;
            let encoded = part
                .get("data")
                .and_then(Value::as_str)
                .ok_or_else(|| ProtocolError("轮播图片分块不匹配".into()))?;
            let chunk = B64
                .decode(encoded)
                .map_err(|_| ProtocolError("轮播图片分块不匹配".into()))?;
            let max = READBACK_CHUNK.min(*size - raw.len());
            if part.get("crc32").and_then(Value::as_str) != Some(crc.as_str())
                || part.get("offset").and_then(Value::as_u64).map(|v| v as usize) != Some(raw.len())
                || chunk.is_empty()
                || chunk.len() > max
            {
                return Err(ProtocolError("轮播图片分块不匹配".into()));
            }
            raw.extend_from_slice(&chunk);
        }
        if crc32_hex(&raw) != *crc {
            return Err(ProtocolError("轮播图片 CRC 校验失败".into()));
        }
        if let Some(picture) = state
            .pointer_mut(&format!("/home_images/{kind}"))
            .and_then(Value::as_object_mut)
        {
            picture.insert(
                "data".into(),
                Value::Array(raw.into_iter().map(|b| json!(b)).collect()),
            );
        }
    }

    if !transferred {
        return Ok(state);
    }
    let home_id = state
        .get("home_id")
        .cloned()
        .ok_or_else(|| ProtocolError("回读期间轮播配置已变化，请重新检测连接".into()))?;
    let mut latest = device.status()?;
    if latest.get("mode").and_then(Value::as_str) != Some("home")
        || latest.get("home_id") != Some(&home_id)
    {
        return Err(ProtocolError(
            "回读期间轮播配置已变化，请重新检测连接".into(),
        ));
    }
    for (key, checksum, payload) in [
        ("task", "labels_crc", "labels"),
        ("radar", "crc32", "radar_data"),
    ] {
        if let Some(old) = state.get(key) {
            let old_sum = old.get(checksum);
            let new_sum = latest.pointer(&format!("/{key}/{checksum}"));
            if old_sum != new_sum {
                return Err(ProtocolError("回读期间轮播内容已变化".into()));
            }
            if let Some(data) = old.get(payload) {
                if let Some(obj) = latest
                    .pointer_mut(&format!("/{key}"))
                    .and_then(Value::as_object_mut)
                {
                    obj.insert(payload.into(), data.clone());
                }
            }
        }
    }
    if let Some(images) = state.get("home_images").and_then(Value::as_object) {
        for (kind, picture) in images {
            let old_crc = picture.get("crc32");
            let new_crc = latest.pointer(&format!("/home_images/{kind}/crc32"));
            if old_crc != new_crc {
                return Err(ProtocolError("回读期间轮播图片已变化".into()));
            }
            if let Some(data) = picture.get("data") {
                if let Some(obj) = latest
                    .pointer_mut(&format!("/home_images/{kind}"))
                    .and_then(Value::as_object_mut)
                {
                    obj.insert("data".into(), data.clone());
                }
            }
        }
    }
    if let Some(cal) = state.get("calendar") {
        if cal.get("calendar_signature") != latest.pointer("/calendar/calendar_signature") {
            return Err(ProtocolError("回读期间日历已变化".into()));
        }
        if let (Some(old_pages), Some(new_pages)) = (
            cal.get("calendar_pages").and_then(Value::as_array),
            latest
                .pointer_mut("/calendar/calendar_pages")
                .and_then(Value::as_array_mut),
        ) {
            for (new, old) in new_pages.iter_mut().zip(old_pages) {
                if let Some(data) = old.get("data") {
                    if let Some(obj) = new.as_object_mut() {
                        obj.insert("data".into(), data.clone());
                    }
                }
            }
        }
    }
    Ok(latest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::PROTOCOL_PREFIX;
    // PROTOCOL_PREFIX used by FakePort framing helper

    /// In-memory serial stand-in for framing tests without hardware.
    struct FakePort {
        written: Vec<u8>,
        replies: Vec<u8>,
        read_pos: usize,
    }

    impl Read for FakePort {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.read_pos >= self.replies.len() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "timeout",
                ));
            }
            let n = (self.replies.len() - self.read_pos).min(buf.len());
            buf[..n].copy_from_slice(&self.replies[self.read_pos..self.read_pos + n]);
            self.read_pos += n;
            Ok(n)
        }
    }

    impl Write for FakePort {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            // Decode command and push a matching ACK into replies.
            if let Some(line_end) = buf.iter().position(|&b| b == b'\n') {
                let line = &buf[..=line_end];
                if line.starts_with(PROTOCOL_PREFIX) {
                    if let Ok(cmd) = serde_json::from_slice::<Value>(&line[PROTOCOL_PREFIX.len()..line.len()-1]) {
                        let id = cmd.get("id").cloned().unwrap_or(json!("0"));
                        let reply = json!({"id": id, "status": "ok", "app": "usb_display", "mode": "idle", "width": 320, "height": 240});
                        let mut packet = PROTOCOL_PREFIX.to_vec();
                        packet.extend_from_slice(serde_json::to_string(&reply).unwrap().as_bytes());
                        packet.push(b'\n');
                        self.replies.extend_from_slice(&packet);
                    }
                }
            }
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    // serialport::SerialPort is a large trait; unit tests for request path live in protocol.
    // Here we only cover port filtering helpers offline.

    #[test]
    fn find_port_respects_explicit_name() {
        let port = find_holocubic_port(Some("COM4")).unwrap();
        assert_eq!(port, "COM4");
    }

    #[test]
    fn port_info_json_includes_vid_pid_fields() {
        let info = PortInfo {
            device: "/dev/ttyACM0".into(),
            description: "USB JTAG".into(),
            vid: Some(USB_VID),
            pid: Some(USB_PID),
            serial_number: Some("ABC".into()),
        };
        let value = info.to_json();
        assert_eq!(value["vid"], USB_VID);
        assert_eq!(value["pid"], USB_PID);
        assert_eq!(value["device"], "/dev/ttyACM0");
    }

    #[test]
    fn unused_fake_port_compiles_helpers() {
        let mut fake = FakePort {
            written: vec![],
            replies: vec![],
            read_pos: 0,
        };
        let _ = fake.write(b"x");
        let mut buf = [0u8; 4];
        let err = fake.read(&mut buf).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
    }
}
