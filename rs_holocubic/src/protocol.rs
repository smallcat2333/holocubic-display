//! `@HCUSB/1` framing helpers shared by the in-process USB layer.
//!
//! Wire format matches `holo_usb_display.py`: prefix + compact ASCII JSON + `\n`.
//! CRC32 is the lower 32 bits of zlib/IEEE CRC, rendered as an 8-char lowercase hex string.

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde_json::Value;

/// Exact device wire prefix, including the trailing space.
pub const PROTOCOL_PREFIX: &[u8] = b"@HCUSB/1 ";
/// JPEG and response_read chunk size used by the Lua app.
pub const CHUNK_SIZE: usize = 96;
/// Label / radar / home-asset readback uses 192-byte chunks.
pub const READBACK_CHUNK: usize = 192;
pub const USB_VID: u16 = 0x303A;
pub const USB_PID: u16 = 0x1001;

/// Host-side USB protocol error (mirrors Python `HoloCubicError`).
#[derive(Debug, Clone)]
pub struct ProtocolError(pub String);

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ProtocolError {}

impl From<&str> for ProtocolError {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<String> for ProtocolError {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// zlib-compatible CRC32 as an 8-character lowercase hex string.
pub fn crc32_hex(data: &[u8]) -> String {
    format!("{:08x}", crc32fast::hash(data))
}

/// Encode one outbound command line: `@HCUSB/1 ` + JSON + `\n`.
pub fn encode_frame(command: &Value) -> Result<Vec<u8>, ProtocolError> {
    let body = serde_json::to_string(command).map_err(|e| ProtocolError(e.to_string()))?;
    let mut out = Vec::with_capacity(PROTOCOL_PREFIX.len() + body.len() + 1);
    out.extend_from_slice(PROTOCOL_PREFIX);
    out.extend_from_slice(body.as_bytes());
    out.push(b'\n');
    Ok(out)
}

/// Parse one complete line. Non-prefix / malformed JSON lines yield `Ok(None)`.
pub fn decode_line(line: &[u8]) -> Result<Option<Value>, ProtocolError> {
    let trimmed = line.strip_suffix(&[b'\n']).unwrap_or(line);
    let trimmed = trimmed.strip_suffix(&[b'\r']).unwrap_or(trimmed);
    if !trimmed.starts_with(PROTOCOL_PREFIX) {
        return Ok(None);
    }
    let payload = &trimmed[PROTOCOL_PREFIX.len()..];
    let text = match std::str::from_utf8(payload) {
        Ok(text) => text,
        Err(_) => return Ok(None),
    };
    match serde_json::from_str::<Value>(text) {
        Ok(value) => Ok(Some(value)),
        Err(_) => Ok(None),
    }
}

/// Split binary payload into ordered base64 chunks of at most `CHUNK_SIZE` bytes.
pub fn jpeg_chunks(data: &[u8]) -> Vec<(usize, String)> {
    data.chunks(CHUNK_SIZE)
        .enumerate()
        .map(|(seq, chunk)| (seq, B64.encode(chunk)))
        .collect()
}

/// Validate multipart metadata from a `status: multipart` header.
pub fn validate_multipart_meta(response: &Value) -> Result<(usize, String), ProtocolError> {
    let size = response
        .get("size")
        .and_then(Value::as_u64)
        .ok_or_else(|| ProtocolError("USB 分块回执元数据无效。".into()))? as usize;
    let checksum = response
        .get("crc32")
        .and_then(Value::as_str)
        .ok_or_else(|| ProtocolError("USB 分块回执元数据无效。".into()))?
        .to_owned();
    if !(1..=16384).contains(&size) || checksum.len() != 8 {
        return Err(ProtocolError("USB 分块回执元数据无效。".into()));
    }
    Ok((size, checksum))
}

/// Decode one `response_read` part and verify offset / length against remaining bytes.
pub fn decode_response_part(
    part: &Value,
    expected_offset: usize,
    remaining: usize,
) -> Result<Vec<u8>, ProtocolError> {
    if part.get("status").and_then(Value::as_str) != Some("part") {
        return Err(ProtocolError("USB 分块回执顺序或长度不匹配。".into()));
    }
    let offset = part
        .get("offset")
        .and_then(Value::as_u64)
        .ok_or_else(|| ProtocolError("USB 分块回执顺序或长度不匹配。".into()))?
        as usize;
    if offset != expected_offset {
        return Err(ProtocolError("USB 分块回执顺序或长度不匹配。".into()));
    }
    let encoded = part
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(|| ProtocolError("USB 分块回执顺序或长度不匹配。".into()))?;
    let chunk = B64
        .decode(encoded)
        .map_err(|_| ProtocolError("USB 分块回执顺序或长度不匹配。".into()))?;
    let max = CHUNK_SIZE.min(remaining);
    if chunk.is_empty() || chunk.len() > max {
        return Err(ProtocolError("USB 分块回执顺序或长度不匹配。".into()));
    }
    Ok(chunk)
}

/// After multipart bytes are assembled, verify CRC and parse the inner JSON object.
pub fn assemble_multipart_body(
    data: &[u8],
    checksum: &str,
    request_id: &str,
) -> Result<Value, ProtocolError> {
    if crc32_hex(data) != checksum {
        return Err(ProtocolError("USB 分块回执 CRC32 不匹配。".into()));
    }
    let response: Value = serde_json::from_slice(data)
        .map_err(|_| ProtocolError("USB 分块回执不是合法 JSON。".into()))?;
    if response.get("id").and_then(Value::as_str) != Some(request_id) {
        return Err(ProtocolError("USB 分块回执身份不匹配。".into()));
    }
    if !response.is_object() {
        return Err(ProtocolError("USB 分块回执身份不匹配。".into()));
    }
    Ok(response)
}

/// True when `target` is a supported image upload destination (incl. calendar slots).
pub fn is_valid_image_target(target: &str) -> bool {
    matches!(
        target,
        "frame" | "task_labels" | "home_text" | "home_image"
    ) || is_calendar_target(target)
}

fn is_calendar_target(target: &str) -> bool {
    let bytes = target.as_bytes();
    if bytes.len() != 13 || !target.starts_with("calendar_") {
        return false;
    }
    let slot = bytes[9];
    if slot != b'a' && slot != b'b' {
        return false;
    }
    if bytes[10] != b'_' {
        return false;
    }
    let n = std::str::from_utf8(&bytes[11..])
        .ok()
        .and_then(|s| s.parse::<u32>().ok());
    matches!(n, Some(1..=32))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn crc32_matches_python_zlib_style() {
        // zlib.crc32(b"123456789") & 0xffffffff == 0xcbf43926
        assert_eq!(crc32_hex(b"123456789"), "cbf43926");
        assert_eq!(crc32_hex(b""), "00000000");
        let payload: Vec<u8> = (0..200).map(|i| i as u8).collect();
        assert_eq!(crc32_hex(&payload).len(), 8);
    }

    #[test]
    fn encode_frame_uses_prefix_and_newline() {
        let frame = encode_frame(&json!({"cmd":"hello","id":"1-1"})).unwrap();
        assert!(frame.starts_with(PROTOCOL_PREFIX));
        assert!(frame.ends_with(b"\n"));
        let body = std::str::from_utf8(&frame[PROTOCOL_PREFIX.len()..frame.len() - 1]).unwrap();
        assert!(body.contains("\"cmd\":\"hello\""));
        assert!(!body.contains(' '));
    }

    #[test]
    fn decode_ignores_non_prefix_and_bad_json() {
        assert!(decode_line(b"noise\n").unwrap().is_none());
        assert!(decode_line(b"@HCUSB/1 not-json\n").unwrap().is_none());
        let ok = decode_line(b"@HCUSB/1 {\"id\":\"1\",\"status\":\"ok\"}\n")
            .unwrap()
            .unwrap();
        assert_eq!(ok["id"], "1");
    }

    #[test]
    fn jpeg_chunks_are_96_bytes_base64() {
        let data: Vec<u8> = (0..200).map(|i| i as u8).collect();
        let chunks = jpeg_chunks(&data);
        assert_eq!(chunks.len(), 3);
        assert_eq!((chunks[0].0, chunks[1].0, chunks[2].0), (0, 1, 2));
        assert_eq!(B64.decode(&chunks[0].1).unwrap().len(), 96);
        assert_eq!(B64.decode(&chunks[1].1).unwrap().len(), 96);
        assert_eq!(B64.decode(&chunks[2].1).unwrap().len(), 8);
    }

    #[test]
    fn multipart_assembly_checks_crc_and_id() {
        let inner = json!({"id":"t-1","status":"ok","mode":"home","padding":"x".repeat(100)});
        let raw = serde_json::to_vec(&inner).unwrap();
        let checksum = crc32_hex(&raw);
        let assembled = assemble_multipart_body(&raw, &checksum, "t-1").unwrap();
        assert_eq!(assembled["mode"], "home");
        assert!(assemble_multipart_body(&raw, "deadbeef", "t-1").is_err());
        assert!(assemble_multipart_body(&raw, &checksum, "other").is_err());
    }

    #[test]
    fn response_part_rejects_bad_offset_or_length() {
        let chunk = vec![1u8; 10];
        let part = json!({
            "status":"part",
            "offset":0,
            "data": B64.encode(&chunk)
        });
        assert_eq!(decode_response_part(&part, 0, 50).unwrap(), chunk);
        assert!(decode_response_part(&part, 1, 50).is_err());
        let too_big = json!({
            "status":"part",
            "offset":0,
            "data": B64.encode(&vec![0u8; 97])
        });
        assert!(decode_response_part(&too_big, 0, 200).is_err());
    }

    #[test]
    fn image_target_validation_matches_python() {
        assert!(is_valid_image_target("frame"));
        assert!(is_valid_image_target("task_labels"));
        assert!(is_valid_image_target("home_text"));
        assert!(is_valid_image_target("calendar_a_01"));
        assert!(is_valid_image_target("calendar_b_32"));
        assert!(!is_valid_image_target("calendar_a_1")); // Python requires len==13 (%02d)
        assert!(!is_valid_image_target("calendar_c_01"));
        assert!(!is_valid_image_target("calendar_a_00"));
        assert!(!is_valid_image_target("calendar_a_33"));
        assert!(!is_valid_image_target("other"));
    }

    #[test]
    fn validate_multipart_meta_bounds() {
        assert!(validate_multipart_meta(&json!({"size":100,"crc32":"abcdef01"})).is_ok());
        assert!(validate_multipart_meta(&json!({"size":0,"crc32":"abcdef01"})).is_err());
        assert!(validate_multipart_meta(&json!({"size":20000,"crc32":"abcdef01"})).is_err());
        assert!(validate_multipart_meta(&json!({"size":10,"crc32":"abcd"})).is_err());
    }
}
