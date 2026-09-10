//! 雷达设置与设备预览；采集是待发送数据，运行画面只取 USB 已确认快照。
use crate::bridge;
use eframe::egui::{self, Color32, RichText, Stroke};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    sync::mpsc::{self, Receiver},
    time::{Duration, Instant},
};

/// 设备卡片：[模型强度名称、任务数、颜色、百分分数、美分、十分分钟]。
#[derive(Clone, Debug, Deserialize)]
struct Card(String, u32, u32, i32, i32, i32);
/// 设备曲线：[短模型名、颜色、请求数、24个小时桶]，Other 已由桥接层合并。
#[derive(Clone, Debug, Deserialize)]
struct Series(String, u32, u64, [u64; 24]);
/// 已经 CRC 校验的设备 RAM 快照，禁止从桌面待发数据补齐字段。
#[derive(Clone, Debug, Deserialize)]
struct DeviceData {
    cards: Vec<Card>,
    series: Vec<Series>,
    total: i64,
    reset: i64,
    age: u64,
    warn: bool,
    tasks: bool,
}
impl DeviceData {
    /// 外部数据必须满足固件范围和计数守恒，错误快照不能成为预览。
    fn validate(&self) -> Result<(), String> {
        let cards_ok = self.cards.len() <= 10
            && self.cards.iter().all(|c| {
                c.0.is_ascii()
                    && c.0.len() <= 20
                    && c.1 <= 10
                    && c.2 <= 0xffffff
                    && [c.3, c.4, c.5]
                        .iter()
                        .all(|v| (-1..=10_000_000).contains(v))
            });
        let series_ok = self.series.len() <= 4
            && self.series.iter().all(|s| {
                s.0.is_ascii()
                    && s.0.len() <= 10
                    && s.1 <= 0xffffff
                    && s.2 <= 10_000_000
                    && s.3.iter().all(|v| *v <= 10_000_000)
                    && s.3.iter().sum::<u64>() == s.2
            });
        let sum: u64 = self.series.iter().map(|s| s.2).sum();
        if !cards_ok
            || !series_ok
            || !(-1..=10_000_000).contains(&self.total)
            || !(-1..=10_000_000).contains(&self.reset)
            || self.age > 30
            || (self.total == -1 && !self.series.is_empty())
            || (self.total >= 0 && sum != self.total as u64)
        {
            return Err("设备雷达数据超出范围或计数不一致".into());
        }
        Ok(())
    }
}
/// 保留设备快照和相对时钟，断连后继续设备本地轮播规则并提示过期。
struct DeviceView {
    data: DeviceData,
    previous: DeviceData,
    crc: String,
    age: u64,
    scene_seconds: u64,
    contact: Instant,
    changed: Instant,
}
impl DeviceView {
    /// 六秒轮播两张卡；回读校准场景相位，不从第一页重新开始。
    fn page(&self, elapsed: Duration) -> usize {
        let pages = self.data.cards.len().div_ceil(2).max(1);
        ((self.scene_seconds + elapsed.as_secs()) / 6) as usize % pages
    }
    /// 与设备同样按相对秒数计算重置预告，过期只提示查看确认。
    fn reset_text(&self, elapsed: Duration) -> String {
        if self.data.reset < 0 {
            return "NO DEADLINE / SEE PC".into();
        }
        let passed = self.age - self.data.age + elapsed.as_secs();
        let seconds = self.data.reset.saturating_sub(passed as i64);
        if seconds <= 0 {
            "WINDOW ENDED / CHECK PC".into()
        } else {
            format!(
                "EST. {:02}:{:02}:{:02}",
                seconds / 3600,
                seconds / 60 % 60,
                seconds % 60
            )
        }
    }
    /// 逐项对齐 radar_scene.lua 的320×240坐标、字体、折线与400ms过渡。
    fn paint(&self, painter: &egui::Painter, rect: egui::Rect) {
        let c = Canvas { painter, rect };
        let elapsed = self.contact.elapsed();
        let age = self.age + elapsed.as_secs();
        let t = ease(self.changed.elapsed().as_secs_f32() / 0.4);
        let white = 0xe1f8ff;
        let muted = 0x8296aa;
        c.label("AI RADAR", [8., 5., 160., 24.], 20., white);
        let badge = if age > 15 {
            format!("STALE {age}s")
        } else if self.data.warn {
            "SOURCE WARN".into()
        } else {
            "USB LIVE".into()
        };
        c.label(&badge, [220., 10., 96., 14.], 10., muted);
        c.line(&[[8., 30.], [312., 30.]], 1., 0x253547);
        c.line(&[[153., 37.], [153., 194.]], 1., 0x253547);
        c.label("CODE / LAST 10 TASKS", [8., 34., 143., 15.], 10., muted);
        c.label("REQUESTS / 24H", [163., 34., 151., 15.], 10., muted);
        let page = self.page(elapsed);
        for (i, card) in self.data.cards.iter().skip(page * 2).take(2).enumerate() {
            let y = 52. + i as f32 * 70.;
            let old = self.previous.cards.iter().find(|old| old.0 == card.0);
            let score = if card.3 < 0 {
                "--".into()
            } else {
                format!(
                    "{:.0}",
                    blend(old.map(|v| v.3 as f32), card.3 as f32, t) / 100.
                )
            };
            c.label(&card.0, [8., y, 141., 14.], 10., card.2);
            c.label(&score, [9., y + 14., 91., 35.], 28., card.2);
            c.label(
                &format!("x{}", card.1),
                [108., y + 21., 39., 16.],
                12.,
                muted,
            );
            let price = if card.4 < 0 {
                "$--".into()
            } else {
                format!("${:.2}", card.4 as f32 / 100.)
            };
            let minutes = if card.5 < 0 {
                "--m".into()
            } else {
                format!("{:.0}m", card.5 as f32 / 10.)
            };
            c.label(
                &format!("{price}   {minutes}"),
                [9., y + 50., 140., 15.],
                10.,
                muted,
            );
        }
        let pages = if !self.data.tasks {
            "TASK DATA UNAVAILABLE".into()
        } else if self.data.cards.is_empty() {
            "NO TASKS".into()
        } else {
            format!("CODE  {}/{}", page + 1, self.data.cards.len().div_ceil(2))
        };
        c.label(&pages, [8., 192., 143., 13.], 10., muted);
        let mut peak = 1_f32;
        let mut counts = Vec::new();
        let mut curves = Vec::new();
        for row in &self.data.series {
            let old = self.previous.series.iter().find(|s| s.0 == row.0);
            counts.push(blend(old.map(|s| s.2 as f32), row.2 as f32, t));
            let values: Vec<_> = row
                .3
                .iter()
                .enumerate()
                .map(|(n, v)| blend(old.map(|s| s.3[n] as f32), *v as f32, t))
                .collect();
            peak = values.iter().copied().fold(peak, f32::max);
            curves.push(values);
        }
        let sum: f32 = counts.iter().sum();
        for (i, row) in self.data.series.iter().enumerate() {
            let points: Vec<_> = curves[i]
                .iter()
                .enumerate()
                .map(|(n, v)| {
                    [
                        164. + (n as f32 * 147. / 23.).floor(),
                        96. - (43. * v / peak).floor(),
                    ]
                })
                .collect();
            c.line(&points, 2., row.1);
            let share = if sum > 0. { counts[i] * 100. / sum } else { 0. };
            c.label(
                &format!("{} {share:.0}%", row.0),
                [224., 123. + i as f32 * 15., 92., 14.],
                10.,
                row.1,
            );
        }
        for i in 1..=64 {
            let angle =
                (i as f32 - 0.5) * std::f32::consts::TAU / 64. - std::f32::consts::FRAC_PI_2;
            let position = (i as f32 - 0.5) / 64. * sum;
            let mut cumulative = 0.;
            let mut color = 0x253547;
            for (row, count) in self.data.series.iter().zip(&counts) {
                cumulative += count;
                if sum > 0. && position < cumulative {
                    color = row.1;
                    break;
                }
            }
            c.line(
                &[
                    [
                        (192. + angle.cos() * 17.).floor(),
                        (152. + angle.sin() * 17.).floor(),
                    ],
                    [
                        (192. + angle.cos() * 27.).floor(),
                        (152. + angle.sin() * 27.).floor(),
                    ],
                ],
                3.,
                color,
            );
        }
        let total = if self.data.total < 0 {
            "USAGE UNAVAILABLE".into()
        } else {
            format!("{sum:.0} requests")
        };
        c.label(&total, [163., 102., 151., 18.], 12., white);
        c.line(&[[8., 208.], [312., 208.]], 1., 0x253547);
        c.label("RESET", [8., 217., 51., 17.], 12., 0x24d7ea);
        c.label(
            &self.reset_text(elapsed),
            [65., 217., 249., 17.],
            12.,
            white,
        );
    }
}
/// 设备坐标等比映射，标签边界及线宽和硬件采用同一组数值。
struct Canvas<'a> {
    painter: &'a egui::Painter,
    rect: egui::Rect,
}
impl Canvas<'_> {
    /// 使用设备同源 Montserrat 字体，按硬件标签矩形裁剪。
    fn label(&self, text: &str, bounds: [f32; 4], size: f32, color: u32) {
        let scale = self.rect.width() / 320.;
        let position = self.rect.min + egui::vec2(bounds[0], bounds[1]) * scale;
        let clip = egui::Rect::from_min_size(position, egui::vec2(bounds[2], bounds[3]) * scale);
        self.painter.with_clip_rect(clip.intersect(self.rect)).text(
            position,
            egui::Align2::LEFT_TOP,
            text,
            egui::FontId::new(size * scale, egui::FontFamily::Name("device".into())),
            rgb(color),
        );
    }
    /// 直接使用设备折线点，不重新分桶或平滑曲线。
    fn line(&self, points: &[[f32; 2]], width: f32, color: u32) {
        let scale = self.rect.width() / 320.;
        let points = points
            .iter()
            .map(|p| self.rect.min + egui::vec2(p[0], p[1]) * scale)
            .collect();
        self.painter.add(egui::Shape::line(
            points,
            Stroke::new(width * scale, rgb(color)),
        ));
    }
}
/// 待发统计与设备已确认快照分开持有，采集不会覆盖硬件预览。
#[derive(Default)]
pub struct RadarPage {
    data: Option<Value>,
    device: Option<DeviceView>,
    worker: Option<Receiver<Result<Value, String>>>,
    started: Option<Instant>,
    error: Option<String>,
    attempted: bool,
    revision: u64,
}
impl RadarPage {
    /// 启动唯一采集线程，复用生产桥接，不读 USB 或并发堆积请求。
    fn refresh(&mut self, ctx: &egui::Context) {
        if self.worker.is_some() {
            return;
        }
        let cache = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .join("radar_cache");
        let (sender, receiver) = mpsc::channel();
        self.worker = Some(receiver);
        self.started = Some(Instant::now());
        let context = ctx.clone();
        std::thread::spawn(move || {
            let result = bridge::call(json!({"action":"radar","cache_dir":cache}));
            let _ = sender.send(result);
            context.request_repaint();
        });
    }
    /// 采集只更新下一次待发版本；画面由 accept_device 消费 USB 回执。
    pub fn tick(&mut self, ctx: &egui::Context, active: bool) {
        if let Some(result) = self.worker.as_ref().and_then(|w| w.try_recv().ok()) {
            self.worker = None;
            self.attempted = true;
            match result {
                Ok(data) => {
                    self.data = Some(data);
                    self.revision += 1;
                    self.error = None;
                }
                Err(error) => self.error = Some(error),
            }
        }
        if refresh_due(
            active,
            self.worker.is_some(),
            self.started.map(|t| t.elapsed()),
        ) {
            self.refresh(ctx);
        }
    }
    /// 截图等到首次采集结束，包括真实错误结果。
    pub fn ready(&self) -> bool {
        self.attempted
    }
    /// 合并慢连接的待发版本，不排队发送过时数据。
    pub fn revision(&self) -> u64 {
        self.revision
    }
    /// 同一份生产统计交给 Python compact，不在 Rust 重算统计口径。
    pub fn snapshot(&self) -> Option<Value> {
        self.data.clone()
    }
    /// 告知桥接端已缓存的设备数据，内容相同不再回读。
    pub fn device_crc(&self) -> Option<&str> {
        self.device.as_ref().map(|d| d.crc.as_str())
    }
    /// 用户切换串口时清除旧设备缓存，不能把另一台设备画面留在新端口下。
    pub fn clear_device(&mut self) {
        self.device = None;
    }

    /// 主页轮播复用同一份雷达画面，不创建另一份动画或统计。
    pub fn paint_device(&self, painter: &egui::Painter, rect: egui::Rect) {
        if let Some(device) = &self.device {
            device.paint(painter, rect);
        }
    }
    /// 仅接受完整校验的设备快照；切换显示模式时清除旧预览。
    pub fn accept_device(&mut self, value: &mut Value) -> Result<(), String> {
        if value["mode"] != "radar" {
            self.device = None;
            return Ok(());
        }
        if value["radar_mirror_protocol"] != 1 {
            return Err("请更新设备程序以回读雷达预览".into());
        }
        let crc = value["crc32"]
            .as_str()
            .ok_or("设备缺少雷达 CRC")?
            .to_owned();
        let age = value["age"].as_u64().ok_or("设备缺少雷达数据年龄")?;
        let scene_seconds = value["scene_seconds"]
            .as_u64()
            .ok_or("设备缺少雷达轮播时钟")?;
        let old = self.device.as_ref();
        let changed = old.is_none_or(|old| old.crc != crc);
        let data = if let Some(raw) = value.as_object_mut().unwrap().remove("radar_data") {
            serde_json::from_value::<DeviceData>(raw)
                .map_err(|e| format!("设备雷达数据无效：{e}"))?
        } else if let Some(old) = old.filter(|old| old.crc == crc) {
            old.data.clone()
        } else {
            return Err("尚未完整回读雷达，请重新检测连接".into());
        };
        data.validate()?;
        if age < data.age || crc.len() != 8 || !crc.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err("雷达时钟或校验值无效".into());
        }
        let previous = if changed {
            old.map_or_else(|| data.clone(), |old| old.data.clone())
        } else {
            old.unwrap().previous.clone()
        };
        let sampled_at =
            Instant::now() - Duration::from_millis(value["_transport_ms"].as_u64().unwrap_or(0));
        let changed_at = if changed {
            sampled_at
        } else {
            old.unwrap().changed
        };
        self.device = Some(DeviceView {
            data,
            previous,
            crc,
            age,
            scene_seconds,
            contact: sampled_at,
            changed: changed_at,
        });
        Ok(())
    }
    /// 左侧沿用沙漏设置栏样式，展示固定规则及刷新入口，不新增无关配置。
    pub fn editor(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.label(RichText::new("雷达设置").size(16.).strong());
        ui.add_space(14.);
        for (title, description) in [
            ("刷新间隔", "5 秒 · 发送数值，设备本地绘图"),
            (
                "编码评分",
                "最近 10 个主任务，各取最新一轮；只取 Code 编码能力",
            ),
            (
                "请求统计",
                "近 24 小时全部 Codex 成功代理请求；前三模型 + Other",
            ),
            ("屏幕布局", "两张卡片每 6 秒轮播；24 个小时桶；纯黑背景"),
            (
                "重置雷达",
                "网站全局预告，不是个人额度；到点不代表已确认重置",
            ),
        ] {
            ui.label(
                RichText::new(title)
                    .size(12.)
                    .color(Color32::from_rgb(105, 116, 123)),
            );
            ui.label(description);
            ui.add_space(12.);
        }
        if let Some(data) = &self.data {
            ui.label(format!(
                "最近采集：{}",
                data["collected_label"].as_str().unwrap()
            ));
            if let Some(errors) = data["errors"].as_array().filter(|v| !v.is_empty()) {
                ui.colored_label(Color32::from_rgb(184, 95, 20), "部分数据源暂不可用")
                    .on_hover_text(
                        errors
                            .iter()
                            .map(|e| e.as_str().unwrap())
                            .collect::<Vec<_>>()
                            .join("\n"),
                    );
            }
        }
        if let Some(error) = &self.error {
            ui.colored_label(Color32::DARK_RED, error);
        }
        ui.add_space(12.);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(self.worker.is_none(), egui::Button::new("刷新数据"))
                .clicked()
            {
                self.refresh(ctx);
            }
            if self.worker.is_some() {
                ui.spinner();
            }
            ui.hyperlink_to("评分 / 重置来源 ↗", "https://codex-reset-radar.pages.dev/");
        });
    }
    /// 右侧只显示设备确认画面；未检测留空，断连保留最后快照并明确标识。
    pub fn preview(&self, ui: &mut egui::Ui, connected: bool) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("设备运行画面").size(16.).strong());
            ui.label(RichText::new("320 × 240").size(12.).color(Color32::GRAY));
        });
        ui.add_space(12.);
        let width = ui.available_width().min(384.);
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(width, width * 0.75), egui::Sense::hover());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0., Color32::BLACK);
        if let Some(device) = &self.device {
            device.paint(&painter, rect);
            ui.add_space(8.);
            ui.label(if connected {
                "已回读设备 · 轮播与倒计时在两端独立计算"
            } else {
                "上次确认的设备画面 · 尚未重新连接"
            });
        } else {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "检测连接后显示设备雷达画面",
                egui::FontId::proportional(14.),
                Color32::GRAY,
            );
            ui.add_space(8.);
            ui.label("尚未回读雷达；草稿不会代替设备画面");
        }
    }
}
/// 24位设备颜色转为不透明颜色，不受桌面主题影响。
fn rgb(value: u32) -> Color32 {
    Color32::from_rgb((value >> 16) as u8, (value >> 8) as u8, value as u8)
}
/// 与设备一致的有界缓动，避免超调。
fn ease(t: f32) -> f32 {
    let t = t.clamp(0., 1.);
    t * t * (3. - 2. * t)
}
/// 首帧或旧值未知时直接采用新值，不从零伪造动画。
fn blend(from: Option<f32>, to: f32, t: f32) -> f32 {
    let from = from.filter(|v| *v >= 0.).unwrap_or(to);
    from + (to - from) * t
}
/// 页面可见或 USB 同步启用时才五秒采集，在途请求不重复启动。
fn refresh_due(active: bool, busy: bool, elapsed: Option<Duration>) -> bool {
    active && !busy && elapsed.is_none_or(|age| age >= Duration::from_secs(5))
}
#[cfg(test)]
mod tests {
    use super::*;
    /// 构造和 Lua 协议相同的回执，不含任何任务正文。
    fn reply() -> Value {
        json!({"mode":"radar","radar_mirror_protocol":1,"crc32":"12345678","age":5,"scene_seconds":6,
            "radar_data":{"cards":[["Astra max",8,16743965,10650,500,250],["Luna high",1,11977943,8000,-1,100],["Sol high",1,15909390,9800,300,200]],
            "series":[["Astra",16743965,24,vec![1;24]]],"total":24,"reset":10,"age":2,"warn":false,"tasks":true}})
    }
    /// 本机新的采集不能覆盖设备已确认画面，轮播和重置由设备相位计算。
    #[test]
    fn preview_uses_acknowledged_data_and_device_clock() {
        let mut page = RadarPage::default();
        page.accept_device(&mut reply()).unwrap();
        page.data = Some(json!({"usage":{"total":9999}}));
        let device = page.device.as_ref().unwrap();
        assert_eq!(device.data.total, 24);
        assert_eq!(device.page(Duration::ZERO), 1);
        assert_eq!(device.page(Duration::from_secs(6)), 0);
        assert_eq!(device.reset_text(Duration::ZERO), "EST. 00:00:07");
        assert_eq!(
            device.reset_text(Duration::from_secs(8)),
            "WINDOW ENDED / CHECK PC"
        );
    }
    /// CRC改变缺数据或计数不一致时不得覆盖旧画面，相同CRC可复用缓存。
    #[test]
    fn readback_crc_and_counts_must_match() {
        let mut page = RadarPage::default();
        let mut state = reply();
        page.accept_device(&mut state).unwrap();
        assert!(state.get("radar_data").is_none());
        page.accept_device(&mut state).unwrap();
        state["crc32"] = json!("87654321");
        assert!(page.accept_device(&mut state).is_err());
        let mut invalid = reply();
        invalid["radar_data"]["total"] = json!(25);
        assert!(page.accept_device(&mut invalid).is_err());
        assert_eq!(page.device_crc(), Some("12345678"));
        page.accept_device(&mut json!({"mode":"task"})).unwrap();
        assert!(page.device.is_none());
    }
    /// 未回读设备时不能制造预览，隐藏页面也不会启动后台任务。
    #[test]
    fn hidden_page_has_no_refresh_worker() {
        let mut page = RadarPage::default();
        page.tick(&egui::Context::default(), false);
        assert!(page.worker.is_none());
        assert!(!page.ready());
        assert!(page.device.is_none());
    }
    /// 采集间隔、重叠保护和动画边界保持确定性。
    #[test]
    fn refresh_and_animation_boundaries() {
        assert!(refresh_due(true, false, None));
        assert!(!refresh_due(true, false, Some(Duration::from_millis(4999))));
        assert!(refresh_due(true, false, Some(Duration::from_secs(5))));
        assert!(!refresh_due(false, false, Some(Duration::from_secs(30))));
        assert!(!refresh_due(true, true, Some(Duration::from_secs(30))));
        assert_eq!(ease(-1.), 0.);
        assert_eq!(ease(2.), 1.);
        assert_eq!(blend(Some(10.), 20., ease(0.5)), 15.);
        assert_eq!(blend(Some(-1.), 20., ease(0.5)), 20.);
    }
}
