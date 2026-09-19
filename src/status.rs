//! 画面左下に重ねるステータス行。
//!
//! ブラウザの絵がターミナルを覆っているとき(drmterm のペインや --auto)は、
//! ターミナルに出す「-- INSERT --」や操作ログが見えない。そこで入力中の
//! プロンプトと直近のメッセージをフレームにも描く。
//! プロンプト(入力モード中ずっと出す)がメッセージ(数秒で消える)より優先。

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// メッセージを出しておく時間
const MSG_TTL: Duration = Duration::from_secs(4);

struct State {
    prompt: Option<String>,
    msg: Option<(String, Instant)>,
    generation: u64,
}

static STATE: Mutex<State> = Mutex::new(State { prompt: None, msg: None, generation: 0 });
static REDRAW: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

/// 表示内容が変わったときに呼ぶ再描画関数を登録する。
pub fn set_redraw(f: impl Fn() + Send + Sync + 'static) {
    let _ = REDRAW.set(Box::new(f));
}

fn redraw() {
    if let Some(f) = REDRAW.get() {
        f();
    }
}

/// いま描くべき 1 行(無ければ None)。
pub fn current() -> Option<String> {
    let s = STATE.lock().unwrap();
    if let Some(p) = &s.prompt {
        return Some(p.clone());
    }
    s.msg.as_ref().filter(|(_, t)| t.elapsed() < MSG_TTL).map(|(m, _)| m.clone())
}

/// 入力モードのプロンプトを設定する(None で消す)。
pub fn set_prompt(p: Option<String>) {
    {
        let mut s = STATE.lock().unwrap();
        if s.prompt == p {
            return;
        }
        s.prompt = p;
    }
    redraw();
}

/// 一定時間だけ出すメッセージ。
pub fn message(text: &str) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    let generation = {
        let mut s = STATE.lock().unwrap();
        s.msg = Some((text.to_string(), Instant::now()));
        s.generation += 1;
        s.generation
    };
    redraw();
    // 時間が来たら消す(その間に新しいメッセージが来ていればそちらに任せる)
    std::thread::spawn(move || {
        std::thread::sleep(MSG_TTL);
        let expired = {
            let mut s = STATE.lock().unwrap();
            let expired = s.generation == generation;
            if expired {
                s.msg = None;
            }
            expired
        };
        if expired {
            redraw();
        }
    });
}
