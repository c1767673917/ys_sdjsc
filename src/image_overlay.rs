//! 在导出图片左上角叠加单据编号 (等价 app/core/image_overlay.py)
//!
//! 布局算法与原实现一致: 字号 = clamp(短边×4%, 22, 96), 超宽时按 0.85 递减到不小于 18;
//! 半透明黑色圆角底框 (alpha 170) + 白字; 先按 EXIF 方向校正; 保持原格式输出(JPEG 质量 92)。

use ab_glyph::{Font, FontArc, PxScale, ScaleFont};
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, Rgba, RgbaImage};
use once_cell::sync::OnceCell;
use std::io::Cursor;

const LOG: &str = "app.core.image_overlay";

const FONT_CANDIDATES: [&str; 15] = [
    "/System/Library/Fonts/Hiragino Sans GB.ttc",
    "/System/Library/Fonts/STHeiti Light.ttc",
    "/System/Library/Fonts/PingFang.ttc",
    "/Library/Fonts/Arial Unicode.ttf",
    "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
    "/System/Library/Fonts/Supplemental/Arial.ttf",
    "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
    "/usr/share/fonts/truetype/wqy/wqy-microhei.ttf",
    "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationSans-Bold.ttf",
    "C:\\Windows\\Fonts\\msyh.ttc",
    "C:\\Windows\\Fonts\\arialbd.ttf",
    "C:\\Windows\\Fonts\\arial.ttf",
];

static FONT: OnceCell<Option<FontArc>> = OnceCell::new();

fn load_font() -> Option<&'static FontArc> {
    FONT.get_or_init(|| {
        for path in FONT_CANDIDATES {
            if !std::path::Path::new(path).is_file() {
                continue;
            }
            let Ok(data) = std::fs::read(path) else { continue };
            // TTC 取第0个字体
            if let Ok(f) = ab_glyph::FontVec::try_from_vec_and_index(data.clone(), 0) {
                return Some(FontArc::new(f));
            }
            if let Ok(f) = ab_glyph::FontVec::try_from_vec(data) {
                return Some(FontArc::new(f));
            }
            log::warn!(target: LOG, "无法加载字体 {}，改用默认字体", path);
        }
        None
    })
    .as_ref()
}

/// 在图片左上角写入单据编号；失败或无单号时返回原图字节
pub fn overlay_doc_number(image_bytes: &[u8], doc_number: Option<&str>) -> Vec<u8> {
    let text = doc_number.map(|s| crate::pyutil::py_strip(s)).unwrap_or("");
    if text.is_empty() {
        return image_bytes.to_vec();
    }
    match overlay_inner(image_bytes, text) {
        Ok(b) => b,
        Err(e) => {
            log::warn!(target: LOG, "叠加单据编号失败 doc_number={}，回退原图: {}", text, e);
            image_bytes.to_vec()
        }
    }
}

/// 文本墨迹边界 (等价 Pillow textbbox((0,0), text) 以 ascender 为顶的坐标)
struct TextBox {
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
}

fn layout(font: &FontArc, size: f32, text: &str) -> (PxScale, Vec<ab_glyph::Glyph>, TextBox) {
    // Pillow 的字号即 em 像素大小: PxScale = size * height / units_per_em
    let scale = match font.units_per_em() {
        Some(upem) if upem > 0.0 => PxScale::from(size * font.height_unscaled() / upem),
        _ => PxScale::from(size),
    };
    let scaled = font.as_scaled(scale);
    let ascent = scaled.ascent().ceil();
    let mut glyphs = Vec::new();
    let mut x = 0.0f32;
    let mut prev: Option<ab_glyph::GlyphId> = None;
    let (mut ix0, mut iy0, mut ix1, mut iy1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for ch in text.chars() {
        let id = scaled.glyph_id(ch);
        if let Some(p) = prev {
            x += scaled.kern(p, id);
        }
        let g = id.with_scale_and_position(scale, ab_glyph::point(x, ascent));
        if let Some(o) = font.outline_glyph(g.clone()) {
            let b = o.px_bounds();
            ix0 = ix0.min(b.min.x);
            iy0 = iy0.min(b.min.y);
            ix1 = ix1.max(b.max.x);
            iy1 = iy1.max(b.max.y);
        }
        x += scaled.h_advance(id);
        prev = Some(id);
        glyphs.push(g);
    }
    let bbox = if ix0 == f32::MAX {
        TextBox { x0: 0, y0: 0, x1: x.round() as i32, y1: 0 }
    } else {
        TextBox {
            x0: (ix0.floor() as i32).min(0),
            y0: iy0.floor() as i32,
            x1: (x.round() as i32).max(ix1.ceil() as i32),
            y1: iy1.ceil() as i32,
        }
    };
    (scale, glyphs, bbox)
}

/// Pillow 的 paste(color, mask): 各通道 out = ink*m + dst*(1-m)
fn blend_onto(dst: &mut Rgba<u8>, ink: [u8; 4], m: f32) {
    for c in 0..4 {
        let v = ink[c] as f32 * m + dst[c] as f32 * (1.0 - m);
        dst[c] = v.round().clamp(0.0, 255.0) as u8;
    }
}

/// Pillow 的 rounded_rectangle 填充 (包含边界像素)
fn fill_rounded_rect(img: &mut RgbaImage, x0: i32, y0: i32, x1: i32, y1: i32, r: i32, color: [u8; 4]) {
    let (w, h) = (img.width() as i32, img.height() as i32);
    let r = r.max(0) as f32;
    let corner_inside = |x: i32, y: i32| -> bool {
        let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
        let (left, top) = (x0 as f32, y0 as f32);
        let (right, bottom) = (x1 as f32 + 1.0, y1 as f32 + 1.0);
        let cx = if px < left + r {
            left + r
        } else if px > right - r {
            right - r
        } else {
            return true;
        };
        let cy = if py < top + r {
            top + r
        } else if py > bottom - r {
            bottom - r
        } else {
            return true;
        };
        let (dx, dy) = (px - cx, py - cy);
        dx * dx + dy * dy <= r * r
    };
    for y in y0.max(0)..=y1.min(h - 1) {
        for x in x0.max(0)..=x1.min(w - 1) {
            if corner_inside(x, y) {
                img.put_pixel(x as u32, y as u32, Rgba(color));
            }
        }
    }
}

fn overlay_inner(image_bytes: &[u8], text: &str) -> Result<Vec<u8>, String> {
    let reader = ImageReader::new(Cursor::new(image_bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    let format = reader.format();
    let mut decoder = reader.into_decoder().map_err(|e| e.to_string())?;
    let orientation = decoder.orientation().map_err(|e| e.to_string())?;
    let mut img = DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    img.apply_orientation(orientation);

    let font = load_font().ok_or_else(|| "未找到可用字体".to_string())?;

    let mut base = img.to_rgba8();
    let (width, height) = (base.width() as i32, base.height() as i32);
    let mut overlay = RgbaImage::from_pixel(base.width(), base.height(), Rgba([0, 0, 0, 0]));

    let min_side = width.min(height);
    let mut font_size = 22.max(((min_side as f64 * 0.04) as i32).min(96));
    let margin = 12.max((min_side as f64 * 0.018) as i32);
    let max_box_w = (width - margin * 2).max(1);

    let pads = |fs: i32| (10.max((fs as f64 * 0.45) as i32), 6.max((fs as f64 * 0.28) as i32));
    let (mut pad_x, mut pad_y) = pads(font_size);
    let (mut scale, mut glyphs, mut bbox) = layout(font, font_size as f32, text);
    let mut text_w = bbox.x1 - bbox.x0;
    let mut text_h = bbox.y1 - bbox.y0;

    while font_size > 18 && (text_w + pad_x * 2) > max_box_w {
        font_size = 18.max((font_size as f64 * 0.85) as i32);
        (pad_x, pad_y) = pads(font_size);
        (scale, glyphs, bbox) = layout(font, font_size as f32, text);
        text_w = bbox.x1 - bbox.x0;
        text_h = bbox.y1 - bbox.y0;
    }

    let bx1 = (margin + text_w + pad_x * 2).min(width - margin);
    let by1 = (margin + text_h + pad_y * 2).min(height - margin);
    let radius = 6.max(((font_size as f64 * 0.22) as i32).min((by1 - margin).div_euclid(2)));
    fill_rounded_rect(&mut overlay, margin, margin, bx1, by1, radius, [0, 0, 0, 170]);

    let text_x = (margin + pad_x - bbox.x0) as f32;
    let text_y = (margin + pad_y - bbox.y0) as f32;
    for g in glyphs {
        let mut g = g;
        g.position = ab_glyph::point(g.position.x + text_x, g.position.y + text_y);
        let _ = scale;
        if let Some(o) = font.outline_glyph(g) {
            let b = o.px_bounds();
            o.draw(|gx, gy, cov| {
                let x = b.min.x as i32 + gx as i32;
                let y = b.min.y as i32 + gy as i32;
                if x >= 0 && y >= 0 && x < width && y < height && cov > 0.0 {
                    let p = overlay.get_pixel_mut(x as u32, y as u32);
                    blend_onto(p, [255, 255, 255, 255], cov.min(1.0));
                }
            });
        }
    }

    // alpha_composite(base, overlay)
    for (bp, op) in base.pixels_mut().zip(overlay.pixels()) {
        let sa = op[3] as f32 / 255.0;
        if sa == 0.0 {
            continue;
        }
        let da = bp[3] as f32 / 255.0;
        let oa = sa + da * (1.0 - sa);
        for c in 0..3 {
            let v = (op[c] as f32 * sa + bp[c] as f32 * da * (1.0 - sa)) / oa;
            bp[c] = v.round().clamp(0.0, 255.0) as u8;
        }
        bp[3] = (oa * 255.0).round().clamp(0.0, 255.0) as u8;
    }

    let out = DynamicImage::ImageRgba8(base);
    let mut buf = Cursor::new(Vec::new());
    match format {
        Some(ImageFormat::Png) => out.write_to(&mut buf, ImageFormat::Png).map_err(|e| e.to_string())?,
        Some(ImageFormat::WebP) => out.write_to(&mut buf, ImageFormat::WebP).map_err(|e| e.to_string())?,
        Some(ImageFormat::Gif) => DynamicImage::ImageRgb8(out.to_rgb8())
            .write_to(&mut buf, ImageFormat::Gif)
            .map_err(|e| e.to_string())?,
        _ => {
            let rgb = out.to_rgb8();
            let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 92);
            rgb.write_with_encoder(enc).map_err(|e| e.to_string())?;
        }
    }
    Ok(buf.into_inner())
}

/// 供测试比对: 返回 (font_size 22/40/96 下) 文本框尺寸
#[cfg(test)]
pub fn measure(size: f32, text: &str) -> Option<(i32, i32, i32, i32)> {
    let font = load_font()?;
    let (_, _, b) = layout(font, size, text);
    Some((b.x0, b.y0, b.x1, b.y1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_doc_number_returns_original() {
        let data = b"not an image".to_vec();
        assert_eq!(overlay_doc_number(&data, Some("  ")), data);
        assert_eq!(overlay_doc_number(&data, None), data);
        // 非图片: 回退原字节
        assert_eq!(overlay_doc_number(&data, Some("SO1")), data);
    }

    #[test]
    fn bbox_close_to_pillow() {
        // Pillow 参考值 (Hiragino Sans GB, size 40): (0, 4, 348, 37)
        if let Some((x0, y0, x1, y1)) = measure(40.0, "SO20250103001") {
            if std::path::Path::new(FONT_CANDIDATES[0]).exists() {
                assert_eq!(x0, 0);
                assert!((y0 - 4).abs() <= 1, "y0={}", y0);
                assert!((x1 - 348).abs() <= 3, "x1={}", x1);
                assert!((y1 - 37).abs() <= 1, "y1={}", y1);
            }
        }
    }

    #[test]
    fn overlays_png_and_keeps_size() {
        let img = RgbaImage::from_pixel(400, 300, Rgba([200, 200, 200, 255]));
        let mut buf = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(img).write_to(&mut buf, ImageFormat::Png).unwrap();
        let out = overlay_doc_number(buf.get_ref(), Some("SO20250103001"));
        if load_font().is_some() {
            assert_ne!(&out, buf.get_ref());
            let decoded = image::load_from_memory(&out).unwrap();
            assert_eq!((decoded.width(), decoded.height()), (400, 300));
            // 左上角底框区域应变暗
            let p = decoded.to_rgba8().get_pixel(20, 20).0;
            assert!(p[0] < 120, "{:?}", p);
        }
    }
}

