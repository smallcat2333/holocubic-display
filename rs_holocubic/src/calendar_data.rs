//! Read-only CalendarTask day-cell collect (no USB).

use crate::render::{self, RenderError};
use chrono::Local;
use html_escape::decode_html_entities;
use regex::Regex;
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

fn entity_wrap_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\|(&[A-Za-z0-9#]+;)\|").unwrap())
}

fn font_tag_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)</?font\b[^>]*>").unwrap())
}

pub fn decode_text(value: &str) -> String {
    let step = entity_wrap_re().replace_all(value, |caps: &regex::Captures| {
        decode_html_entities(&caps[1]).to_string()
    });
    let step = font_tag_re().replace_all(&step, "");
    decode_html_entities(&step).replace(" ", " ")
}

pub fn parse_items(content: &str) -> Vec<Value> {
    let mut items = Vec::new();
    for line in decode_text(content).lines() {
        let mut text = line.trim().to_string();
        let mut done = false;
        if let Some(rest) = text.strip_prefix("[+]") {
            done = true;
            text = rest.trim_start().to_string();
        }
        if !text.is_empty() {
            items.push(json!({"text": text, "done": done}));
        }
    }
    items
}

fn open_ro(path: &Path) -> Result<Connection, String> {
    let uri = format!("file:{}?mode=ro", path.to_string_lossy().replace('\\', "/"));
    let db = Connection::open_with_flags(
        &uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|e| e.to_string())?;
    db.execute_batch("PRAGMA query_only=ON")
        .map_err(|e| e.to_string())?;
    Ok(db)
}

fn default_db_path() -> PathBuf {
    if let Ok(appdata) = std::env::var("APPDATA") {
        return PathBuf::from(appdata).join("CalendarTask/Db/calendar.db");
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("CalendarTask/Db/calendar.db")
}

/// Python `json.dumps(obj, ensure_ascii=False, sort_keys=True)` spacing.
fn json_dumps_sorted(value: &Value) -> String {
    match value {
        Value::Null => "null".into(),
        Value::Bool(b) => if *b { "true" } else { "false" }.into(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into()),
        Value::Array(arr) => {
            let parts: Vec<String> = arr.iter().map(json_dumps_sorted).collect();
            format!("[{}]", parts.join(", "))
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .into_iter()
                .map(|k| {
                    format!(
                        "{}: {}",
                        serde_json::to_string(k).unwrap(),
                        json_dumps_sorted(&map[k])
                    )
                })
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
    }
}

pub fn collect(db_path: Option<&Path>, day: Option<chrono::NaiveDate>) -> Result<Value, String> {
    let path = db_path.map(Path::to_path_buf).unwrap_or_else(default_db_path);
    let day = day.unwrap_or_else(|| Local::now().date_naive());
    let result = {
        let db = open_ro(&path)?;
        let mut settings: HashMap<String, (i64, String)> = HashMap::new();
        {
            let mut stmt = db
                .prepare(
                    "SELECT st_name,st_nval,st_sval FROM setting_table WHERE st_name IN ('sys_current_sub_account','sys_sub_account_list','group_id')",
                )
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .map_err(|e| e.to_string())?;
            for row in rows {
                let (name, nval, sval) = row.map_err(|e| e.to_string())?;
                settings.insert(name, (nval, sval));
            }
        }
        let account_id = settings
            .get("sys_current_sub_account")
            .map(|v| v.0)
            .ok_or_else(|| "请在日历清单中选择一个具体子日历".to_string())?;
        if account_id < 0 {
            return Err("请在日历清单中选择一个具体子日历".into());
        }
        let list_raw = settings
            .get("sys_sub_account_list")
            .map(|v| v.1.as_str())
            .ok_or_else(|| "请在日历清单中选择一个具体子日历".to_string())?;
        let accounts: Value =
            serde_json::from_str(&decode_text(list_raw)).map_err(|e| e.to_string())?;
        let list = accounts
            .pointer("/vdata/list")
            .and_then(|v| v.as_array())
            .ok_or_else(|| "子日历列表无效".to_string())?;
        let account = list
            .iter()
            .find(|row| row.get("id").and_then(|v| v.as_i64()) == Some(account_id))
            .and_then(|row| row.get("name"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| "请在日历清单中选择一个具体子日历".to_string())?
            .to_owned();
        let group = settings
            .get("group_id")
            .map(|v| v.1.clone())
            .unwrap_or_default();
        let unique = format!("dkcal_mdays_{}", day.format("%Y%m%d"));
        let rows: Vec<String> = {
            let mut stmt = db
                .prepare(
                    "SELECT it_content FROM item_table WHERE u_id=?1 AND pj_id=0 AND COALESCE(group_id,'')=?2 AND it_unique_id=?3",
                )
                .map_err(|e| e.to_string())?;
            stmt.query_map(rusqlite::params![account_id, group, unique], |row| row.get(0))
                .map_err(|e| e.to_string())?
                .collect::<Result<_, _>>()
                .map_err(|e| e.to_string())?
        };
        if rows.len() > 1 {
            return Err("当天日格存在多条记录，需先核对日历数据，未擅自合并".into());
        }
        let items = rows.first().map(|c| parse_items(c)).unwrap_or_default();
        let done = items
            .iter()
            .filter(|item| item.get("done") == Some(&json!(true)))
            .count();
        let weekday =
            ["一", "二", "三", "四", "五", "六", "日"][chrono::Datelike::weekday(&day).num_days_from_monday() as usize];
        let mut data = json!({
            "date": day.format("%Y-%m-%d").to_string(),
            "weekday": format!("周{weekday}"),
            "account": account,
            "account_id": account_id,
            "items": items.clone(),
            "total": items.len(),
            "done": done,
            "layout": 2,
        });
        let mut hasher = Sha256::new();
        hasher.update(json_dumps_sorted(&data).as_bytes());
        let signature = format!("{:x}", hasher.finalize())[..16].to_string();
        data.as_object_mut().unwrap().insert("signature".into(), json!(signature));
        data.as_object_mut()
            .unwrap()
            .insert("db_path".into(), json!(path.to_string_lossy()));
        // Connection drops at end of block.
        data
    };
    Ok(result)
}

pub fn render(snapshot: &Value) -> Result<Vec<Vec<u8>>, RenderError> {
    render::render_calendar_pages(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn parse_completion_and_color_markup() {
        let items = parse_items(
            "普通[+]正文\r\n|&lt;|font color=|&quot;|#FF0000|&quot;||&gt;|[+]完成|&lt;|/font|&gt;|\r\n\r\n",
        );
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["text"], "普通[+]正文");
        assert_eq!(items[0]["done"], false);
        assert_eq!(items[1]["text"], "完成");
        assert_eq!(items[1]["done"], true);
    }

    #[test]
    fn collect_today_account_only() {
        let dir = std::env::temp_dir().join(format!(
            "holo-cal-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("calendar.db");
        {
            let db = Connection::open(&path).unwrap();
            db.execute_batch(
                "CREATE TABLE setting_table(st_name TEXT,st_nval INTEGER,st_sval TEXT);
                 CREATE TABLE item_table(u_id INTEGER,pj_id INTEGER,group_id TEXT,it_unique_id TEXT,it_content TEXT);",
            )
            .unwrap();
            let accounts = r#"{"vdata":{"list":[{"id":1,"name":"工作"},{"id":2,"name":"生活"}]}}"#
                .replace('"', "|&quot;|");
            db.execute(
                "INSERT INTO setting_table VALUES(?1,?2,?3)",
                rusqlite::params!["sys_current_sub_account", 1i64, ""],
            )
            .unwrap();
            db.execute(
                "INSERT INTO setting_table VALUES(?1,?2,?3)",
                rusqlite::params!["sys_sub_account_list", 0i64, accounts],
            )
            .unwrap();
            db.execute(
                "INSERT INTO setting_table VALUES(?1,?2,?3)",
                rusqlite::params!["group_id", 0i64, ""],
            )
            .unwrap();
            db.execute(
                "INSERT INTO setting_table VALUES(?1,?2,?3)",
                rusqlite::params!["user_token", 0i64, "DO_NOT_READ"],
            )
            .unwrap();
            for (u, g, uid, content) in [
                (1i64, "", "dkcal_mdays_20260908", "待办\r\n[+]已完成"),
                (2, "", "dkcal_mdays_20260908", "别的子日历"),
                (1, "team", "dkcal_mdays_20260908", "别的团队"),
                (1, "", "dkcal_mdays_20260907", "不是当日"),
            ] {
                db.execute(
                    "INSERT INTO item_table VALUES(?1,0,?2,?3,?4)",
                    rusqlite::params![u, g, uid, content],
                )
                .unwrap();
            }
        }
        let before = std::fs::read(&path).unwrap();
        let day = chrono::NaiveDate::from_ymd_opt(2026, 9, 8).unwrap();
        let result = collect(Some(&path), Some(day)).unwrap();
        assert_eq!(result["total"], 2);
        assert_eq!(result["done"], 1);
        assert_eq!(result["account"], "工作");
        assert_eq!(result["items"][0]["text"], "待办");
        assert_eq!(result["items"][1]["done"], true);
        assert!(!serde_json::to_string(&result).unwrap().contains("DO_NOT_READ"));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let again = collect(Some(&path), Some(day)).unwrap();
        assert_eq!(result["signature"], again["signature"]);
        let empty = collect(
            Some(&path),
            Some(chrono::NaiveDate::from_ymd_opt(2026, 9, 9).unwrap()),
        )
        .unwrap();
        assert_eq!(empty["total"], 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
