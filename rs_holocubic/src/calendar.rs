//! 当日日历只读采集与设备固定第一页预览；内容未变不重新上传。
use crate::bridge;
use eframe::egui::{self, Color32, RichText};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    sync::mpsc::{self, Receiver},
    time::{Duration, Instant},
};

/// 原应用的一行任务，完成状态来自 [+]，不允许在本控制台修改源数据库。
#[derive(Deserialize)]
struct Item {
    text: String,
    done: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    /// 新采集只成为待发数据，不伪装为设备已经显示的内容。
    #[test]
    fn collection_does_not_replace_device_preview() {
        let mut page = CalendarPage {
            source: Some(json!({"signature":"1234567890abcdef"})),
            ..CalendarPage::default()
        };
        assert!(page.needs_send());
        assert!(page.display.is_none());
        page.tick(&egui::Context::default(), false);
        assert!(page.worker.is_none());
    }
    /// 设备未提供完整页面或相位越界时拒绝预览，不能变成空白的成功状态。
    #[test]
    fn invalid_page_metadata_is_rejected() {
        let mut page = CalendarPage::default();
        let mut state = json!({"mode":"calendar","calendar_signature":"1234567890abcdef",
            "calendar_elapsed_ms":0,"calendar_page_ms":8000,"calendar_pages":[]});
        assert!(page.accept(&mut state, &egui::Context::default()).is_err());
        assert!(page.display.is_none());
    }
}
/// 本机最新日格，签名不含采集时间，从而让空闲刷新保持零图片传输。
#[derive(Deserialize)]
struct Snapshot {
    date: String,
    account: String,
    db_path: String,
    items: Vec<Item>,
}
/// 已确认的设备首页及签名，不建立翻页时钟。
struct Display {
    signature: String,
    pages: Vec<egui::TextureHandle>,
}
/// 采集线程与 USB 发送线程分离，源错误保留上一次设备画面。
#[derive(Default)]
pub struct CalendarPage {
    source: Option<Value>,
    display: Option<Display>,
    worker: Option<Receiver<Result<Value, String>>>,
    started: Option<Instant>,
    error: Option<String>,
    attempted: bool,
}
impl CalendarPage {
    /// 只读本机日历数据，不请求公网，不接触串口。
    fn refresh(&mut self, ctx: &egui::Context) {
        if self.worker.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.worker = Some(rx);
        self.started = Some(Instant::now());
        let context = ctx.clone();
        std::thread::spawn(move || {
            let result = bridge::call(json!({"action":"calendar_data"}));
            let _ = tx.send(result);
            context.request_repaint();
        });
    }
    /// 页面可见或日历已应用时五秒采集，跨零点自然重新读取当天日格。
    pub fn tick(&mut self, ctx: &egui::Context, active: bool) {
        if let Some(result) = self.worker.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.worker = None;
            self.attempted = true;
            match result {
                Ok(data) => {
                    self.source = Some(data);
                    self.error = None;
                }
                Err(e) => self.error = Some(e),
            }
        }
        if active
            && self.worker.is_none()
            && self
                .started
                .is_none_or(|at| at.elapsed() >= Duration::from_secs(5))
        {
            self.refresh(ctx);
        }
    }
    /// 返回待发送快照，不以草稿代替实际设备预览。
    pub fn snapshot(&self) -> Option<Value> {
        self.source.clone()
    }
    /// 比较内容签名而非轮询次数；数据库其它表变化不会触发重新传图。
    pub fn needs_send(&self) -> bool {
        self.source
            .as_ref()
            .is_some_and(|s| s["signature"].as_str() != self.signature())
    }
    /// 提供缓存标识，重新检测设备时相同内容不再回读所有页面。
    pub fn signature(&self) -> Option<&str> {
        self.display.as_ref().map(|d| d.signature.as_str())
    }
    /// 首次采集完成即可截图，包括真实读取错误，不无限等待。
    pub fn ready(&self) -> bool {
        self.attempted
    }
    /// 用户改串口或设备切换模式后清除旧设备缓存。
    pub fn clear_device(&mut self) {
        self.display = None;
    }
    /// 只消费上传成功或 CRC 回读成功的 JPEG 页，缺页时保持错误而不伪造画面。
    pub fn accept(&mut self, value: &mut Value, ctx: &egui::Context) -> Result<(), String> {
        if value["mode"] != "calendar" {
            self.clear_device();
            return Ok(());
        }
        let signature = value["calendar_signature"]
            .as_str()
            .ok_or("日历缺少签名")?
            .to_owned();
        let elapsed = value["calendar_elapsed_ms"]
            .as_u64()
            .ok_or("日历缺少分页相位")?;
        let page_ms = value["calendar_page_ms"]
            .as_u64()
            .ok_or("日历缺少分页时长")?;
        let old = self.display.as_ref().filter(|d| d.signature == signature);
        let raw_pages = value["calendar_pages"]
            .as_array_mut()
            .ok_or("日历缺少页面列表")?;
        if raw_pages.len() != 1 || page_ms != 0 || elapsed != 0 {
            return Err("日历分页回执无效".into());
        }
        let mut pages = Vec::new();
        for (index, meta) in raw_pages.iter_mut().enumerate() {
            if let Some(data) = meta.as_object_mut().unwrap().remove("data") {
                let bytes: Vec<u8> = serde_json::from_value(data).map_err(|e| e.to_string())?;
                if meta["size"].as_u64() != Some(bytes.len() as u64) {
                    return Err("日历图片大小不一致".into());
                }
                let image = image::load_from_memory(&bytes)
                    .map_err(|e| e.to_string())?
                    .to_rgba8();
                if image.dimensions() != (320, 240) {
                    return Err("日历图片尺寸不是320×240".into());
                }
                pages.push(ctx.load_texture(
                    format!("calendar-{index}"),
                    egui::ColorImage::from_rgba_unmultiplied([320, 240], image.as_raw()),
                    egui::TextureOptions::LINEAR,
                ));
            } else if let Some(texture) = old.and_then(|d| d.pages.get(index)) {
                pages.push(texture.clone());
            } else {
                return Err("日历页面尚未完整回读，请重新检测连接".into());
            }
        }
        self.display = Some(Display { signature, pages });
        Ok(())
    }
    /// 左侧展示本机最新任务，已完成文字加删除线；所有控件均不写源库。
    pub fn editor(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.label(RichText::new("当日日历清单").size(16.).strong());
        ui.label("跟随日历清单当前子日历 · 5 秒只读刷新");
        ui.add_space(12.);
        if let Some(raw) = &self.source {
            match serde_json::from_value::<Snapshot>(raw.clone()) {
                Ok(data) => {
                    ui.label(format!("{} · {}", data.date, data.account));
                    ui.label(RichText::new(&data.db_path).size(10.).color(Color32::GRAY));
                    ui.add_space(12.);
                    for (index, item) in data.items.iter().enumerate() {
                        let text = RichText::new(format!("{}. {}", index + 1, item.text));
                        ui.label(if item.done {
                            text.strikethrough().color(Color32::GRAY)
                        } else {
                            text
                        });
                        ui.add_space(6.);
                    }
                    if data.items.is_empty() {
                        ui.label("当日暂无任务");
                    }
                }
                Err(error) => {
                    ui.colored_label(Color32::DARK_RED, error.to_string());
                }
            }
        } else {
            ui.label("正在读取本地日历…");
        }
        if let Some(error) = &self.error {
            ui.colored_label(Color32::DARK_RED, error);
        }
        ui.add_space(14.);
        if ui
            .add_enabled(self.worker.is_none(), egui::Button::new("刷新清单"))
            .clicked()
        {
            self.refresh(ctx);
        }
        ui.label(
            RichText::new(
                "完成状态请在日历清单中修改；内容变化才更新屏幕。固定第一页，不自动翻页。",
            )
            .size(12.)
            .color(Color32::GRAY),
        );
    }
    /// 始终显示设备已确认的第一页，主页也复用同一份图像。
    pub fn paint_device(&self, p: &egui::Painter, rect: egui::Rect) {
        if let Some(display) = &self.display {
            p.image(
                display.pages[0].id(),
                rect,
                egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1., 1.)),
                Color32::WHITE,
            );
        } else {
            p.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "尚未回读设备日历画面",
                egui::FontId::proportional(14.),
                Color32::GRAY,
            );
        }
    }
    /// 右侧只展示设备已确认内容，未发送的最新清单不会覆盖旧画面。
    pub fn preview(&self, ui: &mut egui::Ui) {
        ui.label(RichText::new("设备日历画面 · 320 × 240").size(16.).strong());
        ui.add_space(12.);
        let width = ui.available_width().min(384.);
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(width, width * 0.75), egui::Sense::hover());
        let p = ui.painter_at(rect);
        p.rect_filled(rect, 0., Color32::BLACK);
        self.paint_device(&p, rect);
        ui.add_space(8.);
        ui.label("左侧为本地最新清单，右侧为设备确认的内容。");
    }
}
