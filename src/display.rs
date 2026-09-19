//! フレームバッファ(/dev/fb0 または drmterm の共有メモリ)への書き込み。
//!
//! 描画領域 (x, y, w, h) は「論理向き(回転後)」の座標系で持ち、書き込む直前に
//! 物理フレームバッファ座標へ変換する。fb-server から届いた描画禁止矩形(clip)は
//! 行ごとに避けて書く(上位レイヤーと交互に上書きし合ってチカチカするのを防ぐ)。

use crate::drmterm::Overlay;
use crate::render::{self, Frame};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

pub struct Display {
    pub rotation: u8,
    /// 描画領域(論理向き)
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    /// フレームバッファ全体の行幅(ピクセル、物理向き)
    pub stride: u32,
    /// 物理フレームバッファの全画面サイズ
    phys: Option<(u32, u32)>,
    fb: Option<File>,
    /// fb-server から通知された表示可否
    pub visible: bool,
    /// tmux のウィンドウ/セッションが表示中か
    pub window_active: bool,
    /// mpv の映像をペイン全体へ描いている間は true(ブラウザの絵で上書きしない)
    pub video_overlay: bool,
    clip: Vec<Rect>,
    /// 直前に描いたフレーム(物理向き)。上書き検出時や再表示時に描き直す。
    last: Option<Frame>,
    sentinel: Vec<u8>,
    pub drmterm: Option<Overlay>,
}

impl Display {
    pub fn new(rotation: u8) -> Display {
        let mut d = Display {
            rotation,
            x: 0,
            y: 0,
            w: 1366,
            h: 768,
            stride: 1376,
            phys: None,
            fb: None,
            visible: true,
            window_active: true,
            video_overlay: false,
            clip: Vec::new(),
            last: None,
            sentinel: Vec::new(),
            drmterm: None,
        };
        match crate::tmux::read_fb_info() {
            Some((w, h, stride)) => {
                d.phys = Some((w, h));
                d.stride = stride;
                if rotation % 2 == 1 {
                    (d.w, d.h) = (h, w);
                } else {
                    (d.w, d.h) = (w, h);
                }
            }
            None if rotation != 0 => {
                eprintln!("[rotate] フレームバッファ情報を取得できないため回転を無効化します");
                d.rotation = 0;
            }
            None => {}
        }
        d
    }

    /// drmterm のオーバーレイへ出力する。全画面サイズは drmterm の解像度になる
    /// (外部モニターは fbterm の回転とは無関係)。
    pub fn attach_drmterm(&mut self, ov: Overlay) {
        self.rotation = 0;
        self.phys = Some((ov.info.width, ov.info.height));
        self.set_region(0, 0, ov.info.width, ov.info.height, Some(ov.info.stride));
        self.drmterm = Some(ov);
    }

    pub fn is_drmterm(&self) -> bool {
        self.drmterm.is_some()
    }

    fn fb_path(&self) -> String {
        match &self.drmterm {
            Some(ov) => ov.info.fb.clone(),
            // FBROWSE_FB: 書き込み先を差し替える(動作確認用。サイズは fb0 と同じ前提)
            None => std::env::var("FBROWSE_FB").unwrap_or_else(|_| "/dev/fb0".to_string()),
        }
    }

    pub fn open_fb(&mut self) {
        self.fb = OpenOptions::new().read(true).write(true).open(self.fb_path()).ok();
        if self.fb.is_none() {
            eprintln!("{} を開けません (video グループに入っていますか)", self.fb_path());
        }
    }

    pub fn close(&mut self) {
        if let Some(ov) = self.drmterm.as_mut() {
            ov.set_rect(None);
            ov.stop();
        }
        self.drmterm = None;
        self.fb = None;
    }

    /// drmterm へ「いまこの矩形を合成してほしい」と申告する(非表示なら取り下げ)。
    pub fn sync_drmterm_rect(&mut self) {
        let show = self.window_active && self.visible;
        let r = (self.x, self.y, self.w, self.h);
        if let Some(ov) = self.drmterm.as_mut() {
            ov.set_rect(show.then_some(r));
        }
    }

    pub fn set_region(&mut self, x: u32, y: u32, w: u32, h: u32, stride: Option<u32>) {
        self.x = x;
        self.y = y;
        self.w = w;
        self.h = h;
        if let Some(s) = stride {
            self.stride = s;
        }
    }

    pub fn set_phys(&mut self, w: u32, h: u32, stride: u32) {
        self.phys = Some((w, h));
        self.stride = stride;
    }

    /// 論理向き(回転後)の全画面ピクセルサイズ。
    pub fn logical_full(&self) -> (u32, u32) {
        match self.phys {
            None => (self.x + self.w, self.y + self.h),
            Some((w, h)) if self.rotation % 2 == 1 => (h, w),
            Some((w, h)) => (w, h),
        }
    }

    /// 論理座標の矩形を物理フレームバッファ上の矩形へ変換する。
    pub fn to_phys(&self, lx: u32, ly: u32, lw: u32, lh: u32) -> Rect {
        if self.rotation == 0 {
            return Rect { x: lx, y: ly, w: lw, h: lh };
        }
        let (wl, hl) = self.logical_full();
        match self.rotation {
            1 => Rect { x: hl.saturating_sub(ly + lh), y: lx, w: lh, h: lw },
            2 => Rect { x: wl.saturating_sub(lx + lw), y: hl.saturating_sub(ly + lh), w: lw, h: lh },
            _ => Rect { x: ly, y: wl.saturating_sub(lx + lw), w: lh, h: lw },
        }
    }

    /// 現在の描画領域(物理座標)。fb-server へ申告して重なり調停に参加する。
    pub fn fb_rect(&self) -> Rect {
        self.to_phys(self.x, self.y, self.w, self.h)
    }

    /// タッチの画面全体の割合を、ペイン内ビューポートのピクセル座標へ変換する。
    pub fn global_frac_to_local(&self, fx: f64, fy: f64) -> (i32, i32) {
        let (fw, fh) = self.logical_full();
        let x = (fx * fw as f64 - self.x as f64).round() as i32;
        let y = (fy * fh as f64 - self.y as f64).round() as i32;
        (x.clamp(0, self.w as i32 - 1), y.clamp(0, self.h as i32 - 1))
    }

    /// 物理矩形 pr の絶対行 abs_y について、clip を避けて書くべきローカル列スパン。
    fn unclipped_spans(&self, pr: &Rect, abs_y: u32) -> Vec<(u32, u32)> {
        let mut skips: Vec<(u32, u32)> = Vec::new();
        for c in &self.clip {
            if abs_y < c.y || abs_y >= c.y + c.h {
                continue;
            }
            let x0 = c.x.max(pr.x);
            let x1 = (c.x + c.w).min(pr.x + pr.w);
            if x1 > x0 {
                skips.push((x0 - pr.x, x1 - pr.x));
            }
        }
        if skips.is_empty() {
            return vec![(0, pr.w)];
        }
        skips.sort();
        let mut spans = Vec::new();
        let mut col = 0;
        for (s, e) in skips {
            if col < s {
                spans.push((col, s));
            }
            col = col.max(e);
        }
        if col < pr.w {
            spans.push((col, pr.w));
        }
        spans
    }

    /// 物理矩形 pr へ clip を避けながら行単位で書き込む。
    /// constant_row=true なら data を 1 行ぶんの繰り返しとして扱う(クリア用)。
    fn write_rows(&mut self, data: &[u8], src_row_bytes: usize, pr: Rect, rows: u32, constant_row: bool) {
        let Some(fb) = self.fb.as_ref() else { return };
        for y in 0..rows {
            let abs_y = pr.y + y;
            let src_row = if constant_row { 0 } else { y as usize * src_row_bytes };
            for (s, e) in self.unclipped_spans(&pr, abs_y) {
                let off = ((abs_y as u64 * self.stride as u64) + (pr.x + s) as u64) * 4;
                let a = src_row + s as usize * 4;
                let b = src_row + e as usize * 4;
                if b <= data.len() {
                    let _ = fb.write_at(&data[a..b], off);
                }
            }
        }
        // drmterm は共有メモリを勝手には見に来ないので、書いたら再描画を促す
        if let Some(ov) = self.drmterm.as_mut() {
            ov.frame();
        }
    }

    fn save_sentinel(&mut self, f: &Frame) {
        let n = f.w.min(64) as usize * 4;
        self.sentinel = f.data[..n.min(f.data.len())].to_vec();
    }

    /// ブラウザのフレーム(論理向き、描画領域と同寸)を描く。
    pub fn blit(&mut self, frame: Frame) {
        if !self.visible || self.video_overlay || self.fb.is_none() {
            return;
        }
        let (fw, fh) = (frame.w, frame.h);
        let rot = render::rotate(frame, self.rotation);
        let pr = self.to_phys(self.x, self.y, fw, fh);
        self.write_rows(&rot.data, rot.w as usize * 4, pr, rot.h, false);
        self.save_sentinel(&rot);
        self.last = Some(rot);
    }

    /// 論理座標 (x, y) を左上に、フレームをそのまま描く(mpv の映像用)。
    pub fn blit_at(&mut self, frame: Frame, x: u32, y: u32) {
        if !self.visible || self.fb.is_none() {
            return;
        }
        let (fw, fh) = (frame.w, frame.h);
        let rot = render::rotate(frame, self.rotation);
        let pr = self.to_phys(x, y, fw, fh);
        self.write_rows(&rot.data, rot.w as usize * 4, pr, rot.h, false);
    }

    /// fb-server から届いた描画禁止矩形を反映する。変化したら描き直す
    /// (バーが消えた跡にページを埋め戻す)。
    pub fn set_clip(&mut self, clip: Vec<Rect>) {
        if clip == self.clip {
            return;
        }
        self.clip = clip;
        self.force_redraw();
    }

    pub fn force_redraw(&mut self) {
        if self.video_overlay || self.fb.is_none() {
            return;
        }
        let Some(last) = self.last.take() else { return };
        let pr = self.to_phys(self.x, self.y, self.w, self.h);
        if last.w == pr.w && last.h == pr.h {
            self.write_rows(&last.data, last.w as usize * 4, pr, last.h, false);
            self.save_sentinel(&last);
        }
        self.last = Some(last);
    }

    pub fn clear_region(&mut self, x: u32, y: u32, w: u32, h: u32) {
        if self.fb.is_none() {
            return;
        }
        let pr = self.to_phys(x, y, w, h);
        let row = vec![0u8; pr.w as usize * 4];
        self.write_rows(&row, row.len(), pr, pr.h, true);
    }

    pub fn clear_own(&mut self) {
        self.clear_region(self.x, self.y, self.w, self.h);
    }

    /// fbterm などに上書きされたかを、描画領域の先頭数ピクセルで確かめる。
    pub fn sentinel_changed(&self) -> bool {
        let Some(fb) = self.fb.as_ref() else { return false };
        if self.sentinel.is_empty() || self.drmterm.is_some() {
            return false;
        }
        let pr = self.to_phys(self.x, self.y, self.w, self.h);
        let off = (pr.y as u64 * self.stride as u64 + pr.x as u64) * 4;
        let mut buf = vec![0u8; self.sentinel.len()];
        match fb.read_at(&mut buf, off) {
            Ok(n) if n == buf.len() => buf != self.sentinel,
            _ => false,
        }
    }

    /// fb-server からの可視性通知を反映する。非表示化した瞬間は自分の領域を
    /// クリアし、再表示時は直前のフレームを描き直す。戻り値は「tmux に再描画を
    /// 頼むべきか」(セッション切替で隠れたときだけ。全画面アプリの描画は壊さない)。
    pub fn set_visible(&mut self, visible: bool, reason: Option<&str>) -> bool {
        if visible == self.visible {
            return false;
        }
        self.visible = visible;
        self.sync_drmterm_rect();
        if visible {
            self.force_redraw();
            false
        } else {
            self.clear_own();
            reason == Some("session")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disp(rotation: u8) -> Display {
        let mut d = Display::new(0);
        d.rotation = rotation;
        d.set_phys(1366, 768, 1376);
        d
    }

    #[test]
    fn rotates_rects_to_physical() {
        let d = disp(1); // 論理 768x1366。論理の上端は物理の右端になる
        assert_eq!(d.to_phys(0, 0, 768, 100), Rect { x: 1266, y: 0, w: 100, h: 768 });
        let d = disp(3);
        assert_eq!(d.to_phys(0, 0, 768, 100), Rect { x: 0, y: 0, w: 100, h: 768 });
        let d = disp(2);
        assert_eq!(d.to_phys(0, 0, 100, 50), Rect { x: 1266, y: 718, w: 100, h: 50 });
    }

    #[test]
    fn skips_clipped_columns() {
        let mut d = disp(0);
        d.clip = vec![Rect { x: 10, y: 0, w: 5, h: 10 }, Rect { x: 12, y: 0, w: 10, h: 10 }];
        let pr = Rect { x: 0, y: 0, w: 30, h: 20 };
        assert_eq!(d.unclipped_spans(&pr, 5), vec![(0, 10), (22, 30)]);
        assert_eq!(d.unclipped_spans(&pr, 15), vec![(0, 30)]);
    }

    #[test]
    fn rotate_roundtrip_dims() {
        let f = Frame { data: (0..(3 * 2 * 4)).map(|i| i as u8).collect(), w: 3, h: 2 };
        let r = render::rotate(f.clone(), 1);
        assert_eq!((r.w, r.h), (2, 3));
        let back = render::rotate(render::rotate(r, 1), 2);
        assert_eq!(back.data, f.data);
    }
}
