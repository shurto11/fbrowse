//! tmux への問い合わせ(ペイン座標・セッション・クライアント)。

use std::process::{Command, Stdio};

/// tmux を実行して標準出力(末尾の改行を除く)を返す。失敗なら None。
pub fn run(args: &[&str]) -> Option<String> {
    let out = Command::new("tmux").args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

pub fn inside() -> bool {
    std::env::var("TMUX").map(|v| !v.is_empty()).unwrap_or(false)
}

fn own_pane() -> Option<String> {
    std::env::var("TMUX_PANE").ok().filter(|s| !s.is_empty())
}

/// 自分のペインについて tmux のフォーマット文字列を展開する。
fn display(fmt: &str, pane: Option<&str>) -> Option<String> {
    if !inside() {
        return None;
    }
    let mut args = vec!["display-message", "-p"];
    let own = own_pane();
    if let Some(p) = pane.or(own.as_deref()) {
        args.extend(["-t", p]);
    }
    args.push(fmt);
    run(&args)
}

/// このペインの tmux セッション名(tmux 外なら "")。
pub fn own_session_name() -> String {
    display("#{session_name}", None).unwrap_or_default()
}

/// このペインのセッション ID(`$0` 形式)。fb-server への申告に使う。
pub fn own_session_id() -> Option<String> {
    display("#{session_id}", None).filter(|s| !s.is_empty())
}

/// 文字列を tmux のペーストバッファへ入れる(prefix+] で貼り付けられる)。
/// `-w` で set-clipboard 経由のシステムクリップボード(OSC 52)にも送る。
pub fn copy_buffer(text: &str) -> bool {
    if !inside() {
        return false;
    }
    run(&["set-buffer", "-w", "--", text]).is_some() || run(&["set-buffer", "--", text]).is_some()
}

/// 接続中の全 tmux クライアントに画面全体の再描画を要求する。
pub fn refresh_clients() {
    if !inside() {
        return;
    }
    if let Some(out) = run(&["list-clients", "-F", "#{client_name}"]) {
        for name in out.lines().filter(|l| !l.is_empty()) {
            let _ = run(&["refresh-client", "-t", name]);
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PaneRegion {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub active: bool,
    pub session: String,
}

/// tmux ペインの画面上の座標を返す(論理向きのピクセル座標)。
/// fb_w/fb_h は物理 fb サイズ。回転時は tmux のセルが論理向きに並ぶため、
/// 論理向きのサイズに直して計算する。`cell` を渡すとセル寸法の推定をやめて
/// その値を使う(drmterm は実寸を教えてくれる。回転とも無関係)。
pub fn pane_region(
    mut fb_w: u32,
    mut fb_h: u32,
    pane: Option<&str>,
    cell: Option<(u32, u32)>,
    rotation: u8,
) -> Option<PaneRegion> {
    if cell.is_none() && rotation % 2 == 1 {
        std::mem::swap(&mut fb_w, &mut fb_h);
    }
    let sock = std::env::var("TMUX").ok()?;
    let sock = sock.split(',').next()?;
    if sock.is_empty() || !std::path::Path::new(sock).exists() {
        return None;
    }
    let fmt = "#{window_active},#{pane_left},#{pane_top},#{pane_width},#{pane_height},#{window_width},#{window_height},#{client_width},#{client_height},#{status-position},#{session_attached},#{session_name}";
    let out = display(fmt, pane)?;
    let parts: Vec<&str> = out.splitn(12, ',').collect();
    let n = |i: usize| parts.get(i).and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
    let (active, pl, pt, pw, ph, ww, wh, cw_cells, ch_cells) = (n(0), n(1), n(2), n(3), n(4), n(5), n(6), n(7), n(8));
    let status_pos = parts.get(9).copied().filter(|s| !s.is_empty()).unwrap_or("bottom");
    let attached = parts.get(10).and_then(|s| s.parse::<u32>().ok()).unwrap_or(1);
    let session = parts.get(11).unwrap_or(&"").to_string();
    if ww == 0 || wh == 0 {
        return None;
    }
    // セルサイズはステータスバーを含む端末全体の行列数から求める
    // (window_height はステータスバー分小さいため、それで割るとセルが過大になる)。
    let cols = if cw_cells > 0 { cw_cells } else { ww };
    let rows_all = if ch_cells > 0 { ch_cells } else { wh };
    // 画面下部を他(task-var のバー等)が占めていると端末行数だけが縮み、
    // 画面高/行数ではセルが過大になる。セル高はセル幅の 2 倍を上限に抑える。
    let (cw, ch) = match cell {
        Some(c) => c,
        None => {
            let cw = fb_w / cols;
            (cw, (fb_h / rows_all).min(cw * 2))
        }
    };
    if cw == 0 || ch == 0 {
        return None;
    }
    // 端末全体の行数。drmterm 配下では実セル高から割り出す
    // (client_height は別クライアントの値になりうるため)。
    let rows = if cell.is_some() { fb_h / ch } else { rows_all };
    // pane_top はウィンドウ内相対なので、上部ステータスバーならその分下へずらす。
    let status_rows = rows.saturating_sub(wh);
    let y_off = if status_pos == "top" { status_rows * ch } else { 0 };
    Some(PaneRegion {
        x: pl * cw,
        y: y_off + pt * ch,
        w: pw * cw,
        h: ph * ch,
        // 自セッションがどのクライアントにも表示されていなければ隠れている扱い
        active: active == 1 && attached > 0,
        session,
    })
}

/// sysfs から /dev/fb0 の物理サイズとストライド(ピクセル)を読む。
pub fn read_fb_info() -> Option<(u32, u32, u32)> {
    let vs = std::fs::read_to_string("/sys/class/graphics/fb0/virtual_size").ok()?;
    let (w, h) = vs.trim().split_once(',')?;
    let stride: u32 = std::fs::read_to_string("/sys/class/graphics/fb0/stride").ok()?.trim().parse().ok()?;
    Some((w.parse().ok()?, h.parse().ok()?, stride / 4))
}
