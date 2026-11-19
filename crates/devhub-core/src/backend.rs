//! バックエンドとの契約 (v2)。
//!
//! フロントエンド (device-hub) は **`HubBackend` / `DeviceDriver` だけ**を知る。
//! HID・プロトコルの詳細はすべて実装側 (devhub-hid) に閉じる。
//!
//! # スレッドと呼ばれ方 (実装者向け)
//!
//! - すべて**同期 API**。UI はデバイスごとの専用ワーカースレッドから呼ぶので、
//!   HID の往復 (数 ms〜数百 ms) でブロックしてよい。UI スレッドからは呼ばれない。
//! - 同じドライバのメソッドが**同時に**呼ばれることは無い (ワーカーが 1 本)。
//!   ただし `HubBackend::scan` は別スレッド (ポーリング) から並行に呼ばれる。
//! - `apply` はスライダーのドラッグ中に連続で来る。UI 側で同じ対象の古い変更は
//!   捨ててから送る (`Change::same_target`) ので、ドライバ側の間引きは不要。
//! - エラーは `HubError`。UI は Display 文字列をそのままトーストに出す。

use crate::model::*;
use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::sync::Arc;

// ------------------------------------------------------------------ エラー

#[derive(Debug)]
pub enum HubError {
    /// デバイス ID が見つからない (抜かれた等)。
    NotFound(DeviceId),
    /// この機能をデバイス/ドライバが持たない。
    Unsupported(&'static str),
    /// プロトコル未実装。
    NotImplemented(&'static str),
    /// 通信エラー。
    Io(String),
    /// 応答が壊れている/ACK が来ない等。
    Protocol(String),
    /// 引数が caps の範囲外など。
    Invalid(String),
    /// デバイスが切断された / 無線がスリープ中。
    Disconnected,
}

impl fmt::Display for HubError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HubError::NotFound(id) => write!(f, "デバイスが見つかりません: {id}"),
            HubError::Unsupported(what) => write!(f, "この機能には対応していません: {what}"),
            HubError::NotImplemented(what) => write!(f, "未実装です: {what}"),
            HubError::Io(e) => write!(f, "通信エラー: {e}"),
            HubError::Protocol(e) => write!(f, "応答エラー: {e}"),
            HubError::Invalid(e) => write!(f, "値が不正です: {e}"),
            HubError::Disconnected => write!(f, "デバイスが切断されました"),
        }
    }
}

impl std::error::Error for HubError {}

pub type HubResult<T> = Result<T, HubError>;

// ------------------------------------------------------------------ イベント

/// バックエンドから UI への非同期通知 (任意)。
/// 実装が能動的に検知できる場合だけ `HubBackend::watch` の callback から呼ぶ。
/// 何も呼ばなくても UI は 2 秒ごとの `scan` と 30 秒ごとの `battery` で動く。
#[derive(Clone, Debug)]
pub enum HubEvent {
    /// Full held-physical-input snapshot; indices refer to MouseCaps::button_activity.
    /// Independent of assigned keys/macros. Empty on release/disconnection.
    ButtonActivity { id: DeviceId, pressed: Vec<usize> },
    /// A resident operation failed asynchronously.
    RuntimeError { id: DeviceId, message: String },
    /// 挿抜。UI は `scan` し直す。
    DevicesChanged,
    /// デバイス側で設定が変わった (本体のボタンで DPI やプロファイルを変えた等)。
    /// UI は `read` し直す。
    StateChanged(DeviceId),
    Battery(DeviceId, Battery),
    /// ドライバ内部で変わった `DeviceInfo` (休眠状態・裏で取れたバッテリー等)。
    /// UI はセッションとデバイス一覧へ反映する。
    Info(DeviceId, Box<DeviceInfo>),
    /// 時間のかかる操作 (ファーム更新・キャリブレーション・大量書き込み) の進み具合。
    /// `fraction` は 0..1 (分からなければ None = 不定のスピナー)。`done` で表示を消す。
    /// `run_action` の実行中にドライバから `watch` の callback 経由で送る。
    Progress {
        id: DeviceId,
        label: String,
        fraction: Option<f32>,
        done: bool,
    },
}

// ------------------------------------------------------------------ トレイト

/// ハブ全体の入口。`devhub_hid::connect()` が作る。
pub trait HubBackend: Send + Sync {
    /// いま見えているデバイスの一覧。未対応機も `supported=false` で返すと UI に出る。
    /// UI が 2 秒ごとに呼ぶので軽く保つ (HID の列挙のみ。デバイスへの問い合わせはしない)。
    fn scan(&self) -> HubResult<Vec<DeviceInfo>>;

    /// デバイス 1 台のドライバ。同じ ID には同じインスタンスを返してよい。
    fn driver(&self, id: &DeviceId) -> HubResult<Arc<dyn DeviceDriver>>;

    /// キャッシュ済みドライバを捨てる (Disconnected 後に呼ぶ)。
    /// 同一パスへの抜き差しではデバイスパスが変わらないが中身のハンドルは
    /// 死んでいるので、次回 `driver()` で開き直す必要がある。
    fn invalidate(&self, _id: &DeviceId) {}

    /// イベント通知の登録 (任意)。
    fn watch(&self, _cb: Box<dyn Fn(HubEvent) + Send + Sync>) {}

    /// デモ用など、実機でないことを UI に示す。
    fn is_demo(&self) -> bool {
        false
    }
}

/// デバイス 1 台との会話。
pub trait DeviceDriver: Send + Sync {
    /// 名札 (firmware 等を開いた後の情報で埋めて返す)。
    fn info(&self) -> DeviceInfo;

    /// 機能の宣言。**開いた時点で 1 回だけ**呼ばれ、UI はセッション中キャッシュする。
    /// (プロファイル切替などで変わる場合は `HubEvent::StateChanged` を出せば読み直す。)
    fn caps(&self) -> Capabilities;

    /// 全設定を読む。読めなかった汎用設定は map に入れなければ UI がその行を無効表示する。
    fn read(&self) -> HubResult<DeviceState>;

    /// 変更 1 件を即時適用する。成功 = デバイスが受理した (ACK が無いなら送信成功)。
    /// 失敗時 UI はトーストを出し、`read` で読み直して表示を実機に合わせる。
    fn apply(&self, change: &Change) -> HubResult<()>;

    /// `ActionSpec` の実行。`file` は `ActionSpec::file` が Some のとき選ばれたパス。
    fn run_action(&self, id: &str, file: Option<&Path>) -> HubResult<ActionOutcome> {
        let _ = (id, file);
        Err(HubError::Unsupported("action"))
    }

    /// 最新のバッテリー。
    fn battery(&self) -> HubResult<Battery> {
        Err(HubError::Unsupported("battery"))
    }

    /// 押下深さのリアルタイム値 (mm、layout index 順)。`PerfCaps::live_travel` のときだけ
    /// UI が「ストロークテスト」中に 30Hz 程度で呼ぶ。
    fn live_travel(&self) -> HubResult<Vec<f32>> {
        Err(HubError::Unsupported("live_travel"))
    }

    /// ストローク計測の生データ (route mm + ADC + 状態、layout index 順)。
    /// 対応ドライバでは UI が `live_travel` の代わりにこちらをポーリングして詳細表示する。
    fn live_axis(&self) -> HubResult<Vec<AxisSample>> {
        Err(HubError::Unsupported("live_axis"))
    }

    /// `MouseCaps::live_buttons` と同じ並びで、最新の押下深さ。UI がテスト画面で
    /// 60Hz で呼ぶ。通知 (push) で届くデバイスは受信スレッドが保存した最新値を
    /// 返すだけにし、ここで HID を叩かないこと。
    fn live_buttons(&self) -> HubResult<Vec<ButtonDepth>> {
        Err(HubError::Unsupported("live_buttons"))
    }

    /// 実測の input report レート (Hz)。デバイスが流してくる自発レポートを
    /// 受信スレッドが数えたもので、設定値ではなく実測値。ストリームを持たない
    /// デバイス/未実装なら None。UI は 0.5〜1 秒おきに呼ぶ。HID は叩かないこと。
    fn live_report_hz(&self) -> Option<f32> {
        None
    }

    /// キーごとの実設定 (layout 順、mm)。取得できないドライバは Unsupported を返す。
    fn live_key_perf(&self) -> HubResult<Vec<KeyPerf>> {
        Err(HubError::Unsupported("live_key_perf"))
    }

    /// 磁気ボタンの高速作動設定 (live_buttons 順、0 は無効)。
    fn live_button_fast(&self) -> HubResult<Vec<Option<u8>>> {
        Err(HubError::Unsupported("live_button_fast"))
    }

    /// キーごとの作動点 (mm、layout 順)。RT は First touch。
    fn live_actuation(&self) -> HubResult<Vec<f32>> {
        Err(HubError::Unsupported("live_actuation"))
    }

    /// DirectDrive: per-LED のライブ書き込み (応答なし・永続しない、ファームの
    /// アニメーションを一時的に上書きする)。`cells` はキー index → 色で、
    /// LED セルへの展開はドライバ側。`LightArea::direct_drive` のときだけ UI が
    /// フレームを流す (5fps 程度)。対応しないドライバは Unsupported。
    fn direct_drive(&self, _area: u8, _cells: &BTreeMap<usize, Rgb>) -> HubResult<()> {
        Err(HubError::Unsupported("direct_drive"))
    }

    /// DirectDrive を抜けてファーム側のアニメーションに戻す。
    /// 対応ドライバは保存済みのライティング設定を再送して実機の点灯を復帰させる。
    fn direct_drive_stop(&self, _area: u8) -> HubResult<()> {
        Err(HubError::Unsupported("direct_drive"))
    }

    /// ライブ点灯色の読み戻し — キー index → **いま実際に光っている色**
    /// (ファームが描画したエフェクトの現在フレーム。ユーザーカスタム色も含む)。
    /// 公式 HUB のプレビューと同じ仕組みで、UI はライティング画面表示中
    /// ~6Hz で呼んで実機そのままの見た目を描く。シミュレーションではない。
    /// 非対応ドライバは Unsupported — UI はローカル近似プレビューに切り替える。
    fn live_lighting(&self, _area: u8) -> HubResult<BTreeMap<usize, Rgb>> {
        Err(HubError::Unsupported("live_lighting"))
    }
}
