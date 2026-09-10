//! 用一次 USB 校准取得任务参数，两端分别计时；布局与计算公式对齐 quota_scene.lua。

use crate::model::{format_duration, format_percent};
use eframe::egui::{self, Color32};
use serde::Deserialize;
use serde_json::Value;
use std::time::{Duration, Instant};

/// 设备已确认的任务状态；不包含编辑草稿，缺失字段按协议错误处理。
#[derive(Clone, Debug, Deserialize)]
pub struct TaskState {
    pub bp: u32,
    pub initial_bp: u32,
    pub timed: bool,
    pub looping: bool,
    pub reset_seconds: u32,
    pub counter: u32,
    pub frames: u64,
    pub paused: bool,
    pub duration: u32,
    pub accent: u32,
    pub labels_crc: String,
}

impl TaskState {
    /// 验证外部回执的范围和计时关系，防止错误数据伪装成正常画面。
    fn parse(value: Value) -> Result<Self, String> {
        if value.get("initial_bp").is_none() {
            return Err("设备程序需更新：缺少本地计时所需的 initial_bp".into());
        }
        if value.get("looping").is_none() {
            return Err("设备程序需更新：缺少循环模式状态".into());
        }
        let state: Self =
            serde_json::from_value(value).map_err(|e| format!("设备画面状态无效：{e}"))?;
        if state.bp > 10000
            || state.initial_bp > 10000
            || state.accent > 0xffffff
            || !(1..=604800).contains(&state.duration)
            || state.reset_seconds > state.duration
            || state.counter != state.duration - state.reset_seconds
            || (state.looping && state.reset_seconds == 0)
        {
            return Err("设备画面状态超出范围".into());
        }
        Ok(state)
    }

    /// 状态徽标严格沿用设备暂停、完成、运行的判断顺序。
    fn badge(&self) -> &str {
        if self.paused {
            "PAUSE"
        } else if self.reset_seconds == 0 {
            "DONE"
        } else {
            "RUN"
        }
    }

    /// 从校准快照和本机单调时钟推算当前值；不累加帧误差，也不依赖串口连接。
    fn project(&self, age: Duration) -> Self {
        let mut current = self.clone();
        if self.paused || self.reset_seconds == 0 {
            return current;
        }
        let elapsed_ms = if self.looping {
            age.as_millis()
        } else {
            age.as_millis().min(self.reset_seconds as u128 * 1000)
        } as u64;
        if self.looping {
            let phase =
                (self.counter as u128 * 1000 + elapsed_ms as u128) % (self.duration as u128 * 1000);
            current.counter = (phase / 1000) as u32;
            current.reset_seconds = self.duration - current.counter;
        } else {
            current.reset_seconds -= (elapsed_ms / 1000) as u32;
            current.counter = current.duration - current.reset_seconds;
        }
        current.frames += elapsed_ms / 50;
        if current.timed && elapsed_ms >= 1000 {
            // 固件使用 float32；计算顺序与 Lua 完全一致，最后取整数百分位。
            current.bp = (current.initial_bp as f32
                * (current.reset_seconds as f32 / current.duration as f32)
                + 0.5)
                .floor() as u32;
        }
        current
    }
}

/// 保留一次校准快照和单调时钟起点；后续绘制独立运行，命令回执重新校准。
#[derive(Default)]
pub struct Mirror {
    task: Option<TaskState>,
    labels: Option<egui::TextureHandle>,
    crc: Option<String>,
    received_at: Option<Instant>,
}

impl Mirror {
    /// 告知桥接端本地已持有的设备标签，避免重复通过 USB 传输静态图片。
    pub fn labels_crc(&self) -> Option<&str> {
        self.crc.as_deref()
    }

    /// 基于最近确认的参数计算本帧，编辑草稿和连接状态均不参与计算。
    pub fn current_task(&self) -> Option<TaskState> {
        self.task
            .as_ref()
            .map(|state| state.project(self.received_at.unwrap().elapsed()))
    }

    /// 仅接收完整、校验过的设备快照；标签缺失时不使用草稿补画。
    pub fn update(&mut self, value: &mut Value, ctx: &egui::Context) -> Result<(), String> {
        if value["mode"] != "task" {
            *self = Self::default();
            return Ok(());
        }
        let state = TaskState::parse(value.clone())?;
        if let Some(bytes) = value.as_object_mut().unwrap().remove("labels") {
            let bytes: Vec<u8> = serde_json::from_value(bytes).map_err(|e| e.to_string())?;
            let img = image::load_from_memory(&bytes)
                .map_err(|e| e.to_string())?
                .to_rgba8();
            if img.dimensions() != (320, 240) {
                return Err("设备标签不是 320×240".into());
            }
            self.labels = Some(ctx.load_texture(
                "device-task-labels",
                egui::ColorImage::from_rgba_unmultiplied([320, 240], img.as_raw()),
                egui::TextureOptions::NEAREST,
            ));
            self.crc = Some(state.labels_crc.clone());
        }
        if self.crc.as_deref() != Some(&state.labels_crc) || self.labels.is_none() {
            return Err("设备标签尚未完整同步，请重新检测连接".into());
        }
        self.task = Some(state);
        self.received_at = Some(
            Instant::now() - Duration::from_millis(value["_transport_ms"].as_u64().unwrap_or(0)),
        );
        Ok(())
    }

    /// 明示本地独立计时，不把上次通信成功伪装成持续在线检测。
    pub fn caption(&self, connected: bool, changing: bool) -> String {
        if changing {
            return "正在更新设备，等待确认…".into();
        }
        if !connected {
            return if self.task.is_some() {
                "通信未确认 · 按上次参数继续本地运行"
            } else {
                "连接设备后读取实际沙漏"
            }
            .into();
        }
        if self.task.is_some() {
            "本地独立计算 · 无后台查询".into()
        } else {
            "设备当前未运行任务沙漏".into()
        }
    }

    /// 按设备 320×240 坐标绘制所有元素，标题来自实际 JPEG，未发送草稿不参与。
    pub fn paint(&self, p: &egui::Painter, rect: egui::Rect, state: Option<&TaskState>) {
        p.rect_filled(rect, 0.0, Color32::BLACK);
        let Some(state) = state else {
            p.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "等待设备沙漏画面",
                egui::FontId::proportional(16.0),
                Color32::GRAY,
            );
            return;
        };
        let scale = rect.width() / 320.0;
        let point = |x: f32, y: f32| rect.min + egui::vec2(x * scale, y * scale);
        let box_at = |x: f32, y: f32, w: f32, h: f32| {
            egui::Rect::from_min_size(point(x, y), egui::vec2(w * scale, h * scale))
        };
        let block = |x: f32, y: f32, w: f32, h: f32, rgb: u32, radius: f32| {
            p.rect_filled(box_at(x, y, w, h), radius * scale, color(rgb));
        };
        let text = |x: f32, y: f32, w: f32, h: f32, value: &str, size: f32, rgb: u32| {
            p.with_clip_rect(box_at(x, y, w, h)).text(
                point(x, y),
                egui::Align2::LEFT_TOP,
                value,
                egui::FontId::new(size * scale, egui::FontFamily::Name("device".into())),
                color(rgb),
            );
        };
        p.image(
            self.labels.as_ref().unwrap().id(),
            rect,
            egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1., 1.)),
            Color32::WHITE,
        );
        block(241., 12., 63., 22., 0x153039, 5.);
        text(249., 16., 54., 16., state.badge(), 12., 0xffc276);
        block(16., 43., 288., 1., 0x19343d, 0.);
        block(32., 56., 96., 5., 0x73d8e8, 2.);
        block(32., 201., 96., 5., 0x73d8e8, 2.);
        for outline in [
            [(36., 64.), (76., 126.), (76., 134.), (36., 196.)],
            [(124., 64.), (84., 126.), (84., 134.), (124., 196.)],
        ] {
            for segment in outline.windows(2) {
                p.line_segment(
                    [
                        point(segment[0].0, segment[0].1),
                        point(segment[1].0, segment[1].1),
                    ],
                    egui::Stroke::new(2. * scale, color(0x76d5e6)),
                );
            }
        }
        for index in 0..30 {
            if let Some((x, y, width)) = sand_row(state.bp, index, true) {
                block(x, y, width, 2., state.accent, 0.);
            }
            if let Some((x, y, width)) = sand_row(state.bp, index, false) {
                block(
                    x,
                    y,
                    width,
                    2.,
                    if state.bp <= 2000 { 0x80512b } else { 0x287d90 },
                    0.,
                );
            }
        }
        if state.bp > 0 && state.reset_seconds > 0 {
            let frame = state.frames.saturating_sub(1);
            let span = (3. + 60. * (state.bp as f32 / 10000.).sqrt()).max(2.);
            for index in 1..=7 {
                let phase = ((frame * 2 + index * 9) % 64) as f32 / 64.;
                let x = 78. + ((index + frame / 8) % 3) as f32;
                block(x, 128. + (phase * span).floor(), 2., 3., 0xabf5ff, 0.);
            }
        }
        text(162., 59., 142., 18., "REMAINING", 12., 0x619aa8);
        text(
            160.,
            79.,
            144.,
            35.,
            &format_percent(state.bp),
            28.,
            state.accent,
        );
        block(162., 119., 142., 4., 0x102b34, 1.);
        if state.bp > 0 {
            block(
                162.,
                119.,
                (142. * state.bp as f32 / 10000.).floor().max(1.),
                4.,
                state.accent,
                1.,
            );
        }
        text(162., 139., 142., 17., "TIME LEFT", 12., 0x619aa8);
        text(
            162.,
            161.,
            142.,
            27.,
            &format_duration(state.reset_seconds),
            20.,
            0xe1f8ff,
        );
        text(
            162.,
            191.,
            142.,
            18.,
            &format!("ELAPSED {:04}", state.counter),
            12.,
            0x619aa8,
        );
        block(16., 216., 288., 1., 0x19343d, 0.);
    }
}

/// 复刻设备的整像素沙层公式，返回当前可见行的横坐标、纵坐标和宽度。
fn sand_row(bp: u32, index: u32, upper: bool) -> Option<(f32, f32, f32)> {
    let fraction = (bp as f32 / 10000.).sqrt();
    let y = index as f32 * 2.;
    let midpoint = y + 1.;
    let (surface, visible) = if upper {
        (60. * (1. - fraction), bp > 0)
    } else {
        (60. * fraction, bp < 10000)
    };
    if !visible || midpoint < surface {
        return None;
    }
    let half = (39.
        * if upper {
            1. - midpoint / 60.
        } else {
            midpoint / 60.
        })
    .floor()
    .max(1.);
    Some((
        80. - half,
        if upper { 66. + y } else { 134. + y },
        half * 2.,
    ))
}

/// 将设备使用的 24 位 RGB 整数转换为不透明桌面颜色。
fn color(rgb: u32) -> Color32 {
    Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 提供一份设备回执，而不是根据编辑框生成预览状态。
    fn sample() -> Value {
        json!({"bp":7235,"reset_seconds":119,"counter":1,"frames":21,"paused":false,
            "duration":120,"accent":0x35e7ff,"labels_crc":"1234abcd","initial_bp":7235,"timed":false,"looping":false})
    }

    /// 缺字段或计时不一致必须报错，不能退回假数据。
    #[test]
    fn device_state_is_required() {
        let state = TaskState::parse(sample()).unwrap();
        assert_eq!(state.bp, 7235);
        assert_eq!(state.badge(), "RUN");
        let mut bad = sample();
        bad["counter"] = json!(2);
        assert!(TaskState::parse(bad).is_err());
        assert!(TaskState::parse(json!({"bp":10000})).is_err());
    }

    /// 本地持续运行不再受 1.5 秒补间上限影响，手动百分比只改变时间。
    #[test]
    fn independent_manual_clock_advances_without_connection() {
        let state = TaskState::parse(sample()).unwrap();
        let current = state.project(Duration::from_secs(60));
        assert_eq!(current.reset_seconds, 59);
        assert_eq!(current.counter, 61);
        assert_eq!(current.frames, 1221);
        assert_eq!(current.bp, 7235);
        assert_eq!(state.reset_seconds, 119);
    }

    /// 定时百分比使用原始启动值计算，不拿已经四舍五入的当前值反推。
    #[test]
    fn independent_timed_clock_matches_device_formula() {
        let mut raw = sample();
        raw["timed"] = json!(true);
        raw["bp"] = json!(7175);
        let state = TaskState::parse(raw).unwrap();
        let current = state.project(Duration::from_secs(60));
        assert_eq!(current.bp, 3557);
        assert_eq!(current.reset_seconds, 59);
        let done = state.project(Duration::from_secs(604800));
        assert_eq!(done.bp, 0);
        assert_eq!(done.reset_seconds, 0);
        assert_eq!(done.counter, 120);
        assert_eq!(done.badge(), "DONE");
    }

    /// 整轮边界立即满时长；跨过多轮仍保留余数，不丢失计时误差。
    #[test]
    fn looping_restarts_at_boundary_and_handles_multiple_rounds() {
        let mut state = TaskState::parse(sample()).unwrap();
        state.looping = true;
        state.timed = true;
        let boundary = state.project(Duration::from_secs(119));
        assert_eq!(boundary.reset_seconds, 120);
        assert_eq!(boundary.counter, 0);
        assert_eq!(boundary.bp, 7235);
        assert_eq!(boundary.badge(), "RUN");
        let next = state.project(Duration::from_secs(360));
        assert_eq!(next.reset_seconds, 119);
        assert_eq!(next.counter, 1);
        assert_eq!(next.bp, 7175);
        state.paused = true;
        assert_eq!(state.project(Duration::from_secs(600)).counter, 1);
        state.paused = false;
        state.timed = false;
        assert_eq!(state.project(Duration::from_secs(360)).bp, 7235);
    }

    /// 暂停和结束冻结本地时钟；继续或重置回执作为新的校准起点。
    #[test]
    fn pause_completion_and_recalibration() {
        let mut state = TaskState::parse(sample()).unwrap();
        state.paused = true;
        let held = state.project(Duration::from_secs(60));
        assert_eq!(held.badge(), "PAUSE");
        assert_eq!(held.reset_seconds, 119);
        assert_eq!(held.frames, 21);
        state.paused = false;
        assert_eq!(state.project(Duration::from_secs(1)).reset_seconds, 118);
        state.reset_seconds = 0;
        state.counter = 120;
        let done = state.project(Duration::from_secs(60));
        assert_eq!(done.badge(), "DONE");
        assert_eq!(done.frames, 21);
        state.reset_seconds = 120;
        state.counter = 0;
        let restarted = state.project(Duration::from_secs(3));
        assert_eq!(restarted.reset_seconds, 117);
        assert_eq!(restarted.counter, 3);
    }

    /// 真机只读取两次，中间 3 秒完全不通信；允许串口延迟带来的 1 秒相位差。
    #[test]
    #[ignore = "需 COM4 正在运行新版任务沙漏；仅读取状态，不重启任务"]
    fn usb_clock_advances_without_intermediate_queries() {
        let first = crate::bridge::call(json!({"action":"status","port":"COM4"})).unwrap();
        let baseline = TaskState::parse(first).unwrap();
        let started = Instant::now();
        std::thread::sleep(Duration::from_secs(3));
        let second = crate::bridge::call(
            json!({"action":"status","port":"COM4","labels_crc":baseline.labels_crc}),
        )
        .unwrap();
        let actual = TaskState::parse(second).unwrap();
        let local = baseline.project(started.elapsed());
        assert_eq!(actual.duration, baseline.duration);
        assert_eq!(actual.looping, baseline.looping);
        let delta = local.reset_seconds.abs_diff(actual.reset_seconds);
        let boundary = baseline.looping && delta >= baseline.duration.saturating_sub(1);
        assert!(delta <= 1 || boundary);
        let rounding_bound = baseline.initial_bp.div_ceil(baseline.duration) + 1;
        if !boundary {
            assert!(local.bp.abs_diff(actual.bp) <= rounding_bound);
        }
        println!(
            "Only 2 USB samples / 3 seconds: local={} / {} bp; device={} / {} bp",
            local.reset_seconds, local.bp, actual.reset_seconds, actual.bp
        );
    }

    /// 空沙、满沙和中间值遵循设备整像素三角形的边界。
    #[test]
    fn sand_matches_device_rows() {
        assert!(sand_row(0, 29, true).is_none());
        assert!(sand_row(10000, 29, false).is_none());
        assert_eq!(sand_row(10000, 0, true), Some((42., 66., 76.)));
        assert_eq!(sand_row(0, 29, false), Some((42., 192., 76.)));
        assert!(sand_row(7235, 0, true).is_none());
        assert!(sand_row(7235, 10, true).is_some());
    }
}
