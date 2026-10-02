//! fbrowse — ターミナルからキーボードで操作するブラウザ。
//!
//! Chromium(Xvfb 上の headful)を CDP で動かし、その画面を /dev/fb0 や
//! drmterm のペインへ直接描く。

mod app;
mod browser;
mod cdp;
mod chrome;
mod display;
mod drmterm;
mod favorites;
mod fbserver;
mod keys;
mod mpv;
mod render;
mod status;
mod tmux;
mod touch;

use anyhow::Result;
use app::{App, AppEvent};
use browser::{log, Controller};
use display::Display;
use favorites::Favorites;
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

struct Args {
    auto: bool,
    url: Option<String>,
    profile: Option<PathBuf>,
}

fn usage() -> ! {
    println!(
        "使い方: fbrowse [--auto] [--profile DIR] [URL]

  --auto          tmux ペインの位置を追いかけて描く
  --profile DIR   Chromium のプロファイル (既定: ~/.local/share/fbrowse/profile)
  URL             起動してすぐ開くページ (指定時は一時プロファイルを使う)

URL を省くとプロンプトが出ます。お気に入り名・URL・検索語を入力してください
(list でお気に入り管理、help でキー一覧)。"
    );
    std::process::exit(0)
}

fn parse_args() -> Args {
    let mut a = Args { auto: false, url: None, profile: None };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--auto" => a.auto = true,
            "--profile" => a.profile = it.next().map(PathBuf::from),
            "-h" | "--help" => usage(),
            s if s.starts_with("--") => {
                eprintln!("不明なオプション: {s}");
                std::process::exit(2);
            }
            "" => {}
            s => a.url = Some(s.to_string()),
        }
    }
    a
}

fn data_dir() -> PathBuf {
    std::env::var("XDG_DATA_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/share"))
        .join("fbrowse")
}

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
}

// ---- 起動前のプロンプト ----------------------------------------------------------

#[derive(rustyline::Helper, rustyline::Hinter, rustyline::Highlighter, rustyline::Validator)]
struct FavCompleter {
    names: Vec<String>,
}

impl rustyline::completion::Completer for FavCompleter {
    type Candidate = String;
    fn complete(&self, line: &str, _pos: usize, _: &rustyline::Context<'_>) -> rustyline::Result<(usize, Vec<String>)> {
        let lower = line.to_lowercase();
        let hits: Vec<String> = self.names.iter().filter(|n| n.starts_with(&lower)).cloned().collect();
        Ok((0, hits))
    }
}

type Editor = rustyline::Editor<FavCompleter, rustyline::history::DefaultHistory>;

fn ask(ed: &mut Editor, prompt: &str) -> Option<String> {
    match ed.readline(prompt) {
        Ok(l) => Some(l),
        Err(rustyline::error::ReadlineError::Interrupted) | Err(rustyline::error::ReadlineError::Eof) => None,
        Err(_) => None,
    }
}

/// お気に入りの追加・削除・並び替え。
fn manage_favorites(ed: &mut Editor, favs: &mut Favorites) {
    loop {
        println!("\n=== お気に入り管理 ===");
        for (i, (name, url)) in favs.iter().enumerate() {
            println!("  {}. {name:<12} {url}", i + 1);
        }
        if favs.is_empty() {
            println!("  (なし)");
        }
        println!("\n  a: 追加  d: 削除  m: 移動  q: 戻る");
        let Some(action) = ask(ed, "> ") else { return };
        match action.trim().to_lowercase().as_str() {
            "a" => {
                let name = ask(ed, "名前: ").unwrap_or_default();
                let url = ask(ed, "URL: ").unwrap_or_default();
                if !name.trim().is_empty() && !url.trim().is_empty() {
                    favs.insert(name.trim().to_lowercase(), url.trim().to_string());
                    favorites::save(favs);
                    println!("追加しました: {} -> {}", name.trim(), url.trim());
                }
            }
            "d" => {
                let n = ask(ed, "削除する番号: ").and_then(|s| s.trim().parse::<usize>().ok());
                match n.filter(|n| *n >= 1 && *n <= favs.len()) {
                    Some(n) => {
                        let (k, _) = favs.shift_remove_index(n - 1).unwrap();
                        favorites::save(favs);
                        println!("削除しました: {k}");
                    }
                    None => println!("無効な番号です"),
                }
            }
            "m" => {
                let from = ask(ed, "移動元の番号: ").and_then(|s| s.trim().parse::<usize>().ok());
                let to = ask(ed, "移動先の番号: ").and_then(|s| s.trim().parse::<usize>().ok());
                let ok = |n: Option<usize>| n.filter(|n| *n >= 1 && *n <= favs.len());
                match (ok(from), ok(to)) {
                    (Some(f), Some(t)) => {
                        favs.move_index(f - 1, t - 1);
                        favorites::save(favs);
                        println!("移動しました: {}", favs.get_index(t - 1).unwrap().0);
                    }
                    _ => println!("無効な番号です"),
                }
            }
            "q" | "" => return,
            _ => {}
        }
    }
}

/// 最初に開くページを尋ねる。終了が選ばれたら None。
fn prompt_first_url(favs: &mut Favorites) -> Option<String> {
    let mut ed: Editor = Editor::new().ok()?;
    loop {
        ed.set_helper(Some(FavCompleter { names: favs.keys().cloned().collect() }));
        let line = ask(&mut ed, "> ")?;
        let input = line.trim();
        let cmd = input.split_whitespace().next().unwrap_or("").to_lowercase();
        match cmd.as_str() {
            "" => {}
            "list" => manage_favorites(&mut ed, favs),
            "quit" | "exit" => return None,
            "help" | "h" => app::print_help(favs),
            "goto" => {
                if let Some(u) = input.split_whitespace().nth(1) {
                    return Some(u.to_string());
                }
            }
            _ => return favorites::resolve(input, favs),
        }
    }
}

// ---- 端末 ----------------------------------------------------------------------

/// 端末を 1 文字ずつ読めるようにする(出力の改行変換は残す)。元の設定を返す。
fn set_raw_mode() -> Option<libc::termios> {
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(0, &mut t) != 0 {
            return None;
        }
        let orig = t;
        t.c_iflag &= !(libc::BRKINT | libc::ICRNL | libc::INPCK | libc::ISTRIP | libc::IXON);
        t.c_cflag |= libc::CS8;
        t.c_lflag &= !(libc::ECHO | libc::ICANON | libc::IEXTEN | libc::ISIG);
        t.c_cc[libc::VMIN] = 1;
        t.c_cc[libc::VTIME] = 0;
        libc::tcsetattr(0, libc::TCSANOW, &t);
        Some(orig)
    }
}

fn restore_mode(t: &Option<libc::termios>) {
    if let Some(t) = t {
        unsafe { libc::tcsetattr(0, libc::TCSANOW, t) };
    }
}

/// 標準入力を読んだ塊ごとに 1 キーとして送る(矢印キー等のエスケープ列も 1 塊で届く)。
fn spawn_key_reader(tx: mpsc::UnboundedSender<AppEvent>) {
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 256];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    if tx.send(AppEvent::Key(buf[..n].to_vec())).is_err() {
                        return;
                    }
                }
            }
        }
    });
}

// ---- main ----------------------------------------------------------------------

fn main() {
    let args = parse_args();
    let mut favs = favorites::load();

    // URL 指定時(T で新ペインへ移したときなど)は一時プロファイルで起動する
    let (profile, temp_profile) = match (&args.url, &args.profile) {
        (_, Some(p)) => (p.clone(), false),
        (Some(_), None) => (PathBuf::from(format!("/dev/shm/fbrowse-profile-{}", std::process::id())), true),
        (None, None) => (data_dir().join("profile"), false),
    };

    let url = match &args.url {
        Some(u) => favorites::resolve(u, &favs).unwrap_or_else(|| u.clone()),
        None => match prompt_first_url(&mut favs) {
            Some(u) => u,
            None => return,
        },
    };

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio runtime");
    let code = match rt.block_on(run(args, url, profile.clone(), favs)) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("\r\nfbrowse: {e:#}");
            1
        }
    };
    if temp_profile {
        let _ = std::fs::remove_dir_all(&profile);
    }
    rt.shutdown_timeout(std::time::Duration::from_millis(500));
    std::process::exit(code);
}

async fn run(args: Args, url: String, profile: PathBuf, favs: Favorites) -> Result<()> {
    let rotation = touch::read_screen_rotate();
    let mut disp = Display::new(rotation);

    // drmterm(外部モニター)の中で起動されたなら、そのペインへ描く
    let on_drmterm = match drmterm::connect() {
        Some(ov) => {
            println!(
                "drmterm 出力: {}x{} (セル {}x{})",
                ov.info.width, ov.info.height, ov.info.cell_w, ov.info.cell_h
            );
            disp.attach_drmterm(ov);
            true
        }
        None => false,
    };
    disp.open_fb();
    if disp.rotation != 0 {
        println!("[rotate] screen-rotate={}: ビューポート {}x{}", disp.rotation, disp.w, disp.h);
    }
    let (full_w, full_h) = disp.logical_full();

    let mut chrome = chrome::launch(&profile, full_w, full_h).await?;
    let cdp = cdp::Cdp::connect(&chrome.ws_url).await?;
    let downloads = home().join("Downloads");
    let pip_dir = PathBuf::from(format!("/dev/shm/fbrowse-pip-{}", std::process::id()));
    let ctrl = Controller::new(cdp, disp, downloads, pip_dir.clone());

    if args.auto {
        ctrl.enable_tmux_track(args.url.is_some());
    }
    ctrl.disp.lock().unwrap().sync_drmterm_rect();

    let (tx, mut rx) = mpsc::unbounded_channel::<AppEvent>();

    // fb-server(/dev/fb0 の重なり調停)とタッチパネルは内蔵画面のものなので、
    // drmterm(外部モニター)出力中は繋がない
    if !on_drmterm {
        let (vtx, mut vrx) = mpsc::unbounded_channel::<fbserver::VisMsg>();
        let c = ctrl.clone();
        let rect = Arc::new(move || Some(c.disp.lock().unwrap().fb_rect()));
        fbserver::spawn(tmux::own_session_id(), rect, vtx);
        let c = ctrl.clone();
        tokio::spawn(async move {
            while let Some(m) = vrx.recv().await {
                c.on_fb_visibility(m.visible, m.reason.as_deref(), m.clip);
            }
        });

        let (ttx, mut trx) = mpsc::unbounded_channel::<touch::Touch>();
        touch::start(ttx);
        let tx2 = tx.clone();
        tokio::spawn(async move {
            while let Some(t) = trx.recv().await {
                if tx2.send(AppEvent::Touch(t)).is_err() {
                    break;
                }
            }
        });
    }

    ctrl.set_app_tx(tx.clone());
    let result = async {
        ctrl.start(&url).await?;
        let mut app = App::new(ctrl.clone(), favs, tx.clone());
        if args.auto && args.url.is_some() {
            ctrl.start_auto().await;
            app.set_video_mode(true);
        }
        let saved = set_raw_mode();
        spawn_key_reader(tx.clone());
        println!("\r\n? でキー一覧、Q で終了");

        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        let mut hup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
        tokio::select! {
            _ = app.run(&mut rx) => {}
            _ = term.recv() => {}
            _ = hup.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
        restore_mode(&saved);
        println!();
        anyhow::Ok(())
    }
    .await;

    if let Err(e) = &result {
        log(&format!("{e:#}"));
    }
    ctrl.close().await;
    chrome.kill().await;
    let _ = std::fs::remove_dir_all(&pip_dir);
    result
}
