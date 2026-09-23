//! Host-side 320×240 JPEG rendering (text / image / task labels / calendar).
//!
//! Fonts prefer Windows Chinese faces; Linux CI falls back to Noto Sans CJK or
//! the bundled Montserrat Medium. Pixel layout mirrors the former Pillow paths.

use ab_glyph::{Font, FontRef, FontVec, PxScale, ScaleFont};
use image::imageops::{FilterType, overlay};
use image::{DynamicImage, ImageBuffer, ImageEncoder, Rgb, RgbImage};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const SCREEN_W: u32 = 320;
pub const SCREEN_H: u32 = 240;
pub const SCREEN_SIZE: (u32, u32) = (SCREEN_W, SCREEN_H);

#[derive(Debug, Clone)]
pub struct RenderError(pub String);

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RenderError {}

impl From<&str> for RenderError {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<String> for RenderError {
    fn from(value: String) -> Self {
        Self(value)
    }
}

enum OwnedFont {
    Vec(FontVec),
    // Keep bytes alive for FontRef borrows from system files.
    Ref { _bytes: Vec<u8>, font: FontRef<'static> },
}

// SAFETY: bytes are owned and never moved; FontRef points into the leaked-or-stable allocation.
// We use Box::leak for the bytes so FontRef<'static> is sound for process lifetime caches.
unsafe impl Send for OwnedFont {}
unsafe impl Sync for OwnedFont {}

struct FontCache {
    regular: OwnedFont,
    bold: OwnedFont,
}

fn load_font_file(path: &Path, index: u32) -> Result<OwnedFont, RenderError> {
    let bytes = std::fs::read(path).map_err(|e| RenderError(format!("读取字体失败：{e}")))?;
    // Leak so FontRef can be 'static inside the OnceLock cache.
    let leaked: &'static [u8] = Box::leak(bytes.into_boxed_slice());
    let font = FontRef::try_from_slice_and_index(leaked, index)
        .map_err(|_| RenderError(format!("无法解析字体：{}", path.display())))?;
    Ok(OwnedFont::Ref {
        _bytes: Vec::new(),
        font,
    })
}

fn load_embedded_montserrat() -> Result<OwnedFont, RenderError> {
    let bytes = include_bytes!("../assets/Montserrat-Medium.ttf");
    let font = FontVec::try_from_vec(bytes.to_vec())
        .map_err(|_| RenderError("无法解析内嵌 Montserrat 字体".into()))?;
    Ok(OwnedFont::Vec(font))
}

fn windows_font_dir() -> PathBuf {
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
    PathBuf::from(windir).join("Fonts")
}

fn font_candidates(bold: bool) -> Vec<(PathBuf, u32)> {
    let mut out = Vec::new();
    let win = windows_font_dir();
    if bold {
        for name in ["msyhbd.ttc", "simhei.ttf", "arialbd.ttf"] {
            out.push((win.join(name), 0));
        }
    } else {
        for name in ["msyh.ttc", "simsun.ttc", "arial.ttf"] {
            out.push((win.join(name), 0));
        }
    }
    // Linux / CI: Noto Sans CJK (SC is typically face 3 in Google's TTC).
    let noto = PathBuf::from("/usr/share/fonts/opentype/noto");
    if bold {
        out.push((noto.join("NotoSansCJK-Bold.ttc"), 3));
    } else {
        out.push((noto.join("NotoSansCJK-Regular.ttc"), 3));
    }
    out
}

fn fonts() -> Result<&'static FontCache, RenderError> {
    static CACHE: OnceLock<Result<FontCache, String>> = OnceLock::new();
    let cached = CACHE.get_or_init(|| {
        let mut regular = None;
        let mut bold = None;
        for (path, index) in font_candidates(false) {
            if path.is_file() {
                if let Ok(font) = load_font_file(&path, index) {
                    regular = Some(font);
                    break;
                }
            }
        }
        for (path, index) in font_candidates(true) {
            if path.is_file() {
                if let Ok(font) = load_font_file(&path, index) {
                    bold = Some(font);
                    break;
                }
            }
        }
        let regular = match regular {
            Some(f) => f,
            None => load_embedded_montserrat().map_err(|e| e.0)?,
        };
        let bold = match bold {
            Some(f) => f,
            None => load_embedded_montserrat().map_err(|e| e.0)?,
        };
        Ok(FontCache { regular, bold })
    });
    match cached {
        Ok(cache) => Ok(cache),
        Err(e) => Err(RenderError(e.clone())),
    }
}

fn px_scale(size: f32) -> PxScale {
    PxScale::from(size)
}

fn text_width_font<F: Font>(font: &F, size: f32, text: &str) -> f32 {
    let scale = px_scale(size);
    let scaled = font.as_scaled(scale);
    let sample = if text.is_empty() { " " } else { text };
    let mut width = 0.0f32;
    let mut last = None;
    for ch in sample.chars() {
        let id = font.glyph_id(ch);
        if let Some(prev) = last {
            width += scaled.kern(prev, id);
        }
        width += scaled.h_advance(id);
        last = Some(id);
    }
    width
}

fn text_width(size: f32, bold: bool, text: &str) -> Result<f32, RenderError> {
    let cache = fonts()?;
    Ok(match if bold { &cache.bold } else { &cache.regular } {
        OwnedFont::Vec(font) => text_width_font(font, size, text),
        OwnedFont::Ref { font, .. } => text_width_font(font, size, text),
    })
}

fn wrap_text(size: f32, bold: bool, text: &str, max_width: f32) -> Result<Vec<String>, RenderError> {
    let mut lines = Vec::new();
    let paragraphs: Vec<&str> = if text.is_empty() {
        vec![""]
    } else {
        text.split('\n').collect()
    };
    for paragraph in paragraphs {
        let mut current = String::new();
        for ch in paragraph.chars() {
            let mut candidate = current.clone();
            candidate.push(ch);
            if current.is_empty() || text_width(size, bold, &candidate)? <= max_width {
                current = candidate;
            } else {
                lines.push(current);
                current = ch.to_string();
            }
        }
        lines.push(current);
    }
    Ok(lines)
}

fn fit_single_line_font(
    text: &str,
    max_width: f32,
    start_size: i32,
    minimum_size: i32,
    bold: bool,
) -> Result<f32, RenderError> {
    for size in (minimum_size..=start_size).rev() {
        if text_width(size as f32, bold, text)? <= max_width {
            return Ok(size as f32);
        }
    }
    Ok(minimum_size as f32)
}

fn fit_body(text: &str, max_width: f32, max_height: f32) -> Result<(f32, Vec<String>, f32), RenderError> {
    for size in (14..=27).rev() {
        let lines = wrap_text(size as f32, false, text, max_width)?;
        let line_height = size as f32 + 7.0;
        if (lines.len() as f32) * line_height <= max_height {
            return Ok((size as f32, lines, line_height));
        }
    }
    let lines = wrap_text(14.0, false, text, max_width)?;
    let max_lines = (max_height / 21.0).floor().max(1.0) as usize;
    Ok((14.0, lines.into_iter().take(max_lines).collect(), 21.0))
}

fn aligned_x(text: &str, size: f32, bold: bool, alignment: &str, left: f32, width: f32) -> Result<f32, RenderError> {
    let measured = text_width(size, bold, text)?;
    Ok(match alignment {
        "left" => left,
        "right" => left + width - measured,
        _ => left + (width - measured) / 2.0,
    })
}

fn draw_text_font<F: Font>(
    image: &mut RgbImage,
    font: &F,
    x: f32,
    y: f32,
    text: &str,
    size: f32,
    color: [u8; 3],
) {
    let scale = px_scale(size);
    let scaled = font.as_scaled(scale);
    let baseline = y + scaled.ascent();
    let mut caret = x;
    let mut last = None;
    for ch in text.chars() {
        let id = font.glyph_id(ch);
        if let Some(prev) = last {
            caret += scaled.kern(prev, id);
        }
        let glyph = id.with_scale_and_position(scale, ab_glyph::point(caret, baseline));
        if let Some(outlined) = font.outline_glyph(glyph) {
            let bounds = outlined.px_bounds();
            outlined.draw(|gx, gy, cover| {
                let px = bounds.min.x as i32 + gx as i32;
                let py = bounds.min.y as i32 + gy as i32;
                if px < 0 || py < 0 {
                    return;
                }
                let (px, py) = (px as u32, py as u32);
                if px >= image.width() || py >= image.height() {
                    return;
                }
                let cover = cover.clamp(0.0, 1.0);
                if cover <= 0.0 {
                    return;
                }
                let pixel = image.get_pixel_mut(px, py);
                for i in 0..3 {
                    let src = color[i] as f32;
                    let dst = pixel[i] as f32;
                    pixel[i] = (dst * (1.0 - cover) + src * cover).round() as u8;
                }
            });
        }
        caret += scaled.h_advance(id);
        last = Some(id);
    }
}

fn draw_text(
    image: &mut RgbImage,
    x: f32,
    y: f32,
    text: &str,
    size: f32,
    bold: bool,
    color: [u8; 3],
) -> Result<(), RenderError> {
    let cache = fonts()?;
    match if bold { &cache.bold } else { &cache.regular } {
        OwnedFont::Vec(font) => draw_text_font(image, font, x, y, text, size, color),
        OwnedFont::Ref { font, .. } => draw_text_font(image, font, x, y, text, size, color),
    }
    Ok(())
}

fn parse_color(value: &str) -> Result<[u8; 3], RenderError> {
    let raw = value.trim().trim_start_matches('#');
    if raw.len() != 6 || !raw.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(RenderError(format!("颜色必须是 #RRGGBB：{value}")));
    }
    let r = u8::from_str_radix(&raw[0..2], 16).unwrap();
    let g = u8::from_str_radix(&raw[2..4], 16).unwrap();
    let b = u8::from_str_radix(&raw[4..6], 16).unwrap();
    Ok([r, g, b])
}

fn draw_line(image: &mut RgbImage, x0: i32, y0: i32, x1: i32, y1: i32, color: [u8; 3], width: i32) {
    // Horizontal / near-horizontal lines used by layouts.
    if y0 == y1 {
        let (xa, xb) = if x0 <= x1 { (x0, x1) } else { (x1, x0) };
        for t in 0..width.max(1) {
            let y = y0 + t;
            if y < 0 || y as u32 >= image.height() {
                continue;
            }
            for x in xa..=xb {
                if x >= 0 && (x as u32) < image.width() {
                    image.put_pixel(x as u32, y as u32, Rgb(color));
                }
            }
        }
        return;
    }
    let dx = (x1 - x0) as f32;
    let dy = (y1 - y0) as f32;
    let steps = dx.abs().max(dy.abs()).max(1.0) as i32;
    for i in 0..=steps {
        let t = i as f32 / steps as f32;
        let x = (x0 as f32 + dx * t).round() as i32;
        let y = (y0 as f32 + dy * t).round() as i32;
        for ox in 0..width.max(1) {
            for oy in 0..width.max(1) {
                let px = x + ox;
                let py = y + oy;
                if px >= 0 && py >= 0 && (px as u32) < image.width() && (py as u32) < image.height() {
                    image.put_pixel(px as u32, py as u32, Rgb(color));
                }
            }
        }
    }
}

fn draw_filled_circle(image: &mut RgbImage, cx: i32, cy: i32, r: i32, color: [u8; 3]) {
    for y in (cy - r)..=(cy + r) {
        for x in (cx - r)..=(cx + r) {
            if (x - cx) * (x - cx) + (y - cy) * (y - cy) <= r * r
                && x >= 0
                && y >= 0
                && (x as u32) < image.width()
                && (y as u32) < image.height()
            {
                image.put_pixel(x as u32, y as u32, Rgb(color));
            }
        }
    }
}

fn draw_rounded_rect_outline(
    image: &mut RgbImage,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    radius: i32,
    color: [u8; 3],
    width: i32,
) {
    // Approximate Pillow rounded_rectangle outline.
    draw_line(image, x0 + radius, y0, x1 - radius, y0, color, width);
    draw_line(image, x0 + radius, y1, x1 - radius, y1, color, width);
    draw_line(image, x0, y0 + radius, x0, y1 - radius, color, width);
    draw_line(image, x1, y0 + radius, x1, y1 - radius, color, width);
    for t in 0..width.max(1) {
        let r = (radius - t).max(1);
        for (ox, oy) in [(1, 1), (-1, 1), (1, -1), (-1, -1)] {
            let cx = if ox > 0 { x0 + radius } else { x1 - radius };
            let cy = if oy > 0 { y0 + radius } else { y1 - radius };
            for a in 0..90 {
                let rad = (a as f32).to_radians();
                let x = cx + ox * (r as f32 * rad.cos()).round() as i32;
                let y = cy + oy * (r as f32 * rad.sin()).round() as i32;
                if x >= 0 && y >= 0 && (x as u32) < image.width() && (y as u32) < image.height() {
                    image.put_pixel(x as u32, y as u32, Rgb(color));
                }
            }
        }
    }
}

/// Encode one exact-size baseline JPEG for the HoloCubic decoder (quality 88).
pub fn encode_jpeg(image: &RgbImage) -> Result<Vec<u8>, RenderError> {
    if image.dimensions() != SCREEN_SIZE {
        return Err(RenderError(format!(
            "画面必须是 {}x{}。",
            SCREEN_W, SCREEN_H
        )));
    }
    let mut buf = Cursor::new(Vec::new());
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 88);
    encoder
        .write_image(
            image.as_raw(),
            SCREEN_W,
            SCREEN_H,
            image::ExtendedColorType::Rgb8,
        )
        .map_err(|e| RenderError(format!("JPEG 编码失败：{e}")))?;
    Ok(buf.into_inner())
}

pub fn render_text_frame(
    title: &str,
    body: &str,
    footer: &str,
    accent: &str,
) -> Result<RgbImage, RenderError> {
    let bg = parse_color("#02070B")?;
    let fg = parse_color("#F4FBFF")?;
    let highlight = parse_color(accent)?;
    let mut image = ImageBuffer::from_pixel(SCREEN_W, SCREEN_H, Rgb(bg));
    draw_rounded_rect_outline(&mut image, 8, 8, 311, 231, 13, highlight, 2);
    draw_line(&mut image, 22, 61, 297, 61, highlight, 1);
    draw_filled_circle(&mut image, 23, 24, 4, highlight);

    let title_size = fit_single_line_font(title, 252.0, 31, 17, true)?;
    let title_x = aligned_x(title, title_size, true, "center", 34.0, 263.0)?;
    draw_text(&mut image, title_x, 17.0, title, title_size, true, fg)?;

    let (body_size, lines, line_height) = fit_body(body, 274.0, 124.0)?;
    let block_height = lines.len() as f32 * line_height;
    let mut body_y = 73.0 + (124.0 - block_height).max(0.0) / 2.0;
    for line in &lines {
        let line_x = aligned_x(line, body_size, false, "center", 23.0, 274.0)?;
        draw_text(&mut image, line_x, body_y, line, body_size, false, fg)?;
        body_y += line_height;
    }

    let footer_size = fit_single_line_font(footer, 260.0, 14, 11, true)?;
    let footer_x = aligned_x(footer, footer_size, true, "center", 30.0, 260.0)?;
    draw_text(
        &mut image,
        footer_x,
        207.0,
        footer,
        footer_size,
        true,
        highlight,
    )?;
    Ok(image)
}

pub fn render_image_frame(path: &Path, background: &str) -> Result<RgbImage, RenderError> {
    let bg = parse_color(background)?;
    let source = image::open(path).map_err(|e| RenderError(format!("无法打开图片：{e}")))?;
    // image crate applies EXIF orientation via `ImageReader` optionally; DynamicImage::open
    // does not auto-transpose in older versions — use `image::imageops` when orientation present.
    let source = match source {
        DynamicImage::ImageRgba8(rgba) => {
            let mut base = ImageBuffer::from_pixel(rgba.width(), rgba.height(), image::Rgba([bg[0], bg[1], bg[2], 255]));
            overlay(&mut base, &rgba, 0, 0);
            DynamicImage::ImageRgba8(base).to_rgb8()
        }
        other if other.color().has_alpha() => {
            let rgba = other.to_rgba8();
            let mut base = ImageBuffer::from_pixel(rgba.width(), rgba.height(), image::Rgba([bg[0], bg[1], bg[2], 255]));
            overlay(&mut base, &rgba, 0, 0);
            DynamicImage::ImageRgba8(base).to_rgb8()
        }
        other => other.to_rgb8(),
    };
    let mut source = DynamicImage::ImageRgb8(source);
    source = source.resize(SCREEN_W, SCREEN_H, FilterType::Lanczos3);
    let fitted = source.to_rgb8();
    let mut frame = ImageBuffer::from_pixel(SCREEN_W, SCREEN_H, Rgb(bg));
    let x = (SCREEN_W - fitted.width()) / 2;
    let y = (SCREEN_H - fitted.height()) / 2;
    overlay(&mut frame, &fitted, x as i64, y as i64);
    Ok(frame)
}

/// Centered bottom title + up to two top note lines on a black sparse JPEG.
pub fn task_labels(title: &str, footer: &str) -> Result<Vec<u8>, RenderError> {
    let mut frame = ImageBuffer::from_pixel(SCREEN_W, SCREEN_H, Rgb([0, 0, 0]));
    let mut note_size = 13.0;
    let mut note_lines = wrap_text(note_size, false, footer, 215.0)?;
    for size in (10..=13).rev() {
        let lines = wrap_text(size as f32, false, footer, 215.0)?;
        if lines.len() <= 2 {
            note_size = size as f32;
            note_lines = lines;
            break;
        }
        note_size = size as f32;
        note_lines = lines;
    }
    for (index, line) in note_lines.iter().enumerate() {
        draw_text(
            &mut frame,
            16.0,
            8.0 + index as f32 * 16.0,
            line,
            note_size,
            false,
            [130, 185, 196],
        )?;
    }
    let title_size = fit_single_line_font(title, 288.0, 18, 8, true)?;
    let width = text_width(title_size, true, title)?;
    draw_text(
        &mut frame,
        (320.0 - width) / 2.0,
        218.0,
        title,
        title_size,
        true,
        [225, 248, 255],
    )?;
    encode_jpeg(&frame)
}

/// Calendar first-page JPEG list (always one page).
pub fn render_calendar_pages(snapshot: &serde_json::Value) -> Result<Vec<Vec<u8>>, RenderError> {
    let items = snapshot
        .get("items")
        .and_then(|v| v.as_array())
        .ok_or_else(|| RenderError("日历快照缺少 items".into()))?;
    let done = snapshot.get("done").and_then(|v| v.as_u64()).unwrap_or(0) as i64;
    let total = snapshot.get("total").and_then(|v| v.as_u64()).unwrap_or(0) as i64;
    let date = snapshot.get("date").and_then(|v| v.as_str()).unwrap_or("");
    let weekday = snapshot.get("weekday").and_then(|v| v.as_str()).unwrap_or("");
    let account = snapshot.get("account").and_then(|v| v.as_str()).unwrap_or("");

    let font_size = 14.0;
    let mut rows: Vec<(usize, bool, String, bool, f32)> = Vec::new();
    let mut y = 60.0f32;
    let mut truncated = false;
    for (index, item) in items.iter().enumerate() {
        let text = item.get("text").and_then(|v| v.as_str()).unwrap_or("");
        let is_done = item.get("done").and_then(|v| v.as_bool()).unwrap_or(false);
        let lines = wrap_text(font_size, false, text, 270.0)?;
        for (line_index, line) in lines.iter().enumerate() {
            if y + 18.0 > 217.0 {
                truncated = true;
                break;
            }
            let show_number = line_index == 0 || rows.is_empty();
            rows.push((index + 1, show_number, line.clone(), is_done, y));
            y += 18.0;
        }
        if truncated {
            break;
        }
        y += 6.0;
    }

    let mut frame = ImageBuffer::from_pixel(SCREEN_W, SCREEN_H, Rgb([0, 0, 0]));
    draw_text(&mut frame, 12.0, 7.0, "日历清单", 18.0, true, [222, 247, 255])?;
    let progress = format!("完成 {done}/{total}");
    draw_text(&mut frame, 226.0, 10.0, &progress, 11.0, false, [81, 221, 199])?;
    let heading = format!("{date}  {weekday} · {account}");
    let heading_size = fit_single_line_font(&heading, 296.0, 11, 8, false)?;
    draw_text(
        &mut frame,
        12.0,
        33.0,
        &heading,
        heading_size,
        false,
        [120, 155, 169],
    )?;
    draw_line(&mut frame, 12, 51, 308, 51, [30, 57, 65], 1);
    if rows.is_empty() {
        draw_text(
            &mut frame,
            75.0,
            116.0,
            "当日暂无任务",
            20.0,
            false,
            [133, 166, 174],
        )?;
    }
    for (number, show_number, text, is_done, top) in &rows {
        let color = if *is_done {
            [121, 146, 154]
        } else {
            [227, 241, 247]
        };
        if *show_number {
            draw_text(
                &mut frame,
                12.0,
                *top + 2.0,
                &format!("{number:02}"),
                10.0,
                false,
                [81, 221, 199],
            )?;
        }
        draw_text(&mut frame, 38.0, *top, text, font_size, false, color)?;
        if *is_done {
            let width = text_width(font_size, false, text)?;
            // Approximate mid-line of the text row for strike-through.
            let mid_y = (*top + font_size * 0.55).round() as i32;
            draw_line(
                &mut frame,
                38,
                mid_y,
                38 + width.round() as i32,
                mid_y,
                color,
                1,
            );
        }
    }
    draw_line(&mut frame, 12, 220, 308, 220, [30, 57, 65], 1);
    draw_text(
        &mut frame,
        12.0,
        225.0,
        "本地只读",
        10.0,
        false,
        [120, 155, 169],
    )?;
    let footer = if truncated {
        "仅第一页 · 还有内容"
    } else {
        "固定显示第一页"
    };
    draw_text(
        &mut frame,
        158.0,
        225.0,
        footer,
        10.0,
        false,
        [120, 155, 169],
    )?;
    Ok(vec![encode_jpeg(&frame)?])
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::GenericImageView;

    #[test]
    fn jpeg_is_exact_screen_size() {
        let frame = ImageBuffer::from_pixel(SCREEN_W, SCREEN_H, Rgb([0, 0, 0]));
        let jpeg = encode_jpeg(&frame).unwrap();
        let decoded = image::load_from_memory(&jpeg).unwrap();
        assert_eq!(decoded.dimensions(), SCREEN_SIZE);
        assert!(jpeg.len() < 20_000);
    }

    #[test]
    fn task_labels_are_sparse_black() {
        let raw = task_labels("良率数据核对", "先核对口径，再核对数据").unwrap();
        let frame = image::load_from_memory(&raw).unwrap().to_rgb8();
        assert_eq!(frame.dimensions(), SCREEN_SIZE);
        assert_eq!(*frame.get_pixel(0, 0), Rgb([0, 0, 0]));
        assert_eq!(*frame.get_pixel(160, 120), Rgb([0, 0, 0]));
        assert!(raw.len() < 12_000);
    }

    #[test]
    fn text_frame_renders() {
        let frame = render_text_frame("今日任务", "专注开发\n保持节奏", "USB DIRECT", "#35E7FF").unwrap();
        assert_eq!(frame.dimensions(), SCREEN_SIZE);
        let jpeg = encode_jpeg(&frame).unwrap();
        assert!(!jpeg.is_empty());
    }
}
