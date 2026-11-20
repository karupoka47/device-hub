# フロントエンドの構造 (crates/device-hub)

```
src/
  main.rs        窓を開く (TitleBar::window_options, 1320x840, 背面時 250ms 間引き)
  app.rs         Shell: 全状態・ルート (Home / Device / Settings)・タブ・サイドバー・キー入力・ドラッグ
  store.rs       Hub / Session: デバイスごとのワーカースレッド、楽観的反映 (apply_local)、futures channel で結果受信
  button_activity.rs  物理ボタンの押下・解放後850msの余韻・最後に押した番号（設定値とは別の一時状態）
  overlay.rs     ポップアップメニュー・モーダル (確認 / ボタン割り当て / キー取り込み / 色 / テキスト)・トースト
  ui.rs          部品: card / row / toggle / slider(_curve) / segmented / select / stepper / tab_bar / button / chip
  glass.rs       Liquid Glass の面 (Panel / Clear / Frost / Sheet) と泡レンズ
  motion.rs      臨界減衰ばね・要素矩形の記録 (track / rect / hovered)。動いている間だけ次フレーム要求
  theme.rs       配色 (ダーク/ライト・アクセント 6 色)・フォント
  wallpaper.rs   オーロラ壁紙の生成 / ユーザー画像
  prefs.rs       %APPDATA%\DeviceHub\prefs.json
  keycode.rs     GPUI キー名 → HID usage
  palette.rs     コマンドパレット (Ctrl+K)
  presets.rs     PC に保存するプリセット (%APPDATA%\DeviceHub\presets\<model>\*.json)
  pages/
    home.rs       デバイス一覧
    device.rs     ヘッダ (名前・接続・プロファイル・同期) + タブ + 読込中/失敗/未対応
    generic.rs    SettingSpec / ActionSpec を section ごとのカードに自動配置
    mouse.rs      DPI ステージ・ボタン割り当て
    keyboard.rs   キーボード図 (共通)・キー割り当て・トリガー (磁気スイッチ / ストロークテスト)
    advanced.rs   高度キー (DKS/MPT/MT/TGL/END/SOCD/RS)
    lighting.rs   照明 (領域・エフェクト・色・キーごとの塗り)
    macros.rs     マクロ (記録・挿入・遅延編集)
    system.rs     デバイス情報・プロファイル・書き出し/読み込み・メンテナンス
    settings.rs   アプリ設定
    test.rs       テスト (クリック/ホイール/チャタリング、キー入力・同時押し)
    calibrate.rs  磁軸キャリブレーション・ウィザード
```

## タブの出し分け (`app::tabs_for`)

| caps | タブ |
|---|---|
| mouse | DPI, ボタン (buttons があれば), 性能 (performance/sensor/click/magnetic/power の設定があれば) |
| keyboard | キー割り当て, トリガー (perf), 高度キー (advanced) |
| lighting / lighting section | ライティング |
| macros | マクロ |
| 常に | 一般 (system / connection / keyboard / 未知の section、情報・プロファイル・バックアップ) |

## データの流れ

UI 操作 → `Shell::apply(Change)` → `Hub::apply` が UI の状態を即書き換え (apply_local) → ワーカーへ送信 →
ワーカーが同じ対象の古い変更を捨てて `DeviceDriver::apply` → 結果 (`Msg::Applied`) → 失敗ならトースト + `read` し直し。
ヘッダの点: 黄 = 書き込み待ちあり / 緑 = 同期済み。

### ボタン割り当ての押下表示

`MouseCaps.button_activity` は「物理入力のindex → 対応する割り当て欄index群」。
空なら非対応。G502は通常側とGシフト側の欄を同じ物理グループに入れる。
ドライバの `HubEvent::ButtonActivity { id, pressed }`（押下中の物理入力indexの全体）
を `Session.button_activity` に取り込み、`pages/mouse.rs` が番号・欄の背景と枠を発光させる。
短いクリックも見えるよう解放後850msでフェードし、最後に押した番号と名前は残す。
番号チップをクリックすると該当欄までリストをスクロールする。

割り当てたキー入力や合成クリックから物理番号を推測しない。
UIはcapsとイベントだけを使い、デバイス名では分岐しない。
通知で設定を再読込せず、非表示・他のタブでは入力通知を契機とした再描画もしない。
画面が見えていれば、別ウィンドウにフォーカスがある状態でも発光を更新する。

## 新しい画面が必要になったら

構造を持つ新機能が契約に追加されたとき (`docs/FRONTEND_TODO.md`) だけ。`pages/` にファイルを足し、
`Tab` と `tabs_for` に 1 行。スカラー設定・操作は generic.rs が自動で描くので画面追加は不要。

## 動作確認

```bash
DEVHUB_DEMO=1 cargo run -p device-hub
```
デモは HTS Plus Pro (無線) / HTX Ultra (有線・磁気ボタン) / SU66 Max / ATK F1 (未対応) の 4 台。

## Web 版より便利にしている機能 (フロントだけで実現)

| 機能 | 場所 | 中身 |
|---|---|---|
| 元に戻す / やり直し | ヘッダの ↶↷、Ctrl+Z / Ctrl+Y | 変更前の `DeviceState` を履歴に積み、差分 (`store::diff_changes`) を書き戻す。0.8 秒以内の連続操作は 1 回分 |
| プリセット | 一般タブ・ヘッダのドロップダウン | 現在の設定を PC に名前付きで保存し、ワンクリックで適用 (差分だけ書く)。オンボードの枠数に縛られない |
| コマンドパレット | Ctrl+K / サイドバーの検索 | 画面・設定・操作・デバイスを名前で探して移動・実行 |
| テスト | テストタブ | クリック回数・間隔・チャタリング疑い、ホイール、移動イベント数、キー入力・同時押し数・二重入力 |
| キャリブレーション・ウィザード | 一般タブの「磁軸キャリブレーション」 | 押下深さを見ながら、底まで押したキーを緑で表示 |
| トリガーのプリセット | トリガータブ | 最速 RT / 競技 RT / 標準 / タイピングを選択キーに一括適用 |
| キー検索 | キー割り当て | 全グループから名前で検索 |
| レイヤーのコピー | キー割り当て | 表示中のレイヤーを別レイヤーへ複製 |
| 進捗表示 | ヘッダ | `HubEvent::Progress` を受けて進捗バー |
| バッテリー低下の通知 | トースト | 15% 以下で 1 回 |
| 設定プロファイル | 自動化 (サイドバー) | デバイス画面での変更を `Change` 列として録画し、任意タイミングで再生 (prefs.json の `profiles`)。「今の状態」でスナップショット記録 (設定/DPI/性能/ライティング/EQ — キー割り当て等の構造系は除外)、「名前」でリネーム (ルール参照も追従) |
| プロセス連動 | 自動化 | `proc_rules` の exe 名がフォアグラウンドの間 `on_fg` プロファイルを適用、外れたら `on_leave`。`procmon.rs` が ~1 秒おきに前面窓を見る (最小化中も動作・実機検証済)。プロセスは「実行中のアプリ」ドロップダウン (可視窓を持つ exe 一覧+タイトル) から選択可、ルールは ON/OFF トグル・編集・実行中チップつき |
| 常時レート計測 | 性能/テストタブ | Raw Input `RIDEV_INPUTSINK` で実測 Hz を更新 (~400ms おき、デバイス画面を開いている間)。**Windows 11 (Moment 3以降) は背面プロセスへの raw input 配信を制限する**ため、窓が背面の間は値が落ちて信用できない — 表示側は `last_window_active` で出し分け、背面中は「OS が入力を制限中」と薄字表示にする (MouseTester と同じ OS 制限)。計測スレッドは MMCSS "Pro Audio" + ABOVE_NORMAL、間隔の中央値→Hz 逆算と p95 ばらつき警告も併記 |

### ポーリング真値の検証 (アプリ外)

WM_INPUT は「OS が届けた分」を数える層なので、デバイスが実際にワイヤに出したレポート数
(真の 8K 検証) を取りたいときは **USBPcap + Wireshark** を使う:

1. USBPcap をインストール (カーネルフィルタドライバ — 管理者権限が要る)。
2. Wireshark で対象ホストコントローラを選択してキャプチャ開始。
3. マウスを動かす → `usb.bInterfaceClass == 3` / interrupt-IN URB の
   `usb.dst` (host) 方向のフレーム間隔を読む。8K なら ~0.125ms 間隔が並ぶ。
4. ドングル経由の場合も同じ — レシーバーがバスに出す分が見える。

ベンダー I/F (ff02 等) の直接 read は代替にならない — 自発レポートはイベント通知のみ
(実測 ~10Hz) で、モーションデータはシステム TLC 専用 (Windows はそちらを読ませない)。

