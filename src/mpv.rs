//! YouTube 動画の mpv 再生。
//!
//! 再生のしかたは 3 通り:
//!   - PiP: tmux の右ペインで mpv を動かし、映像を描画領域の右下 1/4 に描く
//!   - ペイン全体: DRM マスターを他(drmterm など)が握っているとき。映像を
//!     --vo=image で受け取り、描画領域いっぱいに描く
//!   - 全画面: --vo=drm で画面へ直接出す
//!
//! キー操作は IPC ソケット経由で送る。コメント表示と弾幕は mpv-comments.lua が描く。

use crate::app::AppEvent;
use crate::browser::{log, Controller};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::Command;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

pub const SOCK: &str = "/tmp/fbrowse-mpv.sock";
const COOKIES: &str = "/tmp/fbrowse-cookies.txt";
const COMMENTS: &str = "/tmp/fbrowse-comments.json";
const DANMAKU: &str = "/tmp/fbrowse-danmaku.json";

const COMMENTS_LUA: &str = include_str!("../assets/mpv-comments.lua");
const PIP_INPUT_CONF: &str = include_str!("../assets/mpv-pip-input.conf");

/// 同梱の Lua スクリプト等を置く場所($XDG_RUNTIME_DIR/fbrowse)。
fn asset_dir() -> PathBuf {
    let base = std::env::var("XDG_RUNTIME_DIR").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/tmp".into());
    PathBuf::from(base).join("fbrowse")
}

fn write_assets() -> (PathBuf, PathBuf) {
    let dir = asset_dir();
    let _ = std::fs::create_dir_all(&dir);
    let lua = dir.join("mpv-comments.lua");
    let conf = dir.join("mpv-pip-input.conf");
    let _ = std::fs::write(&lua, COMMENTS_LUA);
    let _ = std::fs::write(&conf, PIP_INPUT_CONF);
    (lua, conf)
}

fn screenshot_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let dir = PathBuf::from(home).join("Pictures/fbrowse");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

// ---- IPC -------------------------------------------------------------------

/// mpv へコマンドを 1 つ送る(応答は読まない)。
pub async fn send(cmd: Value) {
    if let Ok(Ok(mut s)) = tokio::time::timeout(Duration::from_millis(500), UnixStream::connect(SOCK)).await {
        let _ = s.write_all(format!("{cmd}\n").as_bytes()).await;
        let _ = s.shutdown().await;
    }
}

/// プロパティを読む。応答が無ければ None。
pub async fn get(prop: &str) -> Option<Value> {
    let fut = async {
        let mut s = UnixStream::connect(SOCK).await.ok()?;
        let req = json!({ "command": ["get_property", prop], "request_id": 77 });
        s.write_all(format!("{req}\n").as_bytes()).await.ok()?;
        let mut lines = BufReader::new(s).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
            if v["request_id"] == 77 {
                return Some(v["data"].clone());
            }
        }
        None
    };
    tokio::time::timeout(Duration::from_millis(500), fut).await.ok().flatten()
}

async fn sock_alive() -> bool {
    tokio::time::timeout(Duration::from_millis(500), UnixStream::connect(SOCK)).await.is_ok_and(|r| r.is_ok())
}

// ---- 再生 ------------------------------------------------------------------

/// YouTube の watch/shorts/youtu.be URL なら mpv に渡す watch URL を返す。
pub fn youtube_url(url: &str) -> Option<(String, bool)> {
    if let Some(i) = url.find("youtube.com/shorts/") {
        let id: String = url[i + 19..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').collect();
        return (!id.is_empty()).then(|| (format!("https://www.youtube.com/watch?v={id}"), true));
    }
    (url.contains("youtube.com/watch?v=") || url.contains("youtu.be/")).then(|| (url.to_string(), false))
}

/// DRM マスターを握っているプロセス(drmterm など)の名前。いなければ None。
/// マスターが取られていると mpv の --vo=drm は起動できない。
fn drm_master_holder() -> Option<String> {
    let me = std::process::id().to_string();
    for e in std::fs::read_dir("/proc").ok()?.flatten() {
        let pid = e.file_name().to_string_lossy().to_string();
        if !pid.bytes().all(|b| b.is_ascii_digit()) || pid == me {
            continue;
        }
        let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else { continue };
        for fd in fds.flatten() {
            let Ok(link) = std::fs::read_link(fd.path()) else { continue };
            let l = link.to_string_lossy();
            // renderD* は描画専用でマスターとは無関係なので card* だけを見る
            if l.strip_prefix("/dev/dri/card").is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())) {
                let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
                let comm = comm.trim();
                return Some(if comm.is_empty() { format!("pid {pid}") } else { comm.to_string() });
            }
        }
    }
    None
}

fn base_args(lua: &Path) -> Vec<String> {
    [
        format!("--input-ipc-server={SOCK}"),
        "--hwdec=no".into(),
        "--ytdl-format=bestvideo[height<=720][vcodec^=avc]+bestaudio/bestvideo[height<=720]+bestaudio/best[height<=720]/best".into(),
        format!("--ytdl-raw-options-append=cookies={COOKIES}"),
        "--ytdl-raw-options-append=js-runtimes=node".into(),
        "--ytdl-raw-options-append=remote-components=ejs:github".into(),
        "--sid=no".into(),
        "--slang=ja,en".into(),
        "--sub-back-color=0.0/0.0/0.0/0.75".into(),
        "--sub-color=1.0/1.0/1.0/1.0".into(),
        "--sub-border-size=0".into(),
        "--sub-shadow-offset=0".into(),
        "--sub-font-size=40".into(),
        format!("--screenshot-directory={}", screenshot_dir().display()),
        "--screenshot-format=png".into(),
        format!("--script={}", lua.display()),
    ]
    .into()
}

/// 再生の種類。PiP は mpv を tmux ペインで動かすので、fbrowse のキー操作はそのまま。
#[derive(PartialEq, Clone, Copy)]
pub enum Kind {
    Pip,
    Pane,
    Drm,
}

/// 再生を始める。戻り値は (種類, mpv の pid)。PiP は tmux 側の mpv なので pid 無し。
pub async fn play(ctrl: &Arc<Controller>, url: &str, events: UnboundedSender<AppEvent>) -> (Kind, Option<u32>) {
    ctrl.mute_all().await;
    if let Err(e) = ctrl.export_cookies(COOKIES).await {
        log(&format!("クッキーを書き出せません: {e}"));
    }
    fetch_comments(url.to_string());
    let (lua, pip_conf) = write_assets();
    let _ = std::fs::remove_file(SOCK);
    let mut args = base_args(&lua);

    if ctrl.is_pip() {
        // PiP: tmux 右ペインで mpv(キー入力は mpv が直接受ける)、映像は右下 1/4 へ
        ctrl.cleanup_pip_dir();
        args.extend([
            "--vo=image".into(),
            "--vo-image-format=jpg".into(),
            format!("--vo-image-outdir={}", ctrl.pip_dir.display()),
            "--vf=fps=10".into(),
            "--term-osd=no".into(),
            format!("--input-conf={}", pip_conf.display()),
            url.to_string(),
        ]);
        let shell = std::iter::once("mpv".to_string())
            .chain(args)
            .map(|a| format!("'{}'", a.replace('\'', "'\\''")))
            .collect::<Vec<_>>()
            .join(" ");
        let ok = Command::new("tmux")
            .args(["split-window", "-d", "-h", "-l", "50%", &shell])
            .status()
            .await
            .is_ok_and(|s| s.success());
        if !ok {
            log("tmux split に失敗しました");
        }
        let c = ctrl.clone();
        tokio::spawn(async move {
            let drawer = {
                let c = c.clone();
                tokio::spawn(async move {
                    let mut iv = tokio::time::interval(Duration::from_millis(100));
                    loop {
                        iv.tick().await;
                        let c = c.clone();
                        let _ = tokio::task::spawn_blocking(move || c.write_pip_frame()).await;
                    }
                })
            };
            // IPC ソケットに 3 回続けて繋がらなければ終了とみなす
            let mut fails = 0;
            while fails < 3 {
                tokio::time::sleep(Duration::from_secs(2)).await;
                fails = if sock_alive().await { 0 } else { fails + 1 };
            }
            drawer.abort();
            c.unmute_all().await;
            c.cleanup_pip_dir();
            c.screenshot().await;
            log("mpv PiP 終了");
            let _ = events.send(AppEvent::PipEnded);
        });
        return (Kind::Pip, None);
    }

    // DRM マスターを他プロセスが握っていると --vo=drm は起動できない
    let holder = if ctrl.disp.lock().unwrap().is_drmterm() { Some("drmterm".to_string()) } else { drm_master_holder() };
    args.insert(0, "--no-input-terminal".into());
    args.push("--msg-level=osd/libass=error".into());

    if let Some(holder) = holder {
        log(&format!("{holder} が DRM を使用中のため、ペイン全体に描画して再生します"));
        ctrl.start_pane_video();
        // mpv 側で描画領域のサイズまで縮めてから書き出させる(変換を軽くする)
        let (w, h) = {
            let d = ctrl.disp.lock().unwrap();
            ((d.w & !1).max(2), (d.h & !1).max(2))
        };
        args.extend([
            "--vo=image".into(),
            "--vo-image-format=jpg".into(),
            format!("--vo-image-outdir={}", ctrl.pip_dir.display()),
            format!("--vf=fps=15,scale={w}:{h}:force_original_aspect_ratio=decrease"),
            "--term-osd=no".into(),
            url.to_string(),
        ]);
        // 端末はフレームで覆われるので出力は捨て、失敗時だけ末尾を見せる
        let child = Command::new("mpv").args(&args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                log(&format!("mpv を起動できません: {e}"));
                ctrl.stop_pane_video();
                ctrl.unmute_all().await;
                return (Kind::Pane, None);
            }
        };
        let pid = child.id();
        let c = ctrl.clone();
        tokio::spawn(async move {
            let drawer = {
                let c = c.clone();
                tokio::spawn(async move {
                    let mut iv = tokio::time::interval(Duration::from_millis(100));
                    loop {
                        iv.tick().await;
                        let c = c.clone();
                        let _ = tokio::task::spawn_blocking(move || c.write_pane_video_frame()).await;
                    }
                })
            };
            let mut err = String::new();
            if let Some(mut e) = child.stderr.take() {
                let _ = e.read_to_string(&mut err).await;
            }
            let status = child.wait().await;
            drawer.abort();
            c.stop_pane_video();
            let code = status.ok().and_then(|s| s.code());
            log(&format!("mpv 終了 (code: {})", code.map(|c| c.to_string()).unwrap_or("-".into())));
            if code != Some(0) {
                // 進行表示や IPC の切断メッセージは除いて、エラーらしい行だけを見せる
                let tail: Vec<&str> = err
                    .lines()
                    .map(|l| l.rsplit('\r').next().unwrap_or(l).trim())
                    .filter(|l| !l.is_empty() && !l.starts_with("V:") && !l.starts_with("A:") && !l.starts_with("AV:") && !l.starts_with("[ipc"))
                    .filter(|l| !l.starts_with("Exiting") && !l.contains("Quit"))
                    .collect();
                for l in &tail[tail.len().saturating_sub(5)..] {
                    log(l);
                }
            }
            let _ = events.send(AppEvent::MpvExited);
        });
        return (Kind::Pane, pid);
    }

    // 全画面: DRM へ直接出す
    args.insert(0, "--vo=drm".into());
    args.push("--fs".into());
    args.push(url.to_string());
    let mut child = match Command::new("mpv").args(&args).stdin(Stdio::null()).spawn() {
        Ok(c) => c,
        Err(e) => {
            log(&format!("mpv を起動できません: {e}"));
            ctrl.unmute_all().await;
            return (Kind::Drm, None);
        }
    };
    let pid = child.id();
    let c = ctrl.clone();
    tokio::spawn(async move {
        let status = child.wait().await;
        let code = status.ok().and_then(|s| s.code());
        log(&format!("mpv 終了 (code: {})", code.map(|c| c.to_string()).unwrap_or("-".into())));
        // DRM → fbcon 復帰: VT を切り替えて fbcon に scanout を戻す
        if let Ok(vt) = std::fs::read_to_string("/sys/class/tty/tty0/active") {
            let vt = vt.trim().trim_start_matches("tty").to_string();
            let _ = Command::new("sh").args(["-c", &format!("sudo chvt 63 && sudo chvt {vt}")]).status().await;
        }
        c.disp.lock().unwrap().open_fb();
        tokio::time::sleep(Duration::from_millis(200)).await;
        let _ = events.send(AppEvent::MpvExited);
    });
    (Kind::Drm, pid)
}

/// mpv の再生開始を待ってからブラウザも同じ動画を開き(視聴履歴のため)、
/// 音は消したまま広告を飛ばして止める。
pub async fn open_in_browser_muted(ctrl: Arc<Controller>, url: String) {
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if get("playback-time").await.is_some_and(|v| !v.is_null()) {
            break;
        }
    }
    log("mpv 再生確認、YouTube 側処理開始");
    let mute = "document.querySelectorAll('video, audio').forEach(v => v.muted = true); \
                new MutationObserver(() => document.querySelectorAll('video, audio').forEach(v => v.muted = true)) \
                  .observe(document.documentElement, { childList: true, subtree: true });";
    let _ = ctrl.evaluate(mute).await;
    if let Err(e) = ctrl.goto(&url).await {
        log(&format!("{e}"));
    }
    let _ = ctrl.evaluate(mute).await;
    ctrl.mute_all().await;
    skip_ads_and_pause(ctrl).await;
}

/// YouTube の広告を飛ばし、本編を少し再生させてから止める(履歴登録のため)。
pub async fn skip_ads_and_pause(ctrl: Arc<Controller>) {
    let js = "(() => { document.querySelectorAll('video').forEach(v => v.muted = true); \
              const ad = !!document.querySelector('.ad-showing, .ad-interrupting'); \
              const skip = document.querySelector('.ytp-skip-ad-button, .ytp-ad-skip-button, .ytp-ad-skip-button-modern, button[class*=\"skip\"]'); \
              if (skip) { skip.click(); return 'skipped'; } return ad ? 'ad' : 'main'; })()";
    let pause = "document.querySelectorAll('video').forEach(v => v.pause())";
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        match ctrl.evaluate(js).await.ok().and_then(|v| v.as_str().map(str::to_string)).as_deref() {
            Some("skipped") => {
                log("広告スキップ");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Some("main") => {
                tokio::time::sleep(Duration::from_secs(5)).await;
                let _ = ctrl.evaluate(pause).await;
                return;
            }
            _ => {}
        }
    }
    let _ = ctrl.evaluate(pause).await;
}

/// yt-dlp でコメントを取って mpv-comments.lua 用のファイルへ書く(バックグラウンド)。
fn fetch_comments(url: String) {
    let _ = std::fs::write(COMMENTS, "[]");
    tokio::spawn(async move {
        let out = Command::new("yt-dlp")
            .args([
                "--skip-download",
                "--write-comments",
                "--dump-json",
                "--cookies",
                COOKIES,
                "--js-runtimes",
                "node",
                "--remote-components",
                "ejs:github",
                "--extractor-args",
                "youtube:max_comments=30;comment_sort=top",
                &url,
            ])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .await;
        let Ok(out) = out else { return };
        let Ok(v) = serde_json::from_slice::<Value>(&out.stdout) else {
            log("コメント取得: 0件");
            return;
        };
        let comments: Vec<Value> = v["comments"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|c| {
                json!({
                    "author": c["author"].as_str().unwrap_or_default(),
                    "text": c["text"].as_str().unwrap_or_default(),
                    "likes": c["like_count"].as_u64().unwrap_or(0).to_string(),
                })
            })
            .collect();
        let _ = std::fs::write(COMMENTS, serde_json::to_string_pretty(&comments).unwrap_or_default());
        log(&format!("コメント取得: {}件", comments.len()));
    });
}

/// ライブチャットを 2 秒ごとに読み、新着を弾幕ファイルへ追記する。
pub fn start_danmaku(ctrl: Arc<Controller>) -> JoinHandle<()> {
    let _ = std::fs::write(DANMAKU, "[]");
    tokio::spawn(async move {
        let js = "(() => { const f = document.querySelector('#chatframe'); \
                  if (!f || !f.contentDocument) return []; \
                  return Array.from(f.contentDocument.querySelectorAll('yt-live-chat-text-message-renderer, yt-live-chat-paid-message-renderer')).map(el => { \
                    const text = (el.querySelector('#message')?.textContent || '').trim(); \
                    const author = (el.querySelector('#author-name')?.textContent || '').trim(); \
                    return { id: el.id || (author + ':' + text), text }; }).filter(m => m.text); })()";
        let mut seen: HashSet<String> = HashSet::new();
        let mut all: Vec<Value> = Vec::new();
        let mut first = true;
        let mut iv = tokio::time::interval(Duration::from_secs(2));
        loop {
            iv.tick().await;
            let Ok(Value::Array(msgs)) = ctrl.evaluate(js).await else { continue };
            if msgs.is_empty() {
                continue;
            }
            // 初回は既存メッセージを既読にするだけ
            if first {
                first = false;
                for m in &msgs {
                    seen.insert(m["id"].as_str().unwrap_or_default().to_string());
                }
                log(&format!("弾幕: 既存{}件をスキップ", msgs.len()));
                continue;
            }
            let mut new = 0;
            for m in msgs {
                let id = m["id"].as_str().unwrap_or_default().to_string();
                if seen.insert(id) {
                    all.push(json!({ "text": m["text"] }));
                    new += 1;
                }
            }
            if new > 0 {
                let _ = std::fs::write(DANMAKU, serde_json::to_string(&all).unwrap_or_default());
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::youtube_url;

    #[test]
    fn detects_youtube_urls() {
        assert_eq!(
            youtube_url("https://www.youtube.com/shorts/abc_D-1?feature=x"),
            Some(("https://www.youtube.com/watch?v=abc_D-1".to_string(), true))
        );
        assert_eq!(
            youtube_url("https://www.youtube.com/watch?v=xyz&t=3"),
            Some(("https://www.youtube.com/watch?v=xyz&t=3".to_string(), false))
        );
        assert_eq!(youtube_url("https://youtu.be/xyz").map(|u| u.1), Some(false));
        assert_eq!(youtube_url("https://www.youtube.com/"), None);
    }
}
