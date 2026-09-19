//! Chrome DevTools Protocol の薄いクライアント。
//!
//! ブラウザ全体の WebSocket 1 本に、各タブを flatten モードのセッションとして
//! 相乗りさせる(メッセージの `sessionId` でタブを区別する)。
//! 応答は id で待ち合わせ、イベントは broadcast で購読者全員へ配る。

use anyhow::{anyhow, bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;

/// 応答を待つ上限。ページ読み込みなど長いものは呼び出し側で別途待つ。
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// CDP のイベント 1 件。
#[derive(Debug)]
pub struct Event {
    pub method: String,
    pub params: Value,
    pub session: Option<String>,
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>;

#[derive(Clone)]
pub struct Cdp {
    tx: mpsc::UnboundedSender<Message>,
    next_id: Arc<AtomicU64>,
    pending: Pending,
    events: broadcast::Sender<Arc<Event>>,
}

impl Cdp {
    /// ブラウザの WebSocket エンドポイントへ接続し、受信タスクを起動する。
    pub async fn connect(ws_url: &str) -> Result<Cdp> {
        let mut cfg = WebSocketConfig::default();
        cfg.max_message_size = Some(256 << 20);
        cfg.max_frame_size = Some(256 << 20);
        let (ws, _) = tokio_tungstenite::connect_async_with_config(ws_url, Some(cfg), false)
            .await
            .with_context(|| format!("DevTools に接続できません: {ws_url}"))?;
        let (mut sink, mut stream) = ws.split();
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (events, _) = broadcast::channel(1024);

        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if sink.send(msg).await.is_err() {
                    break;
                }
            }
        });

        let pend = pending.clone();
        let ev_tx = events.clone();
        tokio::spawn(async move {
            while let Some(Ok(msg)) = stream.next().await {
                let text = match msg {
                    Message::Text(t) => t,
                    Message::Close(_) => break,
                    _ => continue,
                };
                let Ok(mut v) = serde_json::from_str::<Value>(&text) else { continue };
                if std::env::var_os("FBROWSE_CDP_DEBUG").is_some() {
                    eprintln!("CDP< {} {}", v.get("id").map(|x| x.to_string()).unwrap_or_default(), v.get("method").and_then(Value::as_str).unwrap_or(""));
                }
                if let Some(id) = v.get("id").and_then(Value::as_u64) {
                    let Some(waiter) = pend.lock().unwrap().remove(&id) else { continue };
                    let res = if let Some(err) = v.get("error") {
                        Err(anyhow!(
                            "{}",
                            err.get("message").and_then(Value::as_str).unwrap_or("CDP error")
                        ))
                    } else {
                        Ok(v.get_mut("result").map(Value::take).unwrap_or(Value::Null))
                    };
                    let _ = waiter.send(res);
                } else if let Some(method) = v.get("method").and_then(Value::as_str) {
                    let ev = Event {
                        method: method.to_string(),
                        session: v.get("sessionId").and_then(Value::as_str).map(str::to_string),
                        params: v.get_mut("params").map(Value::take).unwrap_or(Value::Null),
                    };
                    let _ = ev_tx.send(Arc::new(ev));
                }
            }
            // 切断: 待っている呼び出しをすべて失敗させる
            for (_, w) in pend.lock().unwrap().drain() {
                let _ = w.send(Err(anyhow!("DevTools との接続が切れました")));
            }
        });

        Ok(Cdp { tx, next_id: Arc::new(AtomicU64::new(1)), pending, events })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Event>> {
        self.events.subscribe()
    }

    /// コマンドを送って応答を待つ。`session` が None ならブラウザ宛て。
    pub async fn call(&self, method: &str, params: Value, session: Option<&str>) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut msg = json!({ "id": id, "method": method, "params": params });
        if let Some(s) = session {
            msg["sessionId"] = Value::String(s.to_string());
        }
        let (tx, rx) = oneshot::channel();
        if std::env::var_os("FBROWSE_CDP_DEBUG").is_some() {
            eprintln!("CDP> {id} {method} {:?}", session);
        }
        self.pending.lock().unwrap().insert(id, tx);
        if self.tx.send(Message::Text(msg.to_string().into())).is_err() {
            self.pending.lock().unwrap().remove(&id);
            bail!("DevTools との接続が切れました");
        }
        match tokio::time::timeout(CALL_TIMEOUT, rx).await {
            Ok(Ok(res)) => res.with_context(|| method.to_string()),
            Ok(Err(_)) => bail!("{method}: 応答がありません"),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                bail!("{method}: タイムアウト")
            }
        }
    }

    /// 応答を待たずに送る(screencastFrameAck など)。
    pub fn send(&self, method: &str, params: Value, session: Option<&str>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut msg = json!({ "id": id, "method": method, "params": params });
        if let Some(s) = session {
            msg["sessionId"] = Value::String(s.to_string());
        }
        let _ = self.tx.send(Message::Text(msg.to_string().into()));
    }
}
