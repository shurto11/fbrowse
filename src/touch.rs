//! タッチ入力。
//!
//! touch-server (~/ssd/fb/touch/touch-server) に接続し、このペインがアクティブな
//! ときだけ配信される down/move/up を受け取ってジェスチャに変換する。
//! サーバーに繋がらなければタッチデバイス(/dev/input/eventN)を直接読む。
//!
//! 座標は画面全体に対する 0..1 の割合(回転後の論理向き)で通知する。

use serde::Deserialize;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Instant;
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug, Clone)]
pub enum Touch {
    /// 接地(1 タッチ分の状態リセット用)
    Start,
    /// 縦ドラッグ。直前からの移動量(画面高に対する割合、下向きが正)
    Scroll(f64),
    /// 横ドラッグ。直前からの移動量(画面幅に対する割合、右向きが正)
    ScrollH(f64),
    /// 2 本指ピンチ。直前からの拡大率
    Pinch(f64),
    Tap(f64, f64),
    /// 長押し(今は何もしない)
    LongPress,
    SwipeLeft,
    SwipeRight,
}

fn env_f64(name: &str, default: f64) -> f64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn swipe_frac() -> f64 {
    env_f64("TOUCH_SWIPE_FRAC", 0.12)
}
fn scroll_frac() -> f64 {
    env_f64("TOUCH_SCROLL_FRAC", 0.015)
}
fn long_press_sec() -> f64 {
    env_f64("TOUCH_LONGPRESS_SEC", 0.6)
}

/// 画面回転量 (fbterm の screen-rotate 値: 0=なし 1=時計回り90° 2=180° 3=反時計回り90°)。
/// touch-server と同じ優先順位: 環境変数 TOUCH_ROTATE → ~/.fbtermrc の screen-rotate=。
pub fn read_screen_rotate() -> u8 {
    if let Ok(v) = std::env::var("TOUCH_ROTATE") {
        if let Ok(n) = v.trim().parse::<u32>() {
            return (n % 4) as u8;
        }
    }
    let Ok(home) = std::env::var("HOME") else { return 0 };
    let Ok(text) = std::fs::read_to_string(format!("{home}/.fbtermrc")) else { return 0 };
    for line in text.lines() {
        if let Some(rest) = line.trim().strip_prefix("screen-rotate=") {
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(n) = digits.parse::<u32>() {
                return (n % 4) as u8;
            }
        }
    }
    0
}

/// タッチパネルの生座標(frac)を回転後の論理画面座標(frac)へ変換する。
fn rotate_frac(fx: f64, fy: f64, rotate: u8) -> (f64, f64) {
    match rotate {
        1 => (fy, 1.0 - fx),
        2 => (1.0 - fx, 1.0 - fy),
        3 => (1.0 - fy, fx),
        _ => (fx, fy),
    }
}

// ---- touch-server クライアント ----------------------------------------------

#[derive(Deserialize, Clone, Copy)]
struct PaneCtx {
    win_w: f64,
    win_h: f64,
    left: f64,
    top: f64,
    right: f64,
    bottom: f64,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Ev {
    Down,
    Move {
        #[serde(default)]
        dx: f64,
        #[serde(default)]
        dy: f64,
        #[serde(default = "one")]
        n: usize,
        #[serde(default)]
        spread: f64,
    },
    Up {
        fx0: f64,
        fy0: f64,
        fx1: f64,
        fy1: f64,
        #[serde(default)]
        dur: f64,
        #[serde(default)]
        ctx: Option<PaneCtx>,
    },
}

fn one() -> usize {
    1
}

fn server_socket_path() -> String {
    if let Ok(p) = std::env::var("TOUCH_SERVER_SOCK") {
        if !p.is_empty() {
            return p;
        }
    }
    match std::env::var("XDG_RUNTIME_DIR") {
        Ok(d) if !d.is_empty() => format!("{d}/touch-server.sock"),
        _ => "/tmp/touch-server.sock".to_string(),
    }
}

/// グローバル frac をアクティブペイン内の frac に変換する(TOUCH_LOCALIZE=1 のとき)。
fn localize(fx: f64, fy: f64, ctx: Option<PaneCtx>) -> (f64, f64) {
    let Some(c) = ctx else { return (fx, fy) };
    let w = c.right - c.left + 1.0;
    let h = c.bottom - c.top + 1.0;
    let lx = if w > 0.0 { (fx * c.win_w - c.left) / w } else { fx };
    let ly = if h > 0.0 { (fy * c.win_h - c.top) / h } else { fy };
    (lx.clamp(0.0, 1.0), ly.clamp(0.0, 1.0))
}

#[derive(PartialEq, Clone, Copy)]
enum Axis {
    None,
    V,
    H,
    Pinch,
}

/// タッチ入力を開始する。touch-server に繋がらなければデバイス直読みに切り替える。
pub fn start(tx: UnboundedSender<Touch>) {
    std::thread::spawn(move || {
        let path = server_socket_path();
        match UnixStream::connect(&path) {
            Ok(s) => {
                eprintln!("[touch] touch-server に接続 ({path})");
                server_loop(s, &tx);
            }
            Err(e) => {
                eprintln!("[touch] touch-server 未接続 ({e}) → デバイス直読みにフォールバック");
                device_loop(&tx);
            }
        }
    });
}

fn server_loop(mut sock: UnixStream, tx: &UnboundedSender<Touch>) {
    let hello = serde_json::json!({ "hello": crate::fbserver::CLIENT_NAME, "pane": std::env::var("TMUX_PANE").ok() });
    if sock.write_all(format!("{hello}\n").as_bytes()).is_err() {
        return;
    }
    let do_localize = std::env::var("TOUCH_LOCALIZE").as_deref() == Ok("1");
    let (sf, swf, lp) = (scroll_frac(), swipe_frac(), long_press_sec());
    let mut axis = Axis::None;
    let (mut total_dx, mut total_dy) = (0.0f64, 0.0f64);
    let mut last_spread = 0.0f64;
    for line in BufReader::new(sock).lines() {
        let Ok(line) = line else { return };
        let Ok(ev) = serde_json::from_str::<Ev>(&line) else { continue };
        let out = match ev {
            Ev::Down => {
                axis = Axis::None;
                (total_dx, total_dy, last_spread) = (0.0, 0.0, 0.0);
                Some(Touch::Start)
            }
            Ev::Move { dx, dy, n, spread } => {
                // 2 本指はピンチズーム扱い(スクロールとは排他)
                if n >= 2 {
                    let mut out = None;
                    if spread > 0.0 {
                        if last_spread > 0.0 && spread != last_spread {
                            out = Some(Touch::Pinch(spread / last_spread));
                        }
                        last_spread = spread;
                    }
                    axis = Axis::Pinch;
                    out
                } else {
                    total_dx += dx;
                    total_dy += dy;
                    if axis == Axis::None {
                        if total_dy.abs() >= sf && total_dy.abs() >= total_dx.abs() {
                            axis = Axis::V;
                        } else if total_dx.abs() >= sf && total_dx.abs() > total_dy.abs() {
                            axis = Axis::H;
                        }
                    }
                    match axis {
                        Axis::V => Some(Touch::Scroll(dy)),
                        Axis::H => Some(Touch::ScrollH(dx)),
                        _ => None,
                    }
                }
            }
            Ev::Up { fx0, fy0, fx1, fy1, dur, ctx } => {
                let was = axis;
                axis = Axis::None;
                last_spread = 0.0;
                if was == Axis::Pinch || was == Axis::V {
                    None
                } else {
                    let (dx, dy) = (fx1 - fx0, fy1 - fy0);
                    if dx.abs() >= swf && dx.abs() > dy.abs() {
                        Some(if dx < 0.0 { Touch::SwipeLeft } else { Touch::SwipeRight })
                    } else if was == Axis::H {
                        None
                    } else {
                        let (fx, fy) = if do_localize { localize(fx0, fy0, ctx) } else { (fx0, fy0) };
                        Some(if dur >= lp { Touch::LongPress } else { Touch::Tap(fx, fy) })
                    }
                }
            }
        };
        if let Some(t) = out {
            if tx.send(t).is_err() {
                return;
            }
        }
    }
}

// ---- デバイス直読み ---------------------------------------------------------

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_ABS: u16 = 0x03;
const SYN_REPORT: u16 = 0x00;
const BTN_TOUCH: u16 = 0x14a;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const ABS_MT_POSITION_X: u16 = 0x35;
const ABS_MT_POSITION_Y: u16 = 0x36;
const ABS_MT_TRACKING_ID: u16 = 0x39;

/// EVIOCGABS(code) で座標範囲 (min, max) を取る。
fn abs_range(fd: i32, code: u16) -> Option<(i32, i32)> {
    let mut info = [0i32; 6];
    let req = (2u64 << 30) | (24u64 << 16) | ((b'E' as u64) << 8) | (0x40 + code as u64);
    let r = unsafe { libc::ioctl(fd, req as _, info.as_mut_ptr()) };
    (r >= 0 && info[2] > info[1]).then_some((info[1], info[2]))
}

fn device_loop(tx: &UnboundedSender<Touch>) {
    use std::os::fd::AsRawFd;
    let dev = std::env::var("TOUCH_DEV").unwrap_or_else(|_| "/dev/input/event6".to_string());
    let mut file = match std::fs::File::open(&dev) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("[touch] タッチデバイスを開けませんでした ({dev}): {e}");
            return;
        }
    };
    let fd = file.as_raw_fd();
    let rx = abs_range(fd, ABS_MT_POSITION_X).or_else(|| abs_range(fd, ABS_X));
    let ry = abs_range(fd, ABS_MT_POSITION_Y).or_else(|| abs_range(fd, ABS_Y));
    // 範囲が取れなければ接地点から動的に広げる
    let auto_cal = rx.is_none() || ry.is_none();
    let (mut xmin, mut xmax) = rx.map(|(a, b)| (a as f64, b as f64)).unwrap_or((f64::MAX, f64::MIN));
    let (mut ymin, mut ymax) = ry.map(|(a, b)| (a as f64, b as f64)).unwrap_or((f64::MAX, f64::MIN));
    if auto_cal {
        eprintln!("[touch] 座標範囲の自動取得に失敗。タッチしながら自動補正します。");
    }
    let rotate = read_screen_rotate();
    let (sf, swf, lp) = (scroll_frac(), swipe_frac(), long_press_sec());

    let frac = |v: f64, mn: f64, mx: f64| if mx <= mn { 0.0 } else { ((v - mn) / (mx - mn)).clamp(0.0, 1.0) };

    let (mut cur_x, mut cur_y) = (None::<f64>, None::<f64>);
    let (mut start_x, mut start_y) = (0.0f64, 0.0f64);
    let (mut last_x, mut last_y) = (0.0f64, 0.0f64);
    let mut down_time = Instant::now();
    let (mut touching, mut pending_down, mut pending_up) = (false, false, false);
    let mut axis = Axis::None;
    let mut buf = [0u8; 24 * 64];
    let mut leftover: Vec<u8> = Vec::new();

    loop {
        let n = match file.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        leftover.extend_from_slice(&buf[..n]);
        let whole = leftover.len() / 24 * 24;
        let events: Vec<u8> = leftover.drain(..whole).collect();
        for ev in events.chunks_exact(24) {
            let etype = u16::from_le_bytes([ev[16], ev[17]]);
            let code = u16::from_le_bytes([ev[18], ev[19]]);
            let value = i32::from_le_bytes([ev[20], ev[21], ev[22], ev[23]]);
            let mut out: Option<Touch> = None;
            if etype == EV_ABS {
                match code {
                    ABS_MT_POSITION_X | ABS_X => cur_x = Some(value as f64),
                    ABS_MT_POSITION_Y | ABS_Y => cur_y = Some(value as f64),
                    ABS_MT_TRACKING_ID => {
                        if value != -1 && !touching {
                            pending_down = true;
                        } else if value == -1 && touching {
                            pending_up = true;
                        }
                    }
                    _ => {}
                }
            } else if etype == EV_KEY && code == BTN_TOUCH {
                if value == 1 && !touching {
                    pending_down = true;
                } else if value == 0 && touching {
                    pending_up = true;
                }
            } else if etype == EV_SYN && code == SYN_REPORT {
                let (Some(cx), Some(cy)) = (cur_x, cur_y) else { continue };
                if auto_cal {
                    (xmin, xmax) = (xmin.min(cx), xmax.max(cx));
                    (ymin, ymax) = (ymin.min(cy), ymax.max(cy));
                }
                let rf = |x: f64, y: f64| rotate_frac(frac(x, xmin, xmax), frac(y, ymin, ymax), rotate);
                if pending_down {
                    pending_down = false;
                    touching = true;
                    (start_x, start_y, last_x, last_y) = (cx, cy, cx, cy);
                    axis = Axis::None;
                    down_time = Instant::now();
                    out = Some(Touch::Start);
                } else if pending_up {
                    pending_up = false;
                    touching = false;
                    let was = axis;
                    axis = Axis::None;
                    if was != Axis::V {
                        let (fx0, fy0) = rf(start_x, start_y);
                        let (fx1, fy1) = rf(cx, cy);
                        let (dx, dy) = (fx1 - fx0, fy1 - fy0);
                        if dx.abs() >= swf && dx.abs() > dy.abs() {
                            out = Some(if dx < 0.0 { Touch::SwipeLeft } else { Touch::SwipeRight });
                        } else if was != Axis::H {
                            let dur = down_time.elapsed().as_secs_f64();
                            out = Some(if dur >= lp { Touch::LongPress } else { Touch::Tap(fx0, fy0) });
                        }
                    }
                } else if touching {
                    let (cfx, cfy) = rf(cx, cy);
                    let (sfx, sfy) = rf(start_x, start_y);
                    let (fdx, fdy) = (cfx - sfx, cfy - sfy);
                    if axis == Axis::None {
                        if fdy.abs() >= sf && fdy.abs() >= fdx.abs() {
                            axis = Axis::V;
                            (last_x, last_y) = (cx, cy);
                        } else if fdx.abs() >= sf && fdx.abs() > fdy.abs() {
                            axis = Axis::H;
                            (last_x, last_y) = (cx, cy);
                        }
                    }
                    let (lfx, lfy) = rf(last_x, last_y);
                    if axis == Axis::V && cfy != lfy {
                        (last_x, last_y) = (cx, cy);
                        out = Some(Touch::Scroll(cfy - lfy));
                    } else if axis == Axis::H && cfx != lfx {
                        (last_x, last_y) = (cx, cy);
                        out = Some(Touch::ScrollH(cfx - lfx));
                    }
                }
            }
            if let Some(t) = out {
                if tx.send(t).is_err() {
                    return;
                }
            }
        }
    }
}
