//! AI radar read-only collect: local SQLite + public site, no USB.

use chrono::{FixedOffset, TimeZone};
use regex::Regex;
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

const SITE: &str = "https://codex-reset-radar.pages.dev";

fn china_tz() -> FixedOffset {
    FixedOffset::east_opt(8 * 3600).unwrap()
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn open_ro(path: &Path) -> Result<Connection, String> {
    let uri = format!("file:{}?mode=ro", path.to_string_lossy().replace('\\', "/"));
    let db = Connection::open_with_flags(
        &uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|e| e.to_string())?;
    let _ = db.execute_batch("PRAGMA query_only=ON");
    Ok(db)
}

fn load_cache(path: &Path) -> Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}))
}

fn save_cache(path: &Path, value: &Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let pending = path.with_extension("tmp");
    let text = serde_json::to_string(value).map_err(|e| e.to_string())?;
    {
        let mut f = File::create(&pending).map_err(|e| e.to_string())?;
        f.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
    }
    std::fs::rename(&pending, path).map_err(|e| e.to_string())
}

fn context_line(line: &[u8]) -> Result<Option<Value>, String> {
    let head = &line[..line.len().min(180)];
    if !head.windows(20).any(|w| w == br#""type":"turn_context""#)
        && !head.windows(22).any(|w| w == br#""type": "turn_context""#)
    {
        // Fast reject like Python's `in line[:180]`.
        let as_str = String::from_utf8_lossy(head);
        if !as_str.contains("\"type\":\"turn_context\"") && !as_str.contains("\"type\": \"turn_context\"")
        {
            return Ok(None);
        }
    }
    let item: Value = serde_json::from_slice(line).map_err(|e| e.to_string())?;
    if item.get("type").and_then(|v| v.as_str()) != Some("turn_context") {
        return Ok(None);
    }
    let payload = item
        .get("payload")
        .ok_or_else(|| "模型/强度字段无效".to_string())?;
    let model = payload
        .get("model")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "模型/强度字段无效".to_string())?;
    let effort = match payload.get("effort") {
        None | Some(Value::Null) => "unknown".to_string(),
        Some(Value::String(s)) => s.clone(),
        _ => return Err("模型/强度字段无效".into()),
    };
    Ok(Some(json!({"model": model, "effort": effort})))
}

fn latest_context(path: &Path, cached: &mut Value) -> Result<Value, String> {
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    let size = meta.len();
    #[cfg(unix)]
    let inode = {
        use std::os::unix::fs::MetadataExt;
        meta.ino()
    };
    #[cfg(not(unix))]
    let inode = 0u64;

    let key = path.to_string_lossy().to_string();
    let previous = cached.get(&key).cloned().unwrap_or(json!({}));
    let mut file = File::open(path).map_err(|e| e.to_string())?;

    let prev_inode = previous.get("inode").and_then(|v| v.as_u64());
    let prev_offset = previous.get("offset").and_then(|v| v.as_u64()).unwrap_or(size + 1);

    let (latest, offset) = if prev_inode == Some(inode) && prev_offset <= size {
        let offset = prev_offset;
        file.seek(SeekFrom::Start(offset)).map_err(|e| e.to_string())?;
        let mut block = Vec::new();
        file.read_to_end(&mut block).map_err(|e| e.to_string())?;
        let complete = match block.iter().rposition(|&b| b == b'\n') {
            Some(i) => i + 1,
            None => 0,
        };
        let mut latest = previous.get("context").cloned().unwrap_or(Value::Null);
        for line in block[..complete].split(|b| *b == b'\n') {
            if line.is_empty() {
                continue;
            }
            if let Some(result) = context_line(line)? {
                latest = result;
            }
        }
        (latest, offset + complete as u64)
    } else {
        let mut position = size as i64;
        let mut tail: Vec<u8> = Vec::new();
        let mut latest = None;
        let mut offset = size;
        let mut first = true;
        while position > 0 && latest.is_none() {
            let length = position.min(1024 * 1024) as u64;
            position -= length as i64;
            file.seek(SeekFrom::Start(position as u64))
                .map_err(|e| e.to_string())?;
            let mut block = vec![0u8; length as usize];
            file.read_exact(&mut block).map_err(|e| e.to_string())?;
            block.extend_from_slice(&tail);
            let mut lines: Vec<&[u8]> = block.split(|b| *b == b'\n').collect();
            if first {
                offset = position as u64
                    + block
                        .iter()
                        .rposition(|&b| b == b'\n')
                        .map(|i| i as u64 + 1)
                        .unwrap_or(0);
                lines.pop(); // drop possibly incomplete last line
                first = false;
            }
            tail = if position > 0 {
                lines.first().map(|l| l.to_vec()).unwrap_or_default()
            } else {
                Vec::new()
            };
            if position > 0 && !lines.is_empty() {
                lines.remove(0);
            }
            for line in lines.into_iter().rev() {
                if line.is_empty() {
                    continue;
                }
                if let Some(result) = context_line(line)? {
                    latest = Some(result);
                    break;
                }
            }
        }
        (
            latest.ok_or_else(|| "任务尚无模型记录".to_string())?,
            offset,
        )
    };

    if latest.is_null() {
        return Err("任务尚无模型记录".into());
    }
    cached[&key] = json!({
        "inode": inode,
        "offset": offset,
        "context": latest,
    });
    Ok(cached[&key]["context"].clone())
}

fn task_counts(state_path: &Path, cache_path: &Path) -> Result<(Value, Vec<String>), String> {
    let mut cached = load_cache(cache_path);
    let mut warnings = Vec::new();
    let mut selected = Vec::new();
    let mut paths = Vec::new();
    {
        let db = open_ro(state_path)?;
        let mut stmt = db
            .prepare(
                "SELECT rollout_path FROM threads
            WHERE source IN ('cli','vscode','exec','appServer')
              AND (thread_source IS NULL OR thread_source='user')
            ORDER BY COALESCE(recency_at_ms,updated_at_ms,updated_at*1000) DESC LIMIT 30",
            )
            .map_err(|e| e.to_string())?;
        let rows: Vec<String> = stmt
            .query_map([], |row| row.get(0))
            .map_err(|e| e.to_string())?
            .collect::<Result<_, _>>()
            .map_err(|e| e.to_string())?;
        drop(stmt);
        drop(db);
        for rollout in rows {
            match latest_context(Path::new(&rollout), &mut cached) {
                Ok(ctx) => {
                    selected.push(ctx);
                    paths.push(rollout);
                }
                Err(error) => warnings.push(format!(
                    "一条任务元数据未读取：{}",
                    error.split_whitespace().next().unwrap_or("Error")
                )),
            }
            if selected.len() == 10 {
                break;
            }
        }
    }
    let mut keep = json!({});
    for path in &paths {
        if let Some(v) = cached.get(path) {
            keep[path] = v.clone();
        }
    }
    save_cache(cache_path, &keep)?;
    let mut counts: HashMap<(String, String), u64> = HashMap::new();
    for row in &selected {
        let model = row["model"].as_str().unwrap_or("").to_string();
        let effort = row["effort"].as_str().unwrap_or("unknown").to_string();
        *counts.entry((model, effort)).or_default() += 1;
    }
    let mut combos: Vec<_> = counts
        .into_iter()
        .map(|((model, effort), count)| json!({"model": model, "effort": effort, "count": count}))
        .collect();
    combos.sort_by(|a, b| {
        let ca = b["count"].as_u64().unwrap_or(0).cmp(&a["count"].as_u64().unwrap_or(0));
        if ca != std::cmp::Ordering::Equal {
            return ca;
        }
        let ma = a["model"].as_str().unwrap_or("");
        let mb = b["model"].as_str().unwrap_or("");
        ma.cmp(mb).then_with(|| {
            a["effort"]
                .as_str()
                .unwrap_or("")
                .cmp(b["effort"].as_str().unwrap_or(""))
        })
    });
    Ok((
        json!({"total": selected.len(), "combos": combos}),
        warnings,
    ))
}

fn usage_counts(db_path: &Path, now: i64) -> Result<Value, String> {
    let start = now - 86400;
    let db = open_ro(db_path)?;
    let mut stmt = db
        .prepare(
            "SELECT model, CAST((created_at-?1)/900 AS INTEGER) AS bucket,
                COUNT(*) AS n, SUM(input_tokens+output_tokens+cache_read_tokens+cache_creation_tokens=0) AS missing
            FROM proxy_request_logs WHERE app_type='codex' AND data_source='proxy'
              AND status_code>=200 AND status_code<300 AND (error_message IS NULL OR error_message='')
              AND created_at>=?2 AND created_at<?3 GROUP BY model,bucket",
        )
        .map_err(|e| e.to_string())?;
    let mut models: HashMap<String, Value> = HashMap::new();
    let mut missing = 0i64;
    let rows = stmt
        .query_map(rusqlite::params![start, start, now], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })
        .map_err(|e| e.to_string())?;
    for row in rows {
        let (model, bucket, n, miss) = row.map_err(|e| e.to_string())?;
        if model.is_empty() || !(0..96).contains(&bucket) {
            return Err("代理模型或时间字段无效".into());
        }
        let entry = models.entry(model.clone()).or_insert_with(|| {
            json!({
                "model": model,
                "effort": "unknown",
                "count": 0,
                "bins": vec![0u64; 96],
            })
        });
        entry["count"] = json!(entry["count"].as_u64().unwrap_or(0) + n as u64);
        if let Some(bins) = entry.get_mut("bins").and_then(|v| v.as_array_mut()) {
            if let Some(Value::Number(num)) = bins.get_mut(bucket as usize) {
                let cur = num.as_u64().unwrap_or(0);
                bins[bucket as usize] = json!(cur + n as u64);
            }
        }
        missing += miss;
    }
    drop(stmt);
    drop(db);
    let mut values: Vec<Value> = models.into_values().collect();
    values.sort_by(|a, b| {
        b["count"]
            .as_u64()
            .unwrap_or(0)
            .cmp(&a["count"].as_u64().unwrap_or(0))
            .then_with(|| {
                a["model"]
                    .as_str()
                    .unwrap_or("")
                    .cmp(b["model"].as_str().unwrap_or(""))
            })
    });
    let total: u64 = values.iter().map(|v| v["count"].as_u64().unwrap_or(0)).sum();
    let labels: Vec<String> = [0i64, 6, 12, 18, 24]
        .into_iter()
        .map(|n| {
            china_tz()
                .timestamp_opt(start + n * 3600, 0)
                .single()
                .map(|t| t.format("%H:%M").to_string())
                .unwrap_or_else(|| "00:00".into())
        })
        .collect();
    Ok(json!({
        "total": total,
        "models": values,
        "missing_usage": missing,
        "start": start,
        "end": now,
        "labels": labels,
    }))
}

fn public_data(cache_dir: &Path, name: &str, url: &str) -> Result<(String, String), String> {
    let path = cache_dir.join(format!("{name}.json"));
    let cached = load_cache(&path);
    let now = now_secs() as f64;
    if cached
        .get("expires")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0)
        > now
    {
        return Ok((
            cached["body"].as_str().unwrap_or("").to_string(),
            cached
                .get("warning")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        ));
    }
    let mut req = ureq::get(url)
        .set("User-Agent", "HoloCubic-Radar/0.1")
        .set("Accept-Encoding", "gzip");
    if let Some(etag) = cached.get("etag").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
        req = req.set("If-None-Match", etag);
    }
    if let Some(modified) = cached
        .get("modified")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        req = req.set("If-Modified-Since", modified);
    }
    let (body, etag, modified, cache_control, edge) = match req.call() {
        Ok(resp) => {
            let etag = resp.header("ETag").unwrap_or("").to_string();
            let modified = resp.header("Last-Modified").unwrap_or("").to_string();
            let cache_control = resp.header("Cache-Control").unwrap_or("").to_string();
            let edge = resp.header("X-Codex-Cache").unwrap_or("").to_string();
            let body = resp.into_string().map_err(|e| e.to_string())?;
            if body.len() > 8 * 1024 * 1024 {
                return Err("网站响应超过8MB".into());
            }
            (body, etag, modified, cache_control, edge)
        }
        Err(ureq::Error::Status(304, resp)) => {
            if cached.get("body").is_none() {
                return Err("HTTP 304 without cache".into());
            }
            let etag = resp.header("ETag").unwrap_or("").to_string();
            let modified = resp.header("Last-Modified").unwrap_or("").to_string();
            let cache_control = resp.header("Cache-Control").unwrap_or("").to_string();
            let edge = resp.header("X-Codex-Cache").unwrap_or("").to_string();
            (
                cached["body"].as_str().unwrap_or("").to_string(),
                etag,
                modified,
                cache_control,
                edge,
            )
        }
        Err(err) => {
            if let Some(body) = cached.get("body").and_then(|v| v.as_str()) {
                return Ok((
                    body.to_string(),
                    format!("网站暂不可用，显示旧缓存：{}", err_name(&err)),
                ));
            }
            return Err(err.to_string());
        }
    };
    let max_age_re = max_age_regex();
    let ttl = max_age_re
        .captures(&cache_control)
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse::<i64>().ok())
        .unwrap_or(30)
        .clamp(5, 300);
    let mut warning = String::new();
    let mut ttl = ttl;
    if edge.starts_with("STALE") || edge == "ERROR" {
        warning = format!("网站返回缓存数据（{edge}）");
        ttl = 5;
    }
    let record = json!({
        "body": body,
        "expires": now + ttl as f64,
        "etag": etag,
        "modified": modified,
        "warning": warning,
    });
    let _ = save_cache(&path, &record);
    Ok((body, warning))
}

fn max_age_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?:^|,)\s*max-age=(\d+)").unwrap())
}

fn err_name(err: &ureq::Error) -> &'static str {
    match err {
        ureq::Error::Status(_, _) => "HTTPError",
        _ => "Error",
    }
}

fn finite(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64().filter(|v| v.is_finite()),
        _ => None,
    }
}

fn coding_scores(software: &Value) -> Result<Value, String> {
    if software.get("schema").and_then(|v| v.as_u64()) != Some(3)
        || software.get("mode").and_then(|v| v.as_str()) != Some("equal_latest_3")
    {
        return Err("雷达评分接口结构已变化".into());
    }
    let mut values = Vec::new();
    for left in software
        .get("points")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "雷达评分接口结构已变化".to_string())?
    {
        let total = finite(&left["total"]);
        let score = finite(&left["iq"]);
        if total.map(|t| t <= 0.0).unwrap_or(true) || score.map(|s| s < 0.0).unwrap_or(true) {
            continue;
        }
        let mut item = json!({
            "model": left["model"],
            "effort": left["effort"],
        });
        for (source, target) in [
            ("iq", "score"),
            ("average_price_usd", "price"),
            ("average_minutes", "minutes"),
        ] {
            item[target] = match finite(&left[source]) {
                Some(v) => json!(v),
                None => Value::Null,
            };
        }
        values.push(item);
    }
    if values.is_empty() {
        return Err("暂无有效的编码评分".into());
    }
    let updated = software
        .get("source_updated_at")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "雷达评分接口结构已变化".to_string())?;
    let parsed = chrono::DateTime::parse_from_rfc3339(&updated.replace('Z', "+00:00"))
        .or_else(|_| {
            chrono::DateTime::parse_from_str(updated, "%Y-%m-%dT%H:%M:%S%.f%z")
        })
        .map_err(|_| "雷达评分接口结构已变化".to_string())?;
    let label = parsed
        .with_timezone(&china_tz())
        .format("%m-%d %H:%M")
        .to_string();
    Ok(json!({"points": values, "updated": label}))
}

fn parse_reset_html(html: &str) -> Result<Value, String> {
    // Lightweight extraction mirroring ResetParser for the announcement section.
    let section_re = Regex::new(
        r#"(?is)<section[^>]*class="[^"]*site-announcement-reset[^"]*"[^>]*>(.*?)</section>"#,
    )
    .unwrap();
    let section = section_re
        .captures(html)
        .map(|c| c.get(0).unwrap().as_str().to_string())
        .ok_or_else(|| "网站当前没有可识别的重置公告".to_string())?;

    fn field(section: &str, class_suffix: &str) -> String {
        let re = Regex::new(&format!(
            r#"(?is)class="[^"]*site-announcement-{class_suffix}[^"]*"[^>]*>(.*?)</"#
        ))
        .unwrap();
        re.captures(section)
            .map(|c| {
                let raw = c.get(1).unwrap().as_str();
                let no_tags = Regex::new(r"(?is)<[^>]+>").unwrap().replace_all(raw, "");
                no_tags.split_whitespace().collect::<Vec<_>>().join(" ")
            })
            .unwrap_or_default()
    }

    let headline = field(&section, "headline");
    if headline.is_empty() {
        return Err("网站当前没有可识别的重置公告".into());
    }
    let lead = field(&section, "lead");
    let detail = field(&section, "reset-detail");
    let deadline = Regex::new(r#"data-window-closes-at="([^"]+)""#)
        .unwrap()
        .captures(&section)
        .and_then(|c| {
            let raw = c.get(1).unwrap().as_str().replace('Z', "+00:00");
            chrono::DateTime::parse_from_rfc3339(&raw)
                .ok()
                .map(|t| t.timestamp())
        });
    let expired = Regex::new(r#"data-expired-text="([^"]*)""#)
        .unwrap()
        .captures(&section)
        .map(|c| c.get(1).unwrap().as_str().to_string())
        .unwrap_or_else(|| "等待网站确认重置状态".into());
    let source = Regex::new(r#"(?is)<a[^>]*class="[^"]*site-announcement-reset-source[^"]*"[^>]*href="([^"]*)""#)
        .unwrap()
        .captures(&section)
        .map(|c| c.get(1).unwrap().as_str().to_string())
        .unwrap_or_default();
    Ok(json!({
        "headline": headline,
        "lead": lead,
        "detail": detail,
        "deadline": deadline,
        "source": source,
        "expired": expired,
    }))
}

pub fn collect(cache_dir: &Path, profile: Option<&Path>) -> Result<Value, String> {
    let profile = profile
        .map(Path::to_path_buf)
        .unwrap_or_else(|| {
            std::env::var_os("USERPROFILE")
                .or_else(|| std::env::var_os("HOME"))
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
        });
    std::fs::create_dir_all(cache_dir).map_err(|e| e.to_string())?;
    let now = now_secs();
    let mut errors: Vec<Value> = Vec::new();
    let mut tasks = Value::Null;
    let mut usage = Value::Null;
    let mut scores = Value::Null;
    let mut reset = Value::Null;

    match task_counts(
        &profile.join(".codex/state_5.sqlite"),
        &cache_dir.join("contexts.json"),
    ) {
        Ok((value, warnings)) => {
            tasks = value;
            errors.extend(warnings.into_iter().map(Value::String));
        }
        Err(error) => errors.push(json!(format!("Codex 任务读取失败：{error}"))),
    }
    match usage_counts(&profile.join(".cc-switch/cc-switch.db"), now) {
        Ok(value) => usage = value,
        Err(error) => errors.push(json!(format!("CC Switch 读取失败：{error}"))),
    }

    let jobs = [
        ("software", "/api/intelligence-efficiency-metrics"),
        ("reset_page", "/"),
    ];
    let mut bodies: HashMap<&str, String> = HashMap::new();
    for (name, endpoint) in jobs {
        match public_data(cache_dir, name, &format!("{SITE}{endpoint}")) {
            Ok((body, warning)) => {
                if !warning.is_empty() {
                    errors.push(json!(format!("{name}：{warning}")));
                }
                bodies.insert(name, body);
            }
            Err(error) => errors.push(json!(format!("{name} 读取失败：{error}"))),
        }
    }
    if let Some(software) = bodies.get("software") {
        match serde_json::from_str::<Value>(software) {
            Ok(v) => match coding_scores(&v) {
                Ok(value) => scores = value,
                Err(e) => errors.push(json!(format!("评分解析失败：{e}"))),
            },
            Err(error) => errors.push(json!(format!("评分解析失败：{error}"))),
        }
    }
    if let Some(page) = bodies.get("reset_page") {
        match parse_reset_html(page) {
            Ok(value) => reset = value,
            Err(error) => errors.push(json!(format!("重置公告解析失败：{error}"))),
        }
    }
    let result = json!({
        "collected_at": now,
        "collected_label": china_tz()
            .timestamp_opt(now, 0)
            .single()
            .map(|t| t.format("%H:%M:%S").to_string())
            .unwrap_or_else(|| "00:00:00".into()),
        "tasks": tasks,
        "usage": usage,
        "scores": scores,
        "reset": reset,
        "errors": errors,
    });
    Ok(result)
}

/// Compact snapshot into device RAM JSON payload (radar_usb.compact).
pub fn compact(snapshot: &Value, now: Option<i64>) -> Result<Value, String> {
    let now = now.unwrap_or_else(now_secs);
    let collected_at = snapshot
        .get("collected_at")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| "雷达数据已过期，请等待新采集结果".to_string())?;
    let age = now - collected_at;
    if !(0..=30).contains(&age) {
        return Err("雷达数据已过期，请等待新采集结果".into());
    }
    let models: HashMap<&str, (&str, u32)> = [
        ("gpt-6-astra", ("Astra", 0xFF7E1D)),
        ("gpt-5.6-sol", ("Sol", 0xF2C20E)),
        ("gpt-5.6-terra", ("Terra", 0x589FFF)),
        ("gpt-5.6-luna", ("Luna", 0xB6C4D7)),
        ("gpt-5.5", ("5.5", 0x24D7EA)),
    ]
    .into_iter()
    .collect();

    fn bounded(value: i64, maximum: i64) -> Result<i64, String> {
        if !(0..=maximum).contains(&value) {
            return Err("雷达计数超出设备范围".into());
        }
        Ok(value)
    }
    fn ascii_label(value: &str, length: usize) -> String {
        value
            .chars()
            .map(|c| if c.is_ascii() { c } else { '?' })
            .take(length)
            .collect()
    }

    let mut cards = Vec::new();
    let scores: HashMap<(String, String), &Value> = snapshot
        .pointer("/scores/points")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|p| {
            Some((
                (
                    p.get("model")?.as_str()?.to_string(),
                    p.get("effort")?.as_str()?.to_string(),
                ),
                p,
            ))
        })
        .collect();
    let combos = snapshot
        .pointer("/tasks/combos")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for combo in combos {
        let model = combo.get("model").and_then(|v| v.as_str()).unwrap_or("");
        let effort = combo.get("effort").and_then(|v| v.as_str()).unwrap_or("unknown");
        let fallback = ascii_label(model, 10);
        let (name, color) = models
            .get(model)
            .map(|(n, c)| (*n, *c))
            .unwrap_or((fallback.as_str(), 0xB186EF));
        let score = scores.get(&(model.to_string(), effort.to_string()));
        let mut values = Vec::new();
        for (key, scale) in [("score", 100.0), ("price", 100.0), ("minutes", 10.0)] {
            match score.and_then(|s| s.get(key)) {
                None | Some(Value::Null) => values.push(json!(-1)),
                Some(v) => {
                    let num = finite(v).ok_or_else(|| "编码评分字段无效".to_string())?;
                    if num < 0.0 {
                        return Err("编码评分字段无效".into());
                    }
                    values.push(json!(bounded((num * scale).round() as i64, 10_000_000)?));
                }
            }
        }
        let caption = if effort == "unknown" {
            name.to_string()
        } else {
            format!("{name} {effort}")
        };
        let count = combo.get("count").and_then(|v| v.as_i64()).unwrap_or(0);
        let mut card = vec![
            json!(ascii_label(&caption, 20)),
            json!(bounded(count, 10)?),
            json!(color),
        ];
        card.extend(values);
        cards.push(Value::Array(card));
    }
    if cards.len() > 10 {
        return Err("任务卡片超过十种组合".into());
    }

    let usage = snapshot.get("usage");
    let mut series = Vec::new();
    let total = if usage.map(|u| u.is_null()).unwrap_or(true) {
        -1
    } else {
        bounded(usage.and_then(|u| u.get("total")).and_then(|v| v.as_i64()).unwrap_or(0), 10_000_000)?
    };
    if let Some(usage) = usage.filter(|u| !u.is_null()) {
        let rows = usage
            .get("models")
            .and_then(|v| v.as_array())
            .ok_or_else(|| "请求趋势与计数不一致".to_string())?;
        for (index, row) in rows.iter().enumerate() {
            let bins = row
                .get("bins")
                .and_then(|v| v.as_array())
                .ok_or_else(|| "请求趋势与计数不一致".to_string())?;
            let count = row.get("count").and_then(|v| v.as_i64()).unwrap_or(0);
            if bins.len() != 96
                || bins.iter().map(|b| b.as_i64().unwrap_or(0)).sum::<i64>() != count
            {
                return Err("请求趋势与计数不一致".into());
            }
            let hour_bins: Result<Vec<Value>, String> = (0..96)
                .step_by(4)
                .map(|n| {
                    let sum: i64 = (0..4)
                        .map(|k| bins[n + k].as_i64().unwrap_or(0))
                        .sum();
                    Ok(json!(bounded(sum, 10_000_000)?))
                })
                .collect();
            let hour_bins = hour_bins?;
            let model = row.get("model").and_then(|v| v.as_str()).unwrap_or("");
            if index < 3 {
                let fallback = ascii_label(model, 8);
                let (name, color) = models
                    .get(model)
                    .map(|(n, c)| (*n, *c))
                    .unwrap_or((fallback.as_str(), 0xB186EF));
                series.push(json!([name, color, bounded(count, 10_000_000)?, hour_bins]));
            } else if index == 3 {
                series.push(json!(["Other", 0xB186EF, bounded(count, 10_000_000)?, hour_bins]));
            } else {
                let other = series.get_mut(3).unwrap();
                let arr = other.as_array_mut().unwrap();
                arr[2] = json!(bounded(arr[2].as_i64().unwrap_or(0) + count, 10_000_000)?);
                let bins_mut = arr[3].as_array_mut().unwrap();
                for (a, b) in bins_mut.iter_mut().zip(hour_bins) {
                    *a = json!(bounded(a.as_i64().unwrap_or(0) + b.as_i64().unwrap_or(0), 10_000_000)?);
                }
            }
        }
        if series.iter().map(|r| r[2].as_i64().unwrap_or(0)).sum::<i64>() != total {
            return Err("模型计数与总请求数不一致".into());
        }
    }
    let reset = snapshot.get("reset");
    let deadline = reset.and_then(|r| r.get("deadline")).and_then(|v| {
        if v.is_null() {
            None
        } else {
            v.as_i64()
        }
    });
    let reset_seconds = match deadline {
        None => -1,
        Some(deadline) => {
            let value = (deadline - now).max(0);
            if value > 10_000_000 {
                return Err("重置预告时间超出设备范围".into());
            }
            value
        }
    };
    Ok(json!({
        "cards": cards,
        "series": series,
        "total": total,
        "reset": reset_seconds,
        "age": age,
        "warn": snapshot.get("errors").and_then(|v| v.as_array()).map(|a| !a.is_empty()).unwrap_or(false),
        "tasks": snapshot.get("tasks").map(|t| !t.is_null()).unwrap_or(false),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_context_incremental() {
        let dir = std::env::temp_dir().join(format!("holo-radar-{}", now_secs()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rollout.jsonl");
        let first = br#"{"type":"turn_context","payload":{"model":"gpt-6-astra","effort":"max"}}"#;
        let second = br#"{"type":"turn_context","payload":{"model":"gpt-6-astra","effort":"medium"}}"#;
        let mut file = File::create(&path).unwrap();
        file.write_all(first).unwrap();
        file.write_all(b"\n").unwrap();
        file.write_all(&second[..20]).unwrap();
        drop(file);
        let mut cache = json!({});
        assert_eq!(latest_context(&path, &mut cache).unwrap()["effort"], "max");
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&second[20..]).unwrap();
        file.write_all(b"\n").unwrap();
        drop(file);
        assert_eq!(latest_context(&path, &mut cache).unwrap()["effort"], "medium");
        let _ = std::fs::remove_dir_all(dir);
    }
}
