//! ブラウザ制御の本体。CDP でタブを操作し、スクリーンショット/スクリーンキャストを
//! フレームバッファへ描く。tmux ペインへの追従や fb-server の可視性もここで扱う。
//!
//! 「スクリーンキャスト(動画モード)」中は Chromium から届くフレームを描き続け、
//! それ以外は操作のたびにスクリーンショットを撮って描く。

use crate::app::AppEvent;
use crate::cdp::{Cdp, Event};
use crate::display::{Display, Rect};
use crate::render::{self, Fit, Frame};
use crate::status;
use crate::tmux;
use anyhow::{anyhow, bail, Result};
use base64::Engine;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;

pub const DEFAULT_URL: &str = "https://www.google.com";

pub struct Tab {
    pub target: String,
    pub session: String,
    pub title: String,
    pub url: String,
}

#[derive(Default)]
struct Tabs {
    list: Vec<Tab>,
    cur: usize,
    /// 自分で知っている target(起動時からあるもの・自分で開いたもの)
    known: HashSet<String>,
    /// new_tab() が Target.createTarget を発行中の数(targetCreated を無視するため)
    pending_new: u32,
    /// 自分で閉じている最中の target
    closing: HashSet<String>,
}

struct State {
    mouse: (i32, i32),
    show_cursor: bool,
    zoom: f64,
    /// スクリーンキャスト(動画モード)中か
    screencast: bool,
    screencast_session: Option<String>,
    high_speed: bool,
    pip_mode: bool,
    pip_size: (u32, u32),
    frames: u32,
    fps_start: Instant,
}

struct Track {
    fb_w: u32,
    fb_h: u32,
    stride: u32,
    cell: Option<(u32, u32)>,
}

pub struct Hint {
    pub x: f64,
    pub y: f64,
    pub href: Option<String>,
}

type LastSrc = (Vec<u8>, Option<(Vec<String>, usize)>, (u32, u32));

pub struct Controller {
    pub cdp: Cdp,
    tabs: Mutex<Tabs>,
    pub disp: Mutex<Display>,
    st: Mutex<State>,
    track: Mutex<Option<Track>>,
    /// 直前に描いたページの絵(ステータス行を重ねる前)
    last_clean: Mutex<Option<Frame>>,
    /// last_clean の元になった JPEG・タブバーの内容・大きさ
    last_src: Mutex<Option<LastSrc>>,
    /// カーソル移動の世代(止まってから撮り直すため)
    move_gen: AtomicU64,
    /// スクロールの世代(止まってから撮り直すため)
    scroll_gen: AtomicU64,
    capturing: AtomicBool,
    shoot_again: AtomicBool,
    mpv_busy: AtomicBool,
    downloads: PathBuf,
    download_names: Mutex<HashMap<String, String>>,
    pub pip_dir: PathBuf,
    /// ファイル選択ダイアログをキー操作側へ知らせる先
    app_tx: Mutex<Option<UnboundedSender<AppEvent>>>,
}

/// 別スレッドから時々呼ばれるログ出力(raw モードでも行頭から書く)。
pub fn log(msg: &str) {
    println!("\r\x1b[K{msg}");
    status::message(msg);
}

impl Controller {
    pub fn new(cdp: Cdp, disp: Display, downloads: PathBuf, pip_dir: PathBuf) -> Arc<Controller> {
        let ctrl = Arc::new(Controller {
            cdp,
            tabs: Mutex::new(Tabs::default()),
            disp: Mutex::new(disp),
            st: Mutex::new(State {
                mouse: (0, 0),
                show_cursor: false,
                zoom: 1.0,
                screencast: false,
                screencast_session: None,
                high_speed: false,
                pip_mode: false,
                pip_size: (0, 0),
                frames: 0,
                fps_start: Instant::now(),
            }),
            track: Mutex::new(None),
            move_gen: AtomicU64::new(0),
            scroll_gen: AtomicU64::new(0),
            capturing: AtomicBool::new(false),
            shoot_again: AtomicBool::new(false),
            mpv_busy: AtomicBool::new(false),
            downloads,
            download_names: Mutex::new(HashMap::new()),
            pip_dir,
            last_clean: Mutex::new(None),
            last_src: Mutex::new(None),
            app_tx: Mutex::new(None),
        });
        let weak = Arc::downgrade(&ctrl);
        status::set_redraw(move || {
            if let Some(c) = weak.upgrade() {
                c.redraw_overlay();
            }
        });
        ctrl
    }

    pub fn set_app_tx(&self, tx: UnboundedSender<AppEvent>) {
        *self.app_tx.lock().unwrap() = Some(tx);
    }

    // ---- 起動・タブ -------------------------------------------------------

    /// 最初のタブを掴んでイベント処理を始め、url を開く。
    pub async fn start(self: &Arc<Self>, url: &str) -> Result<()> {
        let _ = std::fs::create_dir_all(&self.downloads);
        let _ = self
            .cdp
            .call(
                "Browser.setDownloadBehavior",
                json!({ "behavior": "allow", "downloadPath": self.downloads, "eventsEnabled": true }),
                None,
            )
            .await;
        let _ = self
            .cdp
            .call(
                "Browser.grantPermissions",
                json!({ "permissions": ["clipboardReadWrite", "clipboardSanitizedWrite"] }),
                None,
            )
            .await;

        let targets = self.cdp.call("Target.getTargets", json!({}), None).await?;
        let mut first: Option<String> = None;
        {
            let mut tabs = self.tabs.lock().unwrap();
            for t in targets["targetInfos"].as_array().into_iter().flatten() {
                let id = t["targetId"].as_str().unwrap_or_default().to_string();
                if t["type"] == "page" && first.is_none() {
                    first = Some(id.clone());
                }
                tabs.known.insert(id);
            }
        }
        let target = match first {
            Some(t) => t,
            None => self.create_target("about:blank").await?,
        };
        let session = self.attach(&target).await?;
        self.tabs.lock().unwrap().list.push(Tab { target, session, title: String::new(), url: String::new() });

        self.clone().spawn_events();
        self.cdp.call("Target.setDiscoverTargets", json!({ "discover": true }), None).await?;
        self.goto(url).await
    }

    async fn create_target(&self, url: &str) -> Result<String> {
        self.tabs.lock().unwrap().pending_new += 1;
        let res = self.cdp.call("Target.createTarget", json!({ "url": url }), None).await;
        let id = match res {
            Ok(v) => v["targetId"].as_str().unwrap_or_default().to_string(),
            Err(e) => {
                let mut t = self.tabs.lock().unwrap();
                t.pending_new = t.pending_new.saturating_sub(1);
                return Err(e);
            }
        };
        self.tabs.lock().unwrap().known.insert(id.clone());
        Ok(id)
    }

    /// target に flatten セッションで接続し、ビューポート等を整える。
    async fn attach(&self, target: &str) -> Result<String> {
        let r = self.cdp.call("Target.attachToTarget", json!({ "targetId": target, "flatten": true }), None).await?;
        let session = r["sessionId"].as_str().ok_or_else(|| anyhow!("sessionId がありません"))?.to_string();
        let s = Some(session.as_str());
        self.cdp.call("Page.enable", json!({}), s).await?;
        let _ = self.cdp.call("Emulation.setFocusEmulationEnabled", json!({ "enabled": true }), s).await;
        // <select> の標準ポップアップは別ウィンドウで撮影に写らないので、ページ内のメニューに置き換える
        let _ = self.cdp.call("Page.addScriptToEvaluateOnNewDocument", json!({ "source": SELECT_JS }), s).await;
        let _ = self.evaluate_in(&session, SELECT_JS).await;
        // ファイル選択ダイアログも撮影に写らず操作できないので、開かずにイベントで受け取る
        let _ = self.cdp.call("Page.setInterceptFileChooserDialog", json!({ "enabled": true }), s).await;
        let (w, h) = self.viewport();
        let _ = self.set_metrics(&session, w, h).await;
        Ok(session)
    }

    async fn set_metrics(&self, session: &str, w: u32, h: u32) -> Result<()> {
        self.cdp
            .call(
                "Emulation.setDeviceMetricsOverride",
                json!({ "width": w, "height": h, "deviceScaleFactor": 1, "mobile": false }),
                Some(session),
            )
            .await
            .map(|_| ())
    }

    /// 現在のタブを前面に出す(headful の Chromium は裏のタブを描かないため、
    /// 撮影やスクリーンキャストの前に必要)。
    async fn bring_to_front(&self) {
        if let Some(s) = self.cur_session() {
            let _ = self.cdp.call("Page.bringToFront", json!({}), Some(&s)).await;
        }
    }

    fn viewport(&self) -> (u32, u32) {
        let d = self.disp.lock().unwrap();
        (d.w, d.h)
    }

    pub fn cur_session(&self) -> Option<String> {
        let t = self.tabs.lock().unwrap();
        t.list.get(t.cur).map(|t| t.session.clone())
    }

    fn cur_session_or_err(&self) -> Result<String> {
        self.cur_session().ok_or_else(|| anyhow!("ブラウザが起動していません"))
    }

    fn all_sessions(&self) -> Vec<String> {
        self.tabs.lock().unwrap().list.iter().map(|t| t.session.clone()).collect()
    }

    pub async fn new_tab(self: &Arc<Self>, url: &str) -> Result<()> {
        let target = self.create_target("about:blank").await?;
        let session = self.attach(&target).await?;
        {
            let mut t = self.tabs.lock().unwrap();
            t.list.push(Tab { target, session, title: String::new(), url: String::new() });
            t.cur = t.list.len() - 1;
        }
        self.bring_to_front().await;
        if self.is_screencast() {
            self.restart_screencast().await;
        }
        self.goto(url).await
    }

    pub async fn next_tab(self: &Arc<Self>) {
        self.switch_tab(1).await
    }

    pub async fn prev_tab(self: &Arc<Self>) {
        self.switch_tab(-1).await
    }

    async fn switch_tab(self: &Arc<Self>, delta: i32) {
        {
            let mut t = self.tabs.lock().unwrap();
            let n = t.list.len() as i32;
            if n <= 1 {
                return;
            }
            t.cur = ((t.cur as i32 + delta).rem_euclid(n)) as usize;
        }
        self.bring_to_front().await;
        // 切り替え先のビューポートを今の描画領域に合わせる
        if let Some(s) = self.cur_session() {
            let (w, h) = self.viewport();
            let _ = self.set_metrics(&s, w, h).await;
        }
        if self.is_screencast() {
            self.restart_screencast().await;
        } else {
            self.screenshot().await;
        }
    }

    pub async fn close_tab(self: &Arc<Self>) {
        let target = {
            let mut t = self.tabs.lock().unwrap();
            if t.list.len() <= 1 {
                return;
            }
            let cur = t.cur;
            let tab = t.list.remove(cur);
            t.cur = cur.min(t.list.len() - 1);
            t.closing.insert(tab.target.clone());
            tab.target
        };
        let _ = self.cdp.call("Target.closeTarget", json!({ "targetId": target }), None).await;
        self.bring_to_front().await;
        if self.is_screencast() {
            self.restart_screencast().await;
        } else {
            self.screenshot().await;
        }
    }

    // ---- CDP イベント -----------------------------------------------------

    fn spawn_events(self: Arc<Self>) {
        let mut rx = self.cdp.subscribe();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(ev) => self.handle_event(ev).await,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
        });
    }

    async fn handle_event(self: &Arc<Self>, ev: Arc<Event>) {
        let p = &ev.params;
        match ev.method.as_str() {
            "Page.screencastFrame" => {
                let sid = p["sessionId"].clone();
                let session = ev.session.clone();
                self.on_screencast_frame(&ev).await;
                self.cdp.send("Page.screencastFrameAck", json!({ "sessionId": sid }), session.as_deref());
            }
            "Page.loadEventFired" => {
                let Some(session) = ev.session.clone() else { return };
                let me = self.clone();
                tokio::spawn(async move {
                    me.refresh_title(&session).await;
                    if me.cur_session().as_deref() == Some(session.as_str()) && !me.is_screencast() {
                        me.wait_for_render().await;
                        me.screenshot().await;
                    }
                });
            }
            "Target.targetInfoChanged" => {
                let info = &p["targetInfo"];
                let id = info["targetId"].as_str().unwrap_or_default();
                let mut t = self.tabs.lock().unwrap();
                if let Some(tab) = t.list.iter_mut().find(|t| t.target == id) {
                    let title = info["title"].as_str().unwrap_or_default();
                    let url = info["url"].as_str().unwrap_or_default();
                    // 読み込み中は URL がタイトル代わりに入ってくるので、それは採らない
                    if !title.is_empty() && !url.ends_with(title) {
                        tab.title = title.to_string();
                    }
                    tab.url = url.to_string();
                }
            }
            "Target.targetCreated" => {
                let info = &p["targetInfo"];
                if info["type"] != "page" {
                    return;
                }
                let id = info["targetId"].as_str().unwrap_or_default().to_string();
                {
                    let mut t = self.tabs.lock().unwrap();
                    if t.known.contains(&id) {
                        return;
                    }
                    // new_tab() が発行した createTarget によるものは無視する
                    if t.pending_new > 0 {
                        t.pending_new -= 1;
                        t.known.insert(id);
                        return;
                    }
                    t.known.insert(id.clone());
                }
                // target="_blank" などでページが開いたタブ → そちらへ切り替える
                let me = self.clone();
                tokio::spawn(async move {
                    if let Err(e) = me.adopt(&id).await {
                        log(&format!("新しいタブを開けませんでした: {e}"));
                    }
                });
            }
            "Target.targetDestroyed" => {
                let id = p["targetId"].as_str().unwrap_or_default();
                let removed_cur = {
                    let mut t = self.tabs.lock().unwrap();
                    if t.closing.remove(id) {
                        return;
                    }
                    let Some(i) = t.list.iter().position(|t| t.target == id) else { return };
                    if t.list.len() <= 1 {
                        return;
                    }
                    t.list.remove(i);
                    let was_cur = i == t.cur;
                    if i < t.cur || t.cur >= t.list.len() {
                        t.cur = t.cur.saturating_sub(1);
                    }
                    was_cur
                };
                if removed_cur {
                    let me = self.clone();
                    tokio::spawn(async move {
                        me.bring_to_front().await;
                        if me.is_screencast() {
                            me.restart_screencast().await;
                        } else {
                            me.screenshot().await;
                        }
                    });
                }
            }
            "Page.fileChooserOpened" => {
                let (Some(session), Some(node)) = (ev.session.clone(), p["backendNodeId"].as_i64()) else {
                    log("ファイル選択: 対象の要素が分かりません");
                    return;
                };
                let multiple = p["mode"] == "selectMultiple";
                if let Some(tx) = self.app_tx.lock().unwrap().as_ref() {
                    let _ = tx.send(AppEvent::FileChooser { session, node, multiple });
                }
            }
            "Browser.downloadWillBegin" => {
                let guid = p["guid"].as_str().unwrap_or_default().to_string();
                let name = p["suggestedFilename"].as_str().unwrap_or_default().to_string();
                self.download_names.lock().unwrap().insert(guid, name);
            }
            "Browser.downloadProgress" => {
                let guid = p["guid"].as_str().unwrap_or_default();
                let state = p["state"].as_str().unwrap_or_default();
                if state == "completed" || state == "canceled" {
                    let name = self.download_names.lock().unwrap().remove(guid).unwrap_or_default();
                    let path = self.downloads.join(&name);
                    if state == "completed" {
                        log(&format!("ダウンロード完了: {}", path.display()));
                    } else {
                        log(&format!("ダウンロード失敗: {name}"));
                    }
                }
            }
            _ => {}
        }
    }

    async fn adopt(self: &Arc<Self>, target: &str) -> Result<()> {
        let session = self.attach(target).await?;
        {
            let mut t = self.tabs.lock().unwrap();
            t.list.push(Tab { target: target.to_string(), session: session.clone(), title: String::new(), url: String::new() });
            t.cur = t.list.len() - 1;
        }
        self.bring_to_front().await;
        if self.is_screencast() {
            self.restart_screencast().await;
            return Ok(());
        }
        // 接続した時点で読み込みが終わっていると load イベントは来ないので、
        // 読み込み完了を待ってから描く
        for _ in 0..100 {
            if let Ok(Value::String(s)) = self.evaluate_in(&session, "document.readyState").await {
                if s == "complete" {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        self.refresh_title(&session).await;
        self.wait_for_render().await;
        self.screenshot().await;
        Ok(())
    }

    /// タブバー用にタイトルを取り直す。
    async fn refresh_title(&self, session: &str) {
        let Ok(Value::String(title)) = self.evaluate_in(session, "document.title").await else { return };
        let mut t = self.tabs.lock().unwrap();
        if let Some(tab) = t.list.iter_mut().find(|t| t.session == session) {
            if !title.is_empty() {
                tab.title = title;
            }
        }
    }

    // ---- 描画 -------------------------------------------------------------

    /// JPEG を描画領域の大きさに合わせ、タブバーとカーソルを重ねて描く。
    fn present(&self, jpeg: &[u8]) -> Result<()> {
        let (w, h) = self.viewport();
        let tabs = {
            let t = self.tabs.lock().unwrap();
            (t.list.len() > 1).then(|| (t.list.iter().map(|t| t.title.clone()).collect::<Vec<_>>(), t.cur))
        };
        // 止まってからの撮り直しなどで前と同じ絵なら、デコードせずに使い回す
        let same = self.last_src.lock().unwrap().as_ref().is_some_and(|(j, t, size)| {
            j.as_slice() == jpeg && *t == tabs && *size == (w, h)
        });
        if same {
            if let Some(f) = self.last_clean.lock().unwrap().clone() {
                self.show(f);
                return Ok(());
            }
        }
        let mut f = render::resize(render::decode_jpeg(jpeg)?, w, h, Fit::Fill);
        if let Some((titles, cur)) = &tabs {
            render::draw_tab_bar(&mut f, titles, *cur);
        }
        *self.last_clean.lock().unwrap() = Some(f.clone());
        *self.last_src.lock().unwrap() = Some((jpeg.to_vec(), tabs, (w, h)));
        self.show(f);
        Ok(())
    }

    /// カーソルとステータス行を重ねて描く。
    fn show(&self, mut f: Frame) {
        let (show_cursor, (mx, my)) = {
            let st = self.st.lock().unwrap();
            (st.show_cursor, st.mouse)
        };
        if show_cursor {
            render::draw_cursor(&mut f, mx, my);
        }
        if let Some(text) = status::current() {
            let bottom = if self.tabs.lock().unwrap().list.len() > 1 { render::TAB_H } else { 0 };
            render::draw_status(&mut f, &text, bottom);
        }
        self.disp.lock().unwrap().blit(f);
    }

    /// カーソル位置やステータス行だけが変わったとき、直前のページの絵に
    /// 重ね直す(撮り直さないので速い)。
    fn redraw_overlay(&self) {
        let Some(f) = self.last_clean.lock().unwrap().clone() else { return };
        let (w, h) = self.viewport();
        if f.w == w && f.h == h {
            self.show(f);
        }
    }

    async fn capture_jpeg(&self, session: &str) -> Option<Vec<u8>> {
        let quality = if self.st.lock().unwrap().high_speed { 1 } else { 30 };
        // ページ遷移の途中などで応答が返らないことがあるので、長くは待たない
        let call = self.cdp.call(
            "Page.captureScreenshot",
            json!({ "format": "jpeg", "quality": quality, "optimizeForSpeed": true }),
            Some(session),
        );
        let r = tokio::time::timeout(Duration::from_secs(3), call).await.ok()?.ok()?;
        base64::engine::general_purpose::STANDARD.decode(r["data"].as_str()?).ok()
    }

    /// 現在のタブを撮って描く。撮影中に呼ばれたら、終わってからもう一度撮る
    /// (クリック直後の撮影と新しいタブの撮影が重なっても最新の状態を描くため)。
    pub async fn screenshot(&self) {
        if self.capturing.swap(true, Ordering::AcqRel) {
            self.shoot_again.store(true, Ordering::Release);
            return;
        }
        loop {
            self.shoot_again.store(false, Ordering::Release);
            if let Some(s) = self.cur_session() {
                if let Some(jpeg) = self.capture_jpeg(&s).await {
                    // 撮っている間にタブが切り替わっていたら描かない(撮り直す)
                    if self.cur_session().as_deref() == Some(s.as_str()) {
                        let _ = self.present(&jpeg);
                    }
                }
            }
            if !self.shoot_again.swap(false, Ordering::AcqRel) {
                break;
            }
        }
        self.capturing.store(false, Ordering::Release);
        // 解放直前に来た要求を取りこぼさない
        if self.shoot_again.load(Ordering::Acquire) && !self.capturing.load(Ordering::Acquire) {
            Box::pin(self.screenshot()).await;
        }
    }

    async fn on_screencast_frame(&self, ev: &Event) {
        let ok = {
            let st = self.st.lock().unwrap();
            st.screencast && st.screencast_session.is_some() && st.screencast_session == ev.session
        };
        if !ok || !self.disp.lock().unwrap().window_active {
            return;
        }
        if self.capturing.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(data) = ev.params["data"].as_str() {
            if let Ok(jpeg) = base64::engine::general_purpose::STANDARD.decode(data) {
                let _ = self.present(&jpeg);
            }
        }
        self.capturing.store(false, Ordering::Release);
        let mut st = self.st.lock().unwrap();
        st.frames += 1;
        let el = st.fps_start.elapsed();
        if el >= Duration::from_secs(1) {
            print!("\r{:.1} fps    ", st.frames as f64 / el.as_secs_f64());
            use std::io::Write;
            let _ = std::io::stdout().flush();
            st.frames = 0;
            st.fps_start = Instant::now();
        }
    }

    // ---- スクリーンキャスト(動画モード) ------------------------------------

    pub fn is_screencast(&self) -> bool {
        self.st.lock().unwrap().screencast
    }

    pub fn is_high_speed(&self) -> bool {
        self.st.lock().unwrap().high_speed
    }

    pub async fn start_auto(self: &Arc<Self>) {
        {
            let mut st = self.st.lock().unwrap();
            if st.screencast {
                return;
            }
            st.screencast = true;
            st.frames = 0;
            st.fps_start = Instant::now();
        }
        self.start_screencast().await;
    }

    pub async fn stop_auto(&self) {
        let was = std::mem::replace(&mut self.st.lock().unwrap().screencast, false);
        if was {
            self.stop_screencast().await;
        }
    }

    async fn start_screencast(&self) {
        let Some(session) = self.cur_session() else { return };
        let (w, h) = self.viewport();
        let quality = if self.is_high_speed() { 1 } else { 30 };
        self.st.lock().unwrap().screencast_session = Some(session.clone());
        let _ = self
            .cdp
            .call(
                "Page.startScreencast",
                json!({ "format": "jpeg", "quality": quality, "maxWidth": w, "maxHeight": h }),
                Some(&session),
            )
            .await;
    }

    async fn stop_screencast(&self) {
        let s = self.st.lock().unwrap().screencast_session.take();
        if let Some(s) = s {
            let _ = self.cdp.call("Page.stopScreencast", json!({}), Some(&s)).await;
        }
    }

    async fn restart_screencast(&self) {
        self.stop_screencast().await;
        if self.is_screencast() {
            self.start_screencast().await;
        }
    }

    pub async fn set_high_speed(&self, on: bool) {
        self.st.lock().unwrap().high_speed = on;
        if self.is_screencast() {
            self.restart_screencast().await;
        }
    }

    // ---- ページ操作 -------------------------------------------------------

    /// 式を評価して値を返す(Promise は待つ)。
    pub async fn evaluate(&self, expr: &str) -> Result<Value> {
        let s = self.cur_session_or_err()?;
        self.evaluate_in(&s, expr).await
    }

    async fn evaluate_in(&self, session: &str, expr: &str) -> Result<Value> {
        let r = self
            .cdp
            .call(
                "Runtime.evaluate",
                json!({ "expression": expr, "returnByValue": true, "awaitPromise": true, "userGesture": true }),
                Some(session),
            )
            .await?;
        if let Some(ex) = r.get("exceptionDetails") {
            let msg = ex["exception"]["description"].as_str().or(ex["text"].as_str()).unwrap_or("JS エラー");
            bail!("{msg}");
        }
        Ok(r["result"]["value"].clone())
    }

    /// ファイル選択ダイアログで開かれた <input type=file> にファイルを渡す(change が発火する)。
    pub async fn set_files(&self, session: &str, node: i64, files: &[String]) -> Result<()> {
        self.cdp
            .call("DOM.setFileInputFiles", json!({ "files": files, "backendNodeId": node }), Some(session))
            .await
            .map(|_| ())
    }

    /// ブラウザが次のフレームを描くまで待つ(描かれない場合に備えて上限つき)。
    pub async fn wait_for_render(&self) {
        let fut = self.evaluate("new Promise(r => requestAnimationFrame(() => requestAnimationFrame(() => r(0))))");
        let _ = tokio::time::timeout(Duration::from_millis(120), fut).await;
    }

    async fn after_input(&self) {
        self.wait_for_render().await;
        if !self.is_screencast() {
            self.screenshot().await;
        }
    }

    pub async fn goto(&self, url: &str) -> Result<()> {
        let s = self.cur_session_or_err()?;
        let mut rx = self.cdp.subscribe();
        let r = self.cdp.call("Page.navigate", json!({ "url": url }), Some(&s)).await?;
        if let Some(err) = r["errorText"].as_str() {
            bail!("{url} を開けません: {err}");
        }
        // 同一文書内の移動(#fragment など)は load が来ないので待たない
        if r.get("loaderId").is_some() {
            let wait = async {
                loop {
                    match rx.recv().await {
                        Ok(ev) if ev.method == "Page.loadEventFired" && ev.session.as_deref() == Some(&s) => break,
                        Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    }
                }
            };
            if tokio::time::timeout(Duration::from_secs(30), wait).await.is_err() {
                log("読み込みが 30 秒で終わりませんでした");
            }
        }
        self.after_input().await;
        Ok(())
    }

    async fn history_go(&self, delta: i64) -> Result<()> {
        let s = self.cur_session_or_err()?;
        let h = self.cdp.call("Page.getNavigationHistory", json!({}), Some(&s)).await?;
        let idx = h["currentIndex"].as_i64().unwrap_or(0) + delta;
        let Some(entry) = h["entries"].as_array().and_then(|e| e.get(idx.max(0) as usize)).filter(|_| idx >= 0) else {
            return Ok(());
        };
        self.cdp.call("Page.navigateToHistoryEntry", json!({ "entryId": entry["id"] }), Some(&s)).await?;
        Ok(()) // 描画は load イベントで
    }

    pub async fn go_back(&self) -> Result<()> {
        self.history_go(-1).await
    }

    pub async fn go_forward(&self) -> Result<()> {
        self.history_go(1).await
    }

    pub async fn reload(&self) -> Result<()> {
        let s = self.cur_session_or_err()?;
        self.cdp.call("Page.reload", json!({}), Some(&s)).await?;
        Ok(())
    }

    pub async fn current_url(&self) -> String {
        if let Ok(Value::String(u)) = self.evaluate("location.href").await {
            return u;
        }
        let t = self.tabs.lock().unwrap();
        t.list.get(t.cur).map(|t| t.url.clone()).unwrap_or_default()
    }

    // ---- マウス・キーボード --------------------------------------------------

    async fn mouse(&self, kind: &str, x: i32, y: i32, count: u32) -> Result<()> {
        let s = self.cur_session_or_err()?;
        let mut p = json!({ "type": kind, "x": x, "y": y });
        if kind != "mouseMoved" {
            p["button"] = json!("left");
            p["clickCount"] = json!(count);
        }
        self.cdp.call("Input.dispatchMouseEvent", p, Some(&s)).await.map(|_| ())
    }

    pub fn mouse_pos(&self) -> (i32, i32) {
        self.st.lock().unwrap().mouse
    }

    pub async fn move_to(&self, x: i32, y: i32) -> Result<()> {
        self.st.lock().unwrap().mouse = (x, y);
        self.mouse("mouseMoved", x, y, 0).await?;
        self.screenshot().await;
        Ok(())
    }

    /// カーソルを動かす。撮り直さずに直前の絵へカーソルを描き直し、
    /// 動きが止まってから 1 回だけ撮り直す(ホバー表示などを反映するため)。
    /// 長押しのキーリピートでも重くならないようにしている。
    pub async fn move_by(self: &Arc<Self>, dx: i32, dy: i32) -> Result<()> {
        let (w, h) = self.viewport();
        let (x, y) = {
            let mut st = self.st.lock().unwrap();
            st.mouse.0 = (st.mouse.0 + dx).clamp(0, w as i32);
            st.mouse.1 = (st.mouse.1 + dy).clamp(0, h as i32);
            st.mouse
        };
        self.redraw_overlay();
        self.mouse("mouseMoved", x, y, 0).await?;
        if !self.is_screencast() {
            let generation = self.move_gen.fetch_add(1, Ordering::AcqRel) + 1;
            let me = self.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(250)).await;
                if me.move_gen.load(Ordering::Acquire) == generation {
                    me.screenshot().await;
                }
            });
        }
        Ok(())
    }

    /// クリック入力だけを送る(描画は呼び出し側で)。
    pub async fn click_fast(&self, x: i32, y: i32) -> Result<()> {
        self.st.lock().unwrap().mouse = (x, y);
        self.mouse("mouseMoved", x, y, 0).await?;
        self.mouse("mousePressed", x, y, 1).await?;
        self.mouse("mouseReleased", x, y, 1).await
    }

    pub async fn click(&self, x: i32, y: i32) -> Result<()> {
        self.click_fast(x, y).await?;
        self.after_input().await;
        Ok(())
    }

    pub async fn click_here(&self) -> Result<()> {
        let (x, y) = self.mouse_pos();
        self.click(x, y).await
    }

    pub async fn dblclick_here(&self) -> Result<()> {
        let (x, y) = self.mouse_pos();
        self.mouse("mousePressed", x, y, 1).await?;
        self.mouse("mouseReleased", x, y, 1).await?;
        self.mouse("mousePressed", x, y, 2).await?;
        self.mouse("mouseReleased", x, y, 2).await?;
        self.after_input().await;
        Ok(())
    }

    /// タッチ操作後の描画反映。
    pub async fn render_touch_result(&self) {
        self.after_input().await;
    }

    pub async fn scroll(self: &Arc<Self>, dx: f64, dy: f64) -> Result<()> {
        let s = self.cur_session_or_err()?;
        let (x, y) = self.mouse_pos();
        self.cdp
            .call(
                "Input.dispatchMouseEvent",
                json!({ "type": "mouseWheel", "x": x, "y": y, "deltaX": dx, "deltaY": dy }),
                Some(&s),
            )
            .await?;
        // captureScreenshot は撮る前に描き直させるので、スクロールだけなら描画を待たなくてよい
        // (ホイールの処理が終わってから応答が返り、スムーズスクロールも切ってある)
        if !self.is_screencast() {
            self.screenshot().await;
            // スクロールで見えてから読み込まれる画像などは、止まってから 1 回だけ撮り直して拾う
            let generation = self.scroll_gen.fetch_add(1, Ordering::AcqRel) + 1;
            let me = self.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(500)).await;
                if me.scroll_gen.load(Ordering::Acquire) == generation && !me.is_screencast() {
                    me.screenshot().await;
                }
            });
        }
        Ok(())
    }

    pub async fn scroll_to_top(&self) -> Result<()> {
        self.evaluate("window.scrollTo(0, 0)").await?;
        self.after_input().await;
        Ok(())
    }

    pub async fn scroll_to_bottom(&self) -> Result<()> {
        self.evaluate("window.scrollTo(0, document.body.scrollHeight)").await?;
        self.after_input().await;
        Ok(())
    }

    async fn key_event(&self, session: &str, kind: &str, def: &crate::keys::KeyDef) -> Result<()> {
        let mut p = json!({
            "type": kind,
            "key": def.key,
            "code": def.code,
            "windowsVirtualKeyCode": def.vk,
            "nativeVirtualKeyCode": def.vk,
        });
        if kind == "keyDown" && !def.text.is_empty() {
            p["text"] = json!(def.text);
            p["unmodifiedText"] = json!(def.text);
        } else if kind == "keyDown" {
            p["type"] = json!("rawKeyDown");
        }
        self.cdp.call("Input.dispatchKeyEvent", p, Some(session)).await.map(|_| ())
    }

    /// キーを 1 回押す("Enter", "ArrowDown" など)。
    pub async fn press(&self, key: &str) -> Result<()> {
        let s = self.cur_session_or_err()?;
        let def = crate::keys::named(key).ok_or_else(|| anyhow!("未知のキー: {key}"))?;
        self.key_event(&s, "keyDown", &def).await?;
        self.key_event(&s, "keyUp", &def).await?;
        self.after_input().await;
        Ok(())
    }

    pub async fn delete(&self, count: usize) -> Result<()> {
        let s = self.cur_session_or_err()?;
        let def = crate::keys::named("Backspace").unwrap();
        for _ in 0..count {
            self.key_event(&s, "keyDown", &def).await?;
            self.key_event(&s, "keyUp", &def).await?;
        }
        self.after_input().await;
        Ok(())
    }

    /// 文字列を入力する。ASCII はキー入力として、それ以外はテキスト挿入で送る。
    pub async fn type_text(&self, text: &str) -> Result<()> {
        let s = self.cur_session_or_err()?;
        let mut pending = String::new();
        for c in text.chars() {
            match crate::keys::for_char(c) {
                Some(def) => {
                    if !pending.is_empty() {
                        let t = std::mem::take(&mut pending);
                        self.cdp.call("Input.insertText", json!({ "text": t }), Some(&s)).await?;
                    }
                    self.key_event(&s, "keyDown", &def).await?;
                    self.key_event(&s, "keyUp", &def).await?;
                }
                None => pending.push(c),
            }
        }
        if !pending.is_empty() {
            self.cdp.call("Input.insertText", json!({ "text": pending }), Some(&s)).await?;
        }
        self.after_input().await;
        Ok(())
    }

    // ---- ズーム・カーソル ------------------------------------------------------

    async fn apply_zoom(&self) -> Result<()> {
        let z = self.st.lock().unwrap().zoom;
        self.evaluate(&format!("document.documentElement.style.zoom = '{z}'")).await.map(|_| ())
    }

    pub async fn zoom_by(&self, delta: f64) -> Result<()> {
        {
            let mut st = self.st.lock().unwrap();
            st.zoom = ((st.zoom + delta) * 10.0).round() / 10.0;
            st.zoom = st.zoom.clamp(0.1, 5.0);
        }
        self.apply_zoom().await
    }

    pub async fn zoom_reset(&self) -> Result<()> {
        self.st.lock().unwrap().zoom = 1.0;
        self.apply_zoom().await
    }

    /// ピンチズーム: 現在の倍率に scale を掛ける(0.1〜5.0)。
    pub async fn pinch_zoom_by(&self, scale: f64) -> Result<()> {
        {
            let mut st = self.st.lock().unwrap();
            st.zoom = (st.zoom * scale).clamp(0.1, 5.0);
        }
        self.apply_zoom().await
    }

    pub fn zoom(&self) -> f64 {
        self.st.lock().unwrap().zoom
    }

    pub fn set_cursor(&self, on: bool) {
        self.st.lock().unwrap().show_cursor = on;
    }

    // ---- ヒント ---------------------------------------------------------------

    pub async fn show_hints(&self) -> Result<HashMap<String, Hint>> {
        let v = self.evaluate(HINTS_JS).await?;
        let mut map = HashMap::new();
        for h in v.as_array().into_iter().flatten() {
            let key = h["key"].as_str().unwrap_or_default().to_string();
            map.insert(
                key,
                Hint {
                    x: h["x"].as_f64().unwrap_or(0.0),
                    y: h["y"].as_f64().unwrap_or(0.0),
                    href: h["href"].as_str().map(str::to_string),
                },
            );
        }
        Ok(map)
    }

    pub async fn clear_hints(&self) {
        let _ = self.evaluate("document.querySelectorAll('.__hint__').forEach(el => el.remove())").await;
    }

    /// ページが水平方向の端まで来ているか (左端, 右端)。
    pub async fn hscroll_state(&self) -> (bool, bool) {
        let js = "(() => { const el = document.scrollingElement || document.documentElement; \
                  const x = window.scrollX || el.scrollLeft || 0; \
                  const max = Math.max(0, el.scrollWidth - el.clientWidth); \
                  return [x <= 2, x >= max - 2]; })()";
        match self.evaluate(js).await {
            Ok(v) => (v[0].as_bool().unwrap_or(true), v[1].as_bool().unwrap_or(true)),
            Err(_) => (true, true),
        }
    }

    // ---- 音声・クッキー・クリップボード ------------------------------------------

    pub async fn mute_all(&self) {
        let js = "document.querySelectorAll('video, audio').forEach(el => { el.muted = true; if (!el.paused) el.pause(); })";
        for s in self.all_sessions() {
            let _ = self.evaluate_in(&s, js).await;
        }
    }

    pub async fn unmute_all(&self) {
        let js = "document.querySelectorAll('video, audio').forEach(el => { el.muted = false; })";
        for s in self.all_sessions() {
            let _ = self.evaluate_in(&s, js).await;
        }
    }

    /// 全クッキーを Netscape 形式で書き出す(yt-dlp / mpv に渡す)。
    pub async fn export_cookies(&self, path: &str) -> Result<()> {
        let r = self.cdp.call("Storage.getCookies", json!({}), None).await?;
        let mut out = String::from("# Netscape HTTP Cookie File\n");
        for c in r["cookies"].as_array().into_iter().flatten() {
            let domain = c["domain"].as_str().unwrap_or_default();
            let flag = if domain.starts_with('.') { "TRUE" } else { "FALSE" };
            let secure = if c["secure"].as_bool().unwrap_or(false) { "TRUE" } else { "FALSE" };
            let expires = c["expires"].as_f64().filter(|e| *e >= 0.0).unwrap_or(0.0) as i64;
            out.push_str(&format!(
                "{domain}\t{flag}\t{}\t{secure}\t{expires}\t{}\t{}\n",
                c["path"].as_str().unwrap_or("/"),
                c["name"].as_str().unwrap_or_default(),
                c["value"].as_str().unwrap_or_default()
            ));
        }
        std::fs::write(path, out)?;
        Ok(())
    }

    pub async fn read_clipboard(&self) -> String {
        match self.evaluate("navigator.clipboard.readText()").await {
            Ok(Value::String(s)) => s,
            _ => String::new(),
        }
    }

    // ---- mpv の映像 ----------------------------------------------------------

    pub fn toggle_pip(&self) -> bool {
        let (w, h) = self.viewport();
        let mut st = self.st.lock().unwrap();
        st.pip_mode = !st.pip_mode;
        if st.pip_mode {
            let _ = std::fs::create_dir_all(&self.pip_dir);
            st.pip_size = (w / 2, h / 2);
        }
        st.pip_mode
    }

    pub fn is_pip(&self) -> bool {
        self.st.lock().unwrap().pip_mode
    }

    pub fn cleanup_pip_dir(&self) {
        if let Ok(rd) = std::fs::read_dir(&self.pip_dir) {
            for e in rd.flatten() {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }

    /// ペイン全体に mpv の映像を描くモードを始める(ブラウザの絵は止める)。
    pub fn start_pane_video(&self) {
        let _ = std::fs::create_dir_all(&self.pip_dir);
        self.cleanup_pip_dir();
        self.disp.lock().unwrap().video_overlay = true;
    }

    pub fn stop_pane_video(&self) {
        self.disp.lock().unwrap().video_overlay = false;
        self.cleanup_pip_dir();
    }

    /// mpv (--vo=image) の最新フレームを取り出す(古いものは消す)。
    fn latest_mpv_frame(&self) -> Option<PathBuf> {
        let mut files: Vec<PathBuf> = std::fs::read_dir(&self.pip_dir)
            .ok()?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "jpg"))
            .collect();
        files.sort();
        let latest = files.pop()?;
        for f in files {
            let _ = std::fs::remove_file(f);
        }
        Some(latest)
    }

    fn write_mpv_frame(&self, x: u32, y: u32, w: u32, h: u32, fit: Fit) {
        if w == 0 || h == 0 || !self.disp.lock().unwrap().visible {
            return;
        }
        if self.mpv_busy.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(path) = self.latest_mpv_frame() {
            if let Ok(bytes) = std::fs::read(&path) {
                if let Ok(f) = render::decode_jpeg(&bytes) {
                    let f = render::resize(f, w, h, fit);
                    self.disp.lock().unwrap().blit_at(f, x, y);
                }
            }
        }
        self.mpv_busy.store(false, Ordering::Release);
    }

    /// PiP のフレームを描画領域の右下 1/4 に描く。
    pub fn write_pip_frame(&self) {
        let (pw, ph) = self.st.lock().unwrap().pip_size;
        let (rx, ry, w, h) = {
            let d = self.disp.lock().unwrap();
            (d.x, d.y, d.w, d.h)
        };
        self.write_mpv_frame(rx + w - pw, ry + h - ph, pw, ph, Fit::Fill);
    }

    /// ペイン全体再生のフレームを描画領域いっぱいに描く。
    pub fn write_pane_video_frame(&self) {
        let (rx, ry, w, h) = {
            let d = self.disp.lock().unwrap();
            (d.x, d.y, d.w, d.h)
        };
        self.write_mpv_frame(rx, ry, w, h, Fit::Contain);
    }

    // ---- tmux ペイン追従・fb-server ----------------------------------------------

    /// tmux ペイン追跡を有効にする。quiet=true ならターミナルへ出力しない。
    pub fn enable_tmux_track(self: &Arc<Self>, quiet: bool) {
        let (info, cell, rotation) = {
            let d = self.disp.lock().unwrap();
            let info = match &d.drmterm {
                Some(ov) => Some((ov.info.width, ov.info.height, ov.info.stride)),
                None => tmux::read_fb_info(),
            };
            (info, d.drmterm.as_ref().map(|o| (o.info.cell_w, o.info.cell_h)), d.rotation)
        };
        let Some((fw, fh, stride)) = info else {
            if !quiet {
                eprintln!("フレームバッファ情報を読み取れませんでした。");
            }
            return;
        };
        {
            let mut d = self.disp.lock().unwrap();
            d.set_phys(fw, fh, stride);
            match tmux::pane_region(fw, fh, None, cell, rotation) {
                Some(r) => {
                    d.set_region(r.x, r.y, r.w, r.h, Some(stride));
                    d.window_active = r.active;
                    d.sync_drmterm_rect();
                    if !quiet {
                        eprintln!("tmux ペイン: ({},{}) {}x{}px", r.x, r.y, r.w, r.h);
                    }
                }
                None => {
                    if !quiet {
                        eprintln!("tmux ペイン情報を取得できませんでした。フルスクリーンで起動します。");
                    }
                    if rotation % 2 == 1 {
                        d.set_region(0, 0, fh, fw, Some(stride));
                    }
                }
            }
        }
        *self.track.lock().unwrap() = Some(Track { fb_w: fw, fb_h: fh, stride, cell });
        let me = self.clone();
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(Duration::from_millis(500));
            loop {
                iv.tick().await;
                me.check_tmux_pane().await;
            }
        });
    }

    async fn check_tmux_pane(self: &Arc<Self>) {
        let Some((fw, fh, stride, cell)) =
            self.track.lock().unwrap().as_ref().map(|t| (t.fb_w, t.fb_h, t.stride, t.cell))
        else {
            return;
        };
        let (rotation, tty) = {
            let d = self.disp.lock().unwrap();
            (d.rotation, d.drmterm.as_ref().map(|o| o.info.tty.clone()))
        };
        let probe = tokio::task::spawn_blocking(move || {
            let reg = tmux::pane_region(fw, fh, None, cell, rotation)?;
            // drmterm 配下では、drmterm のクライアントが今もこのセッションを映しているかも見る
            let on_drmterm = match &tty {
                None => true,
                Some(t) if t == "-" => true,
                Some(t) => crate::drmterm::shows_session(t, &reg.session),
            };
            Some((reg, on_drmterm))
        })
        .await;
        let Ok(Some((reg, on_drmterm))) = probe else { return };

        let visible = self.disp.lock().unwrap().visible;
        let active = reg.active && on_drmterm && visible;
        if !active {
            let was = std::mem::replace(&mut self.disp.lock().unwrap().window_active, false);
            if was {
                self.disp.lock().unwrap().sync_drmterm_rect();
                if self.is_screencast() {
                    self.stop_screencast().await;
                }
            }
            return;
        }
        let was_active = std::mem::replace(&mut self.disp.lock().unwrap().window_active, true);
        if !was_active {
            {
                let mut d = self.disp.lock().unwrap();
                d.sync_drmterm_rect();
                // ウィンドウ復帰時に即座に再描画
                d.force_redraw();
            }
            if self.is_screencast() {
                self.start_screencast().await;
            }
        }

        let (changed, old) = {
            let d = self.disp.lock().unwrap();
            let old = (d.x, d.y, d.w, d.h);
            (old != (reg.x, reg.y, reg.w, reg.h), old)
        };
        if changed {
            {
                let mut d = self.disp.lock().unwrap();
                d.set_region(reg.x, reg.y, reg.w, reg.h, Some(stride));
                d.sync_drmterm_rect();
                // 縮小した場合は旧領域の残像を消す
                if reg.w < old.2 || reg.h < old.3 {
                    d.clear_region(old.0, old.1, old.2, old.3);
                }
            }
            for s in self.all_sessions() {
                let _ = self.set_metrics(&s, reg.w, reg.h).await;
            }
            if self.is_screencast() {
                self.restart_screencast().await;
            } else {
                self.screenshot().await;
            }
        } else if self.disp.lock().unwrap().sentinel_changed() {
            // fbterm などによる上書きを検出したら描き直す
            self.disp.lock().unwrap().force_redraw();
        }
    }

    /// fb-server からの通知を反映する。
    pub fn on_fb_visibility(&self, visible: bool, reason: Option<&str>, clip: Vec<Rect>) {
        let refresh = {
            let mut d = self.disp.lock().unwrap();
            let r = d.set_visible(visible, reason);
            d.set_clip(clip);
            r
        };
        if refresh {
            tmux::refresh_clients();
        }
    }

    /// 現在のページを別の領域(論理座標)へ即時に描く(新ペインへ移すとき用)。
    pub async fn capture_to_region(&self, x: u32, y: u32, w: u32, h: u32) {
        let Some(s) = self.cur_session() else { return };
        let Some(jpeg) = self.capture_jpeg(&s).await else { return };
        let Ok(f) = render::decode_jpeg(&jpeg) else { return };
        let f = render::resize(f, w, h, Fit::Fill);
        self.disp.lock().unwrap().blit_at(f, x, y);
    }

    pub async fn close(&self) {
        self.stop_auto().await;
        let _ = tokio::time::timeout(Duration::from_secs(3), self.cdp.call("Browser.close", json!({}), None)).await;
        self.disp.lock().unwrap().close();
    }
}

/// クリック可能な要素に 2 文字のラベルを出し、その座標を返す。
const HINTS_JS: &str = r#"(() => {
  document.querySelectorAll('.__hint__').forEach(el => el.remove());
  const zoom = parseFloat(document.documentElement.style.zoom) || 1;
  const chars = 'asdjklghqwertuiopzxcvbnm'.split('');
  const keys = [];
  for (const a of chars) for (const b of chars) keys.push(a + b);
  const clickable = document.querySelectorAll('a, button, input, select, textarea, [onclick], [role="button"], [role="link"]');
  const hints = [];
  let i = 0;
  clickable.forEach(el => {
    if (i >= keys.length) return;
    const r = el.getBoundingClientRect();
    if (r.width === 0 || r.height === 0) return;
    if (r.top < 0 || r.top > window.innerHeight) return;
    if (r.left < 0 || r.left > window.innerWidth) return;
    const key = keys[i++];
    const hint = document.createElement('div');
    hint.className = '__hint__';
    hint.textContent = key.toUpperCase();
    hint.style.cssText = `position: fixed; left: ${r.left / zoom}px; top: ${r.top / zoom}px;
      background: #ffcc00; color: #000; font-size: 12px; font-weight: bold; padding: 2px 5px;
      border-radius: 3px; z-index: 999999; font-family: monospace; box-shadow: 0 1px 3px rgba(0,0,0,0.5);`;
    document.body.appendChild(hint);
    const a = el.closest('a');
    hints.push({ key, x: r.left + r.width / 2, y: r.top + r.height / 2, href: a ? a.href : null });
  });
  return hints;
})()"#;

/// <select> をクリックしたとき、標準のポップアップの代わりにページ内へ選択肢のメニューを出す。
/// 選ぶと selectedIndex を変えて input / change を発火する(React などのフォームにも届く)。
const SELECT_JS: &str = r#"(() => {
  if (window.__fbSelect) return;
  window.__fbSelect = true;
  let menu = null;
  const close = () => { if (menu) { menu.remove(); menu = null; } };
  const open = s => {
    close();
    const zoom = parseFloat(document.documentElement.style.zoom) || 1;
    const b = s.getBoundingClientRect();
    const r = { left: b.left / zoom, right: b.right / zoom, top: b.top / zoom, bottom: b.bottom / zoom, width: b.width / zoom };
    menu = document.createElement('div');
    const vh = innerHeight / zoom;
    const below = vh - r.bottom, above = r.top;
    const max = Math.max(below, above) - 8;
    menu.style.cssText = `position:fixed;z-index:2147483647;left:${r.left}px;min-width:${r.width}px;
      max-height:${max}px;overflow-y:auto;background:#fff;color:#000;border:1px solid #888;
      box-shadow:0 2px 8px rgba(0,0,0,.4);font:14px sans-serif;`;
    menu.style[below >= above ? 'top' : 'bottom'] = below >= above ? `${r.bottom}px` : `${vh - r.top}px`;
    Array.from(s.options).forEach((o, i) => {
      const item = document.createElement('div');
      item.textContent = o.label || o.text;
      const sel = i === s.selectedIndex;
      item.style.cssText = `padding:4px 8px;white-space:pre;cursor:default;
        ${o.disabled ? 'color:#999;' : ''}${sel ? 'background:#1a73e8;color:#fff;' : ''}`;
      if (!o.disabled) {
        item.addEventListener('mouseenter', () => { if (i !== s.selectedIndex) item.style.background = '#e8f0fe'; });
        item.addEventListener('mouseleave', () => { if (i !== s.selectedIndex) item.style.background = ''; });
        item.addEventListener('mousedown', e => {
          e.preventDefault();
          e.stopPropagation();
          close();
          if (s.selectedIndex !== i) {
            s.selectedIndex = i;
            s.dispatchEvent(new Event('input', { bubbles: true }));
            s.dispatchEvent(new Event('change', { bubbles: true }));
          }
        }, true);
      }
      menu.appendChild(item);
      if (sel) setTimeout(() => item.scrollIntoView({ block: 'nearest' }));
    });
    document.documentElement.appendChild(menu);
  };
  addEventListener('mousedown', e => {
    if (e.button !== 0) return;
    if (menu && menu.contains(e.target)) return;
    close();
    const s = e.target.closest && e.target.closest('select');
    if (!s || s.multiple || s.size > 1 || s.disabled) return;
    e.preventDefault();
    s.focus();
    open(s);
  }, true);
  addEventListener('keydown', e => { if (menu && e.key === 'Escape') { e.preventDefault(); close(); } }, true);
  addEventListener('scroll', e => { if (menu && !menu.contains(e.target)) close(); }, true);
  addEventListener('resize', close);
})();"#;
