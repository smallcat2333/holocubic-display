//! 左侧模式列表、右侧内容编辑和预览；USB 通信由后台线程串行执行。

use crate::{
    bridge,
    calendar::CalendarPage,
    home::HomePage,
    mirror::Mirror,
    model::{Mode, Settings, format_duration, format_percent},
    radar::RadarPage,
};
use eframe::egui::{self, Color32, RichText, Vec2};
use egui_phosphor::regular as icons;
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::mpsc::{self, Receiver},
    time::{Duration, Instant},
};

const ACCENT: Color32 = Color32::from_rgb(22, 126, 112);
const MUTED: Color32 = Color32::from_rgb(105, 116, 123);
const INK: Color32 = Color32::from_rgb(30, 40, 46);

/// 单次后台任务返回值，动作名称用于区分端口、文件和设备状态。
struct Completion {
    action: String,
    result: Result<Value, String>,
}

/// GUI 保留未提交草稿，以设备确认的任务参数驱动本地独立时钟。
pub struct HoloApp {
    settings: Settings,
    settings_path: PathBuf,
    ports: Vec<String>,
    connected: bool,
    state: Option<Value>,
    mirror: Mirror,
    radar: RadarPage,
    home: HomePage,
    calendar: CalendarPage,
    calendar_sync: bool,
    radar_sync: bool,
    radar_sent: u64,
    last_contact: Instant,
    worker: Option<Receiver<Completion>>,
    pending: String,
    message: String,
    error: bool,
    image: Option<egui::TextureHandle>,
    screenshot: Option<String>,
    capture_at: Instant,
    capture_requested: bool,
}

impl HoloApp {
    /// 加载中文及图标字体和本地草稿，异步枚举串口但不改动屏幕内容。
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        screenshot: Option<String>,
        open_radar: bool,
        open_home: bool,
        open_calendar: bool,
    ) -> Self {
        configure(&cc.egui_ctx);
        let settings_path = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .join("holocubic_settings.json");
        let (mut settings, message, error): (Settings, String, bool) = if settings_path.exists() {
            match std::fs::read(&settings_path)
                .map_err(|e| e.to_string())
                .and_then(|raw| serde_json::from_slice(&raw).map_err(|e| e.to_string()))
            {
                Ok(value) => (value, "已读取本地配置".into(), false),
                Err(e) => (Settings::default(), format!("配置读取失败：{e}"), true),
            }
        } else {
            (Settings::default(), "等待连接设备".into(), false)
        };
        if open_radar {
            settings.mode = Mode::Radar;
        } else if open_home {
            settings.mode = Mode::Home;
        } else if open_calendar {
            settings.mode = Mode::Calendar;
        }
        let home = HomePage::load(settings_path.with_file_name("home_settings.json"))
            .expect("主页配置读取失败，请检查 home_settings.json");
        let mut app = Self {
            settings,
            settings_path,
            message,
            error,
            ports: vec![],
            connected: false,
            state: None,
            mirror: Mirror::default(),
            radar: RadarPage::default(),
            home,
            calendar: CalendarPage::default(),
            calendar_sync: false,
            radar_sync: false,
            radar_sent: 0,
            last_contact: Instant::now(),
            worker: None,
            pending: String::new(),
            image: None,
            screenshot,
            capture_at: Instant::now(),
            capture_requested: false,
        };
        app.submit("ports", &cc.egui_ctx);
        app
    }

    /// 序列化当前草稿后启动唯一工作线程，不允许重复提交占用同一串口。
    fn submit(&mut self, action: &str, ctx: &egui::Context) {
        if self.worker.is_some() {
            return;
        }
        if action == "task" {
            if let Err(error) = self.settings.validate_task() {
                self.fail(error);
                return;
            }
        }
        let mut request = json!({"action":action,"port":self.settings.port,"settings":self.settings,
            "labels_crc":self.mirror.labels_crc(),"radar_crc":self.radar.device_crc(),"home_crcs":self.home.asset_crcs(),"calendar_signature":self.calendar.signature()});
        if matches!(action, "calendar_update" | "calendar_only")
            || (action == "home" && self.home.wants_calendar())
        {
            let Some(data) = self.calendar.snapshot() else {
                self.fail("请等待首次日历读取完成".into());
                return;
            };
            request["calendar_snapshot"] = data;
        }
        if action == "home" {
            match self.home.entries() {
                Ok(entries) => request["home_entries"] = entries,
                Err(error) => {
                    self.fail(error);
                    return;
                }
            }
        }
        if matches!(action, "radar_usb" | "radar_only")
            || (action == "home" && self.home.wants_radar())
        {
            let Some(snapshot) = self.radar.snapshot() else {
                self.fail("请等待雷达首次采集完成".into());
                return;
            };
            request["snapshot"] = snapshot;
            self.radar_sent = self.radar.revision();
        }
        if matches!(action, "task" | "text" | "image" | "clear" | "home") {
            self.radar_sync = false;
            self.calendar_sync = false;
        }
        let (sender, receiver) = mpsc::channel();
        self.worker = Some(receiver);
        self.pending = action.into();
        let action = action.to_owned();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = bridge::call(request);
            let _ = sender.send(Completion { action, result });
            ctx.request_repaint();
        });
    }

    /// 展示错误而不覆盖编辑内容或把旧设备状态当成新回执。
    fn fail(&mut self, error: String) {
        self.message = error;
        self.error = true;
    }

    /// 只消费用户操作的完成结果，不安排任何周期性 USB 查询。
    fn poll(&mut self, ctx: &egui::Context) {
        let completion = self.worker.as_ref().and_then(|rx| rx.try_recv().ok());
        if let Some(completion) = completion {
            self.worker = None;
            match completion.result {
                Err(error) => {
                    if completion.action != "browse" {
                        self.connected = false;
                        self.radar_sync = false;
                        self.calendar_sync = false;
                    }
                    self.fail(error);
                }
                Ok(mut value) => match completion.action.as_str() {
                    "ports" => {
                        self.ports = value["ports"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .filter_map(|p| p["device"].as_str().map(str::to_owned))
                            .collect();
                        if self.settings.port.is_empty() {
                            let candidates: Vec<_> = value["ports"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .filter(|p| p["vid"] == 0x303a && p["pid"] == 0x1001)
                                .collect();
                            if candidates.len() == 1 {
                                self.settings.port =
                                    candidates[0]["device"].as_str().unwrap().into();
                            }
                        }
                        if self.screenshot.is_some() && !self.settings.port.is_empty() {
                            self.submit("status", ctx);
                        }
                    }
                    "browse" => {
                        if let Some(path) = value["path"].as_str().filter(|s| !s.is_empty()) {
                            self.settings.image_path = path.into();
                            self.load_image(ctx);
                        }
                    }
                    _ => {
                        if let Err(error) = self.accept_display(&mut value, ctx) {
                            self.connected = false;
                            self.radar_sync = false;
                            self.calendar_sync = false;
                            self.calendar.clear_device();
                            self.home.clear_device();
                            self.radar.clear_device();
                            self.mirror = Mirror::default();
                            self.fail(error);
                            return;
                        }
                        if value["mode"] == "home" {
                            self.radar_sync = self.home.has_radar();
                            self.calendar_sync = self.home.has_calendar();
                        } else if matches!(completion.action.as_str(), "radar_usb" | "radar_only") {
                            self.radar_sync = true;
                        } else if value["mode"] != "radar" {
                            self.radar_sync = false;
                        }
                        if value["mode"] != "home" {
                            self.calendar_sync = value["mode"] == "calendar";
                        }
                        let was_connected = self.connected;
                        self.last_contact = Instant::now();
                        self.connected = true;
                        self.state = Some(value);
                        if completion.action != "status" || self.error || !was_connected {
                            self.message =
                                format!("{}：设备已确认", action_name(&completion.action));
                            self.error = false;
                        }
                    }
                },
            }
        }
    }

    /// 主页回执保留全部后台场景；单模式回执仍按原来的真实预览路径消费。
    fn accept_display(&mut self, value: &mut Value, ctx: &egui::Context) -> Result<(), String> {
        if value["mode"] == "home" {
            let transport = value.get("_transport_ms").cloned();
            if let Some(task) = value.get_mut("task") {
                if let Some(delay) = &transport {
                    task["_transport_ms"] = delay.clone();
                }
                self.mirror.update(task, ctx)?;
            } else {
                self.mirror = Mirror::default();
            }
            if let Some(radar) = value.get_mut("radar") {
                if let Some(delay) = &transport {
                    radar["_transport_ms"] = delay.clone();
                }
                self.radar.accept_device(radar)?;
            } else {
                self.radar.clear_device();
            }
            if let Some(calendar) = value.get_mut("calendar") {
                if let Some(delay) = &transport {
                    calendar["_transport_ms"] = delay.clone();
                }
                self.calendar.accept(calendar, ctx)?;
            } else {
                self.calendar.clear_device();
            }
        } else {
            self.radar.accept_device(value)?;
            self.mirror.update(value, ctx)?;
            self.calendar.accept(value, ctx)?;
        }
        self.home.accept(value, ctx)
    }

    /// 显式保存草稿至 exe 同目录，不默认设置开机自启或访问外部账号。
    fn save(&mut self) {
        let result = serde_json::to_vec_pretty(&self.settings)
            .map_err(|e| e.to_string())
            .and_then(|raw| std::fs::write(&self.settings_path, raw).map_err(|e| e.to_string()))
            .and_then(|()| self.home.save());
        match result {
            Ok(()) => {
                self.message = "配置已保存".into();
                self.error = false;
            }
            Err(error) => self.fail(format!("保存失败：{error}")),
        }
    }

    /// 读取用户选定的图片，限制像素数并缩成屏幕大小作为编辑预览。
    fn load_image(&mut self, ctx: &egui::Context) {
        let result = (|| -> Result<_, String> {
            let (width, height) =
                image::image_dimensions(&self.settings.image_path).map_err(|e| e.to_string())?;
            if width as u64 * height as u64 > 40_000_000 {
                return Err("图片超过 4000 万像素".into());
            }
            let img = image::open(&self.settings.image_path)
                .map_err(|e| e.to_string())?
                .thumbnail(320, 240)
                .to_rgba8();
            let size = [img.width() as usize, img.height() as usize];
            Ok(ctx.load_texture(
                "selected-image",
                egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw()),
                egui::TextureOptions::LINEAR,
            ))
        })();
        match result {
            Ok(texture) => self.image = Some(texture),
            Err(error) => {
                self.image = None;
                self.fail(error);
            }
        }
    }

    /// 顶部连接工具栏包含端口选择、刷新、检测及配置保存。
    fn header(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("header")
            .exact_height(62.0)
            .frame(
                egui::Frame::new()
                    .fill(Color32::WHITE)
                    .inner_margin(egui::Margin::symmetric(20, 14)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("HoloCubic").size(22.0).strong().color(INK));
                    ui.add_space(8.0);
                    ui.label(RichText::new("显示控制台").color(MUTED));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if icon_button(ui, icons::FLOPPY_DISK, "保存配置").clicked() {
                            self.save();
                        }
                        ui.add_space(6.0);
                        let busy = self.worker.is_some();
                        if ui
                            .add_enabled(
                                !busy,
                                egui::Button::new(format!("{}  检测连接", icons::PLUG)),
                            )
                            .on_hover_text("只读取一次设备状态并校准本地时钟，不持续查询")
                            .clicked()
                        {
                            self.submit("status", ctx);
                        }
                        if ui
                            .add_enabled(!busy, egui::Button::new(icons::ARROWS_CLOCKWISE))
                            .on_hover_text("刷新串口")
                            .clicked()
                        {
                            self.submit("ports", ctx);
                        }
                        let old_port = self.settings.port.clone();
                        ui.add_enabled_ui(!busy, |ui| {
                            egui::ComboBox::from_id_salt("ports")
                                .width(115.0)
                                .selected_text(if self.settings.port.is_empty() {
                                    "自动识别"
                                } else {
                                    &self.settings.port
                                })
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(
                                        &mut self.settings.port,
                                        String::new(),
                                        "自动识别",
                                    );
                                    for port in &self.ports {
                                        ui.selectable_value(
                                            &mut self.settings.port,
                                            port.clone(),
                                            port,
                                        );
                                    }
                                });
                        });
                        if old_port != self.settings.port {
                            self.radar_sync = false;
                            self.radar.clear_device();
                            self.home.clear_device();
                            self.calendar.clear_device();
                            self.calendar_sync = false;
                            self.connected = false;
                            self.state = None;
                            self.mirror = Mirror::default();
                        }
                    });
                });
            });
    }

    /// 左侧固定宽度的模式列表，底部显示 USB 连接与版本。
    fn sidebar(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("modes")
            .exact_width(166.0)
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(Color32::from_rgb(240, 244, 245))
                    .inner_margin(16),
            )
            .show(ctx, |ui| {
                ui.add_space(10.0);
                ui.label(RichText::new("显示模式").size(12.0).color(MUTED));
                ui.add_space(16.0);
                for (mode, icon) in [
                    (Mode::Home, icons::HOUSE),
                    (Mode::Hourglass, icons::HOURGLASS),
                    (Mode::Text, icons::TEXT_T),
                    (Mode::Image, icons::IMAGE),
                    (Mode::Radar, "◎"),
                    (Mode::Calendar, icons::CALENDAR),
                ] {
                    let selected = self.settings.mode == mode;
                    let button = egui::Button::new(
                        RichText::new(format!("{icon}     {}", mode.name())).size(16.0),
                    )
                    .selected(selected)
                    .frame(selected)
                    .min_size(egui::vec2(132.0, 46.0));
                    if ui.add(button).clicked() {
                        self.settings.mode = mode;
                    }
                    ui.add_space(5.0);
                }
                ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                    ui.label(
                        RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                            .size(12.0)
                            .color(MUTED),
                    );
                    ui.add_space(10.0);
                    ui.colored_label(
                        if self.connected { ACCENT } else { MUTED },
                        if self.connected {
                            "USB 最近通信成功"
                        } else {
                            "USB 待检测"
                        },
                    );
                    ui.label(RichText::new("320 × 240").size(12.0).color(MUTED));
                });
            });
    }

    /// 沙漏输入区域：名称、时间、整数百分位对应的两位小数及颜色。
    fn hourglass_editor(&mut self, ui: &mut egui::Ui) {
        section(ui, "任务设置");
        field(ui, "任务名称（底部）");
        ui.add(
            egui::TextEdit::singleline(&mut self.settings.task_name)
                .desired_width(f32::INFINITY)
                .char_limit(24),
        );
        ui.add_space(15.0);
        field(ui, "进度方式");
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.settings.timed, true, "按时间递减");
            ui.selectable_value(&mut self.settings.timed, false, "手动百分比");
        });
        ui.add_space(8.0);
        ui.scope(|ui| {
            ui.visuals_mut().widgets.inactive.bg_stroke =
                egui::Stroke::new(1.0, Color32::from_rgb(142, 164, 171));
            ui.checkbox(&mut self.settings.looping, "循环")
                .on_hover_text("每轮结束后恢复设定时长和初始百分比，自动开始下一轮；应用后生效");
        });
        ui.add_space(15.0);
        field(ui, "任务时长");
        ui.horizontal(|ui| {
            ui.add(
                egui::DragValue::new(&mut self.settings.hours)
                    .range(0..=168)
                    .suffix(" 时")
                    .min_decimals(0),
            );
            ui.add(
                egui::DragValue::new(&mut self.settings.minutes)
                    .range(0..=59)
                    .suffix(" 分")
                    .min_decimals(0),
            );
            ui.add(
                egui::DragValue::new(&mut self.settings.seconds)
                    .range(0..=59)
                    .suffix(" 秒")
                    .min_decimals(0),
            );
        });
        ui.add_space(15.0);
        field(ui, "初始剩余");
        ui.add(
            egui::DragValue::new(&mut self.settings.remaining)
                .range(0.0..=100.0)
                .speed(0.01)
                .fixed_decimals(2)
                .suffix(" %"),
        );
        ui.add_space(15.0);
        field(ui, "顶部说明");
        ui.add(
            egui::TextEdit::singleline(&mut self.settings.footer)
                .desired_width(f32::INFINITY)
                .char_limit(36),
        );
        ui.add_space(15.0);
        self.color_editor(ui);
        ui.add_space(10.0);
        ui.label(
            RichText::new("编辑内容需点击“应用并开始”才会更新硬件及右侧画面。")
                .size(12.0)
                .color(MUTED),
        );
    }

    /// 文字模式编辑中文标题、正文和页脚，使用已有 Pillow 渲染路径。
    fn text_editor(&mut self, ui: &mut egui::Ui) {
        section(ui, "显示内容");
        field(ui, "标题");
        ui.add(
            egui::TextEdit::singleline(&mut self.settings.text_title).desired_width(f32::INFINITY),
        );
        ui.add_space(14.0);
        field(ui, "正文");
        ui.add(
            egui::TextEdit::multiline(&mut self.settings.text_body)
                .desired_width(f32::INFINITY)
                .desired_rows(5),
        );
        ui.add_space(14.0);
        field(ui, "底部说明");
        ui.add(
            egui::TextEdit::singleline(&mut self.settings.text_footer).desired_width(f32::INFINITY),
        );
        ui.add_space(14.0);
        self.color_editor(ui);
    }

    /// 图片模式使用原生文件选择器、路径输入和明确的预览加载命令。
    fn image_editor(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        section(ui, "图片文件");
        field(ui, "文件路径");
        let response = ui.add(
            egui::TextEdit::singleline(&mut self.settings.image_path).desired_width(f32::INFINITY),
        );
        if response.changed() {
            self.image = None;
        }
        ui.add_space(14.0);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    self.worker.is_none(),
                    egui::Button::new(format!("{}  选择图片", icons::FOLDER_OPEN)),
                )
                .clicked()
            {
                self.submit("browse", ctx);
            }
            if ui.button(format!("{}  加载预览", icons::IMAGE)).clicked() {
                self.load_image(ctx);
            }
        });
        ui.add_space(24.0);
        field(ui, "画面尺寸");
        ui.label("320 × 240");
        field(ui, "缩放");
        ui.label("等比适配 · 黑色留边");
    }

    /// 使用颜色控件和有限的高对比配色，不把颜色写成文字按钮。
    fn color_editor(&mut self, ui: &mut egui::Ui) {
        field(ui, "强调色");
        ui.horizontal(|ui| {
            ui.color_edit_button_srgb(&mut self.settings.accent);
            for rgb in [
                [53, 231, 255],
                [97, 231, 170],
                [255, 185, 90],
                [255, 107, 136],
            ] {
                let color = Color32::from_rgb(rgb[0], rgb[1], rgb[2]);
                if ui
                    .add(
                        egui::Button::new("")
                            .fill(color)
                            .min_size(egui::vec2(24.0, 24.0)),
                    )
                    .on_hover_text(format!("#{:02X}{:02X}{:02X}", rgb[0], rgb[1], rgb[2]))
                    .clicked()
                {
                    self.settings.accent = rgb;
                }
            }
        });
    }

    /// 只允许空闲且已检测连接时发送；暂停等命令只对正在运行的任务开放。
    fn actions(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.add_space(10.0);
        ui.separator();
        ui.add_space(8.0);
        let available = self.connected && self.worker.is_none();
        ui.horizontal_wrapped(|ui| {
            let (command, text) = match self.settings.mode {
                Mode::Home => ("home", "应用轮播"),
                Mode::Hourglass => ("task", "应用并开始"),
                Mode::Text => ("text", "发送文字"),
                Mode::Image => ("image", "发送图片"),
                Mode::Radar => ("radar_only", "应用到屏幕"),
                Mode::Calendar => ("calendar_only", "应用到屏幕"),
            };
            if ui
                .add_enabled(
                    available
                        && (!(self.settings.mode == Mode::Calendar
                            || (self.settings.mode == Mode::Home && self.home.wants_calendar()))
                            || self.calendar.snapshot().is_some())
                        && (!(self.settings.mode == Mode::Radar
                            || (self.settings.mode == Mode::Home && self.home.wants_radar()))
                            || self.radar.revision() > 0),
                    egui::Button::new(
                        RichText::new(format!("{}  {text}", icons::PLAY)).color(Color32::WHITE),
                    )
                    .fill(ACCENT)
                    .min_size(egui::vec2(130.0, 36.0)),
                )
                .clicked()
            {
                self.submit(command, ctx);
            }
            if self.settings.mode == Mode::Home
                && ui
                    .add_enabled(
                        available && self.home.running(),
                        egui::Button::new("停止轮播"),
                    )
                    .clicked()
            {
                self.submit("home_stop", ctx);
            }
            if self.settings.mode == Mode::Radar {
                if self.radar_sync
                    && ui
                        .add_enabled(self.worker.is_none(), egui::Button::new("停止同步"))
                        .clicked()
                {
                    self.radar_sync = false;
                }
                ui.label(if self.radar_sync {
                    "USB 雷达 · 每 5 秒传数值"
                } else {
                    "未同步 · 应用后切换设备显示"
                });
            }
            if ui
                .add_enabled(
                    available,
                    egui::Button::new(icons::STOP).min_size(egui::vec2(36.0, 36.0)),
                )
                .on_hover_text("停止并返回等待界面")
                .clicked()
            {
                self.submit("clear", ctx);
            }
            let task = self.mirror.current_task().is_some();
            let paused = self.mirror.current_task().is_some_and(|s| s.paused);
            if self.settings.mode == Mode::Hourglass {
                let (icon, action, hint) = if paused {
                    (icons::PLAY, "resume", "继续计时")
                } else {
                    (icons::PAUSE, "pause", "暂停计时")
                };
                if ui
                    .add_enabled(
                        available && task,
                        egui::Button::new(icon).min_size(egui::vec2(36.0, 36.0)),
                    )
                    .on_hover_text(hint)
                    .clicked()
                {
                    self.submit(action, ctx);
                }
                if ui
                    .add_enabled(
                        available && task,
                        egui::Button::new(icons::ARROW_COUNTER_CLOCKWISE)
                            .min_size(egui::vec2(36.0, 36.0)),
                    )
                    .on_hover_text("按设备原任务重新开始")
                    .clicked()
                {
                    self.submit("reset", ctx);
                }
            }
        });
    }

    /// 沙漏与状态区共享本帧本地计算结果；文字、图片仍展示未发送内容。
    fn preview(&self, ui: &mut egui::Ui) {
        if self.settings.mode == Mode::Calendar {
            self.calendar.preview(ui);
            return;
        }
        if self.settings.mode == Mode::Home {
            self.home
                .preview(ui, &self.mirror, &self.radar, &self.calendar);
            return;
        }
        if self.settings.mode == Mode::Radar {
            self.radar.preview(ui, self.connected);
            return;
        }
        let current_task = self.mirror.current_task();
        ui.horizontal(|ui| {
            section(
                ui,
                if self.settings.mode == Mode::Hourglass {
                    "任务运行画面"
                } else {
                    "编辑预览"
                },
            );
            ui.label(RichText::new("320 × 240").size(12.0).color(MUTED));
        });
        let width = ui.available_width().min(384.0);
        let (rect, _) =
            ui.allocate_exact_size(Vec2::new(width, width * 0.75), egui::Sense::hover());
        let p = ui.painter_at(rect);
        p.rect_filled(rect, 6.0, Color32::from_rgb(2, 7, 11));
        let scale = width / 320.0;
        let point = |x: f32, y: f32| rect.min + egui::vec2(x * scale, y * scale);
        let color = Color32::from_rgb(
            self.settings.accent[0],
            self.settings.accent[1],
            self.settings.accent[2],
        );
        match self.settings.mode {
            Mode::Home => unreachable!("主页使用轮播实际画面入口"),
            Mode::Calendar => unreachable!("日历使用真实分页入口"),
            Mode::Radar => unreachable!("雷达页面使用独立绘制入口"),
            Mode::Hourglass => {
                let changing =
                    self.worker.is_some() && self.pending != "status" && self.pending != "ports";
                self.mirror.paint(&p, rect, current_task.as_ref());
                ui.add_space(8.0);
                ui.label(
                    RichText::new(self.mirror.caption(self.connected, changing))
                        .size(12.0)
                        .color(MUTED),
                );
            }
            Mode::Text => {
                let title =
                    fit_preview_text(&p, &self.settings.text_title, 19.0 * scale, 280.0 * scale);
                p.galley(point(20., 20.), title, color);
                let mut font_size = 24.0 * scale;
                let mut body = p.layout(
                    self.settings.text_body.clone(),
                    egui::FontId::proportional(24.0 * scale),
                    Color32::WHITE,
                    274.0 * scale,
                );
                while body.size().y > 124.0 * scale && font_size > 10.0 * scale {
                    font_size -= scale;
                    body = p.layout(
                        self.settings.text_body.clone(),
                        egui::FontId::proportional(font_size),
                        Color32::WHITE,
                        274.0 * scale,
                    );
                }
                p.with_clip_rect(egui::Rect::from_min_max(point(23., 78.), point(297., 202.)))
                    .galley(point(23., 78.), body, Color32::WHITE);
                let footer =
                    fit_preview_text(&p, &self.settings.text_footer, 12.0 * scale, 280.0 * scale);
                p.galley(point(20., 214.), footer, color);
            }
            Mode::Image => {
                if let Some(texture) = &self.image {
                    let size = texture.size_vec2();
                    let ratio = (rect.width() / size.x).min(rect.height() / size.y);
                    let image_rect = egui::Rect::from_center_size(rect.center(), size * ratio);
                    p.image(
                        texture.id(),
                        image_rect,
                        egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1., 1.)),
                        Color32::WHITE,
                    );
                } else {
                    p.text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        icons::IMAGE,
                        egui::FontId::proportional(40.),
                        Color32::from_rgb(92, 119, 129),
                    );
                }
            }
        }
        ui.add_space(22.0);
        section(
            ui,
            if current_task.is_some() {
                "本地运行状态"
            } else {
                "最近设备状态"
            },
        );
        if let Some(state) = &self.state {
            if let Some(task) = current_task {
                let bp = task.bp;
                let secs = task.reset_seconds;
                let label = if task.paused {
                    "已暂停"
                } else if secs == 0 {
                    "已完成"
                } else if task.looping {
                    "循环运行"
                } else {
                    "运行中"
                };
                ui.horizontal(|ui| {
                    ui.colored_label(ACCENT, label);
                    ui.label(RichText::new(format_percent(bp)).strong().size(20.0));
                });
                ui.label(format!("剩余时间  {}", format_duration(secs)));
            } else {
                ui.label(if state["mode"] == "quota" {
                    "额度演示"
                } else {
                    "图片 / 等待界面"
                });
            }
            ui.add_space(6.0);
            ui.label(
                RichText::new(format!(
                    "上次校准：{} 秒前 · 无后台查询",
                    self.last_contact.elapsed().as_secs()
                ))
                .size(12.0)
                .color(MUTED),
            );
        } else {
            ui.colored_label(MUTED, "尚未检测设备");
        }
    }
}

impl eframe::App for HoloApp {
    /// 绘制窗口并消费后台消息；窄窗口自动纵向排列预览，避免控件重叠。
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll(ctx);
        self.calendar.tick(
            ctx,
            self.settings.mode == Mode::Calendar
                || self.calendar_sync
                || (self.settings.mode == Mode::Home && self.home.wants_calendar()),
        );
        if self.calendar_sync
            && self.connected
            && self.worker.is_none()
            && self.calendar.needs_send()
        {
            self.submit("calendar_update", ctx);
        }
        self.radar.tick(
            ctx,
            self.settings.mode == Mode::Radar
                || self.radar_sync
                || (self.settings.mode == Mode::Home && self.home.wants_radar()),
        );
        if self.radar_sync
            && self.connected
            && self.worker.is_none()
            && self.radar.revision() != self.radar_sent
            && self.last_contact.elapsed() >= Duration::from_secs(5)
        {
            self.submit("radar_usb", ctx);
        }
        self.header(ctx);
        egui::TopBottomPanel::bottom("status")
            .frame(
                egui::Frame::new()
                    .fill(Color32::WHITE)
                    .inner_margin(egui::Margin::symmetric(18, 9)),
            )
            .show(ctx, |ui| {
                ui.horizontal_wrapped(|ui| {
                    if self.worker.is_some() {
                        ui.spinner();
                        ui.label(format!("{}…", action_name(&self.pending)));
                    } else {
                        ui.colored_label(
                            if self.error {
                                Color32::from_rgb(184, 53, 66)
                            } else {
                                MUTED
                            },
                            &self.message,
                        );
                    }
                });
            });
        self.sidebar(ctx);
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(Color32::from_rgb(250, 251, 252))
                    .inner_margin(26),
            )
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(self.settings.mode.name())
                        .size(23.0)
                        .strong()
                        .color(INK),
                );
                ui.add_space(20.0);
                egui::ScrollArea::vertical()
                    .max_height((ui.available_height() - 76.0).max(120.0))
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let wide = ui.available_width() >= 620.0;
                        if wide {
                            ui.columns(2, |columns| {
                                columns[0].set_max_width(columns[0].available_width() - 16.0);
                                match self.settings.mode {
                                    Mode::Home => {
                                        if let Some(mode) = self.home.editor(&mut columns[0]) {
                                            self.settings.mode = mode;
                                        }
                                    }
                                    Mode::Hourglass => self.hourglass_editor(&mut columns[0]),
                                    Mode::Text => self.text_editor(&mut columns[0]),
                                    Mode::Image => self.image_editor(&mut columns[0], ctx),
                                    Mode::Radar => self.radar.editor(&mut columns[0], ctx),
                                    Mode::Calendar => self.calendar.editor(&mut columns[0], ctx),
                                }
                                self.preview(&mut columns[1]);
                            });
                        } else {
                            match self.settings.mode {
                                Mode::Home => {
                                    if let Some(mode) = self.home.editor(ui) {
                                        self.settings.mode = mode;
                                    }
                                }
                                Mode::Hourglass => self.hourglass_editor(ui),
                                Mode::Text => self.text_editor(ui),
                                Mode::Image => self.image_editor(ui, ctx),
                                Mode::Radar => self.radar.editor(ui, ctx),
                                Mode::Calendar => self.calendar.editor(ui, ctx),
                            }
                            ui.add_space(24.0);
                            self.preview(ui);
                        }
                    });
                self.actions(ui, ctx);
            });
        ctx.request_repaint_after(Duration::from_millis(50));
        if let Some(path) = &self.screenshot {
            if !self.capture_requested
                && self.capture_at.elapsed() > Duration::from_secs(3)
                && self.worker.is_none()
                && (self.settings.mode != Mode::Radar || self.radar.ready())
                && (self.settings.mode != Mode::Calendar || self.calendar.ready())
            {
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
                self.capture_requested = true;
            }
            for event in ctx.input(|i| i.events.clone()) {
                if let egui::Event::Screenshot { image, .. } = event {
                    let pixels: Vec<u8> = image.pixels.iter().flat_map(|p| p.to_array()).collect();
                    match image::save_buffer(
                        path,
                        &pixels,
                        image.size[0] as u32,
                        image.size[1] as u32,
                        image::ColorType::Rgba8,
                    ) {
                        Ok(()) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                        Err(error) => eprintln!("截图失败：{error}"),
                    }
                }
            }
        }
    }
}

/// 统一固定尺寸工具图标和悬浮说明。
fn icon_button(ui: &mut egui::Ui, icon: &str, hint: &str) -> egui::Response {
    ui.add(egui::Button::new(RichText::new(icon).size(18.0)).min_size(egui::vec2(32.0, 30.0)))
        .on_hover_text(hint)
}

/// 紧凑分节标题，不使用嵌套卡片。
fn section(ui: &mut egui::Ui, title: &str) {
    ui.label(RichText::new(title).size(15.0).strong().color(INK));
    ui.add_space(10.0);
}

/// 输入标签保持一致的密度和可读对比度。
fn field(ui: &mut egui::Ui, title: &str) {
    ui.label(RichText::new(title).size(13.0).color(MUTED));
    ui.add_space(4.0);
}

/// 在画布宽度内缩小长标题，保证中文预览不覆盖状态徽标。
fn fit_preview_text(
    p: &egui::Painter,
    text: &str,
    start_size: f32,
    width: f32,
) -> std::sync::Arc<egui::Galley> {
    let mut size = start_size;
    loop {
        let galley = p.layout_no_wrap(
            text.to_owned(),
            egui::FontId::proportional(size),
            Color32::WHITE,
        );
        if galley.size().x <= width || size <= 8.0 {
            return galley;
        }
        size -= 0.5;
    }
}

/// 将内部工作名称转成明确的命令反馈。
fn action_name(action: &str) -> &str {
    match action {
        "ports" => "刷新串口",
        "status" => "查询设备",
        "task" => "应用任务",
        "pause" => "暂停",
        "resume" => "继续",
        "reset" => "重置",
        "clear" => "停止显示",
        "text" => "发送文字",
        "image" => "发送图片",
        "home" => "应用轮播",
        "home_stop" => "停止轮播",
        "calendar_update" => "同步日历",
        "calendar_only" => "应用日历",
        "radar_only" => "应用雷达",
        "radar_usb" => "同步雷达",
        "browse" => "选择图片",
        _ => action,
    }
}

/// 加载 Windows 中文字体与 Phosphor 图标，使用轻量桌面工具主题。
fn configure(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "device".into(),
        egui::FontData::from_static(include_bytes!("../assets/Montserrat-Medium.ttf")).into(),
    );
    fonts.families.insert(
        egui::FontFamily::Name("device".into()),
        vec!["device".into()],
    );
    let bytes = std::fs::read(r"C:\Windows\Fonts\msyh.ttc")
        .or_else(|_| std::fs::read(r"C:\Windows\Fonts\simhei.ttf"))
        .expect("无法加载 Windows 中文字体");
    fonts
        .font_data
        .insert("cjk".into(), egui::FontData::from_owned(bytes).into());
    fonts
        .families
        .get_mut(&egui::FontFamily::Proportional)
        .unwrap()
        .push("cjk".into());
    fonts
        .families
        .get_mut(&egui::FontFamily::Monospace)
        .unwrap()
        .push("cjk".into());
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    ctx.set_fonts(fonts);
    let mut style = (*ctx.style()).clone();
    style.visuals = egui::Visuals::light();
    style.visuals.selection.bg_fill = Color32::from_rgb(211, 235, 229);
    style.visuals.selection.stroke.color = ACCENT;
    style.visuals.override_text_color = Some(INK);
    style.visuals.widgets.inactive.bg_fill = Color32::WHITE;
    style.spacing.item_spacing = egui::vec2(8.0, 7.0);
    style.spacing.button_padding = egui::vec2(10.0, 7.0);
    style.spacing.interact_size = egui::vec2(42.0, 30.0);
    style
        .text_styles
        .insert(egui::TextStyle::Body, egui::FontId::proportional(14.0));
    style
        .text_styles
        .insert(egui::TextStyle::Button, egui::FontId::proportional(14.0));
    ctx.set_style(style);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 即便最近通信已经过去很久，空闲 GUI 也不能自动创建 USB 工作线程。
    #[test]
    fn idle_gui_does_not_poll_usb() {
        let mut app = HoloApp {
            settings: Settings {
                port: "UNIT_TEST_NO_SERIAL_PORT".into(),
                ..Settings::default()
            },
            settings_path: PathBuf::new(),
            ports: vec![],
            connected: true,
            state: None,
            mirror: Mirror::default(),
            radar: RadarPage::default(),
            home: HomePage::default(),
            calendar: CalendarPage::default(),
            calendar_sync: false,
            radar_sync: false,
            radar_sent: 0,
            last_contact: Instant::now() - Duration::from_secs(10),
            worker: None,
            pending: String::new(),
            message: String::new(),
            error: false,
            image: None,
            screenshot: None,
            capture_at: Instant::now(),
            capture_requested: false,
        };
        let ctx = egui::Context::default();
        for _ in 0..100 {
            app.poll(&ctx);
            assert!(app.worker.is_none(), "空闲界面不能发起后台 USB 查询");
        }
    }
}
