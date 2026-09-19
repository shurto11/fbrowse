//! fb-server クライアント。
//!
//! `{"hello":"ssbrowse", "session":"$3", "rect":{...}}` を送って重なり調停に参加し、
//! 届く `{"visible":bool, "reason":..., "clip":[...]}` を呼び出し側へ渡す。
//! 描画領域が動いたら `{"rect":...}` を送り直す(tmux ペイン追従のため)。
//! fb-server 未起動・切断時は 500ms ごとに張り直す。未接続の間は visible のまま。
//!
//! fb-server の scenes.toml が "ssbrowse" という名前で層を定義しているため、
//! その名前で名乗る。

use crate::display::Rect;
use serde::{Deserialize, Serialize};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;

pub const CLIENT_NAME: &str = "ssbrowse";

#[derive(Debug)]
pub struct VisMsg {
    pub visible: bool,
    pub reason: Option<String>,
    pub clip: Vec<Rect>,
}

#[derive(Serialize)]
struct Hello<'a> {
    hello: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    session: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rect: Option<Rect>,
}

#[derive(Serialize)]
struct RectUpdate {
    rect: Option<Rect>,
}

#[derive(Deserialize)]
struct RawVisible {
    visible: bool,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    clip: Vec<Rect>,
}

fn socket_path() -> String {
    if let Ok(p) = std::env::var("FB_SERVER_SOCK") {
        if !p.is_empty() {
            return p;
        }
    }
    match std::env::var("XDG_RUNTIME_DIR") {
        Ok(d) if !d.is_empty() => format!("{d}/fb-server.sock"),
        _ => "/tmp/fb-server.sock".to_string(),
    }
}

/// 接続スレッドを起動する。`rect` は現在の描画領域を返す関数。
pub fn spawn(
    session: Option<String>,
    rect: Arc<dyn Fn() -> Option<Rect> + Send + Sync>,
    tx: UnboundedSender<VisMsg>,
) {
    std::thread::spawn(move || {
        let mut announced = false;
        loop {
            if let Ok(s) = UnixStream::connect(socket_path()) {
                if !announced {
                    eprintln!("[fb-client] fb-server に接続 ({})", socket_path());
                    announced = true;
                }
                let _ = run(s, &session, &*rect, &tx);
            }
            if tx.is_closed() {
                return;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    });
}

fn run(
    stream: UnixStream,
    session: &Option<String>,
    rect: &(dyn Fn() -> Option<Rect> + Send + Sync),
    tx: &UnboundedSender<VisMsg>,
) -> std::io::Result<()> {
    let mut cur = rect();
    let hello = Hello { hello: CLIENT_NAME, session: session.clone(), rect: cur };
    (&stream).write_all(format!("{}\n", serde_json::to_string(&hello).unwrap_or_default()).as_bytes())?;
    stream.set_read_timeout(Some(Duration::from_millis(300)))?;
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut last_check = Instant::now();
    loop {
        if last_check.elapsed() >= Duration::from_millis(300) {
            last_check = Instant::now();
            let now = rect();
            if now != cur {
                cur = now;
                let upd = serde_json::to_string(&RectUpdate { rect: now }).unwrap_or_default();
                (&stream).write_all(format!("{upd}\n").as_bytes())?;
            }
        }
        match (&stream).read(&mut chunk) {
            Ok(0) => return Ok(()),
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = buf.drain(..=pos).collect();
                    if let Ok(v) = serde_json::from_slice::<RawVisible>(&line) {
                        let msg = VisMsg { visible: v.visible, reason: v.reason, clip: v.clip };
                        if tx.send(msg).is_err() {
                            return Ok(());
                        }
                    }
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {}
            Err(e) => return Err(e),
        }
    }
}
