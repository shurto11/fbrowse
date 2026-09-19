//! Chromium と、その表示先になる Xvfb の起動。
//!
//! Chromium は headful で動かす(音声・クリップボード・フォーカスが素直に動くため)。
//! 表示先の X サーバは自前で Xvfb を立て、終了時に一緒に片付ける。
//! DevTools のポートはプロファイル直下の `DevToolsActivePort` から読む。

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::process::{Child, Command};

pub struct Chrome {
    pub ws_url: String,
    chrome: Child,
    xvfb: Option<Child>,
}

impl Chrome {
    /// Chromium を強制終了する(Browser.close が効かなかったときの後始末)。
    pub async fn kill(&mut self) {
        let _ = self.chrome.kill().await;
        if let Some(x) = self.xvfb.as_mut() {
            let _ = x.kill().await;
        }
    }
}

/// 起動に使う Chromium の実行ファイルを探す。
/// `$FBROWSE_CHROME` → Playwright が入れた chromium(最新) → PATH 上の chromium/chrome。
pub fn find_chrome() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("FBROWSE_CHROME") {
        if !p.is_empty() {
            return Ok(PathBuf::from(p));
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        let base = Path::new(&home).join(".cache/ms-playwright");
        let mut found: Vec<(u32, PathBuf)> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&base) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                let Some(rev) = name.strip_prefix("chromium-") else { continue };
                let Ok(rev) = rev.parse::<u32>() else { continue };
                for sub in ["chrome-linux64/chrome", "chrome-linux/chrome"] {
                    let p = e.path().join(sub);
                    if p.exists() {
                        found.push((rev, p));
                    }
                }
            }
        }
        found.sort();
        if let Some((_, p)) = found.pop() {
            return Ok(p);
        }
    }
    for name in ["chromium", "chromium-browser", "google-chrome", "google-chrome-stable"] {
        if let Some(p) = which(name) {
            return Ok(p);
        }
    }
    bail!("Chromium が見つかりません (FBROWSE_CHROME で指定するか `npx playwright install chromium` を実行してください)")
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(name)).find(|p| p.is_file())
}

/// 空いているディスプレイ番号で Xvfb を起動する。
async fn start_xvfb(w: u32, h: u32) -> Result<(Child, String)> {
    for n in 99..160u32 {
        if Path::new(&format!("/tmp/.X{n}-lock")).exists() {
            continue;
        }
        let mut child = Command::new("Xvfb")
            .arg(format!(":{n}"))
            .args(["-screen", "0", &format!("{w}x{h}x24"), "-nolisten", "tcp"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("Xvfb を起動できません (xvfb パッケージが必要です)")?;
        let sock = format!("/tmp/.X11-unix/X{n}");
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            if Path::new(&sock).exists() {
                return Ok((child, format!(":{n}")));
            }
            if child.try_wait()?.is_some() {
                break; // この番号は使えなかった → 次へ
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let _ = child.kill().await;
    }
    bail!("Xvfb の起動に失敗しました")
}

/// Chromium を起動して DevTools の WebSocket URL が分かるまで待つ。
pub async fn launch(profile: &Path, w: u32, h: u32) -> Result<Chrome> {
    let exe = find_chrome()?;
    std::fs::create_dir_all(profile)
        .with_context(|| format!("プロファイルを作れません: {}", profile.display()))?;
    let port_file = profile.join("DevToolsActivePort");
    let _ = std::fs::remove_file(&port_file);

    let (xvfb, display) = start_xvfb(w.max(640), h.max(480)).await?;

    let mut cmd = Command::new(&exe);
    cmd.env("DISPLAY", &display)
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg("--remote-debugging-port=0")
        .arg(format!("--window-size={w},{h}"))
        .arg("--window-position=0,0")
        .args([
            "--no-first-run",
            "--no-default-browser-check",
            "--no-sandbox",
            "--password-store=basic",
            "--use-mock-keychain",
            "--autoplay-policy=no-user-gesture-required",
            "--disable-features=PreloadMediaEngagementData,MediaEngagementBypassAutoplayPolicies",
            "--disable-blink-features=AutomationControlled",
            "--disable-infobars",
            "--disable-background-timer-throttling",
            "--disable-backgrounding-occluded-windows",
            "--disable-renderer-backgrounding",
            "--disable-hang-monitor",
            "--disable-ipc-flooding-protection",
            "--disable-popup-blocking",
            "--disable-search-engine-choice-screen",
            "--force-color-profile=srgb",
            "about:blank",
        ])
        // FBROWSE_CHROME_FLAGS: 追加の起動フラグ(空白区切り)
        .args(std::env::var("FBROWSE_CHROME_FLAGS").unwrap_or_default().split_whitespace())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut chrome = cmd
        .spawn()
        .with_context(|| format!("Chromium を起動できません: {}", exe.display()))?;

    let start = Instant::now();
    loop {
        if let Ok(text) = std::fs::read_to_string(&port_file) {
            let mut lines = text.lines();
            if let (Some(port), Some(path)) = (lines.next(), lines.next()) {
                let ws_url = format!("ws://127.0.0.1:{}{}", port.trim(), path.trim());
                return Ok(Chrome { ws_url, chrome, xvfb: Some(xvfb) });
            }
        }
        if let Some(st) = chrome.try_wait()? {
            bail!(
                "Chromium がすぐに終了しました ({st})。同じプロファイルを別の fbrowse が使っていませんか: {}",
                profile.display()
            );
        }
        if start.elapsed() > Duration::from_secs(30) {
            bail!("Chromium の起動待ちがタイムアウトしました");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
