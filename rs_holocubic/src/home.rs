//! 主页轮播配置、设备相位投影与渐变预览；切页不负责重新启动各场景。
use crate::{calendar::CalendarPage, mirror::Mirror, model::Mode, radar::RadarPage};
use eframe::egui::{self, Color32, RichText};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::PathBuf, time::Instant};

/// 一个画面的勾选状态和完整停留时间，不包含1.2秒渐变切换。
#[derive(Clone, Serialize, Deserialize)]
struct Choice {
    mode: Mode,
    enabled: bool,
    seconds: u32,
}

/// 独立保存主页的新配置，不更改既有沙漏等模式的草稿结构。
#[derive(Serialize, Deserialize)]
struct Config {
    choices: Vec<Choice>,
}
impl Default for Config {
    /// 首次默认轮播当前主要使用的沙漏和雷达，其余画面显式勾选后加入。
    fn default() -> Self {
        Self {
            choices: vec![
                Choice {
                    mode: Mode::Hourglass,
                    enabled: true,
                    seconds: 15,
                },
                Choice {
                    mode: Mode::Radar,
                    enabled: true,
                    seconds: 15,
                },
                Choice {
                    mode: Mode::Text,
                    enabled: false,
                    seconds: 10,
                },
                Choice {
                    mode: Mode::Calendar,
                    enabled: false,
                    seconds: 15,
                },
                Choice {
                    mode: Mode::Image,
                    enabled: false,
                    seconds: 10,
                },
            ],
        }
    }
}

/// 设备确认的列表项，使用串口协议名称而非编辑页名称。
#[derive(Clone, Deserialize)]
struct Slot {
    mode: String,
    seconds: u32,
}

/// 设备相位在一个周期内取余，避免传递会损失精度的巨大绝对毫秒时间。
#[derive(Deserialize)]
struct Clock {
    home_id: String,
    home_entries: Vec<Slot>,
    home_elapsed_ms: u64,
    home_transition_ms: u64,
}
impl Clock {
    /// 外部回执必须符合固件的条目数、模式唯一性、时长及过渡范围。
    fn validate(&self) -> Result<(), String> {
        let mut seen = Vec::new();
        for slot in &self.home_entries {
            if display_mode(&slot.mode).is_none()
                || seen.contains(&slot.mode)
                || !(1..=3600).contains(&slot.seconds)
            {
                return Err("轮播回执包含无效画面或时长".into());
            }
            seen.push(slot.mode.clone());
        }
        if !(1..=5).contains(&seen.len())
            || self.home_id.len() != 8
            || !self.home_id.bytes().all(|v| v.is_ascii_hexdigit())
            || self.home_transition_ms != if seen.len() > 1 { 1200 } else { 0 }
            || self.home_elapsed_ms >= self.cycle()
        {
            return Err("轮播时钟回执无效".into());
        }
        Ok(())
    }
    /// 完整周期由每页停留时长加切换时长组成，单页不播放无意义的自切换。
    fn cycle(&self) -> u64 {
        self.home_entries
            .iter()
            .map(|s| s.seconds as u64 * 1000 + self.home_transition_ms)
            .sum()
    }
    /// 对齐 Lua：前半段遮住旧页，后半段揭示下一页；不会丢弃跨周期余量。
    fn project(&self, elapsed_ms: u64) -> (Mode, Option<f32>) {
        let mut phase = (self.home_elapsed_ms + elapsed_ms) % self.cycle();
        for (index, slot) in self.home_entries.iter().enumerate() {
            let hold = slot.seconds as u64 * 1000;
            if phase < hold + self.home_transition_ms {
                let animation =
                    (phase >= hold).then(|| (phase - hold) as f32 / self.home_transition_ms as f32);
                let selected = if animation.is_some_and(|v| v >= 0.5) {
                    (index + 1) % self.home_entries.len()
                } else {
                    index
                };
                return (
                    display_mode(&self.home_entries[selected].mode).unwrap(),
                    animation,
                );
            }
            phase -= hold + self.home_transition_ms;
        }
        unreachable!("已校验的周期必然命中画面")
    }
}

/// 经 CRC 回读或上传确认的静态画面纹理，不能用未应用的图片路径补画。
#[derive(Clone)]
struct Picture {
    crc: String,
    texture: egui::TextureHandle,
}

/// 主页草稿与设备正在执行的列表分别持有，修改勾选不会立即改变硬件。
#[derive(Default)]
pub struct HomePage {
    config: Config,
    path: PathBuf,
    clock: Option<Clock>,
    contact: Option<Instant>,
    pictures: BTreeMap<String, Picture>,
    held: Option<Mode>,
    save_error: Option<String>,
}
impl HomePage {
    /// 只加载本页配置；首次没有文件时提供默认值，损坏配置直接报告错误。
    pub fn load(path: PathBuf) -> Result<Self, String> {
        let config = if path.exists() {
            serde_json::from_slice(&std::fs::read(&path).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?
        } else {
            Config::default()
        };
        Ok(Self {
            config,
            path,
            ..Self::default()
        })
    }
    /// 主页设置写入本机配置文件，不自动应用设备或设置开机运行。
    pub fn save(&self) -> Result<(), String> {
        let data = serde_json::to_vec_pretty(&self.config).map_err(|e| e.to_string())?;
        let pending = self.path.with_extension("json.tmp");
        std::fs::write(&pending, data).map_err(|e| e.to_string())?;
        std::fs::rename(&pending, &self.path).map_err(|e| e.to_string())
    }
    /// 只在控件实际变化时自动保存，空闲重绘不写文件；错误保留在页面提示。
    fn persist_changes(&mut self, changed: bool) {
        if changed {
            self.save_error = self.save().err();
        }
    }
    /// 只发送勾选项并验证唯一性，空列表不允许启动轮播。
    pub fn entries(&self) -> Result<Value, String> {
        let mut entries = Vec::new();
        for choice in self.config.choices.iter().filter(|c| c.enabled) {
            let mode = protocol_mode(choice.mode).ok_or("主页不能作为自己的轮播画面")?;
            if !(1..=3600).contains(&choice.seconds)
                || entries.iter().any(|v: &Value| v["mode"] == mode)
            {
                return Err("轮播时长应为1～3600秒，画面不能重复".into());
            }
            entries.push(json!({"mode":mode,"seconds":choice.seconds}));
        }
        if entries.is_empty() || entries.len() > 5 {
            return Err("请勾选至少一个轮播画面".into());
        }
        Ok(json!(entries))
    }
    /// 待发配置含雷达时，需要先完成一次统计采集。
    pub fn wants_radar(&self) -> bool {
        self.config
            .choices
            .iter()
            .any(|c| c.enabled && c.mode == Mode::Radar)
    }
    /// 勾选日历后提前读取当天内容，应用时无需等待首次发现数据库。
    pub fn wants_calendar(&self) -> bool {
        self.config
            .choices
            .iter()
            .any(|c| c.enabled && c.mode == Mode::Calendar)
    }
    /// 已发布列表含日历时才在后台同步日历变化，不修改未勾选的轮播内容。
    pub fn has_calendar(&self) -> bool {
        self.clock
            .as_ref()
            .is_some_and(|c| c.home_entries.iter().any(|s| s.mode == "calendar"))
    }
    /// 当前设备列表含雷达时，即使编辑页切走也持续更新其后台数据。
    pub fn has_radar(&self) -> bool {
        self.clock
            .as_ref()
            .is_some_and(|c| c.home_entries.iter().any(|s| s.mode == "radar"))
    }
    /// 只根据设备确认状态启用停止按钮。
    pub fn running(&self) -> bool {
        self.clock.is_some()
    }
    /// 每种静态画面独立缓存 CRC，避免每个雷达更新都回读 JPEG。
    pub fn asset_crcs(&self) -> Value {
        json!(
            self.pictures
                .iter()
                .map(|(k, v)| (k, &v.crc))
                .collect::<BTreeMap<_, _>>()
        )
    }
    /// 切换串口时不能继续将旧设备的轮播相位用于新设备。
    pub fn clear_device(&mut self) {
        self.clock = None;
        self.contact = None;
        self.held = None;
        self.pictures.clear();
    }
    /// 消费整个设备轮播回执，所有图片先校验解码成功再替换缓存。
    pub fn accept(&mut self, value: &mut Value, ctx: &egui::Context) -> Result<(), String> {
        if value["mode"] != "home" {
            self.clock = None;
            self.contact = None;
            self.held = value["held_mode"]
                .as_str()
                .and_then(display_mode)
                .or_else(|| {
                    value["mode"].as_str().and_then(|v| match v {
                        "task" => Some(Mode::Hourglass),
                        "radar" => Some(Mode::Radar),
                        "calendar" => Some(Mode::Calendar),
                        _ => None,
                    })
                });
            return Ok(());
        }
        let clock: Clock =
            serde_json::from_value(value.clone()).map_err(|e| format!("轮播状态无效：{e}"))?;
        clock.validate()?;
        let mut pictures = self.pictures.clone();
        if let Some(images) = value.get_mut("home_images").and_then(Value::as_object_mut) {
            for (kind, meta) in images {
                if kind != "text" && kind != "image" {
                    return Err("未知轮播图片类型".into());
                }
                let crc = meta["crc32"].as_str().ok_or("图片缺少校验值")?.to_owned();
                if let Some(data) = meta.as_object_mut().unwrap().remove("data") {
                    let bytes: Vec<u8> = serde_json::from_value(data).map_err(|e| e.to_string())?;
                    if meta["size"].as_u64() != Some(bytes.len() as u64) {
                        return Err("轮播图片大小不匹配".into());
                    }
                    let image = image::load_from_memory(&bytes)
                        .map_err(|e| e.to_string())?
                        .to_rgba8();
                    if image.dimensions() != (320, 240) {
                        return Err("轮播图片尺寸不是320×240".into());
                    }
                    let texture = ctx.load_texture(
                        format!("home-{kind}"),
                        egui::ColorImage::from_rgba_unmultiplied([320, 240], image.as_raw()),
                        egui::TextureOptions::LINEAR,
                    );
                    pictures.insert(
                        kind.clone(),
                        Picture {
                            crc: crc.clone(),
                            texture,
                        },
                    );
                }
                if pictures.get(kind).is_none_or(|p| p.crc != crc) {
                    return Err("轮播图片尚未完整回读".into());
                }
            }
        }
        self.clock = Some(clock);
        self.contact = Some(
            Instant::now()
                - std::time::Duration::from_millis(value["_transport_ms"].as_u64().unwrap_or(0)),
        );
        self.pictures = pictures;
        self.held = None;
        Ok(())
    }
    /// 左侧勾选列表与各页停留秒数，编辑按钮跳到已有内容页面。
    pub fn editor(&mut self, ui: &mut egui::Ui) -> Option<Mode> {
        ui.label(RichText::new("选择轮播画面").size(16.).strong());
        ui.label("按列表顺序循环 · 时长不含1.2秒渐变切换");
        ui.add_space(16.);
        let mut edit = None;
        let mut changed = false;
        for choice in &mut self.config.choices {
            ui.horizontal(|ui| {
                changed |= ui
                    .checkbox(&mut choice.enabled, choice.mode.name())
                    .changed();
                changed |= ui
                    .add_enabled(
                        choice.enabled,
                        egui::DragValue::new(&mut choice.seconds)
                            .range(1..=3600)
                            .suffix(" 秒"),
                    )
                    .changed();
                if ui.small_button("编辑").clicked() {
                    edit = Some(choice.mode);
                }
            });
            ui.add_space(12.);
        }
        self.persist_changes(changed);
        if let Some(error) = &self.save_error {
            ui.colored_label(Color32::DARK_RED, format!("主页配置保存失败：{error}"));
        } else {
            ui.label(
                RichText::new("勾选和时长自动保存到本机；应用轮播后才改变设备。")
                    .size(12.)
                    .color(Color32::GRAY),
            );
        }
        ui.add_space(12.);
        ui.label("沙漏切走后继续计时，切回不重启。");
        ui.label("文字和图片仅应用时上传一次；关闭控制台后，设备仍按列表轮播。");
        ui.add_space(12.);
        ui.colored_label(
            Color32::from_rgb(105, 116, 123),
            "拍击：现有 Lua 接口未提供可靠的单拍/双拍识别，暂不启用。",
        );
        edit
    }
    /// 按设备相位选取真实场景预览并叠加同样的渐变，不发送任何动画帧。
    pub fn preview(
        &self,
        ui: &mut egui::Ui,
        mirror: &Mirror,
        radar: &RadarPage,
        calendar: &CalendarPage,
    ) {
        ui.label(RichText::new("设备运行画面 · 320 × 240").size(16.).strong());
        ui.add_space(12.);
        let width = ui.available_width().min(384.);
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(width, width * 0.75), egui::Sense::hover());
        let p = ui.painter_at(rect);
        p.rect_filled(rect, 0., Color32::BLACK);
        let projected = self
            .clock
            .as_ref()
            .map(|c| c.project(self.contact.unwrap().elapsed().as_millis() as u64));
        let current = projected.map(|v| v.0).or(self.held);
        match current {
            Some(Mode::Hourglass) => mirror.paint(&p, rect, mirror.current_task().as_ref()),
            Some(Mode::Radar) => {
                radar.paint_device(&p, rect);
            }
            Some(Mode::Calendar) => calendar.paint_device(&p, rect),
            Some(Mode::Text | Mode::Image) => {
                let kind = protocol_mode(current.unwrap()).unwrap();
                if let Some(picture) = self.pictures.get(kind) {
                    p.image(
                        picture.texture.id(),
                        rect,
                        egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1., 1.)),
                        Color32::WHITE,
                    );
                } else {
                    p.text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "静态画面尚未回读，请重新应用轮播",
                        egui::FontId::proportional(13.),
                        Color32::GRAY,
                    );
                }
            }
            _ => {
                p.text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "检测连接或应用轮播后显示实际画面",
                    egui::FontId::proportional(13.),
                    Color32::GRAY,
                );
            }
        }
        if let Some(animation) = projected.and_then(|v| v.1) {
            p.rect_filled(rect, 0., Color32::from_black_alpha(fade_opacity(animation)));
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(16));
        }
        ui.add_space(8.);
        if let Some(mode) = current {
            ui.label(format!(
                "{} · {}",
                if self.running() {
                    "正在轮播"
                } else {
                    "当前画面"
                },
                mode.name()
            ));
        }
    }
}

/// 编辑枚举与设备协议模式的一对一转换，主页本身不属于可轮播内容。
fn protocol_mode(mode: Mode) -> Option<&'static str> {
    match mode {
        Mode::Hourglass => Some("task"),
        Mode::Radar => Some("radar"),
        Mode::Calendar => Some("calendar"),
        Mode::Text => Some("text"),
        Mode::Image => Some("image"),
        Mode::Home => None,
    }
}
/// 校验外部设备模式时保留未知值为错误，不假定为某个已知页面。
fn display_mode(value: &str) -> Option<Mode> {
    match value {
        "task" => Some(Mode::Hourglass),
        "radar" => Some(Mode::Radar),
        "calendar" => Some(Mode::Calendar),
        "text" => Some(Mode::Text),
        "image" => Some(Mode::Image),
        _ => None,
    }
}
/// 与 Lua 共用的淡出/淡入曲线，中点全黑完成切页，两端速度平滑归零。
fn fade_opacity(animation: f32) -> u8 {
    let phase = if animation <= 0.5 {
        animation * 2.
    } else {
        (1. - animation) * 2.
    };
    let phase = phase.clamp(0., 1.);
    (phase * phase * (3. - 2. * phase) * 255.).round() as u8
}

#[cfg(test)]
mod tests {
    use super::*;
    /// 勾选和时长自动落盘，重新加载恢复；未变更的界面不产生配置文件。
    #[test]
    fn home_changes_are_persisted_and_reloaded() {
        let directory = std::env::temp_dir().join(format!(
            "holocubic-home-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("home_settings.json");
        let mut page = HomePage::load(path.clone()).unwrap();
        page.persist_changes(false);
        assert!(!path.exists());
        page.config.choices[0].seconds = 37;
        page.config.choices[0].enabled = false;
        page.persist_changes(true);
        assert!(page.save_error.is_none());
        let restored = HomePage::load(path.clone()).unwrap();
        assert_eq!(restored.config.choices[0].seconds, 37);
        assert!(!restored.config.choices[0].enabled);
        page.config.choices[0].seconds = 42;
        page.persist_changes(true);
        assert_eq!(
            HomePage::load(path.clone()).unwrap().config.choices[0].seconds,
            42
        );
        page.path = directory.join("missing").join("home_settings.json");
        page.persist_changes(true);
        assert!(page.save_error.is_some());
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }
    /// 覆盖完整停留时间、遮盖中点切页、下一页停留及多轮取余。
    #[test]
    fn playlist_phase_matches_device() {
        let clock = Clock {
            home_id: "12345678".into(),
            home_entries: vec![
                Slot {
                    mode: "task".into(),
                    seconds: 2,
                },
                Slot {
                    mode: "radar".into(),
                    seconds: 3,
                },
            ],
            home_elapsed_ms: 0,
            home_transition_ms: 1200,
        };
        clock.validate().unwrap();
        assert_eq!(clock.project(1999), (Mode::Hourglass, None));
        assert_eq!(clock.project(2200).0, Mode::Hourglass);
        assert_eq!(clock.project(2600).0, Mode::Radar);
        assert_eq!(clock.project(3200), (Mode::Radar, None));
        assert_eq!(clock.project(14800), (Mode::Hourglass, None));
        assert_eq!(
            [
                fade_opacity(0.),
                fade_opacity(0.25),
                fade_opacity(0.5),
                fade_opacity(0.75),
                fade_opacity(1.)
            ],
            [0, 128, 255, 128, 0]
        );
    }
    /// 空列表、重复模式和非法停留时间不能发送到设备。
    #[test]
    fn selection_validation_and_defaults() {
        let mut page = HomePage::default();
        assert_eq!(page.entries().unwrap().as_array().unwrap().len(), 2);
        for c in &mut page.config.choices {
            c.enabled = false;
        }
        assert!(page.entries().is_err());
        page.config.choices[0].enabled = true;
        page.config.choices[0].seconds = 0;
        assert!(page.entries().is_err());
    }

    /// 日历可作为第五种画面加入，不改变已有两种默认勾选。
    #[test]
    fn calendar_can_join_all_five_modes() {
        let mut page = HomePage::default();
        assert!(!page.wants_calendar());
        for choice in &mut page.config.choices {
            choice.enabled = true;
        }
        let entries = page.entries().unwrap();
        assert_eq!(entries.as_array().unwrap().len(), 5);
        assert!(page.wants_calendar());
    }
}
