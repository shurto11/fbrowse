# fbrowse

ターミナルからキーボードだけで操作するブラウザ。Chromium の画面を
フレームバッファ (`/dev/fb0`) や [drmterm](../../drmterm) のペインへ直接描く。
[ssbrowse](../../ssbrowse) (TypeScript + Playwright) を Rust で作り直したもの。

- X11/Wayland のウィンドウマネージャ不要 (Chromium は自前で立てる Xvfb の上で動く)
- Vim 風のキー操作、リンクヒント、タブ、お気に入り
- tmux ペインへの追従 (`--auto`)、fb-server の重なり調停、画面回転、タッチ操作
- YouTube を mpv で再生 (全画面 / ペイン全体 / PiP、コメント・弾幕)

## 必要なもの

- Rust (ビルド用)
- Chromium: Playwright が入れたもの (`~/.cache/ms-playwright/chromium-*`) を自動で使う。
  無ければ PATH 上の `chromium` / `google-chrome`。`FBROWSE_CHROME` で明示もできる
- Xvfb
- mpv, yt-dlp (YouTube 再生用)
- `/dev/fb0` への書き込み権限 (video グループ)

## ビルドと起動

```bash
cargo build --release
./target/release/fbrowse               # プロンプトでお気に入り名・URL・検索語を入力
./target/release/fbrowse example.com   # すぐ開く (一時プロファイルを使う)
./target/release/fbrowse --auto        # tmux ペインの位置を追いかけて描く
```

URL を省くとプロンプトが出る。`list` でお気に入りの追加・削除・並び替え、
`help` でキー一覧、`quit` で終了。お気に入り名は Tab で補完できる。

drmterm の中で起動すると、自動的にそのペインへ描く (判定の仕組みは ssbrowse と同じ)。
このときタッチ操作と fb-server との連携は無効になる。

## ファイルの置き場所

| 用途 | 場所 |
|------|------|
| Chromium のプロファイル | `~/.local/share/fbrowse/profile` (`--profile` で変更可) |
| お気に入り | `~/.config/fbrowse/favorites.json` |
| ダウンロード | `~/Downloads` |
| mpv のスクリーンショット | `~/Pictures/fbrowse` |

ssbrowse のログイン状態とお気に入りを引き継ぐなら (fbrowse を終了した状態で):

```bash
mkdir -p ~/.local/share/fbrowse/profile ~/.config/fbrowse
# 末尾の "/." で中身をコピーする (profile が既にあっても profile/chrome-data にならない)
cp -r ~/ssd/ssbrowse/chrome-data/. ~/.local/share/fbrowse/profile
cp ~/ssd/ssbrowse/favorites.json ~/.config/fbrowse/favorites.json
```

## キー操作

### ブラウジング

| キー | 動作 |
|------|------|
| `h` `j` `k` `l` | スクロール (左/下/上/右) |
| `gg` / `G` | ページ先頭 / 末尾 |
| `f` | ヒントモード (リンクに 2 文字ラベル。YouTube の動画リンクは mpv で再生) |
| `m` | カーソルモード (hjkl で移動、Space でクリック、d でダブルクリック、m で終了) |
| `i` | 入力モード (Enter で確定して、フォーカス中の欄へ送る) |
| `d` | 直前の入力を削除 |
| `b` / `r` | 戻る / 再読込 |
| `Enter` / `Space` / 矢印 | そのままページへ送る |
| `s` | 撮り直し |
| `+` `-` `0` | ズームイン / アウト / リセット |
| `p` | クリップボード表示 |
| `o` | 現在のページをお気に入りに追加 |
| `c` / `C` | 動画モード (スクリーンキャスト) / 高速動画モード |
| `?` | キー一覧 |
| `Q` | 終了 |

### タブ

| キー | 動作 |
|------|------|
| `t` | 新しいタブ (お気に入り名 / URL / 検索語、Tab で補完) |
| `]` / `[` | 次 / 前のタブ |
| `x` | タブを閉じる |
| `T` | 現在のタブを新しい tmux ペイン (右) の fbrowse へ移す |

### YouTube (mpv)

`M` で開いている動画を、ヒントモードで動画リンクを選ぶとその動画を mpv で再生する。
再生のしかたは自動で選ぶ:

- **全画面**: `--vo=drm` で画面へ直接出す
- **ペイン全体**: drmterm などが DRM を握っていて全画面にできないとき、描画領域いっぱいに描く
- **PiP**: `P` で切り替え。tmux の右ペインで mpv を動かし、映像を描画領域の右下 1/4 に描く
  (その間もブラウザを操作できる)

| キー (再生中) | 動作 |
|------|------|
| `q` | 停止してブラウザに戻る |
| `+` / `-` | 音量 |
| `Space` | 一時停止 / 再開 |
| `h` / `l` | 10 秒戻し / 送り |
| `x` / `z` | 倍速切替 (x1→x4) / 等速 |
| `s` | 字幕切替 |
| `p` | スクリーンショット |
| `c` (`j` / `k`) | コメント表示 (スクロール) |
| `d` | ライブチャットの弾幕 |
| `Ctrl+b` | 次の Shorts (Shorts 再生中) |

### タッチ操作

touch-server に接続し (無ければタッチデバイスを直接読む)、ペインがアクティブなときだけ受け付ける。

| ジェスチャ | 動作 |
|------|------|
| タップ | その位置をクリック |
| 縦 / 横ドラッグ | スクロール |
| 右 / 左スワイプ | 戻る / 進む (横スクロールしきっているときだけ) |
| ピンチ | ズーム |

環境変数: `TOUCH_DEV` (デバイス), `TOUCH_SWIPE_FRAC`, `TOUCH_SCROLL_FRAC`, `TOUCH_LONGPRESS_SEC`,
`TOUCH_ROTATE` (画面回転。無ければ `~/.fbtermrc` の `screen-rotate=`)。

## 他のツールとの連携

- **fb-server**: `ssbrowse` の名前で接続する (`scenes.toml` の層名に合わせている)。
  表示可否と描画禁止矩形 (clip) に従い、tmux セッションが切り替わったら自分の領域を消す
- **touch-server**: 同じく `ssbrowse` として接続する
- **drmterm**: 共有メモリへ書き、ペインの矩形を申告して合成してもらう

## 開発用の環境変数

| 変数 | 用途 |
|------|------|
| `FBROWSE_FB` | 書き込み先を `/dev/fb0` の代わりにこのファイルにする (fb0 と同じサイズ前提) |
| `FBROWSE_CHROME` | 使う Chromium の実行ファイル |
| `FBROWSE_CHROME_FLAGS` | Chromium に足す起動フラグ (空白区切り。例: `--mute-audio`) |
| `FBROWSE_CDP_DEBUG` | CDP の送受信を標準エラーに出す |

## 構成

```
src/
  main.rs      起動・プロンプト・端末
  app.rs       キー操作とタッチ操作 (モード管理)
  browser.rs   タブ・撮影・スクリーンキャスト・入力・tmux 追従
  cdp.rs       Chrome DevTools Protocol クライアント (WebSocket)
  chrome.rs    Chromium と Xvfb の起動
  display.rs   フレームバッファへの書き込み (回転・clip・drmterm)
  render.rs    JPEG デコード・リサイズ・回転・タブバー・カーソル
  mpv.rs       mpv 再生・IPC・コメント・弾幕
  tmux.rs / drmterm.rs / fbserver.rs / touch.rs / favorites.rs / keys.rs
assets/
  mpv-comments.lua     コメント・弾幕の描画 (mpv スクリプト。バイナリに埋め込み)
  mpv-pip-input.conf   PiP 時の mpv キー設定
```
