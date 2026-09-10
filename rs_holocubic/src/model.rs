//! 编辑配置、模式和确定性的百分比/时间换算。

use serde::{Deserialize, Serialize};

/// 首批可实际发送到设备的显示模式。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Mode {
    Home,
    #[default]
    Hourglass,
    Text,
    Image,
    Radar,
    Calendar,
}

impl Mode {
    /// 返回模式栏和编辑标题共用的中文名称。
    pub fn name(self) -> &'static str {
        match self {
            Self::Home => "主页",
            Self::Hourglass => "沙漏",
            Self::Text => "文字",
            Self::Image => "图片",
            Self::Radar => "AI 雷达",
            Self::Calendar => "日历",
        }
    }
}

/// 三个模式各自保留草稿，显式保存到 exe 旁的 JSON 文件。
#[derive(Clone, Serialize, Deserialize)]
pub struct Settings {
    pub mode: Mode,
    pub port: String,
    pub task_name: String,
    pub footer: String,
    pub hours: u32,
    pub minutes: u32,
    pub seconds: u32,
    pub remaining: f64,
    pub timed: bool,
    pub looping: bool,
    pub accent: [u8; 3],
    pub text_title: String,
    pub text_body: String,
    pub text_footer: String,
    pub image_path: String,
}

impl Default for Settings {
    /// 给首次启动提供可编辑的本地任务，不伪装成真实 Codex 额度。
    fn default() -> Self {
        Self {
            mode: Mode::Hourglass,
            port: String::new(),
            task_name: "专注开发".into(),
            footer: "完成一件重要的事".into(),
            hours: 0,
            minutes: 25,
            seconds: 0,
            remaining: 100.0,
            timed: true,
            looping: false,
            accent: [53, 231, 255],
            text_title: "今日任务".into(),
            text_body: "专注开发\n保持节奏".into(),
            text_footer: "USB DIRECT".into(),
            image_path: String::new(),
        }
    }
}

impl Settings {
    /// 合并时分秒并校验设备允许的 1 秒至 7 天时长。
    pub fn duration(&self) -> Result<u32, String> {
        if self.hours > 168 || self.minutes > 59 || self.seconds > 59 {
            return Err("时间字段超出范围".into());
        }
        let value = self.hours * 3600 + self.minutes * 60 + self.seconds;
        if !(1..=604800).contains(&value) {
            return Err("任务时长应在 1 秒至 7 天之间".into());
        }
        Ok(value)
    }

    /// 发送前校验输入；任务名和备注限制保证设备小屏幕可读。
    pub fn validate_task(&self) -> Result<(), String> {
        self.duration()?;
        basis_points(self.remaining)?;
        if self.task_name.trim().is_empty()
            || self.task_name.chars().count() > 24
            || self.task_name.contains(['\n', '\r'])
        {
            return Err("任务名称需为 1～24 个字符".into());
        }
        if self.footer.chars().count() > 36 || self.footer.contains(['\n', '\r']) {
            return Err("顶部说明最多 36 个字符，不能换行".into());
        }
        Ok(())
    }
}

/// 使用整数百分位传输，72.35% 对应 7235，避免固件 float32 精度损失。
pub fn basis_points(value: f64) -> Result<u32, String> {
    if !value.is_finite() || !(0.0..=100.0).contains(&value) {
        return Err("百分比应在 0.00～100.00 之间".into());
    }
    Ok((value * 100.0).round() as u32)
}

/// 统一显示两位小数，输入是精确的整数百分位。
pub fn format_percent(value: u32) -> String {
    format!("{}.{:02}%", value / 100, value % 100)
}

/// 将秒数格式化为可显示超过 24 小时的倒计时。
pub fn format_duration(value: u32) -> String {
    format!(
        "{:02}:{:02}:{:02}",
        value / 3600,
        value / 60 % 60,
        value % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 覆盖小数舍入、最大值和不合法浮点输入。
    #[test]
    fn precise_percent() {
        assert_eq!(basis_points(72.35), Ok(7235));
        assert_eq!(format_percent(1), "0.01%");
        assert_eq!(format_percent(10000), "100.00%");
        assert!(basis_points(f64::NAN).is_err());
        assert!(basis_points(100.01).is_err());
    }

    /// 覆盖任务时长边界和大于一天的显示格式。
    #[test]
    fn duration_bounds() {
        let mut settings = Settings::default();
        assert_eq!(settings.duration(), Ok(1500));
        settings.hours = 168;
        settings.minutes = 0;
        assert_eq!(settings.duration(), Ok(604800));
        settings.seconds = 1;
        assert!(settings.duration().is_err());
        assert_eq!(format_duration(90061), "25:01:01");
    }

    /// 草稿序列化不会丢失中文和小数。
    #[test]
    fn settings_roundtrip() {
        let original = Settings {
            remaining: 72.35,
            looping: true,
            ..Settings::default()
        };
        let bytes = serde_json::to_vec(&original).unwrap();
        let decoded: Settings = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(decoded.task_name, original.task_name);
        assert_eq!(decoded.remaining, 72.35);
        assert!(decoded.looping);
    }
}
