//! 画像処理: JPEG デコード・リサイズ・回転と、タブバー/カーソルの合成。
//!
//! フレームはすべて BGRA(フレームバッファと同じ並び)の `Frame` で扱う。

use anyhow::{anyhow, Result};
use fast_image_resize as fr;
use std::sync::OnceLock;

#[derive(Clone)]
pub struct Frame {
    pub data: Vec<u8>, // BGRA, 行詰め(stride = w*4)
    pub w: u32,
    pub h: u32,
}

/// 縮尺の合わせ方。`Fill` は縦横比を無視して引き伸ばす、`Contain` は黒帯を付ける。
#[derive(Clone, Copy, PartialEq)]
pub enum Fit {
    Fill,
    Contain,
}

pub fn decode_jpeg(bytes: &[u8]) -> Result<Frame> {
    use zune_core::colorspace::ColorSpace;
    use zune_core::options::DecoderOptions;
    let opts = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::BGRA);
    let mut dec = zune_jpeg::JpegDecoder::new_with_options(std::io::Cursor::new(bytes), opts);
    let data = dec.decode().map_err(|e| anyhow!("JPEG デコード失敗: {e:?}"))?;
    let (w, h) = dec.dimensions().ok_or_else(|| anyhow!("JPEG の寸法が不明"))?;
    Ok(Frame { data, w: w as u32, h: h as u32 })
}

/// (w, h) へリサイズする。同寸ならそのまま返す。
pub fn resize(src: Frame, w: u32, h: u32, fit: Fit) -> Frame {
    if w == 0 || h == 0 {
        return Frame { data: Vec::new(), w, h };
    }
    if src.w == w && src.h == h {
        return src;
    }
    let (tw, th) = match fit {
        Fit::Fill => (w, h),
        Fit::Contain => {
            let s = (w as f64 / src.w as f64).min(h as f64 / src.h as f64);
            (((src.w as f64 * s).round() as u32).clamp(1, w), ((src.h as f64 * s).round() as u32).clamp(1, h))
        }
    };
    let scaled = scale(&src, tw, th);
    if tw == w && th == h {
        return scaled;
    }
    // 黒帯付きで中央に置く
    let mut out = vec![0u8; (w * h * 4) as usize];
    for px in out.chunks_exact_mut(4) {
        px[3] = 255;
    }
    let ox = (w - tw) / 2;
    let oy = (h - th) / 2;
    for y in 0..th {
        let s = (y * tw * 4) as usize;
        let d = (((oy + y) * w + ox) * 4) as usize;
        out[d..d + (tw * 4) as usize].copy_from_slice(&scaled.data[s..s + (tw * 4) as usize]);
    }
    Frame { data: out, w, h }
}

fn scale(src: &Frame, w: u32, h: u32) -> Frame {
    let fallback = || nearest(src, w, h);
    let Ok(img) = fr::images::ImageRef::new(src.w, src.h, &src.data, fr::PixelType::U8x4) else {
        return fallback();
    };
    let mut dst = fr::images::Image::new(w, h, fr::PixelType::U8x4);
    let opts = fr::ResizeOptions::new().resize_alg(fr::ResizeAlg::Convolution(fr::FilterType::Bilinear));
    let mut r = fr::Resizer::new();
    if r.resize(&img, &mut dst, &opts).is_err() {
        return fallback();
    }
    Frame { data: dst.into_vec(), w, h }
}

fn nearest(src: &Frame, w: u32, h: u32) -> Frame {
    let mut out = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        let sy = (y as u64 * src.h as u64 / h as u64) as u32;
        for x in 0..w {
            let sx = (x as u64 * src.w as u64 / w as u64) as u32;
            let s = ((sy * src.w + sx) * 4) as usize;
            let d = ((y * w + x) * 4) as usize;
            out[d..d + 4].copy_from_slice(&src.data[s..s + 4]);
        }
    }
    Frame { data: out, w, h }
}

/// 論理向きのフレームを物理向きへ回す(rotation × 90° 時計回り)。
pub fn rotate(src: Frame, rotation: u8) -> Frame {
    if rotation == 0 {
        return src;
    }
    let (w, h) = (src.w as usize, src.h as usize);
    let s: Vec<u32> = src
        .data
        .chunks_exact(4)
        .map(|p| u32::from_ne_bytes([p[0], p[1], p[2], p[3]]))
        .collect();
    let (dw, dh) = if rotation == 2 { (w, h) } else { (h, w) };
    let mut d = vec![0u32; dw * dh];
    match rotation {
        1 => {
            for y in 0..dh {
                for x in 0..dw {
                    d[y * dw + x] = s[(h - 1 - x) * w + y];
                }
            }
        }
        2 => {
            for y in 0..dh {
                for x in 0..dw {
                    d[y * dw + x] = s[(h - 1 - y) * w + (w - 1 - x)];
                }
            }
        }
        _ => {
            for y in 0..dh {
                for x in 0..dw {
                    d[y * dw + x] = s[x * w + (w - 1 - y)];
                }
            }
        }
    }
    let data = d.iter().flat_map(|v| v.to_ne_bytes()).collect();
    Frame { data, w: dw as u32, h: dh as u32 }
}

// ---- 文字描画 -------------------------------------------------------------

struct Fonts {
    primary: Option<ab_glyph::FontVec>,
    fallback: Option<ab_glyph::FontVec>,
}

fn load_font(pattern: &str, default: &str) -> Option<ab_glyph::FontVec> {
    let spec = std::process::Command::new("fc-match")
        .args(["-f", "%{file}:%{index}", pattern])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_else(|| format!("{default}:0"));
    let (file, idx) = spec.rsplit_once(':').unwrap_or((spec.as_str(), "0"));
    let bytes = std::fs::read(file).or_else(|_| std::fs::read(default)).ok()?;
    ab_glyph::FontVec::try_from_vec_and_index(bytes, idx.parse().unwrap_or(0)).ok()
}

fn fonts() -> &'static Fonts {
    static F: OnceLock<Fonts> = OnceLock::new();
    F.get_or_init(|| Fonts {
        primary: load_font("monospace", "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf"),
        fallback: load_font("sans:lang=ja", "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc"),
    })
}

/// 1 文字ぶんのフォントとグリフを選ぶ(等幅に無ければ日本語フォントへ)。
fn glyph_font(c: char) -> Option<&'static ab_glyph::FontVec> {
    use ab_glyph::Font;
    let f = fonts();
    match (&f.primary, &f.fallback) {
        (Some(p), _) if p.glyph_id(c).0 != 0 => Some(p),
        (_, Some(fb)) if fb.glyph_id(c).0 != 0 => Some(fb),
        (Some(p), _) => Some(p),
        (None, fb) => fb.as_ref(),
    }
}

fn text_width(text: &str, px: f32) -> f32 {
    use ab_glyph::{Font, ScaleFont};
    text.chars()
        .filter_map(|c| glyph_font(c).map(|f| f.as_scaled(px).h_advance(f.glyph_id(c))))
        .sum()
}

/// max_w に収まるよう末尾を「…」で切る。
fn ellipsize(text: &str, px: f32, max_w: f32) -> String {
    if text_width(text, px) <= max_w {
        return text.to_string();
    }
    let mut out = String::new();
    for c in text.chars() {
        let mut t = out.clone();
        t.push(c);
        t.push('…');
        if text_width(&t, px) > max_w {
            break;
        }
        out.push(c);
    }
    out.push('…');
    out
}

/// BGRA フレームへ文字を描く。(x, baseline) は左端とベースライン。
fn draw_text(f: &mut Frame, text: &str, x: f32, baseline: f32, px: f32, rgb: [u8; 3], clip_x: (u32, u32)) {
    use ab_glyph::{Font, ScaleFont};
    let mut pen = x;
    for c in text.chars() {
        let Some(font) = glyph_font(c) else { continue };
        let scaled = font.as_scaled(px);
        let gid = font.glyph_id(c);
        let glyph = gid.with_scale_and_position(px, ab_glyph::point(pen, baseline));
        pen += scaled.h_advance(gid);
        let Some(outline) = font.outline_glyph(glyph) else { continue };
        let b = outline.px_bounds();
        outline.draw(|gx, gy, cov| {
            let px_x = b.min.x as i32 + gx as i32;
            let px_y = b.min.y as i32 + gy as i32;
            if px_x < clip_x.0 as i32 || px_x >= clip_x.1 as i32 || px_y < 0 || px_y >= f.h as i32 {
                return;
            }
            let i = ((px_y as u32 * f.w + px_x as u32) * 4) as usize;
            let a = cov.clamp(0.0, 1.0);
            for (k, ch) in [rgb[2], rgb[1], rgb[0]].into_iter().enumerate() {
                f.data[i + k] = (f.data[i + k] as f32 * (1.0 - a) + ch as f32 * a) as u8;
            }
        });
    }
}

fn fill_rect(f: &mut Frame, x: u32, y: u32, w: u32, h: u32, rgb: [u8; 3]) {
    let x1 = (x + w).min(f.w);
    let y1 = (y + h).min(f.h);
    for yy in y.min(f.h)..y1 {
        for xx in x.min(f.w)..x1 {
            let i = ((yy * f.w + xx) * 4) as usize;
            f.data[i] = rgb[2];
            f.data[i + 1] = rgb[1];
            f.data[i + 2] = rgb[0];
            f.data[i + 3] = 255;
        }
    }
}

pub const TAB_H: u32 = 28;

/// 画面下端にタブバーを重ねる(タブが 2 枚以上のときだけ呼ぶ)。
pub fn draw_tab_bar(f: &mut Frame, titles: &[String], current: usize) {
    let n = titles.len().max(1) as u32;
    if f.h < TAB_H || f.w == 0 {
        return;
    }
    let top = f.h - TAB_H;
    let tab_w = f.w / n;
    let mut band = Frame { data: f.data[(top * f.w * 4) as usize..].to_vec(), w: f.w, h: TAB_H };
    for (i, title) in titles.iter().enumerate() {
        let i = i as u32;
        let x = i * tab_w;
        let tw = if i == n - 1 { f.w - x } else { tab_w };
        let active = i as usize == current;
        let bg = if active { [0x4a, 0x90, 0xd9] } else { [0x1e, 0x1e, 0x2e] };
        let fg = if active { [0xff, 0xff, 0xff] } else { [0x99, 0x99, 0xbb] };
        fill_rect(&mut band, x, 0, tw, TAB_H, bg);
        fill_rect(&mut band, (x + tw).saturating_sub(1), 0, 1, TAB_H, [0x33, 0x33, 0x55]);
        let label = if title.is_empty() { format!("Tab {}", i + 1) } else { title.clone() };
        let label = ellipsize(&label, 12.0, tw.saturating_sub(12) as f32);
        draw_text(&mut band, &label, (x + 6) as f32, (TAB_H - 8) as f32, 12.0, fg, (x, x + tw));
    }
    f.data[(top * f.w * 4) as usize..].copy_from_slice(&band.data);
}

// ---- マウスカーソル ---------------------------------------------------------

const CURSOR_SIZE: u32 = 48;

/// 48x48 の矢印カーソル(BGRA, 乗算済みでないアルファ)。
fn cursor_image() -> &'static Frame {
    static C: OnceLock<Frame> = OnceLock::new();
    C.get_or_init(|| {
        use tiny_skia::{FillRule, Paint, PathBuilder, Pixmap, Stroke, Transform};
        let mut pm = Pixmap::new(CURSOR_SIZE, CURSOR_SIZE).unwrap();
        // 元の SVG: M5.5 3.21V20.8l5.08-5.09h6.42L5.5 3.21z (viewBox 24)
        let mut pb = PathBuilder::new();
        pb.move_to(5.5, 3.21);
        pb.line_to(5.5, 20.8);
        pb.line_to(10.58, 15.71);
        pb.line_to(17.0, 15.71);
        pb.close();
        let path = pb.finish().unwrap();
        let scale = CURSOR_SIZE as f32 / 24.0;
        let mut paint = Paint { anti_alias: true, ..Default::default() };
        // 影
        paint.set_color_rgba8(0, 0, 0, 110);
        pm.fill_path(&path, &paint, FillRule::Winding, Transform::from_scale(scale, scale).post_translate(2.0, 2.0), None);
        paint.set_color_rgba8(255, 255, 255, 255);
        pm.fill_path(&path, &paint, FillRule::Winding, Transform::from_scale(scale, scale), None);
        paint.set_color_rgba8(0, 0, 0, 255);
        let stroke = Stroke { width: 1.5, ..Default::default() };
        pm.stroke_path(&path, &paint, &stroke, Transform::from_scale(scale, scale), None);
        let mut data = Vec::with_capacity((CURSOR_SIZE * CURSOR_SIZE * 4) as usize);
        for p in pm.pixels() {
            let c = p.demultiply();
            data.extend_from_slice(&[c.blue(), c.green(), c.red(), c.alpha()]);
        }
        Frame { data, w: CURSOR_SIZE, h: CURSOR_SIZE }
    })
}

/// マウス位置 (mx, my) にカーソルを重ねる。
pub fn draw_cursor(f: &mut Frame, mx: i32, my: i32) {
    let c = cursor_image();
    if f.w < c.w || f.h < c.h {
        return;
    }
    let left = (mx - 2).clamp(0, (f.w - c.w) as i32) as u32;
    let top = (my - 2).clamp(0, (f.h - c.h) as i32) as u32;
    for y in 0..c.h {
        for x in 0..c.w {
            let s = ((y * c.w + x) * 4) as usize;
            let a = c.data[s + 3] as u32;
            if a == 0 {
                continue;
            }
            let d = (((top + y) * f.w + left + x) * 4) as usize;
            for k in 0..3 {
                f.data[d + k] = ((c.data[s + k] as u32 * a + f.data[d + k] as u32 * (255 - a)) / 255) as u8;
            }
        }
    }
}

// ---- ステータス行 ----------------------------------------------------------

const STATUS_PX: f32 = 16.0;
const STATUS_H: u32 = 24;

/// 左下に半透明の帯でステータス行を重ねる。bottom は下端から空ける高さ(タブバー分)。
pub fn draw_status(f: &mut Frame, text: &str, bottom: u32) {
    if f.h < STATUS_H + bottom || f.w < 40 {
        return;
    }
    let max_w = (f.w - 24) as f32;
    let label = ellipsize(text, STATUS_PX, max_w);
    let w = ((text_width(&label, STATUS_PX) as u32) + 16).min(f.w);
    let top = f.h - bottom - STATUS_H;
    for y in top..top + STATUS_H {
        for x in 0..w {
            let i = ((y * f.w + x) * 4) as usize;
            for k in 0..3 {
                f.data[i + k] = (f.data[i + k] as u32 * 3 / 10) as u8;
            }
        }
    }
    draw_text(f, &label, 8.0, (top + STATUS_H - 7) as f32, STATUS_PX, [0xff, 0xff, 0xff], (0, w));
}
