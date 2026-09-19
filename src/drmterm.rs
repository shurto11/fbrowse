//! drmterm(外部モニターの DRM ターミナル)のオーバーレイクライアント。
//!
//! drmterm は PTY の子へ `DRMTERM_OVERLAY_SOCK` を渡すので、その中で起動されたか
//! どうかは環境変数で分かる。接続すると画面全体と同じ大きさの共有メモリのパスと
//! サイズが返るので、/dev/fb0 の代わりにそこへ書く。書き込み範囲(= tmux ペイン)
//! を RECT で申告すると、drmterm が再描画のたびにその矩形だけを画面へ重ねる。
//!
//! ただし tmux サーバが drmterm より先に起動していると環境変数は届かない
//! (`tmux new -A` は既存サーバへアタッチするだけ)。そのため環境変数が無ければ
//! 既定のソケットを探し、drmterm が名乗る tty が本当にこのペインのセッションを
//! 表示しているかを tmux に問い合わせて確かめる。
//!
//! プロトコル(行指向・空白区切り)
//!   C→S  HELLO <名前>
//!   S→C  FB <パス> <幅> <高さ> <ストライドpx> <セル幅> <セル高> [tty]
//!   C→S  RECT <x> <y> <w> <h> | RECT none
//!   C→S  FRAME                (共有メモリを更新したので再描画してほしい)

use crate::tmux;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct Info {
    pub fb: String,
    pub width: u32,
    pub height: u32,
    pub stride: u32, // ピクセル単位
    pub cell_w: u32,
    pub cell_h: u32,
    pub tty: String, // drmterm の PTY(tmux クライアントの端末)。不明なら "-"
}

pub struct Overlay {
    sock: UnixStream,
    pub info: Info,
    declared: String,
    closed: bool,
}

impl Overlay {
    fn send(&mut self, line: &str) {
        if self.closed {
            return;
        }
        if self.sock.write_all(format!("{line}\n").as_bytes()).is_err() {
            self.closed = true;
        }
    }

    /// 合成してほしい矩形(全画面の物理ピクセル座標)。None で取り下げ。
    pub fn set_rect(&mut self, r: Option<(u32, u32, u32, u32)>) {
        let line = match r {
            Some((x, y, w, h)) => format!("RECT {x} {y} {w} {h}"),
            None => "RECT none".to_string(),
        };
        if line == self.declared {
            return;
        }
        self.declared = line.clone();
        self.send(&line);
    }

    /// 共有メモリを書き換えたことを知らせて再描画させる。
    pub fn frame(&mut self) {
        self.send("FRAME");
    }

    pub fn stop(&mut self) {
        self.closed = true;
        let _ = self.sock.shutdown(std::net::Shutdown::Both);
    }
}

/// drmterm のクライアント(tty)が、いま session を表示しているか。
pub fn shows_session(tty: &str, session: &str) -> bool {
    if tty.is_empty() || tty == "-" || session.is_empty() {
        return false;
    }
    let Some(out) = tmux::run(&["list-clients", "-F", "#{client_tty}\t#{client_session}"]) else {
        return false;
    };
    out.lines().any(|l| l.split_once('\t') == Some((tty, session)))
}

/// 探す順: 環境変数 → $XDG_RUNTIME_DIR → /tmp。存在するものだけ返す。
fn socket_candidates() -> Vec<(String, bool)> {
    let mut list: Vec<(String, bool)> = Vec::new();
    if let Ok(p) = std::env::var("DRMTERM_OVERLAY_SOCK") {
        if !p.is_empty() {
            list.push((p, true));
        }
    }
    if let Ok(d) = std::env::var("XDG_RUNTIME_DIR") {
        if !d.is_empty() {
            list.push((format!("{d}/drmterm-overlay.sock"), false));
        }
    }
    list.push((format!("/tmp/drmterm-overlay-{}.sock", unsafe { libc::getuid() }), false));
    let mut seen = Vec::new();
    list.retain(|(p, _)| {
        let keep = !seen.contains(p) && Path::new(p).exists();
        seen.push(p.clone());
        keep
    });
    list
}

/// drmterm の中で動いていれば接続して返す。そうでなければ None。
pub fn connect() -> Option<Overlay> {
    for (path, from_env) in socket_candidates() {
        let Some(mut ov) = try_connect(&path) else { continue };
        // 環境変数で渡された = drmterm の PTY の子。自分で探した場合は、
        // drmterm が本当にこのペインを映しているか確かめてから使う。
        if from_env || shows_session(&ov.info.tty, &tmux::own_session_name()) {
            return Some(ov);
        }
        ov.stop();
    }
    None
}

fn try_connect(path: &str) -> Option<Overlay> {
    let mut sock = UnixStream::connect(path).ok()?;
    sock.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    sock.write_all(b"HELLO ssbrowse\n").ok()?;
    let mut line = String::new();
    BufReader::new(sock.try_clone().ok()?).read_line(&mut line).ok()?;
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.first() != Some(&"FB") || parts.len() < 7 {
        return None;
    }
    let num = |i: usize| parts.get(i).and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
    let info = Info {
        fb: parts[1].to_string(),
        width: num(2),
        height: num(3),
        stride: num(4),
        cell_w: num(5),
        cell_h: num(6),
        tty: parts.get(7).unwrap_or(&"-").to_string(),
    };
    if info.width == 0 || info.height == 0 || info.cell_w == 0 || info.cell_h == 0 {
        return None;
    }
    sock.set_read_timeout(None).ok()?;
    sock.set_write_timeout(Some(Duration::from_millis(200))).ok()?;
    Some(Overlay { sock, info, declared: String::new(), closed: false })
}
