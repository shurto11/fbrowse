//! キー操作とタッチ操作。ブラウザ起動後はずっとここ(ノーマルモード)で動く。

use crate::browser::{Controller, Hint, DEFAULT_URL};
use crate::favorites::{self, Favorites};
use crate::mpv;
use crate::tmux;
use crate::touch::Touch;
use anyhow::Result;
use serde_json::json;
use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;

pub enum AppEvent {
    Key(Vec<u8>),
    Touch(Touch),
    /// fbrowse が直接動かしていた mpv(ペイン全体・全画面)が終わった
    MpvExited,
    /// tmux ペインの mpv(PiP)が終わった
    PipEnded,
}

#[derive(PartialEq)]
enum Mode {
    Normal,
    Insert,
    Hint,
    TabInput,
    Bookmark,
}

/// カーソルモードの移動量(px)
const STEP: i32 = 20;

pub struct App {
    ctrl: Arc<Controller>,
    favs: Favorites,
    tx: UnboundedSender<AppEvent>,
    mode: Mode,
    input: String,
    cursor_mode: bool,
    video_mode: bool,
    last_typed: usize,
    last_g: bool,
    hints: HashMap<String, Hint>,
    mpv_pid: Option<u32>,
    mpv_shorts: bool,
    mpv_speed: u32,
    comments_visible: bool,
    danmaku: Option<JoinHandle<()>>,
    // 1 タッチ分の横スクロール状態(開始時に左右の端まで来ていたか)
    h_sampled: bool,
    h_at_left: bool,
    h_at_right: bool,
    backlog: VecDeque<AppEvent>,
}

/// キーの表示名。
fn key_name(k: &str) -> String {
    match k {
        "\x1b" => "Esc".into(),
        "\r" | "\n" => "Enter".into(),
        " " => "Space".into(),
        "\x7f" => "Backspace".into(),
        "\t" => "Tab".into(),
        "\x1b[A" => "Up".into(),
        "\x1b[B" => "Down".into(),
        "\x1b[C" => "Right".into(),
        "\x1b[D" => "Left".into(),
        _ if !k.is_empty() && k.chars().all(|c| (' '..='~').contains(&c)) => k.into(),
        _ => format!("0x{}", k.bytes().map(|b| format!("{b:02x}")).collect::<String>()),
    }
}

fn log_action(key: &str, action: &str) {
    let line = format!("[{}] {action}", key_name(key));
    print!("\r\x1b[K{line}");
    let _ = std::io::stdout().flush();
    crate::status::message(&line);
}

fn echo(s: &str) {
    print!("{s}");
    let _ = std::io::stdout().flush();
}

/// 制御文字で始まらない入力(日本語などのまとまった入力も含む)。
fn printable(k: &str) -> bool {
    k.chars().next().is_some_and(|c| c >= ' ' && c != '\x7f')
}

pub fn print_help(favs: &Favorites) {
    println!(
        "
=== fbrowse ===

ノーマルモード:
  hjkl          スクロール          gg / G      先頭 / 末尾
  f             リンクヒント        m           カーソルモード (hjkl 移動, Space クリック, d ダブルクリック)
  i             入力 (Enter で確定) d           直前の入力を削除
  b / r         戻る / 再読込       Enter/Space そのままページへ送る
  s             撮り直し            + - 0       ズームイン / アウト / リセット
  t             新しいタブ          ] / [       次 / 前のタブ
  x             タブを閉じる        T           タブを新しい tmux ペインへ移す
  o             お気に入りに追加    p           クリップボード表示
  c / C         動画モード / 高速動画モード
  M             YouTube を mpv で再生
                  再生中: q=停止 +/-=音量 Space=一時停止 h/l=10秒戻し/送り
                          x=倍速(x1-x4) z=等速 s=字幕 p=スクショ c=コメント(j/k) d=弾幕
                          Ctrl+b=次の Shorts
  P             PiP モード切替 (tmux 右ペインで mpv、映像は右下 1/4)
  ?             このヘルプ          Q           終了

お気に入り (起動前のプロンプトで list と入力すると管理できます):"
    );
    for (name, url) in favs {
        println!("  - {name:<10} {url}");
    }
    println!();
}

impl App {
    pub fn new(ctrl: Arc<Controller>, favs: Favorites, tx: UnboundedSender<AppEvent>) -> App {
        App {
            ctrl,
            favs,
            tx,
            mode: Mode::Normal,
            input: String::new(),
            cursor_mode: false,
            video_mode: false,
            last_typed: 0,
            last_g: false,
            hints: HashMap::new(),
            mpv_pid: None,
            mpv_shorts: false,
            mpv_speed: 1,
            comments_visible: false,
            danmaku: None,
            h_sampled: false,
            h_at_left: true,
            h_at_right: true,
            backlog: VecDeque::new(),
        }
    }

    pub fn set_video_mode(&mut self, on: bool) {
        self.video_mode = on;
    }

    /// Q で終わるまでイベントを処理する。
    pub async fn run(&mut self, rx: &mut UnboundedReceiver<AppEvent>) {
        loop {
            let ev = match self.backlog.pop_front() {
                Some(ev) => ev,
                None => match rx.recv().await {
                    Some(ev) => ev,
                    None => return,
                },
            };
            match ev {
                AppEvent::Key(bytes) => {
                    let k = String::from_utf8_lossy(&bytes).to_string();
                    if let Some(d) = self.cursor_delta(&k) {
                        self.move_cursor(d, rx).await;
                        continue;
                    }
                    // 文字入力のモード以外では、まとめて届いた文字("gg" など)を 1 文字ずつ扱う
                    let keys: Vec<String> = if self.takes_text() || k.starts_with('\x1b') || k.chars().count() <= 1 {
                        vec![k]
                    } else {
                        k.chars().map(String::from).collect()
                    };
                    for k in keys {
                        match self.on_key(&k).await {
                            Ok(true) => return,
                            Ok(false) => {}
                            Err(e) => {
                                println!("\r\nエラー: {e}");
                                crate::status::message(&format!("エラー: {e}"));
                            }
                        }
                    }
                    self.update_prompt();
                }
                AppEvent::Touch(t) => self.on_touch(t, rx).await,
                AppEvent::MpvExited => {
                    self.mpv_pid = None;
                    self.reset_mpv_state();
                    self.ctrl.unmute_all().await;
                    self.ctrl.screenshot().await;
                }
                AppEvent::PipEnded => self.reset_mpv_state(),
            }
        }
    }

    /// カーソルモードの hjkl だけからなる入力なら、その移動量を返す。
    fn cursor_delta(&self, k: &str) -> Option<(i32, i32)> {
        if !self.cursor_mode || self.mode != Mode::Normal || self.mpv_pid.is_some() || k.is_empty() {
            return None;
        }
        let mut d = (0, 0);
        for c in k.chars() {
            match c {
                'h' => d.0 -= STEP,
                'l' => d.0 += STEP,
                'k' => d.1 -= STEP,
                'j' => d.1 += STEP,
                _ => return None,
            }
        }
        Some(d)
    }

    /// カーソルを動かす。長押しでキーリピートが溜まっていたらまとめて 1 回で動かす。
    async fn move_cursor(&mut self, mut d: (i32, i32), rx: &mut UnboundedReceiver<AppEvent>) {
        while let Ok(ev) = rx.try_recv() {
            let more = match &ev {
                AppEvent::Key(b) => self.cursor_delta(&String::from_utf8_lossy(b)),
                _ => None,
            };
            match more {
                Some(m) => d = (d.0 + m.0, d.1 + m.1),
                None => {
                    self.backlog.push_back(ev);
                    break;
                }
            }
        }
        if let Err(e) = self.ctrl.move_by(d.0, d.1).await {
            crate::status::message(&format!("エラー: {e}"));
            return;
        }
        // 1 回ごとに画面のメッセージを出すと描き直しが増えるので、端末にだけ出す
        let (x, y) = self.ctrl.mouse_pos();
        print!("\r\x1b[Kカーソル移動 ({x}, {y})");
        let _ = std::io::stdout().flush();
    }

    /// 入力中のモードを画面のステータス行にも出す(ターミナルが隠れていても分かるように)。
    fn update_prompt(&self) {
        let p = if self.mpv_pid.is_some() {
            None
        } else {
            match self.mode {
                Mode::Insert => Some(format!("-- INSERT -- {}▏  (Enter で確定)", self.input)),
                Mode::Hint => Some(format!("-- HINT -- {}  (Esc で取消)", self.input)),
                Mode::TabInput => Some(format!("NEW TAB> {}▏  (Tab で補完 / Esc で取消)", self.input)),
                Mode::Bookmark => Some(format!("お気に入りの名前> {}▏  (Esc で取消)", self.input)),
                Mode::Normal if self.cursor_mode => {
                    Some("-- CURSOR -- hjkl 移動 / Space クリック / d ダブルクリック / m 終了".to_string())
                }
                Mode::Normal => None,
            }
        };
        crate::status::set_prompt(p);
    }

    /// 文字列をまとめて受け取るモードか。
    fn takes_text(&self) -> bool {
        self.mpv_pid.is_none() && matches!(self.mode, Mode::Insert | Mode::TabInput | Mode::Bookmark)
    }

    fn reset_mpv_state(&mut self) {
        self.mpv_shorts = false;
        self.mpv_speed = 1;
        if let Some(h) = self.danmaku.take() {
            h.abort();
        }
    }

    async fn after_mode_exit(&self) {
        if self.video_mode {
            self.ctrl.start_auto().await;
        } else {
            self.ctrl.screenshot().await;
        }
    }

    /// 戻り値 true で終了。
    async fn on_key(&mut self, k: &str) -> Result<bool> {
        if self.mpv_pid.is_some() {
            self.mpv_key(k).await?;
            return Ok(false);
        }
        match self.mode {
            Mode::Insert => self.insert_key(k).await?,
            Mode::Hint => self.hint_key(k).await?,
            Mode::TabInput => self.tab_input_key(k).await?,
            Mode::Bookmark => self.bookmark_key(k).await,
            Mode::Normal => {
                if self.cursor_mode && self.cursor_key(k).await? {
                    return Ok(false);
                }
                let quit = self.normal_key(k).await?;
                if k != "g" {
                    self.last_g = false;
                }
                return Ok(quit);
            }
        }
        Ok(false)
    }

    // ---- mpv 再生中 ------------------------------------------------------------

    async fn mpv_key(&mut self, k: &str) -> Result<()> {
        match k {
            "q" => {
                if let Some(pid) = self.mpv_pid {
                    unsafe { libc::kill(pid as i32, libc::SIGTERM) };
                }
                log_action(k, "mpv停止");
            }
            "+" | "=" => {
                mpv::send(json!({ "command": ["add", "volume", 5] })).await;
                log_action(k, "mpv音量+");
            }
            "-" => {
                mpv::send(json!({ "command": ["add", "volume", -5] })).await;
                log_action(k, "mpv音量-");
            }
            " " => {
                mpv::send(json!({ "command": ["cycle", "pause"] })).await;
                log_action(k, "mpv一時停止/再開");
            }
            "l" | "\x1b[C" => {
                mpv::send(json!({ "command": ["seek", 10] })).await;
                log_action(k, "mpv 10秒送り");
            }
            "h" | "\x1b[D" => {
                mpv::send(json!({ "command": ["seek", -10] })).await;
                log_action(k, "mpv 10秒戻し");
            }
            "x" => {
                self.mpv_speed = if self.mpv_speed >= 4 { 1 } else { self.mpv_speed + 1 };
                mpv::send(json!({ "command": ["set_property", "speed", self.mpv_speed] })).await;
                log_action(k, &format!("mpv速度 x{}", self.mpv_speed));
            }
            "z" => {
                self.mpv_speed = 1;
                mpv::send(json!({ "command": ["set_property", "speed", 1] })).await;
                log_action(k, "mpv速度 x1");
            }
            "s" => {
                mpv::send(json!({ "command": ["cycle", "sub"] })).await;
                tokio::time::sleep(Duration::from_millis(100)).await;
                match mpv::get("current-tracks/sub/lang").await {
                    Some(serde_json::Value::String(lang)) => log_action(k, &format!("字幕 ON ({lang})")),
                    Some(_) => log_action(k, "字幕 OFF"),
                    None => log_action(k, "字幕切り替え"),
                }
            }
            "p" => {
                mpv::send(json!({ "command": ["screenshot", "window"] })).await;
                log_action(k, "スクリーンショット");
            }
            "d" => {
                mpv::send(json!({ "command": ["keypress", "d"] })).await;
                if let Some(h) = self.danmaku.take() {
                    h.abort();
                    log_action(k, "弾幕 OFF");
                } else {
                    self.danmaku = Some(mpv::start_danmaku(self.ctrl.clone()));
                    log_action(k, "弾幕 ON");
                }
            }
            "c" => {
                mpv::send(json!({ "command": ["keypress", "c"] })).await;
                self.comments_visible = !self.comments_visible;
                log_action(k, &format!("コメント {}", if self.comments_visible { "ON" } else { "OFF" }));
            }
            "j" | "k" if self.comments_visible => {
                mpv::send(json!({ "command": ["keypress", k] })).await;
                log_action(k, if k == "j" { "コメント↓" } else { "コメント↑" });
            }
            "\x02" if self.mpv_shorts => {
                // ブラウザ側で次の Shorts へ進み、その動画を mpv に読み込ませる
                self.ctrl.press("ArrowDown").await?;
                tokio::time::sleep(Duration::from_secs(1)).await;
                self.ctrl.mute_all().await;
                let url = self.ctrl.current_url().await;
                match mpv::youtube_url(&url) {
                    Some((watch, true)) => {
                        mpv::send(json!({ "command": ["loadfile", watch, "replace"] })).await;
                        log_action(k, &format!("次のshort: {watch}"));
                    }
                    _ => log_action(k, "次のshort: ID取得失敗"),
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// mpv で再生を始める。from_hint=true ならブラウザも後から同じ動画を開く。
    async fn start_mpv(&mut self, url: String, shorts: bool, from_hint: bool) {
        println!("\r\nmpvで再生: {url}");
        self.mpv_shorts = shorts;
        self.comments_visible = false;
        let (kind, pid) = mpv::play(&self.ctrl, &url, self.tx.clone()).await;
        if kind != mpv::Kind::Pip {
            self.mpv_pid = pid;
        }
        // PiP はユーザーがブラウザを操作中なので勝手に遷移しない
        if kind == mpv::Kind::Pip {
            return;
        }
        let ctrl = self.ctrl.clone();
        if from_hint {
            tokio::spawn(mpv::open_in_browser_muted(ctrl, url));
        } else {
            tokio::spawn(mpv::skip_ads_and_pause(ctrl));
        }
    }

    // ---- 入力系モード -------------------------------------------------------

    async fn insert_key(&mut self, k: &str) -> Result<()> {
        match k {
            "\r" | "\n" => {
                self.mode = Mode::Normal;
                if !self.input.is_empty() {
                    let text = std::mem::take(&mut self.input);
                    self.ctrl.type_text(&text).await?;
                    self.last_typed = text.chars().count();
                }
                log_action(k, "INSERT終了");
                self.after_mode_exit().await;
            }
            "\x7f" => {
                if self.input.pop().is_some() {
                    echo("\x08 \x08");
                }
            }
            "\x1b" => {}
            _ if printable(k) || k == "\t" => {
                self.input.push_str(k);
                echo(k);
            }
            _ => {}
        }
        Ok(())
    }

    async fn hint_key(&mut self, k: &str) -> Result<()> {
        match k {
            "f" | "\x1b" => {
                self.ctrl.clear_hints().await;
                self.ctrl.screenshot().await;
                log_action(k, "ヒントモード終了");
                self.mode = Mode::Normal;
                self.input.clear();
            }
            "\x7f" => {
                if !self.input.is_empty() {
                    self.input.clear();
                    self.hints = self.ctrl.show_hints().await?;
                    self.ctrl.screenshot().await;
                    log_action(k, "ヒントリセット");
                }
            }
            _ => {
                self.input.push_str(&k.to_lowercase());
                if self.input.chars().count() < 2 {
                    return Ok(());
                }
                let typed = std::mem::take(&mut self.input);
                self.mode = Mode::Normal;
                self.ctrl.clear_hints().await;
                let Some(hint) = self.hints.remove(&typed) else {
                    self.ctrl.screenshot().await;
                    log_action(k, &format!("ヒント不一致: {typed}"));
                    return Ok(());
                };
                // YouTube の動画リンクならページ遷移せず mpv で再生
                if let Some((yt, shorts)) = hint.href.as_deref().and_then(mpv::youtube_url) {
                    if hint.href.as_deref().is_some_and(|h| h.contains("youtube.com")) {
                        log_action(k, &format!("mpv再生: {yt}"));
                        self.start_mpv(yt, shorts, true).await;
                        return Ok(());
                    }
                }
                self.ctrl.click(hint.x.round() as i32, hint.y.round() as i32).await?;
                log_action(k, &format!("ヒントクリック: {typed} → ({}, {})", hint.x.round(), hint.y.round()));
            }
        }
        Ok(())
    }

    async fn tab_input_key(&mut self, k: &str) -> Result<()> {
        match k {
            "\x1b" => {
                self.mode = Mode::Normal;
                self.input.clear();
                echo("\r\x1b[K");
                self.after_mode_exit().await;
            }
            "\t" => {
                let prefix = self.input.to_lowercase();
                let matches: Vec<&String> = self.favs.keys().filter(|n| n.starts_with(&prefix)).collect();
                if matches.len() == 1 {
                    let rest = matches[0][prefix.len()..].to_string();
                    self.input.push_str(&rest);
                    echo(&rest);
                } else if matches.len() > 1 {
                    let list = matches.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("  ");
                    println!("\r\n{list}");
                    crate::status::message(&format!("候補: {list}"));
                    echo(&format!("> {}", self.input));
                }
            }
            "\r" | "\n" => {
                let text = std::mem::take(&mut self.input);
                self.mode = Mode::Normal;
                echo("\r\x1b[K");
                let url = favorites::resolve(&text, &self.favs).unwrap_or_else(|| DEFAULT_URL.to_string());
                self.ctrl.new_tab(&url).await?;
                if self.video_mode {
                    self.ctrl.start_auto().await;
                }
            }
            "\x7f" => {
                if self.input.pop().is_some() {
                    echo("\x08 \x08");
                }
            }
            _ if printable(k) => {
                self.input.push_str(k);
                echo(k);
            }
            _ => {}
        }
        Ok(())
    }

    async fn bookmark_key(&mut self, k: &str) {
        match k {
            "\x1b" => {
                self.mode = Mode::Normal;
                self.input.clear();
                echo("\r\x1b[K");
            }
            "\r" | "\n" => {
                let name = std::mem::take(&mut self.input).trim().to_lowercase();
                self.mode = Mode::Normal;
                if !name.is_empty() {
                    let url = self.ctrl.current_url().await;
                    self.favs.insert(name.clone(), url.clone());
                    favorites::save(&self.favs);
                    println!("\r\nお気に入りに追加: {name} -> {url}");
                }
            }
            "\x7f" => {
                if self.input.pop().is_some() {
                    echo("\x08 \x08");
                }
            }
            _ if printable(k) => {
                self.input.push_str(k);
                echo(k);
            }
            _ => {}
        }
    }

    /// カーソルモードのキー。処理したら true(それ以外はノーマルモードへ回す)。
    async fn cursor_key(&mut self, k: &str) -> Result<bool> {
        let action = match k {
            " " => {
                self.ctrl.click_here().await?;
                "クリック"
            }
            "d" => {
                self.ctrl.dblclick_here().await?;
                "ダブルクリック"
            }
            "m" => {
                self.cursor_mode = false;
                self.ctrl.set_cursor(false);
                self.ctrl.screenshot().await;
                log_action(k, "カーソルモード終了");
                return Ok(true);
            }
            _ => return Ok(false),
        };
        let (x, y) = self.ctrl.mouse_pos();
        log_action(k, &format!("カーソル{action} ({x}, {y})"));
        Ok(true)
    }

    // ---- ノーマルモード -------------------------------------------------------

    async fn normal_key(&mut self, k: &str) -> Result<bool> {
        let c = &self.ctrl;
        match k {
            "h" => {
                c.scroll(-640.0, 0.0).await?;
                log_action(k, "←スクロール");
            }
            "j" => {
                c.scroll(0.0, 360.0).await?;
                log_action(k, "↓スクロール");
            }
            "k" => {
                c.scroll(0.0, -360.0).await?;
                log_action(k, "↑スクロール");
            }
            "l" => {
                c.scroll(640.0, 0.0).await?;
                log_action(k, "→スクロール");
            }
            "\x1b[A" | "\x1b[B" | "\x1b[C" | "\x1b[D" => {
                let key = match k {
                    "\x1b[A" => "ArrowUp",
                    "\x1b[B" => "ArrowDown",
                    "\x1b[C" => "ArrowRight",
                    _ => "ArrowLeft",
                };
                c.press(key).await?;
                log_action(k, key);
            }
            "m" => {
                self.cursor_mode = true;
                c.set_cursor(true);
                let (w, h) = {
                    let d = c.disp.lock().unwrap();
                    (d.w as i32, d.h as i32)
                };
                c.move_to(w / 2, h / 2).await?;
                log_action(k, "カーソルモード開始");
            }
            "i" => {
                if self.video_mode {
                    c.stop_auto().await;
                }
                self.mode = Mode::Insert;
                self.input.clear();
                log_action(k, "INSERTモード開始");
                println!("\r\n-- INSERT -- (Enterで確定)");
            }
            "s" => {
                c.screenshot().await;
                log_action(k, "スクリーンショット再取得");
            }
            "d" => {
                if self.last_typed > 0 {
                    c.delete(self.last_typed).await?;
                    log_action(k, &format!("入力削除 ({}文字)", self.last_typed));
                    self.last_typed = 0;
                } else {
                    log_action(k, "入力削除 (削除対象なし)");
                }
            }
            "b" => {
                c.go_back().await?;
                log_action(k, "戻る");
            }
            "r" => {
                c.reload().await?;
                log_action(k, "再読込");
            }
            "\r" | "\n" => {
                c.press("Enter").await?;
                log_action(k, "Enter送信");
            }
            " " => {
                c.press("Space").await?;
                log_action(k, "Space送信");
            }
            "c" => {
                if self.video_mode {
                    c.stop_auto().await;
                    if c.is_high_speed() {
                        c.set_high_speed(false).await;
                    }
                    self.video_mode = false;
                    c.screenshot().await;
                    log_action(k, "通常モードへ切替");
                } else {
                    c.start_auto().await;
                    self.video_mode = true;
                    log_action(k, "動画モードへ切替");
                }
            }
            "C" => {
                if !self.video_mode {
                    c.set_high_speed(true).await;
                    c.start_auto().await;
                    self.video_mode = true;
                    log_action(k, "動画モード + 高速再生 ON");
                } else if c.is_high_speed() {
                    c.set_high_speed(false).await;
                    log_action(k, "高速再生 OFF");
                } else {
                    c.set_high_speed(true).await;
                    log_action(k, "高速再生 ON");
                }
            }
            "g" => {
                if self.last_g {
                    c.scroll_to_top().await?;
                    log_action("g", "ページ先頭へ");
                    self.last_g = false;
                } else {
                    self.last_g = true;
                    log_action(k, "g待機中 (ggで先頭)");
                }
            }
            "G" => {
                c.scroll_to_bottom().await?;
                log_action(k, "ページ末尾へ");
            }
            "M" => {
                let url = c.current_url().await;
                match mpv::youtube_url(&url) {
                    Some((watch, shorts)) => {
                        log_action(k, "YouTube再生開始...");
                        self.start_mpv(watch, shorts, false).await;
                    }
                    None => log_action(k, "mpv再生 (YouTubeの動画ページではありません)"),
                }
            }
            "P" => {
                let on = c.toggle_pip();
                log_action(k, &format!("PiPモード {}", if on { "ON" } else { "OFF" }));
            }
            "t" => {
                if self.video_mode {
                    c.stop_auto().await;
                }
                self.mode = Mode::TabInput;
                self.input.clear();
                log_action(k, "新規タブ入力モード");
                println!("\r\n-- NEW TAB -- (お気に入り/URL入力, Enterで開く, Escで戻る)");
                println!("お気に入り: {}", self.favs.keys().cloned().collect::<Vec<_>>().join(", "));
                echo("> ");
            }
            "]" => {
                c.next_tab().await;
                log_action(k, "次のタブ");
            }
            "[" => {
                c.prev_tab().await;
                log_action(k, "前のタブ");
            }
            "x" => {
                c.close_tab().await;
                log_action(k, "タブを閉じる");
            }
            "T" => self.move_tab_to_new_pane(k).await?,
            "o" => {
                self.mode = Mode::Bookmark;
                self.input.clear();
                log_action(k, "お気に入り追加モード");
                echo("  名前> ");
            }
            "p" => {
                let clip = c.read_clipboard().await;
                log_action(k, "クリップボード表示");
                crate::status::message(&if clip.is_empty() {
                    "クリップボードは空です".to_string()
                } else {
                    format!("クリップボード: {}", clip.split_whitespace().collect::<Vec<_>>().join(" "))
                });
                if clip.is_empty() {
                    println!("\r\n(クリップボードは空です)");
                } else {
                    println!("\r\n--- clipboard ---\n{clip}\n---");
                }
            }
            "+" | "=" | "-" | "0" => {
                match k {
                    "0" => c.zoom_reset().await?,
                    "-" => c.zoom_by(-0.1).await?,
                    _ => c.zoom_by(0.1).await?,
                }
                if !self.video_mode {
                    c.screenshot().await;
                }
                log_action(k, &format!("ズーム {}%", (c.zoom() * 100.0).round()));
            }
            "f" => {
                self.hints = c.show_hints().await?;
                c.screenshot().await;
                self.mode = Mode::Hint;
                self.input.clear();
                log_action(k, &format!("ヒントモード開始 ({}個)", self.hints.len()));
            }
            "?" => print_help(&self.favs),
            "Q" => {
                log_action(k, "終了");
                return Ok(true);
            }
            _ => {}
        }
        Ok(false)
    }

    /// 現在のタブを新しい tmux ペイン(右)の fbrowse へ移す。
    async fn move_tab_to_new_pane(&mut self, k: &str) -> Result<()> {
        if !tmux::inside() {
            println!("\r\ntmux外では使用できません");
            return Ok(());
        }
        let url = self.ctrl.current_url().await;
        let exe = std::env::current_exe()?;
        let q = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
        let cmd = format!("{} --auto {}", q(&exe.to_string_lossy()), q(&url));
        let Some(pane) = tmux::run(&["split-window", "-h", "-P", "-F", "#{pane_id}", &cmd]) else {
            println!("\r\nsplit失敗");
            return Ok(());
        };
        // 新ペインの位置に今のページを即時に描いておく(起動待ちの間の見た目)
        let (full, cell, rot) = {
            let d = self.ctrl.disp.lock().unwrap();
            let full = match &d.drmterm {
                Some(ov) => Some((ov.info.width, ov.info.height)),
                None => tmux::read_fb_info().map(|(w, h, _)| (w, h)),
            };
            (full, d.drmterm.as_ref().map(|o| (o.info.cell_w, o.info.cell_h)), d.rotation)
        };
        if let Some((fw, fh)) = full {
            if let Some(r) = tmux::pane_region(fw, fh, Some(&pane), cell, rot) {
                self.ctrl.capture_to_region(r.x, r.y, r.w, r.h).await;
            }
        }
        self.ctrl.close_tab().await;
        log_action(k, &format!("新規ペインに移動: {url}"));
        Ok(())
    }

    // ---- タッチ -------------------------------------------------------------

    fn touch_allowed(&self) -> bool {
        self.mode == Mode::Normal && !self.cursor_mode && self.mpv_pid.is_none()
    }

    /// 同じ種類のタッチイベントが続けて溜まっていればまとめる(描画が追いつくように)。
    fn drain_same(&mut self, rx: &mut UnboundedReceiver<AppEvent>, mut acc: f64, kind: fn(&Touch) -> Option<f64>, mul: bool) -> f64 {
        while let Ok(ev) = rx.try_recv() {
            match &ev {
                AppEvent::Touch(t) if kind(t).is_some() => {
                    let v = kind(t).unwrap();
                    acc = if mul { acc * v } else { acc + v };
                }
                _ => {
                    self.backlog.push_back(ev);
                    break;
                }
            }
        }
        acc
    }

    async fn on_touch(&mut self, t: Touch, rx: &mut UnboundedReceiver<AppEvent>) {
        let c = self.ctrl.clone();
        match t {
            Touch::Start => {
                self.h_sampled = false;
                self.h_at_left = true;
                self.h_at_right = true;
            }
            Touch::Scroll(dy) => {
                let dy = self.drain_same(rx, dy, |t| if let Touch::Scroll(v) = t { Some(*v) } else { None }, false);
                if !self.touch_allowed() {
                    return;
                }
                // 指が下へ動くと画面は上へ(ホイールは負)
                let h = c.disp.lock().unwrap().h as f64;
                let d = (-dy * h).round();
                if d.abs() >= 1.0 {
                    if let Err(e) = c.scroll(0.0, d).await {
                        eprintln!("[touch] scroll: {e}");
                    }
                }
            }
            Touch::ScrollH(dx) => {
                let dx = self.drain_same(rx, dx, |t| if let Touch::ScrollH(v) = t { Some(*v) } else { None }, false);
                if !self.touch_allowed() {
                    return;
                }
                // 横スクロール開始時に 1 回だけ「どちらの端まで来ているか」を取る
                if !self.h_sampled {
                    self.h_sampled = true;
                    (self.h_at_left, self.h_at_right) = c.hscroll_state().await;
                }
                let w = c.disp.lock().unwrap().w as f64;
                let d = (-dx * w).round();
                if d.abs() >= 1.0 {
                    if let Err(e) = c.scroll(d, 0.0).await {
                        eprintln!("[touch] hscroll: {e}");
                    }
                }
            }
            Touch::Pinch(s) => {
                let s = self.drain_same(rx, s, |t| if let Touch::Pinch(v) = t { Some(*v) } else { None }, true);
                if !self.touch_allowed() || (s - 1.0).abs() < 0.001 {
                    return;
                }
                if let Err(e) = c.pinch_zoom_by(s).await {
                    eprintln!("[touch] pinch: {e}");
                }
                tokio::spawn(async move { c.render_touch_result().await });
            }
            Touch::Tap(fx, fy) => {
                if !self.touch_allowed() {
                    return;
                }
                let (x, y) = c.disp.lock().unwrap().global_frac_to_local(fx, fy);
                match c.click_fast(x, y).await {
                    Ok(()) => log_action("touch", &format!("タップ → クリック ({x}, {y})")),
                    Err(e) => eprintln!("[touch] tap: {e}"),
                }
                tokio::spawn(async move { c.render_touch_result().await });
            }
            Touch::SwipeRight => {
                // 横スクロールしていたなら、開始時に左端まで来ていたときだけ「戻る」
                if !self.touch_allowed() || (self.h_sampled && !self.h_at_left) {
                    return;
                }
                if c.go_back().await.is_ok() {
                    log_action("touch", "スワイプ→ 戻る");
                }
            }
            Touch::SwipeLeft => {
                if !self.touch_allowed() || (self.h_sampled && !self.h_at_right) {
                    return;
                }
                if c.go_forward().await.is_ok() {
                    log_action("touch", "スワイプ← 進む");
                }
            }
            Touch::LongPress => {}
        }
    }
}
