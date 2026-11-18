//! XSYD プロトコル (hub.xsyd.top / Sparklink SDK) — SU66 Max 等ホール効果キーボード。
//!
//! 実機解析済み (1ca6:3009, usage_page 0xFFB0 / usage 1):
//! - report id なし 64B output report + 64B input report
//! - フレーム: `[group][sub][args...]` → 応答 `[group][sub][data...]`
//! - group: Device=1, Global=2, LayoutAndKey=3, Performance=4, Lighting=5,
//!   HigherKey=6, Macro=7, CustomCommand=10, Displayer=12, ThreeMode=13
//! - 応答 `sub==0xFF` は「未対応コマンド」を意味する (実機確認済み)
//!
//! # 検証状況
//! - **実機検証済み**: `1,2` device info, `1,3` feature (レイヤー数), `3,1` キーマップ読み,
//!   `3,4` キー単体書き, `5,1`/`5,2` ライティング読み書き, `4,1`/`4,2` キー性能,
//!   `4,3` axis data (route/calibrate), `2,3` 構成, `2,4` OS, `2,5` ポーリング,
//!   `2,8` ライト領域, `2,12` RT精度, `2,13` スリープ, `2,14` マクロ領域,
//!   `2,16` 防チャタ, `6,1` 高度キー読み, `7,1`/`7,3` マクロ読み。
//! - フレームの一次資料: dshk.xsyd.top の SDK バンドル (リポジトリの `xsyd-sdk.js`)。
//!   全値の u16 はリトルエンディアン (`q`/`ee`)、mm は ×1000 の u16。

use crate::transport::Channel;
use devhub_core::backend::*;
use devhub_core::model::*;
use devhub_core::registry;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const POLL_MS: i32 = 60;
const DEADLINE_MS: u64 = 700;
/// この回数連続でタイムアウトしたらデッド判定 (read() 全体を止めない)。
const FAIL_FAST_AFTER: u32 = 4;
/// デッド判定中に実コマンドで生存確認する間隔。
const PROBE_MS: u64 = 1500;
/// デッド判定中のプローブ用タイムアウト。
const PROBE_WAIT_MS: u64 = 120;

mod grp {
    pub const DEVICE: u8 = 1;
    pub const GLOBAL: u8 = 2;
    pub const LAYOUT_KEY: u8 = 3;
    pub const PERFORMANCE: u8 = 4;
    pub const LIGHTING: u8 = 5;
    pub const HIGHER_KEY: u8 = 6;
    pub const MACRO: u8 = 7;
    pub const CUSTOM: u8 = 10;
    pub const THREE_MODE: u8 = 13;
}
mod sub {
    pub const READ: u8 = 1;
    pub const SETTING: u8 = 2;
    pub const DEVICE_INFO: u8 = 2;
    pub const GET_BASIC_INFO: u8 = 1;
    /// device feature `[1,3]` — 機能ビットマスク: r[2]=axisType(機種 enum),
    /// r[3]=接続方式, r[4]={rgb,knob}, r[5]={画面,haptic,numpad,dotMatrix}。
    #[allow(dead_code)]
    pub const FEATURE: u8 = 3;
    pub const GET_KEY_LAYOUT: u8 = 1;
    /// 推測: 行単位のキーマップ書き込み。read の鏡。
    pub const SET_KEY_LAYOUT: u8 = 2;
    /// group 3: キー 1 個の読み/書き (SDK の GetKeyCode/SetKeyCode)。
    #[allow(dead_code)]
    pub const GET_KEY_CODE: u8 = 3;
    pub const SET_KEY_CODE: u8 = 4;
    /// group 3: 物理レイアウト記述 `[3,5,page]` (u16LE {row:5,col:7,ratio:4} の
    /// セルを最大 ~15 個/ページ)。工場キーマップ `[3,6,(sys<<4)|fn,row]`。
    #[allow(dead_code)]
    pub const GET_KEY_LAYOUT_STYLE: u8 = 5;
    pub const GET_KEY_LAYOUT_DEFAULT: u8 = 6;
    /// group 4 subs (SDK $a): GetPerformance=1, SetPerformance=2, AxisData=3。
    pub const GET_PERF: u8 = 1;
    pub const SET_PERF: u8 = 2;
    pub const AXIS_DATA: u8 = 3;
    /// group 5 subs: 照明基本の読み書きは READ/SETTING を共用。
    /// カスタム色: ReadBack=3, Custom=4, DirectDrive=5, Caps=6 (SDK enum 確定)。
    pub const LIGHT_READBACK: u8 = 3;
    pub const LIGHT_CUSTOM: u8 = 4;
    /// DirectDrive — per-LED ライブ書き込み、応答なし (SDK sendDataNoResponse)。
    pub const LIGHT_DIRECTDRIVE: u8 = 5;
    /// group 7 subs (SDK Xa): GetMacroMode=1, SetMacroMode=2, GetMacro=3, SetMacro=4。
    pub const GET_MACRO_MODE: u8 = 1;
    pub const SET_MACRO_MODE: u8 = 2;
    pub const GET_MACRO: u8 = 3;
    pub const SET_MACRO: u8 = 4;
}
/// group 2 (Global) の sub (SDK Wa)。
mod gfs {
    pub const RESET_FACTORY: u8 = 1;
    pub const SAVE_PARAM: u8 = 2;
    pub const CONFIG_SWITCH: u8 = 3;
    pub const SYSTEM_SETTING: u8 = 4;
    pub const REPORT_RATE: u8 = 5;
    pub const CALIBRATION: u8 = 6;
    pub const AXIS_LIB: u8 = 7;
    pub const EFFECT_AREA: u8 = 8;
    #[allow(dead_code)]
    pub const DEFAULT_AXIS: u8 = 9;
    #[allow(dead_code)]
    pub const DOUBLE_LIGHT: u8 = 10;
    #[allow(dead_code)]
    pub const SPECIAL_LIGHT: u8 = 11;
    pub const RT_PRECISION: u8 = 12;
    pub const SLEEP_TIME: u8 = 13;
    pub const MACRO_SPACE: u8 = 14;
    pub const SHAKE_OPT: u8 = 16;
}
/// SaveParam / ResetFactory のスコープ (SDK qo/Qo)。
mod scope {
    pub const ALL: u8 = 0;
    pub const CALIBRATION: u8 = 1;
    pub const PERFORMANCE: u8 = 2;
    pub const LIGHTING: u8 = 3;
    pub const LAYOUT: u8 = 4;
    pub const HIGHER_KEY: u8 = 5;
    pub const MACRO: u8 = 6;
    #[allow(dead_code)]
    pub const AXIS: u8 = 7;
}
/// AxisData (4,3) の第2引数 (SDK Qa)。
mod axis {
    #[allow(dead_code)]
    pub const ADC: u8 = 0;
    pub const ROUTE: u8 = 1;
    #[allow(dead_code)]
    pub const CALIBRATE: u8 = 2;
    #[allow(dead_code)]
    pub const KEY_STATUS: u8 = 3;
}
/// 高度キー種別 (SDK ja)。wire の mode バイト。
mod hk {
    pub const NONE: u8 = 0;
    pub const DKS: u8 = 1;
    pub const MPT: u8 = 2;
    pub const MT: u8 = 3;
    pub const TGL: u8 = 4;
    pub const END: u8 = 5;
    pub const SOCD: u8 = 6;
    pub const RS: u8 = 7;
}

/// open 時に一度だけ問い合わせるデバイス定数。
struct XsydInfo {
    layers: u8,
    /// RT 精度 (mm)。`2,12` 応答 r[3]/1000。取得失敗時 0.01。
    rt_step: f32,
    /// 全ストローク (mm)。route の axisRangeMax 最大値 /1000。取得失敗時 4.0。
    travel: f32,
    /// ポーリング選択肢 (wire code → Hz)。
    polling: Vec<(u8, &'static str)>,
    /// 構成 (プロファイル) 数。`2,3` list 応答 r[3]。
    configs: u8,
    /// ライティング領域 index 一覧 (0=キーボード, 1..=デコレーション)。
    light_areas: Vec<u8>,
    /// マクロ (スロット数, 1本の最大イベント数)。
    macro_space: Option<(u8, u16)>,
    /// 軸ライブラリ (u16 id 一覧)。空なら switch_types を出さない。
    axes: Vec<u16>,
    /// DoubleLighting (上下 2 ゾーン) か。`2,10,0` 応答 r[3]==1。
    double_lighting: bool,
}

/// セル番号 → 確認済みの BGR。書き込み失敗で適用が不確かなセルは None。
type DdPins = BTreeMap<usize, Option<[u8; 3]>>;

/// 軸フレームキャッシュの型: (kind,row) → (取得時刻, 30 セル)。
type AxisCache = Mutex<HashMap<(u8, u8), (Instant, [u16; 30])>>;

pub struct XsydDriver {
    info: DeviceInfo,
    ch: Channel,
    xs: XsydInfo,
    caps: Capabilities,
    /// 連続コマンドタイムアウト数。閾値を超えたら即 Disconnected を返し、
    /// 定期的に 1 回だけ実コマンドを通して復帰を検出する。
    /// (応答しないデバイスに read() の全コマンドが 700ms ずつ待つのを防ぐ)
    fails: AtomicU32,
    dead_since: Mutex<Option<Instant>>,
    /// DirectDrive で現在ピン留め中の LED セルと最後に送った色
    /// (area → cell → BGR)。次フレームで集合から外れたセルは `[5,4]` で
    /// isCustom を落としてファーム駆動に戻す (DD の flag=0 は「触らない」
    /// であって解放ではない)。同じ色のままのセルも再送しない —
    /// ピンはストリーム停止後も残るので、変化分だけで表示が保たれる。
    dd_pins: Mutex<BTreeMap<u8, DdPins>>,
    /// DD セッション開始前にユーザーカスタム (isCustom=255) だったセルと色
    /// (area → cell → BGR)。DD 解放時に flag=255+保存色で戻すために退避する —
    /// DD とユーザーカスタムは同じ isCustom フラグを共有するので、
    /// 区別しないと DD がユーザーのカスタムを巻き込んで消す。
    dd_pre: Mutex<BTreeMap<u8, BTreeMap<usize, [u8; 3]>>>,
    /// 軸フレームのキャッシュ: (kind,row) → (取得時刻, 30 セル)。
    /// 入力キューは全オープンハンドルへ扇出されるので、公式 HUB 等
    /// 他ツールの要求が起こしたフレームも拾える (相乗り受動化)。
    axis_cache: AxisCache,
    /// 自分が直近で投げた `[4,3,kind,row]` の時刻 — 外部トラフィック検出用。
    axis_self: Mutex<HashMap<(u8, u8), Instant>>,
    /// 自分が直近で投げたコマンド (grp,sub) の時刻 — 非軸フレームが
    /// 「他人の応答」か「自分の応答」かを区別するために使う。
    cmd_self: Mutex<HashMap<(u8, u8), Instant>>,
    /// 外部トラフィックを検出したら要求を休む期限 (受動モード)。
    axis_passive: Mutex<Instant>,
}

/// `2,5` ポーリング wire code → ラベル (SDK Fn)。
fn polling_label(code: u8) -> &'static str {
    match code {
        0 => "8000 Hz",
        1 => "4000 Hz",
        2 => "2000 Hz",
        3 => "1000 Hz",
        4 => "500 Hz",
        5 => "250 Hz",
        _ => "125 Hz",
    }
}

pub fn open(dev: hidapi::HidDevice, info: DeviceInfo) -> HubResult<XsydDriver> {
    dev.set_blocking_mode(false).ok();
    let ch = Channel::open(dev, 65, 65);
    let mut drv = XsydDriver {
        caps: Capabilities::empty(),
        xs: XsydInfo {
            layers: 2,
            rt_step: 0.01,
            travel: 4.0,
            polling: vec![
                (0, "8000 Hz"),
                (1, "4000 Hz"),
                (2, "2000 Hz"),
                (3, "1000 Hz"),
            ],
            configs: 0,
            light_areas: vec![0],
            macro_space: None,
            axes: vec![],
            double_lighting: false,
        },
        info,
        ch,
        fails: AtomicU32::new(0),
        dead_since: Mutex::new(None),
        dd_pins: Mutex::new(BTreeMap::new()),
        dd_pre: Mutex::new(BTreeMap::new()),
        axis_cache: Mutex::new(HashMap::new()),
        axis_self: Mutex::new(HashMap::new()),
        cmd_self: Mutex::new(HashMap::new()),
        axis_passive: Mutex::new(Instant::now() - Duration::from_secs(1)),
    };
    // [1,3] DeviceFeature: r[2]=axisType(機種バリアント enum、磁気=2), r[3]=接続,
    // r[4]={rgb,knob}, r[5]={画面,haptic,numpad,dotMatrix}。レイヤー数ではない。
    // キーマップ層は SDK 同様 fn0..3 固定 (実機 layer4 は全 0 を返す)。
    drv.xs.layers = 4;
    // FW バージョンを開いた時点で拾って名札に載せる。
    let mut info = drv.info.clone();
    if let Ok(r) = drv.cmd(grp::DEVICE, sub::DEVICE_INFO, &[]) {
        info.firmware = parse_fw(&r);
        // 配布 API の引数: board_id = r[4..8] BE u32, appVersion = r[8..12] "." 結合。
        info.board_id = Some(u32::from_be_bytes([r[4], r[5], r[6], r[7]]));
        info.firmware = Some(format!("{}.{}.{}.{}", r[8], r[9], r[10], r[11]));
    }
    info.supported = true;
    drv.info = info;

    // ---- 定数の一括問い合わせ (失敗は全部無視して既定値) ----
    // RT 精度: [2,12,0] → r[3]/1000
    if let Ok(r) = drv.cmd(grp::GLOBAL, gfs::RT_PRECISION, &[0]) {
        let step = r[3] as f32 / 1000.0;
        if step > 0.0 {
            drv.xs.rt_step = step;
        }
    }
    // ポーリング選択肢: [2,5,0] → r[3]=total, r[4..] = codes
    if let Ok(r) = drv.cmd(grp::GLOBAL, gfs::REPORT_RATE, &[0]) {
        let n = (r[3] as usize).min(7);
        let v: Vec<_> = (0..n)
            .map(|i| (r[4 + i], polling_label(r[4 + i])))
            .collect();
        if !v.is_empty() {
            drv.xs.polling = v;
        }
    }
    // 構成数: [2,3,0] → r[3]=total
    if let Ok(r) = drv.cmd(grp::GLOBAL, gfs::CONFIG_SWITCH, &[0]) {
        drv.xs.configs = r[3].clamp(0, 10);
    }
    // ライト領域: [2,8,0] → r[3]=total, r[4+4i]=index
    if let Ok(r) = drv.cmd(grp::GLOBAL, gfs::EFFECT_AREA, &[0]) {
        let n = (r[3] as usize).min(14);
        let areas: Vec<u8> = (0..n).map(|i| r[4 + i * 4]).collect();
        if !areas.is_empty() {
            drv.xs.light_areas = areas;
        }
    }
    // マクロ領域: [2,14,0] → r[3]=スロット数, r[4,5]=最大イベント数
    if let Ok(r) = drv.cmd(grp::GLOBAL, gfs::MACRO_SPACE, &[0]) {
        if r[3] > 0 {
            drv.xs.macro_space = Some((r[3], u16::from_le_bytes([r[4], r[5]])));
        }
    }
    // 軸ライブラリ: [2,7,0] → r[3]=total, r[4..]=u16 id
    if let Ok(r) = drv.cmd(grp::GLOBAL, gfs::AXIS_LIB, &[0]) {
        let n = (r[3] as usize).min(30);
        drv.xs.axes = (0..n)
            .map(|i| u16::from_le_bytes([r[4 + i * 2], r[5 + i * 2]]))
            .collect();
    }
    // DoubleLighting: [2,10,0] → r[3]==1 で上下 2 ゾーン (SDK GFSDoubleLighting)。
    if let Ok(r) = drv.cmd(grp::GLOBAL, gfs::DOUBLE_LIGHT, &[0]) {
        drv.xs.double_lighting = r[3] == 1;
    }
    // 全ストローク: 任意 1 キーの perf 応答 r[23,24] (axisRangeMax) /1000。
    // 末尾順は v2Id(2) + rangeMax(2) + coef(2) — r[25,26] は係数なので読まない。
    if let Some(k) = registry::su66max_layout().first() {
        let (row, col) = registry::su66max_matrix_pos(k.id);
        if let Ok(r) = drv.cmd(grp::PERFORMANCE, sub::GET_PERF, &[row, col]) {
            let t = u16::from_le_bytes([r[23], r[24]]) as f32 / 1000.0;
            if t > 0.5 {
                drv.xs.travel = t;
            }
        }
    }
    drv.caps = build_caps(&drv.xs);
    Ok(drv)
}

impl XsydDriver {
    /// group/sub フレーム → 64B 応答。sub==0xFF (未対応) は Unsupported。
    fn cmd(&self, group: u8, sub: u8, args: &[u8]) -> HubResult<[u8; 64]> {
        // fail-fast: 連続タイムアウト中は即 Disconnected。PROBE_MS ごとに
        // 1 回だけ実コマンドを通して復帰を確認する (デッド状態でも永続化しない)。
        let probing = self.fails.load(Ordering::Relaxed) >= FAIL_FAST_AFTER;
        if probing {
            let mut ds = self.dead_since.lock().unwrap_or_else(|p| p.into_inner());
            match *ds {
                Some(t) if t.elapsed() < Duration::from_millis(PROBE_MS) => {
                    return Err(HubError::Disconnected)
                }
                _ => *ds = Some(Instant::now()),
            }
        }
        let mut w = [0u8; 64];
        w[0] = group;
        w[1] = sub;
        let n = args.len().min(62);
        w[2..2 + n].copy_from_slice(&args[..n]);
        self.ch.send(&w)?;
        // 自分の送信を記録 — drain で「他人の応答」と区別するために使う。
        self.cmd_self
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert((group, sub), Instant::now());

        let wait_ms = if probing { PROBE_WAIT_MS } else { DEADLINE_MS };
        let deadline = Instant::now() + Duration::from_millis(wait_ms);
        while Instant::now() < deadline {
            match self.ch.recv(POLL_MS) {
                Ok(b) if b.first().copied() == Some(group) => {
                    let mut out = [0u8; 64];
                    out[..b.len().min(64)].copy_from_slice(&b[..b.len().min(64)]);
                    if out[1] == 0xFF {
                        return Err(HubError::Unsupported("command"));
                    }
                    if group == grp::LIGHTING && sub == sub::LIGHT_READBACK && b.len() < 64 {
                        return Err(HubError::Protocol("xsyd: short LED readback".into()));
                    }
                    self.fails.store(0, Ordering::Relaxed);
                    *self.dead_since.lock().unwrap_or_else(|p| p.into_inner()) = None;
                    return Ok(out);
                }
                Ok(_) => {}
                Err(HubError::Protocol(_)) => {}
                Err(e) => return Err(e),
            }
        }
        let f = self.fails.fetch_add(1, Ordering::Relaxed) + 1;
        if f >= FAIL_FAST_AFTER {
            *self.dead_since.lock().unwrap_or_else(|p| p.into_inner()) = Some(Instant::now());
            return Err(HubError::Disconnected);
        }
        Err(HubError::Protocol("xsyd: response timeout".into()))
    }

    /// ドライバ破棄時の保険 — DD の isCustom ピンは無期限に残る (実機確認:
    /// ストリーム停止後も固まったまま) ので、ハンドルが閉じる前に追跡中の
    /// ピンだけ解放する。DD を一度も使っていなければ何も送らない (以前は
    /// 全チャンクにゼロペイロードを送り、ユーザーのカスタム色ごと消していた)。
    /// (`direct_drive_stop` 側が通常経路で完全な停止を行うので、
    /// ここではベストエフォートのピン解除だけ行う)
    fn release_dd_pins(&self) {
        let pins = self.dd_pins.lock().map(|p| p.clone()).unwrap_or_default();
        for (area, cells) in pins {
            if cells.is_empty() {
                continue;
            }
            let set: std::collections::HashSet<usize> = cells.keys().copied().collect();
            let _ = self.release_cells(area, &set);
        }
    }

    /// DD セッション開始前に `[5,3]` でユーザーカスタム (isCustom=255) の
    /// セル+色を退避する。DD 解放時に `release_cells` が flag=255+保存色で
    /// 戻すために使う。
    fn capture_dd_pre(&self, area: u8) -> HubResult<()> {
        let mut pre = BTreeMap::new();
        for c in 0..9u8 {
            let rb = self.read_dd_chunk(area, c)?;
            for i in 0..15usize {
                if rb[7 + i * 4] == 255 {
                    pre.insert(
                        c as usize * 15 + i,
                        [rb[4 + i * 4], rb[5 + i * 4], rb[6 + i * 4]],
                    );
                }
            }
        }
        self.dd_pre
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(area, pre);
        Ok(())
    }

    fn read_dd_chunk(&self, area: u8, chunk: u8) -> HubResult<[u8; 64]> {
        let r = self.cmd(grp::LIGHTING, sub::LIGHT_READBACK, &[area, chunk])?;
        if r[1..4] != [sub::LIGHT_READBACK, area, chunk] {
            return Err(HubError::Protocol(format!(
                "xsyd: unexpected LED chunk {chunk}"
            )));
        }
        Ok(r)
    }

    /// `cells` の DD ピンを `[5,4]` で解放する。チャンクごとに `[5,3]` で
    /// 読み戻して対象セルだけ直すので、ユーザーのカスタムや他のピンは温存
    /// される。対象セルが `dd_pre` にあれば DD 前のカスタム色+flag=255 で
    /// 復帰し、無ければ flag=0 で解放 (色バイトは保持)。
    /// 戻り値: 書き込みに失敗したチャンク集合。
    fn release_cells(
        &self,
        area: u8,
        cells: &std::collections::HashSet<usize>,
    ) -> std::collections::HashSet<u8> {
        let pre = self
            .dd_pre
            .lock()
            .map(|p| p.get(&area).cloned().unwrap_or_default())
            .unwrap_or_default();
        let chunks: std::collections::BTreeSet<u8> = cells.iter().map(|c| (c / 15) as u8).collect();
        let mut failed = std::collections::HashSet::new();
        for c in chunks {
            let lo = c as usize * 15;
            let ok = if let Ok(rb) = self.read_dd_chunk(area, c) {
                let mut args = vec![area, c];
                for i in 0..15usize {
                    let cell_no = lo + i;
                    let mut cell = [rb[4 + i * 4], rb[5 + i * 4], rb[6 + i * 4], rb[7 + i * 4]];
                    if cells.contains(&cell_no) {
                        if let Some(bgr) = pre.get(&cell_no) {
                            cell = [bgr[0], bgr[1], bgr[2], 255];
                        } else {
                            cell[3] = 0;
                        }
                    }
                    args.extend_from_slice(&cell);
                }
                self.cmd(grp::LIGHTING, sub::LIGHT_CUSTOM, &args)
                    .is_ok_and(|r| r[1..4] == [sub::LIGHT_CUSTOM, area, c])
            } else {
                false
            };
            if !ok {
                failed.insert(c);
            }
        }
        failed
    }

    /// デバイスがデッド判定中なら Err(Disconnected)。read()/apply() の節目で
    /// 呼んで、応答しないデバイスへの全コマンド掃射を早めに止める。
    fn alive(&self) -> HubResult<()> {
        if self.fails.load(Ordering::Relaxed) >= FAIL_FAST_AFTER {
            Err(HubError::Disconnected)
        } else {
            Ok(())
        }
    }

    /// レイヤーのキーストリームを読む: `[3,1,layer,row]` → r[4..64] = 30×u16 LE。
    /// stream 上の位置は row*30+col。
    fn read_keymap(&self, layer: u8) -> HubResult<Vec<u16>> {
        let mut stream = vec![0u16; 8 * 30];
        let mut got_any = false;
        for row in 0..8u8 {
            match self.cmd(grp::LAYOUT_KEY, sub::GET_KEY_LAYOUT, &[layer, row]) {
                Ok(r) if r[2] == layer && r[3] == row => {
                    for i in 0..30 {
                        stream[row as usize * 30 + i] =
                            u16::from_le_bytes([r[4 + i * 2], r[5 + i * 2]]);
                    }
                    got_any = true;
                }
                Ok(_) => {}
                Err(HubError::Unsupported(_)) | Err(HubError::Protocol(_)) => break,
                Err(e) => return Err(e),
            }
        }
        if !got_any {
            return Err(HubError::Protocol("keymap: no rows".into()));
        }
        Ok(stream)
    }

    /// キー 1 個の書き込み `[3,4,layer,row,col,codeLE]` (SDK の SetKeyCode)。
    fn write_key(&self, layer: u8, layout_index: u16, code: u16) -> HubResult<()> {
        let (row, col) = registry::su66max_matrix_pos(layout_index);
        let mut args = vec![layer, row, col];
        args.extend_from_slice(&code.to_le_bytes());
        self.cmd(grp::LAYOUT_KEY, sub::SET_KEY_CODE, &args)?;
        Ok(())
    }

    /// ストリーム全体を行単位で書き込む。
    fn write_stream(&self, layer: u8, stream: &[u16]) -> HubResult<()> {
        for row in 0..8u8 {
            let mut args = vec![layer, row];
            for i in 0..30 {
                args.extend_from_slice(&stream[row as usize * 30 + i].to_le_bytes());
            }
            self.cmd(grp::LAYOUT_KEY, sub::SET_KEY_LAYOUT, &args)?;
        }
        Ok(())
    }

    /// 工場出荷キーマップ `[3,6,(system<<4)|fn,row]` → 30×u16 のストリーム。
    /// Fn 層の工場値 (数字行→F1-F12 等) が取れる。失敗した行は 0 のまま。
    fn factory_stream(&self, layer: u8) -> Option<Vec<u16>> {
        let sys = match self.read_setting("system_type") {
            Some(SettingValue::Choice(v)) => v.clamp(0, 1) as u8,
            _ => 0,
        };
        let sel = (sys << 4) | layer;
        let mut stream = vec![0u16; 8 * 30];
        let mut got_any = false;
        for row in 0..8u8 {
            match self.cmd(grp::LAYOUT_KEY, sub::GET_KEY_LAYOUT_DEFAULT, &[sel, row]) {
                Ok(r) if r[2] == sel && r[3] == row => {
                    for i in 0..30 {
                        stream[row as usize * 30 + i] =
                            u16::from_le_bytes([r[4 + i * 2], r[5 + i * 2]]);
                    }
                    got_any = true;
                }
                Ok(_) => {}
                Err(HubError::Unsupported(_)) | Err(HubError::Protocol(_)) => break,
                Err(_) => return None,
            }
        }
        got_any.then_some(stream)
    }

    /// caps の settings id → 現在値。失敗は None (出さない)。
    fn read_setting(&self, id: &str) -> Option<SettingValue> {
        match id {
            // [2,5,1] → r[3] = wire code
            "polling" => self
                .cmd(grp::GLOBAL, gfs::REPORT_RATE, &[1])
                .ok()
                .map(|r| SettingValue::Choice(r[3] as i64)),
            // [2,4,1] → r[3] = 0 Win / 1 Mac
            "system_type" => self
                .cmd(grp::GLOBAL, gfs::SYSTEM_SETTING, &[1])
                .ok()
                .map(|r| SettingValue::Choice(r[3] as i64)),
            // [2,16,1] → r[3] = status (1=有効)
            "shake_opt" => self
                .cmd(grp::GLOBAL, gfs::SHAKE_OPT, &[1])
                .ok()
                .map(|r| SettingValue::Bool(r[3] == 1)),
            // [2,13,1] → r[3,4] u16 分
            "light_sleep" => self
                .cmd(grp::GLOBAL, gfs::SLEEP_TIME, &[1])
                .ok()
                .map(|r| SettingValue::Num(u16::from_le_bytes([r[3], r[4]]) as f32)),
            _ => None,
        }
    }

    fn write_setting(&self, id: &str, v: &SettingValue) -> HubResult<()> {
        match id {
            "polling" => {
                self.cmd(grp::GLOBAL, gfs::REPORT_RATE, &[2, v.as_choice() as u8])?;
            }
            "system_type" => {
                self.cmd(grp::GLOBAL, gfs::SYSTEM_SETTING, &[2, v.as_choice() as u8])?;
            }
            "shake_opt" => {
                self.cmd(grp::GLOBAL, gfs::SHAKE_OPT, &[0, v.as_bool() as u8])?;
            }
            "light_sleep" => {
                let m = (v.as_num().clamp(0.0, 120.0) as u16).to_le_bytes();
                self.cmd(grp::GLOBAL, gfs::SLEEP_TIME, &[0, m[0], m[1]])?;
            }
            _ => return Err(HubError::Unsupported("setting")),
        }
        Ok(())
    }

    /// 現在の構成 index `[2,3,1]` → r[3]。
    fn read_profile(&self) -> HubResult<ProfileState> {
        let r = self.cmd(grp::GLOBAL, gfs::CONFIG_SWITCH, &[1])?;
        let active = r[3].min(self.xs.configs.saturating_sub(1));
        let mut names = Vec::new();
        for i in 0..self.xs.configs {
            // [2,3,3,i] → r[4..36] UTF-8
            let name = self
                .cmd(grp::GLOBAL, gfs::CONFIG_SWITCH, &[3, i])
                .ok()
                .map(|r| {
                    let end = r[4..36].iter().position(|&b| b == 0).unwrap_or(32);
                    String::from_utf8_lossy(&r[4..4 + end]).trim().to_string()
                })
                .unwrap_or_default();
            names.push(name);
        }
        Ok(ProfileState { active, names })
    }

    /// SaveParam `[2,2,scope]` — 書き込み後のフラッシュ保存。
    /// 実機は save 直後の読み戻しに古い値を返すことがあるため、反映を待ってから返る。
    fn save(&self, scope: u8) -> HubResult<()> {
        self.cmd(grp::GLOBAL, gfs::SAVE_PARAM, &[scope])?;
        std::thread::sleep(Duration::from_millis(80));
        Ok(())
    }

    /// 範囲指定ファクトリーリセット `[2,1,scope]`。
    fn reset_scope(&self, scope: u8, label: &str) -> HubResult<ActionOutcome> {
        self.cmd(grp::GLOBAL, gfs::RESET_FACTORY, &[scope])?;
        Ok(ActionOutcome {
            message: Some(format!("{label}を初期値に戻しました")),
            reload: true,
        })
    }

    /// perf 応答 (r[4..]) → KeyPerf。u16 値は mm×1000 の LE。
    /// mode: 0=Normal, 1=RT (連続 RT), 2=IntervalRT (非連続 RT) — 公式 enum vn 準拠。
    fn decode_perf(r: &[u8; 64]) -> KeyPerf {
        let mm = |i: usize| u16::from_le_bytes([r[i], r[i + 1]]) as f32 / 1000.0;
        KeyPerf {
            mode: if r[4] == 0 {
                TriggerMode::Normal
            } else {
                TriggerMode::Rapid
            },
            actuation: mm(5),
            release: Some(mm(7)),
            rt_first: mm(9),
            rt_press: mm(11),
            rt_release: mm(13),
            top_deadzone: mm(15),
            bottom_deadzone: mm(17),
            continuous: r[4] == 1,
            switch_type: r[19],
        }
    }

    /// `[4,1,row,col]` → KeyPerf + 追従フィールド (axis/calib/axisV2Id/coef/rangeMax)。
    /// write 時に保持が要るフィールドは生応答を残しておく。
    /// 応答は r[2]=row, r[3]=col のエコー。live travel 等で混入した他 sub の
    /// group-4 応答 (axis データ) を取り違えないよう照合し、ずれていれば 1 回読み直す。
    fn read_perf(&self, row: u8, col: u8) -> HubResult<(KeyPerf, [u8; 64])> {
        let mut r = self.cmd(grp::PERFORMANCE, sub::GET_PERF, &[row, col])?;
        if r[2] != row || r[3] != col {
            r = self.cmd(grp::PERFORMANCE, sub::GET_PERF, &[row, col])?;
        }
        Ok((Self::decode_perf(&r), r))
    }

    /// `[4,2,row,col,...]` — SDK のレイアウト:
    /// mode, nP, nR, rtFirst, rtPress, rtRelease, pDead, rDead (各 u16 LE mm×1000),
    /// axis, calibrate, axisV2Id, axisRangeMax, axisCoefficient (後者 3 つは読み値を維持)。
    fn write_perf(&self, row: u8, col: u8, p: &KeyPerf) -> HubResult<()> {
        let (_, cur) = self.read_perf(row, col)?; // axis 系フィールドを保持するため先読み
        let mm = |v: f32| (v * 1000.0).round().clamp(0.0, 65535.0) as u16;
        let mut args = vec![
            row,
            col,
            // 公式 vn: Normal=0, RT(連続)=1, IntervalRT(非連続)=2
            match p.mode {
                TriggerMode::Normal => 0,
                TriggerMode::Rapid if p.continuous => 1,
                TriggerMode::Rapid => 2,
            },
        ];
        for v in [
            mm(p.actuation),
            mm(p.release.unwrap_or(p.actuation)),
            mm(p.rt_first),
            mm(p.rt_press),
            mm(p.rt_release),
            mm(p.top_deadzone),
            mm(p.bottom_deadzone),
        ] {
            args.extend_from_slice(&v.to_le_bytes());
        }
        args.push(p.switch_type); // axis
                                  // 公式レイアウト: axis + calibrate + axisV2Id(2) + axisRangeMax(2) + axisCoefficient(2)。
                                  // calibrate は公式が常に 0 を送る (calibration 中フラグを残さない)。
                                  // v2Id/rangeMax/coef は読み値を維持する。
        args.push(0);
        args.extend_from_slice(&cur[21..27]);
        self.cmd(grp::PERFORMANCE, sub::SET_PERF, &args)?;
        Ok(())
    }

    /// `[4,3,kind,row]` フレームならキャッシュに入れて true を返す。
    /// フレーム内容は 30×u16 LE。
    fn note_axis_frame(&self, b: &[u8]) -> bool {
        if b.len() >= 64 && b[0] == grp::PERFORMANCE && b[1] == sub::AXIS_DATA {
            let mut out = [0u16; 30];
            for i in 0..30 {
                out[i] = u16::from_le_bytes([b[4 + i * 2], b[5 + i * 2]]);
            }
            self.axis_cache
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert((b[2], b[3]), (Instant::now(), out));
            true
        } else {
            false
        }
    }

    /// 入力キューを軽くドレインして流れている軸フレームをキャッシュする。
    /// HID 入力レポートは全オープンハンドルへ扇出されるので、公式 HUB 等の
    /// 別プロセスが投げた要求の応答もここに届く — 相乗りすれば自分は要求を
    /// 出さずに済み、公式 HUB 側の応答キューに他人のフレームを混ぜない。
    /// 「自分が最近要求していない軸フレームだけ」が流れているか、
    /// 軸以外の既知 group フレーム (= 他人のコマンド応答 — 公式はエコー検証
    /// せず最初のフレームを応答とみなすので混入は致命的) が流れていれば
    /// 外部トラフィック中と判定し、受動モードを延長する (戻り値 = 外部検出)。
    fn drain_axis(&self, budget_ms: u64) -> bool {
        let deadline = Instant::now() + Duration::from_millis(budget_ms);
        let mut saw_any = false;
        let mut saw_ours = false;
        let mut saw_foreign = false;
        while let Ok(b) = self.ch.recv(0) {
            if self.note_axis_frame(&b) {
                saw_any = true;
                let ours = self
                    .axis_self
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .get(&(b[2], b[3]))
                    .is_some_and(|t| t.elapsed() < Duration::from_millis(500));
                saw_ours |= ours;
            } else if self.is_foreign_cmd(&b) {
                // 軸フレーム以外の他人のコマンド応答 (自分の最近の
                // 送信に対応するものは除外済み)。
                saw_foreign = true;
            }
            if Instant::now() >= deadline {
                break;
            }
        }
        // 外部検出で受動化。デバイス側のストリームは他人のコマンドが既に
        // 解除している (任意の非軸コマンドで止まる) ので、こちらからは
        // 何も送らない — 送ればその応答が公式のキューに混ざって逆効果。
        if saw_foreign || (saw_any && !saw_ours) {
            self.note_foreign();
            return true;
        }
        false
    }

    /// 非軸フレームが「他人のコマンド応答」か。既知 group・エラー応答
    /// 以外・かつ自分が最近送った同 (grp,sub) の応答ではないもの。
    fn is_foreign_cmd(&self, b: &[u8]) -> bool {
        (1..=13).contains(&b[0])
            && b.get(1) != Some(&0xFF)
            && !(b[0] == grp::PERFORMANCE && b[1] == sub::AXIS_DATA)
            && self
                .cmd_self
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get(&(b[0], b[1]))
                .is_none_or(|t| t.elapsed() > Duration::from_millis(800))
    }

    /// 外部トラフィックを記録して受動モードを延長する。
    fn note_foreign(&self) {
        *self.axis_passive.lock().unwrap_or_else(|p| p.into_inner()) =
            Instant::now() + Duration::from_secs(8);
    }

    /// `[4,3,kind,row]` → 30×u16 LE (mm×1000)。kind: axis::* (ADC=0,Route=1,Calibrate=2,KeyStatus=3)。
    /// 実機は live 軸データを `[4,3,kind,row]` フレームで自律ストリーム配信する
    /// (全 kind×row 組を ~16ms で巡回)。cmd() の「最初の group-4 パケットを応答と
    /// みなす」受信だと別 kind/別行のフレームを拾って値がずれる (ADC 値が
    /// status/route 欄に混入し OBS オーバーレイが点滅していた原因)。
    /// → エコー (r[2]=kind, r[3]=row) が一致するフレームまで読み続ける。
    fn axis_row(&self, row: u8, kind: u8) -> HubResult<[u16; 30]> {
        let probing = self.fails.load(Ordering::Relaxed) >= FAIL_FAST_AFTER;
        if probing {
            let mut ds = self.dead_since.lock().unwrap_or_else(|p| p.into_inner());
            match *ds {
                Some(t) if t.elapsed() < Duration::from_millis(PROBE_MS) => {
                    return Err(HubError::Disconnected)
                }
                _ => *ds = Some(Instant::now()),
            }
        }
        self.ch
            .send(&[grp::PERFORMANCE, sub::AXIS_DATA, kind, row])?;
        // 自分の要求を記録 — drain で外部トラフィックと区別するために使う。
        self.axis_self
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert((kind, row), Instant::now());
        let wait_ms = if probing { PROBE_WAIT_MS } else { DEADLINE_MS };
        let deadline = Instant::now() + Duration::from_millis(wait_ms);
        while Instant::now() < deadline {
            match self.ch.recv(POLL_MS) {
                Ok(b) => {
                    // 軸フレームはエコー外でもキャッシュに入れる
                    // (要求した 1 枚で他行/他 kind のフレームも流れるため)。
                    if self.note_axis_frame(&b) {
                        if b[2] == kind && b[3] == row {
                            self.fails.store(0, Ordering::Relaxed);
                            *self.dead_since.lock().unwrap_or_else(|p| p.into_inner()) = None;
                            return Ok(self
                                .axis_cache
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .get(&(kind, row))
                                .map(|(_, v)| *v)
                                .unwrap_or([0; 30]));
                        }
                        continue;
                    }
                    if b.first().copied() == Some(grp::PERFORMANCE)
                        && b.get(1).copied() == Some(0xFF)
                    {
                        return Err(HubError::Unsupported("command"));
                    }
                    if self.is_foreign_cmd(&b) {
                        // 他人のコマンド応答 → 受動化 (デバイス側はその
                        // コマンドで既に解除済み、こちらからは送らない)。
                        self.note_foreign();
                    }
                }
                Err(HubError::Protocol(_)) => {}
                Err(e) => return Err(e),
            }
        }
        let f = self.fails.fetch_add(1, Ordering::Relaxed) + 1;
        if f >= FAIL_FAST_AFTER {
            *self.dead_since.lock().unwrap_or_else(|p| p.into_inner()) = Some(Instant::now());
            return Err(HubError::Disconnected);
        }
        Err(HubError::Protocol("xsyd: axis response timeout".into()))
    }

    /// 高度キー読み `[6,1,row,col,0]` → mode!=NONE なら Advanced をデコード。
    fn read_higher(&self, layout_index: usize) -> HubResult<Option<Advanced>> {
        let key = &registry::su66max_layout()[layout_index];
        let (row, col) = registry::su66max_matrix_pos(key.id);
        let r = self.cmd(grp::HIGHER_KEY, sub::READ, &[row, col, hk::NONE])?;
        Ok(decode_higher(&r))
    }

    /// 高度キー書き `[6,2,row,col,mode,...]`。SOCD/RS は 2 パケット。
    /// `binding=None` で解除 (mode=NONE)。
    fn write_higher(&self, layout_index: usize, binding: Option<&Advanced>) -> HubResult<()> {
        let key = &registry::su66max_layout()[layout_index];
        let (row, col) = registry::su66max_matrix_pos(key.id);
        for p in higher_packets(row, col, binding)? {
            self.cmd(grp::HIGHER_KEY, sub::SETTING, &p)?;
        }
        Ok(())
    }

    /// マクロモード `[7,1,id]` → (valid, actNum, repNum, mode)。
    fn read_macro_mode(&self, id: u8) -> HubResult<(bool, u16, u16, u8)> {
        let r = self.cmd(grp::MACRO, sub::GET_MACRO_MODE, &[id])?;
        Ok((
            r[3] == 1,
            u16::from_le_bytes([r[4], r[5]]),
            u16::from_le_bytes([r[6], r[7]]),
            r[8],
        ))
    }

    /// マクロデータ `[7,3,id,offset]` — offset 単位で 15 イベントずつ取得。
    fn read_macro_events(&self, id: u8, count: u16) -> HubResult<Vec<MacroEvent>> {
        let mut out = Vec::new();
        let mut offset = 0u8;
        while (out.len() as u16) < count.min(240) {
            let r = self.cmd(grp::MACRO, sub::GET_MACRO, &[id, offset])?;
            for i in 0..15 {
                let w =
                    u32::from_le_bytes([r[4 + i * 4], r[5 + i * 4], r[6 + i * 4], r[7 + i * 4]]);
                let keycode = w & 0xFFFF;
                let delay = (w >> 16) & 0x7FFF;
                let down = (w >> 31) & 1;
                if keycode == 0 && delay == 0 && down == 0 {
                    break;
                }
                out.push(MacroEvent {
                    input: MacroInput::Key(keycode as u16),
                    edge: if down == 1 {
                        MacroEdge::Down
                    } else {
                        MacroEdge::Up
                    },
                    delay_ms: delay,
                });
                if out.len() as u16 >= count {
                    break;
                }
            }
            offset += 1;
            if r[3] != offset - 1 {
                break; // offset 不一致 = データ終端
            }
        }
        Ok(out)
    }

    /// マクロ書き込み。mode パケット → data パケット (15 イベントずつ) の順。
    fn write_macro(&self, slot: u8, def: Option<&MacroDef>) -> HubResult<()> {
        match def {
            None => {
                self.cmd(grp::MACRO, sub::SET_MACRO_MODE, &[slot, 0, 0, 0, 0, 0, 0])?;
            }
            Some(d) => {
                let act = (d.events.len() as u16).to_le_bytes();
                let rep = d.repeat.to_le_bytes();
                self.cmd(
                    grp::MACRO,
                    sub::SET_MACRO_MODE,
                    &[slot, 1, act[0], act[1], rep[0], rep[1], d.mode],
                )?;
                for (i, chunk) in d.events.chunks(15).enumerate() {
                    let mut args = vec![slot, i as u8];
                    for ev in chunk {
                        let key = match ev.input {
                            MacroInput::Key(k) => k as u32,
                            MacroInput::Mouse(f) => f as u32,
                        };
                        let w = ((ev.edge == MacroEdge::Down) as u32) << 31
                            | ev.delay_ms.min(32767) << 16
                            | (key & 0xFFFF);
                        args.extend_from_slice(&w.to_le_bytes());
                    }
                    self.cmd(grp::MACRO, sub::SET_MACRO, &args)?;
                }
            }
        }
        Ok(())
    }

    /// 全ストローク内の route 値 (mm) を layout index 順に並べる。
    fn route_mm(&self) -> HubResult<Vec<f32>> {
        let layout = registry::su66max_layout();
        let mut out = vec![0.0f32; layout.len()];
        for row in 1..=5u8 {
            match self.axis_row(row, axis::ROUTE) {
                Ok(vals) => {
                    for (i, k) in layout.iter().enumerate() {
                        let (kr, kc) = registry::su66max_matrix_pos(k.id);
                        if kr == row {
                            out[i] = vals[kc as usize] as f32 / 1000.0;
                        }
                    }
                }
                Err(HubError::Unsupported(_)) | Err(HubError::Protocol(_)) => break,
                Err(e) => return Err(e),
            }
        }
        Ok(out)
    }

    /// `[5,1,area,0]` → 照明基本 (SDK lightingBaseData): r[4]=open, r[5]=mode
    /// (0=固定色, 1..16=ダイナミック = Y1 enum), r[6]=輝度 0-100,
    /// r[7]=速度 0-100, r[8]=方向 (0=Forward,1=Backward), r[9]=選択パレット色。
    /// open は DoubleLighting 機では jo enum (0=Close,1=OpenDown,2=OpenUp,3=Open)、
    /// SingleLighting では真偽 (0=Close, 非0=Open)。どちらでも非0=点灯として読む。
    /// 固定色色はパレット `[5,1,area,1]` の選択スロットから取る。
    fn read_lighting(&self, area: u8) -> HubResult<LightingState> {
        let r = self.cmd(grp::LIGHTING, sub::READ, &[area, 0])?;
        let sel = (r[9] as usize).min(7);
        // パレット: [5,1,area,1] → r[4+4i..] = [B,G,R,H]×8
        let palette: Vec<Rgb> = self
            .cmd(grp::LIGHTING, sub::READ, &[area, 1])
            .map(|p| {
                (0..8)
                    .map(|i| Rgb::new(p[6 + i * 4], p[5 + i * 4], p[4 + i * 4]))
                    .collect()
            })
            .unwrap_or_default();
        let color = palette.get(sel).copied().unwrap_or(Rgb::new(255, 255, 255));
        // 彩度 (ColorCorrection): [5,1,area,2] → r[4,5,6] = R,G,B (公式 LightingSaturation)。
        let color_correction = self
            .cmd(grp::LIGHTING, sub::READ, &[area, 2])
            .ok()
            .map(|c| Rgb::new(c[4], c[5], c[6]));
        let mut per_key = BTreeMap::new();
        if area == 0 {
            // [5,3,0,chunk] → 15 セル×4B (B,G,R,isCustom)。9 チャンク=135 セル。
            let layout = registry::su66max_layout();
            let mut cell_key = [usize::MAX; 135];
            for (i, k) in layout.iter().enumerate() {
                if let Some(&cell) = led_cells(k).first() {
                    if cell < 135 {
                        cell_key[cell] = i;
                    }
                }
            }
            // DD ピンの読み戻しにはプレビュー色しかない。本来のカスタム色は退避から読む。
            let ddp = self
                .dd_pins
                .lock()
                .map(|p| p.get(&area).cloned().unwrap_or_default())
                .unwrap_or_default();
            let saved = self
                .dd_pre
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get(&area)
                .cloned()
                .unwrap_or_default();
            // 全チャンク必須 — 1 チャンクでも落ちたら「その範囲にカスタム色が
            // 無い」ではなく読み出し失敗。部分読み出しを完全な状態として返すと
            // 後の write_per_key (未指定セルは isCustom=0 消去) が実機の色を消す。
            for c in 0..9u8 {
                let rb = match self.read_dd_chunk(0, c) {
                    Ok(rb) => rb,
                    Err(HubError::Unsupported(_)) => break,
                    Err(e) => return Err(e),
                };
                for i in 0..15usize {
                    let cell = c as usize * 15 + i;
                    let bgr = if ddp.contains_key(&cell) {
                        saved.get(&cell).copied()
                    } else {
                        (rb[7 + i * 4] == 255)
                            .then(|| [rb[4 + i * 4], rb[5 + i * 4], rb[6 + i * 4]])
                    };
                    if let (Some([b, g, r]), true) = (bgr, cell_key[cell] != usize::MAX) {
                        per_key.insert(cell_key[cell], Rgb::new(r, g, b));
                    }
                }
            }
        }
        // open バイト: DoubleLighting 機の Keyboard → jo {0=Close,1=OpenDown,2=OpenUp,3=Open}、
        // それ以外は真偽。どちらでも非 0 = 点灯として読む。
        let open = r[4];
        let zone_on = if area == 0 && self.xs.double_lighting {
            vec![open == 2 || open == 3, open == 1 || open == 3]
        } else {
            vec![]
        };
        Ok(LightingState {
            on: open != 0,
            // mode バイトはモード一覧のインデックスそのまま (0..19)。
            effect: r[5].min(19),
            color,
            brightness: r[6].min(100),
            speed: r[7].min(100),
            direction: r[8].min(1),
            per_key,
            zone_on,
            palette,
            palette_sel: r[9].min(7),
            color_correction,
        })
    }

    /// `[5,4,0,chunk,…15セル×4B]` — カスタム色を書き込む。
    /// `per_key` に含まれるセルは isCustom=255、それ以外は isCustom=0 で書く
    /// (カスタム状態は `per_key` が絶対状態 — 未指定セルはクリアされる。
    /// 公式と同じく色バイトは既存値を保持し、フラグだけ変える)。
    fn write_per_key(&self, per_key: &BTreeMap<usize, Rgb>) -> HubResult<()> {
        let layout = registry::su66max_layout();
        // 現行 135 セルを読み込む (未指定セルの色バイト保持 + 変更検出のため)
        let mut cells = [[0u8; 4]; 135];
        for c in 0..9u8 {
            if let Ok(rb) = self.cmd(grp::LIGHTING, sub::LIGHT_READBACK, &[0, c]) {
                for i in 0..15usize {
                    cells[c as usize * 15 + i].copy_from_slice(&rb[4 + i * 4..8 + i * 4]);
                }
            }
        }
        let prev = cells;
        for c in cells.iter_mut() {
            c[3] = 0;
        }
        for (&idx, rgb) in per_key {
            if let Some(k) = layout.get(idx) {
                for cell in led_cells(k) {
                    if cell < 135 {
                        cells[cell] = [rgb.b, rgb.g, rgb.r, 255];
                    }
                }
            }
        }
        // 変化した chunk だけ送信する
        for c in 0..9u8 {
            let lo = c as usize * 15;
            let hi = lo + 15;
            if cells[lo..hi] == prev[lo..hi] {
                continue;
            }
            let mut args = vec![0u8, c];
            for cell in &cells[lo..hi] {
                args.extend_from_slice(cell);
            }
            self.cmd(grp::LIGHTING, sub::LIGHT_CUSTOM, &args)?;
        }
        Ok(())
    }

    /// `[5,2,area,0, open, mode, 輝度, 速度, 方向, パレット選択]` (SDK setLightingBase)。
    /// mode はモード一覧のインデックス (0..19)。カスタム色はモードではなく
    /// isCustom セルのオーバーレイなので mode には触れない。
    /// open: Keyboard (area 0) は DoubleLighting — jo enum で上/下ゾーンを
    /// 個別に開閉する (`zone_on = [上, 下]` → 両方=3, 上のみ=2, 下のみ=1, 両方閉=0)。
    /// `zone_on` が無い状態では on → 3 (両ゾーン点灯)。それ以外のエリアは真偽 (0/1)。
    /// 色はパレット — `palette_sel` を選択スロットとして書き、`palette` が
    /// 渡されていれば全スロットを、さもなければ選択スロットに `color` を適用する。
    /// open バイトを状態から組み立てる。Keyboard (area 0) + DoubleLighting は
    /// jo enum (`zone_on=[上,下]` → 両方=3, 上のみ=2, 下のみ=1, 閉=0)、
    /// `zone_on` が無ければ on→3。それ以外のエリアは真偽 (0/1)。
    fn compose_open(&self, area: u8, s: &LightingState) -> u8 {
        if area == 0 && self.xs.double_lighting {
            if s.zone_on.len() >= 2 {
                match (s.zone_on[0], s.zone_on[1]) {
                    (true, true) => 3,
                    (true, false) => 2,
                    (false, true) => 1,
                    (false, false) => 0,
                }
            } else {
                if s.on {
                    3
                } else {
                    0
                }
            }
        } else {
            u8::from(s.on)
        }
    }

    fn write_lighting(&self, area: u8, s: &LightingState) -> HubResult<()> {
        let cur = self.cmd(grp::LIGHTING, sub::READ, &[area, 0])?;
        let open = self.compose_open(area, s);
        // 現行と同じベースなら書き込まない (書いた分だけ HID 往復を減らす)。
        let base = [
            open,
            s.effect.min(19),
            s.brightness.min(100),
            s.speed.min(100),
            s.direction.min(1),
            s.palette_sel.min(7),
        ];
        if cur[4..10] != base {
            let mut args = vec![area, 0];
            args.extend_from_slice(&base);
            self.cmd(grp::LIGHTING, sub::SETTING, &args)?;
        }
        // パレット ([B,G,R,H]×8): 選択スロットに s.color、また s.palette に
        // スロットごとの色があればそれも反映。現行と同じなら書き込まない。
        if let Ok(pal) = self.cmd(grp::LIGHTING, sub::READ, &[area, 1]) {
            let mut bytes = pal[4..36].to_vec();
            for (i, c) in s.palette.iter().take(8).enumerate() {
                bytes[i * 4] = c.b;
                bytes[i * 4 + 1] = c.g;
                bytes[i * 4 + 2] = c.r;
            }
            let sel = s.palette_sel.min(7) as usize;
            bytes[sel * 4] = s.color.b;
            bytes[sel * 4 + 1] = s.color.g;
            bytes[sel * 4 + 2] = s.color.r;
            if bytes != pal[4..36] {
                let mut args = vec![area, 1];
                args.extend_from_slice(&bytes);
                self.cmd(grp::LIGHTING, sub::SETTING, &args)?;
            }
        }
        // 彩度 (ColorCorrection): [5,2,area,2,R,G,B]。現行と同じなら書かない。
        if let Some(cc) = s.color_correction {
            if let Ok(cur) = self.cmd(grp::LIGHTING, sub::READ, &[area, 2]) {
                if cur[4] != cc.r || cur[5] != cc.g || cur[6] != cc.b {
                    self.cmd(
                        grp::LIGHTING,
                        sub::SETTING,
                        &[area, 2, cc.r.min(100), cc.g.min(100), cc.b.min(100)],
                    )?;
                }
            }
        }
        Ok(())
    }

    /// `import_config` — DeviceState を丸ごと書き戻す (各節は既存の
    /// write_* を使い、節ごとに save)。プロファイル切替は行わない。
    fn import_state(&self, st: &DeviceState) -> HubResult<()> {
        let layout = registry::su66max_layout();
        if let Some(kb) = &st.keyboard {
            // キーマップ: レイヤーごとに stream を組み立てて一括書き込み。
            for (layer, actions) in kb.keymap.iter().enumerate().take(self.xs.layers.into()) {
                let mut stream = vec![1u16; 8 * 30];
                for (i, a) in actions.iter().enumerate() {
                    let Some(k) = layout.get(i) else { break };
                    let (row, col) = registry::su66max_matrix_pos(k.id);
                    stream[row as usize * 30 + col as usize] = action_to_code(a).unwrap_or(1);
                }
                self.write_stream(layer as u8, &stream)?;
            }
            self.save(scope::LAYOUT)?;
            // キーごと性能。
            for (i, p) in kb.perf.iter().enumerate() {
                let Some(k) = layout.get(i) else { break };
                let (row, col) = registry::su66max_matrix_pos(k.id);
                self.write_perf(row, col, p)?;
            }
            self.save(scope::PERFORMANCE)?;
            // 高度キー: 絶対状態 (ファイルに無いキーは解除)。
            let bound: BTreeMap<usize, &Advanced> =
                kb.advanced.iter().map(|a| (a.key, &a.binding)).collect();
            for i in 0..layout.len() {
                self.write_higher(i, bound.get(&i).copied())?;
            }
            self.save(scope::HIGHER_KEY)?;
            // マクロ。
            for (i, m) in kb.macros.iter().enumerate() {
                self.write_macro(i as u8, m.as_ref())?;
            }
            self.save(scope::MACRO)?;
        }
        for (area, s) in &st.lighting {
            if !self.xs.light_areas.contains(area) {
                continue;
            }
            if *area == 0 {
                self.write_per_key(&s.per_key)?;
            }
            self.write_lighting(*area, s)?;
        }
        self.save(scope::LIGHTING)?;
        for (id, v) in &st.settings {
            self.write_setting(id, v)?;
        }
        Ok(())
    }

    /// 公式 configurator (hub.xsyd.top) のエクスポート `keyboard_config.json`。
    /// `{"data": "<base64>"}` は CryptoJS `AES.encrypt(json, passphrase)` —
    /// OpenSSL `Salted__` 形式の AES-256-CBC + EVP_BytesToKey(MD5)。
    /// パスフレーズは公式 SDK バンドルにハードコードされている。
    /// `keyboards` を持つ平文 JSON はそのまま返す。
    fn official_plain(&self, v: &serde_json::Value) -> HubResult<serde_json::Value> {
        use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
        use base64::Engine;
        const PASS: &[u8] = b"fDPy6vvnpPsYm2T0g1bh";
        let Some(b64) = v.get("data").and_then(|d| d.as_str()) else {
            return Ok(v.clone());
        };
        let raw = base64::engine::general_purpose::STANDARD
            .decode(b64.trim())
            .map_err(|e| HubError::Invalid(format!("暗号化データ: {e}")))?;
        if raw.len() < 48 || &raw[..8] != b"Salted__" {
            return Err(HubError::Invalid("暗号化データの形式が違います".into()));
        }
        // EVP_BytesToKey: D_i = MD5(D_{i-1} | pass | salt) 連結 → key 32B + iv 16B
        use md5::{Digest, Md5};
        let mut keyiv = Vec::with_capacity(48);
        let mut prev: Vec<u8> = Vec::new();
        while keyiv.len() < 48 {
            let mut h = Md5::new();
            h.update(&prev);
            h.update(PASS);
            h.update(&raw[8..16]);
            prev = h.finalize().to_vec();
            keyiv.extend_from_slice(&prev);
        }
        let mut ct = raw[16..].to_vec();
        let pt = cbc::Decryptor::<aes::Aes256>::new_from_slices(&keyiv[..32], &keyiv[32..48])
            .map_err(|e| HubError::Invalid(format!("AES: {e}")))?
            .decrypt_padded_mut::<Pkcs7>(&mut ct)
            .map_err(|_| HubError::Invalid("復号に失敗しました".into()))?;
        serde_json::from_slice(pt).map_err(|e| HubError::Invalid(format!("復号 JSON: {e}")))
    }

    /// 公式エクスポート → DeviceState (書き込みは `import_state` に任せる)。
    /// ・キーマップ: `customKeys.win.fn{L}.bindKeyValue` → レイヤー L (wire code そのまま)
    /// ・高度キー: `advancedKeys.advancedType` → Advanced
    /// ・性能: `performance` をキーごとにそのまま (公式も全キーに書く)
    /// ・ライト: `v2light.{Keyboard,DecorateN}` — パレット色等の未指定項目は現行値を保持
    /// ・マクロ: `macro.v2list` (無効スロットはクリア)
    /// ・設定: `system.rateOfReturn`/`currentKeyboardSystem`
    /// MAC 側キーマップは書かない (ファイルの mac セクションはテンプレート値)。
    fn official_to_state(&self, v: &serde_json::Value) -> HubResult<DeviceState> {
        let layout = registry::su66max_layout();
        // pid/vid 照合 — 別モデルのファイルを書き込まない (公式も同じ検査をする)
        if let Some(s) = v.get("system") {
            let num = |k: &str| s.get(k).and_then(|x| x.as_u64()).unwrap_or(0) as u32;
            let (vid, pid) = (num("vid"), num("pid"));
            if (vid, pid) != (self.info.vid as u32, self.info.pid as u32) {
                return Err(HubError::Invalid(format!(
                    "このファイルは別モデルのキーボード用です ({vid:04x}:{pid:04x})"
                )));
            }
        }
        // (row, col) → キー行
        let mut grid = std::collections::HashMap::new();
        if let Some(rows) = v.get("keyboards").and_then(|k| k.as_array()) {
            for ko in rows.iter().flat_map(|r| r.as_array().into_iter().flatten()) {
                if let (Some(r), Some(c)) = (
                    ko.get("row").and_then(|x| x.as_u64()),
                    ko.get("col").and_then(|x| x.as_u64()),
                ) {
                    grid.insert((r as u8, c as u8), ko);
                }
            }
        }
        if grid.is_empty() {
            return Err(HubError::Invalid("キーボードデータがありません".into()));
        }
        // ---- キーマップ (win.fn0-3 → レイヤー 0-3) ----
        let nlayers = (self.xs.layers as usize).min(4);
        let keymap: Vec<Vec<KeyAction>> = (0..nlayers)
            .map(|l| {
                layout
                    .iter()
                    .map(|k| {
                        let (row, col) = registry::su66max_matrix_pos(k.id);
                        let code = grid
                            .get(&(row, col))
                            .and_then(|ko| official_bind(ko, l))
                            .unwrap_or(1);
                        code_to_action(code)
                    })
                    .collect()
            })
            .collect();
        // ---- 高度キー ----
        let mut advanced = Vec::new();
        for (i, k) in layout.iter().enumerate() {
            let (row, col) = registry::su66max_matrix_pos(k.id);
            let Some(ak) = grid.get(&(row, col)).and_then(|ko| ko.get("advancedKeys")) else {
                continue;
            };
            if let Some(binding) = official_advanced(ak, &layout) {
                advanced.push(AdvancedKey {
                    key: i,
                    layer: 0,
                    binding,
                });
            }
        }
        // ---- キー性能 (公式も全キーに書く。axis 系は write_perf が現行値を保持) ----
        let perf: Vec<KeyPerf> = layout
            .iter()
            .map(|k| {
                let (row, col) = registry::su66max_matrix_pos(k.id);
                let p = grid.get(&(row, col)).and_then(|ko| ko.get("performance"));
                let num = |n: &str| {
                    p.and_then(|x| x.get(n))
                        .and_then(|x| x.as_f64())
                        .unwrap_or(0.)
                };
                // ファイル値は既に mm (公式は wire 送信時に ×1000 する)
                let mm = |n: &str| num(n) as f32;
                let mode = num("mode") as u8;
                KeyPerf {
                    mode: if mode == 0 {
                        TriggerMode::Normal
                    } else {
                        TriggerMode::Rapid
                    },
                    continuous: mode == 1,
                    actuation: mm("normalPress"),
                    release: Some(mm("normalRelease")),
                    rt_first: mm("rtFirstTouch"),
                    rt_press: mm("rtPress"),
                    rt_release: mm("rtRelease"),
                    top_deadzone: mm("pressDeadStroke"),
                    bottom_deadzone: mm("releaseDeadStroke"),
                    switch_type: num("axis") as u8,
                }
            })
            .collect();
        // ---- マクロ (v2list) ----
        let mut macros = Vec::new();
        if let Some(list) = v
            .get("macro")
            .and_then(|m| m.get("v2list"))
            .and_then(|m| m.as_array())
        {
            for m in list
                .iter()
                .take(self.xs.macro_space.map(|x| x.0).unwrap_or(16).into())
            {
                let valid = m.get("valid").and_then(|x| x.as_bool()).unwrap_or(false);
                let events: Vec<MacroEvent> = if valid {
                    m.get("data")
                        .and_then(|d| d.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|e| {
                                    let kc = e
                                        .get("keyCode")
                                        .or_else(|| e.get("keycode"))
                                        .and_then(|x| x.as_u64())?
                                        as u16;
                                    Some(MacroEvent {
                                        input: MacroInput::Key(kc),
                                        edge: if e
                                            .get("status")
                                            .and_then(|x| x.as_u64())
                                            .unwrap_or(0)
                                            == 1
                                        {
                                            MacroEdge::Down
                                        } else {
                                            MacroEdge::Up
                                        },
                                        delay_ms: e
                                            .get("delay")
                                            .and_then(|x| x.as_u64())
                                            .unwrap_or(0)
                                            .min(32767)
                                            as u32,
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default()
                } else {
                    vec![]
                };
                macros.push(if valid && !events.is_empty() {
                    Some(MacroDef {
                        name: m.get("name").and_then(|x| x.as_str()).unwrap_or("").into(),
                        mode: m.get("mode").and_then(|x| x.as_u64()).unwrap_or(0) as u8,
                        repeat: m.get("repNum").and_then(|x| x.as_u64()).unwrap_or(0) as u16,
                        events,
                    })
                } else {
                    None
                });
            }
        }
        // ---- ライティング (v2light) ----
        let mut lighting = BTreeMap::new();
        if let Some(areas) = v.get("v2light").and_then(|x| x.as_object()) {
            for (name, ad) in areas {
                let area: u8 = match name.as_str() {
                    "Keyboard" => 0,
                    n if n.starts_with("Decorate") => {
                        n.trim_start_matches("Decorate").parse().unwrap_or(255)
                    }
                    _ => continue,
                };
                if !self.xs.light_areas.contains(&area) {
                    continue;
                }
                let light = &ad["light"];
                // ファイルに無い項目 (パレット色・彩度) は現行値をベースに保持
                let mut s = self.read_lighting(area).unwrap_or_else(|_| LightingState {
                    on: true,
                    effect: 0,
                    color: Rgb::new(255, 255, 255),
                    brightness: 80,
                    speed: 80,
                    direction: 0,
                    per_key: BTreeMap::new(),
                    zone_on: vec![],
                    palette: vec![],
                    palette_sel: 0,
                    color_correction: None,
                });
                let zones: [bool; 2] = match light.get("open").and_then(|x| x.as_str()) {
                    Some("Open") => [true, true],
                    Some("OpenUp") => [true, false],
                    Some("OpenDown") => [false, true],
                    _ => [false, false],
                };
                s.on = zones[0] || zones[1];
                if s.zone_on.len() >= 2 {
                    s.zone_on = zones.to_vec();
                }
                let num = |n: &str| light.get(n).and_then(|x| x.as_u64());
                if let Some(m) = num("mode") {
                    s.effect = m as u8;
                }
                if let Some(m) = num("luminance") {
                    s.brightness = m as u8;
                }
                if let Some(m) = num("speed") {
                    s.speed = m as u8;
                }
                s.direction = match light.get("direction").and_then(|x| x.as_str()) {
                    Some("Backward") => 1,
                    _ => 0,
                };
                if let Some(m) = num("selectStaticColor") {
                    s.palette_sel = (m as u8).min(7);
                    // sel だけ変えると write_lighting が新スロットを
                    // 旧スロットの色で上書きする — 選択先の色に合わせる。
                    if let Some(c) = s.palette.get(s.palette_sel as usize).copied() {
                        s.color = c;
                    }
                }
                // キーごとのカスタム色 → per_key (isCustom のみ)
                if area == 0 {
                    s.per_key = layout
                        .iter()
                        .enumerate()
                        .filter_map(|(i, k)| {
                            let (row, col) = registry::su66max_matrix_pos(k.id);
                            let cell =
                                grid.get(&(row, col)).and_then(|ko| ko.get("customLight"))?;
                            cell.get("isCustom")
                                .and_then(|x| x.as_bool())
                                .unwrap_or(false)
                                .then(|| {
                                    let u = |n: &str| {
                                        cell.get(n).and_then(|x| x.as_u64()).unwrap_or(0) as u8
                                    };
                                    (i, Rgb::new(u("R"), u("G"), u("B")))
                                })
                        })
                        .collect();
                }
                lighting.insert(area, s);
            }
        }
        // ---- 汎用設定 ----
        let mut settings = BTreeMap::new();
        if let Some(s) = v.get("system") {
            if let Some(r) = s.get("rateOfReturn").and_then(|x| x.as_u64()) {
                settings.insert("polling".into(), SettingValue::Choice(r as i64));
            }
            if let Some(t) = s.get("currentKeyboardSystem").and_then(|x| x.as_str()) {
                settings.insert(
                    "system_type".into(),
                    SettingValue::Choice(if t == "MAC" { 1 } else { 0 }),
                );
            }
        }
        Ok(DeviceState {
            profile: self.read_profile().unwrap_or_default(),
            settings,
            mouse: None,
            keyboard: Some(KeyboardState {
                keymap,
                perf,
                advanced,
                macros,
            }),
            lighting,
            peq: None,
        })
    }
}

/// 公式 `customKeys.win.fn{L}.bindKeyValue` → wire code。
/// 数値はそのまま、`"@{...keycode=N}"` テンプレートは keycode を拾う。
fn official_bind(ko: &serde_json::Value, layer: usize) -> Option<u16> {
    let b = ko
        .get("customKeys")?
        .get("win")?
        .get(format!("fn{layer}"))?
        .get("bindKeyValue")?;
    match b {
        serde_json::Value::Number(n) => n.as_u64().map(|v| v as u16),
        serde_json::Value::String(s) => s
            .split("keycode=")
            .nth(1)
            .and_then(|t| t.trim_end_matches('}').trim().parse().ok()),
        _ => None,
    }
}

/// 公式 `advancedKeys` オブジェクト → Advanced (advancedType 0/欠落は None)。
fn official_advanced(ak: &serde_json::Value, layout: &[LayoutKey]) -> Option<Advanced> {
    let ty = ak.get("advancedType").and_then(|x| x.as_u64())?;
    let num = |v: &serde_json::Value, k: &str| v.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
    let codes = |v: &serde_json::Value, k: &str| -> Vec<u32> {
        v.get(k)
            .and_then(|x| x.as_array())
            .into_iter()
            .flatten()
            .filter_map(|x| x.as_u64().map(|n| n as u32))
            .collect()
    };
    // ペア相手の (row2,col2) → layout index
    let pair = |v: &serde_json::Value| -> usize {
        let pos = (num(v, "row2") as u8, num(v, "col2") as u8);
        layout
            .iter()
            .position(|k| registry::su66max_matrix_pos(k.id) == pos)
            .unwrap_or(0)
    };
    match ty {
        1 => {
            let d = &ak["dks"];
            let rows = codes(d, "dks")
                .iter()
                .enumerate()
                .map(|(i, &code)| DksRow {
                    code,
                    phases: trp_to_phases(
                        d.get("trps")
                            .and_then(|x| x.as_array())
                            .and_then(|a| a.get(i))
                            .and_then(|x| x.as_u64())
                            .unwrap_or(0) as u8,
                    ),
                })
                .collect();
            Some(Advanced::Dks {
                rows,
                bottom: num(d, "db2") as f32 / 1000.0,
            })
        }
        2 => {
            let d = &ak["mpt"];
            let cs = codes(d, "dks");
            let mut depths = [0f32; 3];
            for (i, v) in codes(d, "dbs").iter().take(3).enumerate() {
                depths[i] = *v as f32 / 1000.0;
            }
            let mut out = [0u32; 3];
            for (i, &c) in cs.iter().take(3).enumerate() {
                out[i] = c;
            }
            Some(Advanced::Mpt { depths, codes: out })
        }
        3 => {
            let d = &ak["mt"]["mt"];
            let cs = codes(d, "dksAll");
            Some(Advanced::Mt {
                tap: cs.first().copied().unwrap_or(0),
                hold: cs.get(1).copied().unwrap_or(0),
                hold_ms: num(d, "delay") as u16,
            })
        }
        4 => {
            let d = &ak["tgl"]["tgl"];
            Some(Advanced::Tgl {
                code: num(d, "kc") as u32,
                delay_ms: num(d, "delay") as u16,
            })
        }
        5 => {
            let d = &ak["end"]["end"];
            let cs = codes(d, "dks");
            Some(Advanced::End {
                code: cs.first().copied().unwrap_or(0),
                code_release: cs.get(1).copied().unwrap_or(0),
                delay_ms: num(d, "delay") as u16,
            })
        }
        6 => {
            // 実データは wrapper (advancedKeys.socd) の内側 socd オブジェクト。
            // socdMode は wrapper 側 (公式 setHigherKeySOCD と同じ)。
            let w = &ak["socd"];
            let d = &w["socd"];
            let cs = codes(d, "kcs");
            Some(Advanced::Socd {
                other: pair(d),
                code_a: cs.first().copied().unwrap_or(0),
                code_b: cs.get(1).copied().unwrap_or(0),
                delay_ms: num(d, "delay") as u16,
                mode: match num(w, "socdMode") {
                    1 => SocdMode::PriorityA,
                    2 => SocdMode::PriorityB,
                    3 => SocdMode::Neutral,
                    _ => SocdMode::Last,
                },
            })
        }
        7 => {
            let d = &ak["rs"]["rs"];
            let cs = codes(d, "kcs");
            Some(Advanced::Rs {
                other: pair(d),
                code_a: cs.first().copied().unwrap_or(0),
                code_b: cs.get(1).copied().unwrap_or(0),
                delay_ms: num(d, "delay") as u16,
            })
        }
        _ => None,
    }
}

/// ファームウェア文字列: device_info 応答末尾の ASCII 塊を拾う
/// (実機: "... 49142 .. 2026101216:41:03" — 末尾がビルド時刻)。
fn parse_fw(r: &[u8; 64]) -> Option<String> {
    let tail: String = r[2..]
        .iter()
        .map(|&b| {
            if (0x20..0x7f).contains(&b) {
                b as char
            } else {
                ' '
            }
        })
        .collect();
    tail.split_whitespace().last().map(|s| s.to_string())
}

/// layout キー → カスタム LED のセル番号 (通常 1 個、分割スペースは複数 LED を持つ)。
///
/// LED グリッドは 6 行×21 列 = 126 セル (実機目視で確定、SDK 既定値とも一致)。
/// 行 1-4 は `cell = 行*21 + floor(x)`。行 5 は右側が圧縮・デッドセル混在のため
/// 実測テーブル: LCtrl=105, LWin=106, LAlt=107, スペースバー下=109-113,
/// RAlt=114, Fn=115, ←=117, ↓=118, →=119 (108/116/120-125 はデッドセル)。
fn led_cells(k: &devhub_core::model::LayoutKey) -> Vec<usize> {
    let row = (k.id >> 4) as usize;
    if row == 5 {
        match k.id & 0x0f {
            0 => vec![105],      // LCtrl
            1 => vec![106],      // LWin
            2 => vec![107],      // LAlt
            4 => vec![109, 110], // Space 左
            6 => vec![111],      // Space 中
            8 => vec![112, 113], // Space 右
            9 => vec![114],      // RAlt
            10 => vec![115],     // Fn
            12 => vec![117],     // ←
            13 => vec![118],     // ↓
            14 => vec![119],     // →
            _ => vec![],
        }
    } else {
        // LED 列 = マトリクス列 (probe10 split で実機確定: '=74, Enter=76,
        // Del=98, ↑=97)。視覚 x は幅キーでずれるので使わない。
        // 死んだマトリクス pad の LED は隣の幅キーの下に実在するので併せて書く:
        //   row3 col12 (=cell75) は ' の右半分下、row4 col1 (=cell85) は LShift 下。
        let col = (k.id & 0x0f) as usize;
        match (row, col) {
            (3, 11) => vec![74, 75], // ' (+右端の死pad)
            (4, 0) => vec![84, 85],  // LShift (+右の死pad)
            _ => vec![row * 21 + col],
        }
    }
}

/// DKS trps ビットマスク → 契約の 4 フェーズ。
/// wire bit 並び (UI 7 チェック列 → 8 bit):
///   b0=押始Tap, b1=押始Hold, b2=底打ちTap, b3|b4=底打ちHold,
///   b5=戻り始めTap, b6=戻り始めHold, b7=全リリースTap。
/// `[6,1]` 応答 r[4]=mode → `Advanced` (公式 higherKey*Data 準拠)。
fn decode_higher(r: &[u8; 64]) -> Option<Advanced> {
    let mm = |i: usize| u16::from_le_bytes([r[i], r[i + 1]]) as f32 / 1000.0;
    let le = |i: usize| u16::from_le_bytes([r[i], r[i + 1]]) as u32;
    let col_to_index = |row2: u8, col2: u8| -> usize {
        registry::su66max_layout()
            .iter()
            .position(|k| registry::su66max_matrix_pos(k.id) == (row2, col2))
            .unwrap_or(0)
    };
    match r[4] {
        hk::DKS => Some(Advanced::Dks {
            rows: (0..4)
                .map(|i| DksRow {
                    code: le(5 + i * 2),
                    phases: trp_to_phases(r[13 + i]),
                })
                .collect(),
            // dbs = [db, db2] = [最小行程, 最大行程]。契約の bottom は底打ち側。
            bottom: mm(19),
        }),
        hk::MPT => Some(Advanced::Mpt {
            depths: [mm(11), mm(13), mm(15)],
            codes: [le(5), le(7), le(9)],
        }),
        hk::MT => Some(Advanced::Mt {
            tap: le(5),
            hold: le(7),
            hold_ms: u16::from_le_bytes([r[9], r[10]]),
        }),
        hk::TGL => Some(Advanced::Tgl {
            code: le(5),
            delay_ms: u16::from_le_bytes([r[7], r[8]]),
        }),
        hk::END => Some(Advanced::End {
            code: le(5),
            code_release: le(7),
            delay_ms: u16::from_le_bytes([r[9], r[10]]),
        }),
        hk::SOCD => Some(Advanced::Socd {
            other: col_to_index(r[5], r[6]),
            code_a: le(7),
            code_b: le(9),
            delay_ms: u16::from_le_bytes([r[11], r[12]]),
            mode: match r[13] {
                1 => SocdMode::PriorityA,
                2 => SocdMode::PriorityB,
                3 => SocdMode::Neutral,
                _ => SocdMode::Last,
            },
        }),
        hk::RS => Some(Advanced::Rs {
            other: col_to_index(r[5], r[6]),
            code_a: le(7),
            code_b: le(9),
            delay_ms: u16::from_le_bytes([r[11], r[12]]),
        }),
        _ => None,
    }
}

/// `[6,2,...]` 書き込みパケット列 (SOCD/RS は 2 パケット、ペアを反転)。
fn higher_packets(row: u8, col: u8, binding: Option<&Advanced>) -> HubResult<Vec<Vec<u8>>> {
    let index_to_pos = |i: usize| -> HubResult<(u8, u8)> {
        registry::su66max_layout()
            .get(i)
            .map(|k| registry::su66max_matrix_pos(k.id))
            .ok_or_else(|| HubError::Invalid("advanced key index".into()))
    };
    let le = |v: u32| (v as u16).to_le_bytes();
    let mm = |v: f32| ((v * 1000.0).round().clamp(0.0, 65535.0) as u16).to_le_bytes();
    let packets: Vec<Vec<u8>> = match binding {
        None => vec![vec![row, col, hk::NONE]],
        Some(Advanced::Dks { rows, bottom }) => {
            let mut p = vec![row, col, hk::DKS];
            let mut rows4: Vec<DksRow> = rows.clone();
            rows4.resize(
                4,
                DksRow {
                    code: 0,
                    phases: [DksPhase::Off; 4],
                },
            );
            for dr in &rows4 {
                p.extend_from_slice(&le(dr.code));
            }
            for dr in &rows4 {
                p.push(phases_to_trp(&dr.phases));
            }
            // dbs = [db, db2]。db は底打ち側の半分を既定に。
            p.extend_from_slice(&mm(bottom / 2.0));
            p.extend_from_slice(&mm(*bottom));
            vec![p]
        }
        Some(Advanced::Mpt { depths, codes }) => {
            let mut p = vec![row, col, hk::MPT];
            for c in codes {
                p.extend_from_slice(&le(*c));
            }
            for d in depths {
                p.extend_from_slice(&mm(*d));
            }
            vec![p]
        }
        Some(Advanced::Mt { tap, hold, hold_ms }) => {
            let mut p = vec![row, col, hk::MT];
            p.extend_from_slice(&le(*tap));
            p.extend_from_slice(&le(*hold));
            p.extend_from_slice(&hold_ms.to_le_bytes());
            vec![p]
        }
        Some(Advanced::Tgl { code, delay_ms }) => {
            let mut p = vec![row, col, hk::TGL];
            p.extend_from_slice(&le(*code));
            p.extend_from_slice(&delay_ms.to_le_bytes());
            vec![p]
        }
        Some(Advanced::End {
            code,
            code_release,
            delay_ms,
        }) => {
            let mut p = vec![row, col, hk::END];
            p.extend_from_slice(&le(*code));
            p.extend_from_slice(&le(*code_release));
            p.extend_from_slice(&delay_ms.to_le_bytes());
            vec![p]
        }
        Some(Advanced::Socd {
            other,
            code_a,
            code_b,
            mode,
            delay_ms,
        }) => {
            let (r2, c2) = index_to_pos(*other)?;
            let (x0, x1) = match mode {
                SocdMode::Last => (0, 0),
                SocdMode::PriorityA | SocdMode::First => (1, 2),
                SocdMode::PriorityB => (2, 1),
                SocdMode::Neutral => (3, 3),
            };
            let mut p1 = vec![row, col, hk::SOCD, r2, c2];
            p1.extend_from_slice(&le(*code_a));
            p1.extend_from_slice(&le(*code_b));
            p1.extend_from_slice(&delay_ms.to_le_bytes());
            p1.push(x0);
            let mut p2 = vec![r2, c2, hk::SOCD, row, col];
            p2.extend_from_slice(&le(*code_b));
            p2.extend_from_slice(&le(*code_a));
            p2.extend_from_slice(&delay_ms.to_le_bytes());
            p2.push(x1);
            vec![p1, p2]
        }
        Some(Advanced::Rs {
            other,
            code_a,
            code_b,
            delay_ms,
        }) => {
            let (r2, c2) = index_to_pos(*other)?;
            let mut p1 = vec![row, col, hk::RS, r2, c2];
            p1.extend_from_slice(&le(*code_a));
            p1.extend_from_slice(&le(*code_b));
            p1.extend_from_slice(&delay_ms.to_le_bytes());
            let mut p2 = vec![r2, c2, hk::RS, row, col];
            p2.extend_from_slice(&le(*code_b));
            p2.extend_from_slice(&le(*code_a));
            p2.extend_from_slice(&delay_ms.to_le_bytes());
            vec![p1, p2]
        }
    };
    Ok(packets)
}

fn trp_to_phases(trp: u8) -> [DksPhase; 4] {
    use DksPhase::*;
    let pick = |tap: u8, hold: u8| {
        if trp & hold != 0 {
            Hold
        } else if trp & tap != 0 {
            Tap
        } else {
            Off
        }
    };
    [
        pick(1, 2),
        pick(4, 8 | 16),
        pick(32, 64),
        if trp & 128 != 0 { Tap } else { Off },
    ]
}

fn phases_to_trp(p: &[DksPhase; 4]) -> u8 {
    let mut t = 0u8;
    match p[0] {
        DksPhase::Tap => t |= 1,
        DksPhase::Hold => t |= 2,
        DksPhase::Off => {}
    }
    match p[1] {
        DksPhase::Tap => t |= 4,
        DksPhase::Hold => t |= 8 | 16,
        DksPhase::Off => {}
    }
    match p[2] {
        DksPhase::Tap => t |= 32,
        DksPhase::Hold => t |= 64,
        DksPhase::Off => {}
    }
    if p[3] == DksPhase::Tap {
        t |= 128;
    }
    t
}

/// 既定レイアウトの刻印 → HID usage (reset 用)。
fn default_code(label: &str) -> u16 {
    match label {
        "Esc" => 0x29,
        "Tab" => 0x2B,
        "Caps" => 0x39,
        "Backspace" => 0x2A,
        "Enter" => 0x28,
        "LShift" => 0xE1,
        "RShift" => 0xE5,
        "LCtrl" => 0xE0,
        "LWin" => 0xE3,
        "LAlt" => 0xE2,
        "RAlt" => 0xE6,
        "Space" => 0x2C,
        "Fn" => 0xF101,
        "Del" => 0x4C,
        "↑" => 0x52,
        "↓" => 0x51,
        "←" => 0x50,
        "→" => 0x4F,
        "-" => 0x2D,
        "=" => 0x2E,
        "[" => 0x2F,
        "]" => 0x30,
        "\\" => 0x31,
        ";" => 0x33,
        "'" => 0x34,
        "," => 0x36,
        "." => 0x37,
        "/" => 0x38,
        "`" => 0x35,
        s if s.len() == 1 && s.as_bytes()[0].is_ascii_alphabetic() => {
            0x04 + (s.as_bytes()[0] - b'A') as u16
        }
        s if s.len() == 1 && s.as_bytes()[0].is_ascii_digit() => {
            let d = s.as_bytes()[0] - b'0';
            if d == 0 {
                0x27
            } else {
                0x1E + d as u16 - 1
            }
        }
        _ => 0,
    }
}

fn default_stream(layer: u8) -> Vec<u16> {
    let mut stream = vec![0u16; 8 * 30];
    for k in registry::su66max_layout() {
        let (row, col) = registry::su66max_matrix_pos(k.id);
        stream[row as usize * 30 + col as usize] = if layer == 0 {
            default_code(&k.label)
        } else {
            1 // 他レイヤーは透過
        };
    }
    stream
}

fn build_caps(xs: &XsydInfo) -> Capabilities {
    use section::*;
    let mut settings = vec![
        SettingSpec {
            id: "polling".into(),
            section: PERFORMANCE.into(),
            label: "ポーリングレート".into(),
            help: None,
            kind: SettingKind::Choice {
                options: xs
                    .polling
                    .iter()
                    .map(|(c, l)| ChoiceOpt::new(*c as i64, *l))
                    .collect(),
            },
            visible_when: None,
            confirm: None,
            enabled_when: None,
        },
        SettingSpec {
            id: "system_type".into(),
            section: KEYBOARD.into(),
            label: "OS 種別".into(),
            help: Some("Win / Mac のキー配列モードを切り替えます".into()),
            kind: SettingKind::Choice {
                options: vec![ChoiceOpt::new(0, "Windows"), ChoiceOpt::new(1, "Mac")],
            },
            visible_when: None,
            confirm: None,
            enabled_when: None,
        },
        SettingSpec::toggle("shake_opt", KEYBOARD, "チャタリング防止"),
        SettingSpec::slider(
            "light_sleep",
            POWER,
            "ライトスリープ",
            0.0,
            120.0,
            1.0,
            "分",
        ),
    ];
    settings[3].help = Some("無操作でライトを消すまでの時間 (分単位)".into());
    // mode バイト 0-19 = 公式 UI の L1-L20 (書き込みは 19 にクランプされるのを実機確認)。
    // OBS カメラによる実機撮像で同定した対応 (docs/PROTOCOL_NOTES.md 参照):
    //  SDK enum Y1 は 1..16 (Ripple..Jump) だが、実機の wire 1..15 は Y1[2..16] に
    //  対応し wire0=パレット静表示、wire16=カスタム表示、wire17-19=キー押下反応。
    //  全モードが選択パレット色を使い、sel=0 はファームの虹 (マルチカラー) 表示。
    let effects = vec![
        LightEffect::new(0, "固定色").color().still(), // 選択パレット色を固定 (sel0=虹)
        LightEffect::new(1, "虹の横ながれ").color().dir(), // 虹グラデが左右にスクロール
        LightEffect::new(2, "ゆっくり変色").color(),   // 均一色 + ゆるやかな色相回転
        LightEffect::new(3, "すばやく変色").color(),   // 色相が速く回転
        LightEffect::new(4, "虹の市松").color(),       // 虹チェッカー盤
        LightEffect::new(5, "虹のめぐり").color(),     // 色相循環グラデーション
        LightEffect::new(6, "虹の明滅").color(),       // 色相循環 + 明滅
        LightEffect::new(7, "ほのかな窓").color().dir(), // ほぼ均一 + 薄い光帯が移動
        LightEffect::new(8, "ゆるやか変形").color(),   // 色パッチが染みのようにモーフ
        LightEffect::new(9, "色帯シフト").color().dir(), // 広い色ゾーンが移動
        LightEffect::new(10, "虹のうねり").color().dir(), // マルチバンドの虹波
        LightEffect::new(11, "斜めの光帯").color().dir(), // 斜めの光帯が掃引
        LightEffect::new(12, "コンフェッティ").color(), // ランダムな色ドットが点在
        LightEffect::new(13, "虹のモザイク").color(),  // 細かい虹チェッカー
        LightEffect::new(14, "光の雨").color().dir(),  // 色ドットが落下
        LightEffect::new(15, "跳ねる光").color(),      // 点灯ブロックが跳んで移動
        LightEffect::new(16, "カスタム").per_key(),    // isCustom セルのみ表示
        LightEffect::new(17, "押下・まわり").color(),  // 押下キー周辺ブロック点灯
        LightEffect::new(18, "押下・行").color(),      // 押下キーの行が点灯
        LightEffect::new(19, "押下・広域").color(),    // 押下キー周囲の広範囲が点灯
    ];
    Capabilities {
        settings,
        actions: vec![
            ActionSpec::new("blackout", LIGHTING, "ブラックアウト切替")
                .help("全 LED を一時消灯します。もう一度押すと元に戻ります。"),
            ActionSpec::new("calibrate", PERFORMANCE, "磁軸キャリブレーション開始")
                .help("キーの押下量センサーを調整します。開始後、全キーを数回強く押してください。"),
            ActionSpec::new("calibrate_finish", PERFORMANCE, "キャリブレーション終了")
                .help("押下データをデバイスに保存してキャリブレーションを終了します。"),
            ActionSpec::new("reset_perf", SYSTEM, "性能設定を初期化")
                .danger("全キーのトリガー・RT・デッドゾーンを初期値に戻します。"),
            ActionSpec::new("reset_keymap", SYSTEM, "キー割り当てを初期化")
                .danger("全レイヤーのキー割り当てを初期値に戻します。"),
            ActionSpec::new("reset_lighting", SYSTEM, "ライティングを初期化")
                .danger("ライティングを初期値に戻します。"),
            ActionSpec::new("reset_advanced", SYSTEM, "高度キーを初期化")
                .danger("すべての DKS/MPT/MT/TGL/END/SOCD/RS を解除します。"),
            ActionSpec::new("export_config", SYSTEM, "設定をエクスポート")
                .help("現在の全設定を JSON ファイルに書き出します。")
                .save_file(&["json"]),
            ActionSpec::new("import_config", SYSTEM, "設定をインポート")
                .help("エクスポートした JSON ファイルから全設定を書き戻します。")
                .file(&["json"]),
            ActionSpec::new("reset_macro", SYSTEM, "マクロを初期化")
                .danger("すべてのマクロを削除します。"),
            ActionSpec::new("factory_reset", SYSTEM, "工場出荷時に戻す")
                .danger("すべての設定 (キー・性能・ライト・マクロ) を初期化します。"),
        ],
        profiles: (xs.configs > 1).then_some(ProfileCaps {
            count: xs.configs,
            renamable: true,
        }),
        mouse: None,
        keyboard: Some(KeyboardCaps {
            layout: registry::su66max_layout(),
            layers: (0..xs.layers)
                .map(|i| {
                    if i == 0 {
                        "メイン".into()
                    } else {
                        format!("Fn{i}")
                    }
                })
                .collect(),
            catalog: xsyd_catalog(),
            // 0x1000|mods<<8|usage (custom-key 画面で確定)。
            combos: true,
            perf: Some(PerfCaps {
                travel: xs.travel,
                min: xs.rt_step,
                step: xs.rt_step,
                rapid_trigger: true,
                separate_release: true,
                rt_first: true,
                deadzones: true,
                continuous_rt: true,
                switch_types: xs.axes.iter().map(|a| format!("軸 {a}")).collect(),
                switch_ids: xs.axes.clone(),
                per_key: true,
                live_travel: true,
            }),
            advanced: vec![
                AdvancedKind::Dks,
                AdvancedKind::Mpt,
                AdvancedKind::Mt,
                AdvancedKind::Tgl,
                AdvancedKind::End,
                AdvancedKind::Socd,
                AdvancedKind::Rs,
            ],
            macros: xs.macro_space.map(|(slots, max_ev)| MacroCaps {
                button_toggle: true,
                slots,
                max_events: max_ev,
                max_delay_ms: 32767,
                modes: vec![
                    ChoiceOpt::new(0, "クリックで実行 (回数指定)"),
                    ChoiceOpt::new(1, "クリックで実行"),
                    ChoiceOpt::new(4, "押している間リピート (離すと即停止)"),
                    ChoiceOpt::new(5, "押している間リピート (完了後停止)"),
                ],
                repeat_modes: vec![0],
                names: false,
                mouse_events: false,
            }),
        }),
        lighting: xs
            .light_areas
            .iter()
            .enumerate()
            .map(|(i, &a)| LightArea {
                id: a,
                name: if a == 0 {
                    "メインキーボード".into()
                } else {
                    format!("デコレーション {i}")
                },
                effects: effects.clone(),
                brightness_max: 100,
                speed_max: 100,
                directions: vec!["正方向".into(), "逆方向".into()],
                switchable: true,
                // Keyboard (area 0) で DoubleLighting 機のみ上/下ゾーン開閉がある。
                zones: if a == 0 && xs.double_lighting {
                    vec!["上部ライト".into(), "下部ライト".into()]
                } else {
                    vec![]
                },
                per_key: a == 0,
                // DirectDrive / ライブ読み戻しはキーボードエリア (per-LED セル) のみ。
                direct_drive: a == 0,
                live_readback: a == 0,
            })
            .collect(),
        battery: false,
        peq: None,
        firmware_feed: None, // caps() で info の vid/pid から入れ直す
    }
}

/// xsyd キーコード空間 (PROTOCOL_NOTES §2.4)。基本キーは HID usage。
fn xsyd_catalog() -> Vec<KeyGroup> {
    let g = |name: &str, v: &[(u32, &str, &str)]| KeyGroup {
        name: name.into(),
        keys: v
            .iter()
            .map(|(c, l, s)| KeyChoice {
                code: *c,
                label: (*l).into(),
                short: Some((*s).into()),
            })
            .collect(),
    };
    vec![
        devhub_core::keys::basic_group(),
        g(
            "マウス",
            &[
                (16640, "マウス左", "M左"),
                (16896, "マウス右", "M右"),
                (17152, "マウス中", "M中"),
                (17408, "マウス進む", "M進"),
                (17664, "マウス戻る", "M戻"),
                (18945, "ホイール上", "W↑"),
                (19201, "ホイール下", "W↓"),
            ],
        ),
        g(
            "メディア",
            &[
                (0x20CD, "再生/一時停止", "⏯"),
                (8373, "次の曲", "⏭"),
                (8374, "前の曲", "⏮"),
                (0x20E9, "音量 +", "🔊"),
                (0x20EA, "音量 −", "🔉"),
                (8418, "ミュート", "🔇"),
            ],
        ),
        g(
            "レイヤー",
            &[
                (61696, "メインレイヤーへ", "L0"),
                (61697, "Fn1 レイヤーへ", "Fn1"),
                (61698, "Fn2 レイヤーへ", "Fn2"),
                (61699, "Fn3 レイヤーへ", "Fn3"),
            ],
        ),
        g(
            "システム",
            &[
                (61957, "構成 1 に切替", "P1"),
                (61958, "構成 2 に切替", "P2"),
                (61961, "Win キーロック", "WinL"),
                (61955, "磁軸キャリブレーション (長押し)", "Cal"),
                (61952, "初期化 (長押し)", "Rst"),
            ],
        ),
        g(
            "ライティング",
            &[
                (62208, "ライティングモード切替", "L模"),
                (62209, "ライティング色切替", "L色"),
                (62210, "明るさ +", "L+"),
                (62211, "明るさ −", "L−"),
                (62215, "ライト ON/OFF", "L⏻"),
            ],
        ),
    ]
}

/// wire u16 → KeyAction。0x0000=未割当, 0x0001=透過,
/// 0x1000|mods<<8|usage = 組み合わせキー (SDK custom-key で確定)。
fn code_to_action(code: u16) -> KeyAction {
    match code {
        0x0000 => KeyAction::None,
        0x0001 => KeyAction::Transparent,
        c if (0x1001..=0x1FFF).contains(&c) => KeyAction::Combo {
            mods: ((c >> 8) & 0x0F) as u8,
            code: (c & 0xFF) as u32,
        },
        // マクロ呼び出し: 0xF500+slot (SDK keycode カタログ macro:62720-62735)。
        c if (0xF500..=0xF50F).contains(&c) => KeyAction::Macro((c - 0xF500) as u8),
        c => KeyAction::Code(c as u32),
    }
}
fn action_to_code(a: &KeyAction) -> HubResult<u16> {
    Ok(match a {
        KeyAction::None => 0,
        KeyAction::Transparent => 1,
        KeyAction::Code(c) => (*c)
            .try_into()
            .map_err(|_| HubError::Invalid("keycode".into()))?,
        KeyAction::Combo { mods, code } => {
            // wire: 0x1000 | mods<<8 | usage (左修飾 bit0-3 のみ)
            if *code > 0xFF {
                return Err(HubError::Invalid("combo keycode".into()));
            }
            0x1000 | ((*mods as u16 & 0x0F) << 8) | (*code as u16 & 0xFF)
        }
        // 0xF500+slot (16 枠まで。実機のスロット数は macro_space 側で制限)。
        KeyAction::Macro(i) if *i < 16 => 0xF500 + *i as u16,
        KeyAction::Macro(_) => return Err(HubError::Invalid("macro slot".into())),
    })
}

impl Drop for XsydDriver {
    fn drop(&mut self) {
        self.release_dd_pins();
    }
}

impl DeviceDriver for XsydDriver {
    fn info(&self) -> DeviceInfo {
        self.info.clone()
    }

    fn caps(&self) -> Capabilities {
        let mut c = self.caps.clone();
        c.firmware_feed = Some(FirmwareFeed::Sparklink {
            vid: self.info.vid,
            pid: self.info.pid,
            board_id: self.info.board_id,
        });
        c
    }

    fn read(&self) -> HubResult<DeviceState> {
        let layout = registry::su66max_layout();
        let mut keymap = Vec::with_capacity(self.xs.layers as usize);
        for layer in 0..self.xs.layers {
            let stream = self.read_keymap(layer)?;
            keymap.push(
                layout
                    .iter()
                    .map(|k| {
                        let (row, col) = registry::su66max_matrix_pos(k.id);
                        code_to_action(stream[row as usize * 30 + col as usize])
                    })
                    .collect::<Vec<_>>(),
            );
        }
        self.alive()?;
        // キーごと性能 — 失敗は既定値ではなく読み込みエラー (部分読み出しを
        // 完全な設定として扱うと差分適用が実機の値を消す)。
        let mut perf = Vec::with_capacity(layout.len());
        for k in &layout {
            self.alive()?;
            let (row, col) = registry::su66max_matrix_pos(k.id);
            perf.push(self.read_perf(row, col).map(|(p, _)| p)?);
        }
        // 高度キー (mode!=NONE のみ) — 途中失敗で部分一覧を返すと「存在しない
        // 高度キー = 削除」と解釈されるため、失敗は伝播する。
        let mut advanced = Vec::new();
        for i in 0..layout.len() {
            self.alive()?;
            if let Some(a) = self.read_higher(i)? {
                advanced.push(AdvancedKey {
                    key: i,
                    layer: 0,
                    binding: a,
                });
            }
        }
        // マクロ (valid スロットのみ中身を読む) — 本文の失敗も伝播する。
        let mut macros = vec![];
        if let Some((slots, _)) = self.xs.macro_space {
            for id in 0..slots {
                let def = match self.read_macro_mode(id) {
                    Ok((true, act, rep, mode)) => Some(MacroDef {
                        name: format!("マクロ {}", id + 1),
                        events: self.read_macro_events(id, act)?,
                        mode,
                        repeat: rep,
                    }),
                    Ok(_) => None,
                    Err(e) => return Err(e),
                };
                macros.push(def);
            }
        }
        self.alive()?;
        let mut lighting = BTreeMap::new();
        for &a in &self.xs.light_areas {
            self.alive()?;
            lighting.insert(a, self.read_lighting(a)?);
        }
        let mut settings = BTreeMap::new();
        for spec in &self.caps.settings {
            self.alive()?;
            if let Some(v) = self.read_setting(&spec.id) {
                settings.insert(spec.id.clone(), v);
            }
        }
        let profile = self.read_profile()?;
        Ok(DeviceState {
            profile,
            settings,
            mouse: None,
            keyboard: Some(KeyboardState {
                keymap,
                perf,
                advanced,
                macros,
            }),
            lighting,
            peq: None,
        })
    }

    fn apply(&self, change: &Change) -> HubResult<()> {
        match change {
            Change::KeyMap { layer, key, action } => {
                let code = action_to_code(action)?;
                let layout_index = self
                    .caps
                    .keyboard
                    .as_ref()
                    .and_then(|k| k.layout.get(*key))
                    .map(|k| k.id)
                    .ok_or_else(|| HubError::Invalid("key index".into()))?;
                self.write_key(*layer, layout_index, code)?;
                self.save(scope::LAYOUT)
            }
            Change::KeyMapReset { layer } => {
                let stream = self
                    .factory_stream(*layer)
                    .unwrap_or_else(|| default_stream(*layer));
                self.write_stream(*layer, &stream)?;
                self.save(scope::LAYOUT)
            }
            Change::Lighting { area, state } if self.xs.light_areas.contains(area) => {
                // per_key はベースモードと独立したカスタムオーバーレイ (公式の isCustom
                // セマンティクス)。area 0 では常に絶対状態として書く — 変化した
                // chunk のみ送信されるので、変更なしなら readback のコストのみ。
                if *area == 0 {
                    self.write_per_key(&state.per_key)?;
                    // write_per_key は DD ピン (isCustom) も巻き込んで消す —
                    // 追跡を破棄して次の direct_drive が全セルを再ピンするようにする。
                    // dd_pre も破棄: 「DD 前のカスタム」は今適用した per_key に
                    // 取って代わられたので、次セッションで取り直す。
                    if let Ok(mut p) = self.dd_pins.lock() {
                        p.remove(area);
                    }
                    if let Ok(mut m) = self.dd_pre.lock() {
                        m.remove(area);
                    }
                }
                self.write_lighting(*area, state)?;
                self.save(scope::LIGHTING)?;
                // save() は反映を待って返るが、それでも古い値を返す実機に備えて
                // ベース値を検証し、ずれていれば 1 回だけ書き直す。
                if let Ok(r) = self.cmd(grp::LIGHTING, sub::READ, &[*area, 0]) {
                    let exp = [
                        self.compose_open(*area, state),
                        state.effect.min(19),
                        state.brightness.min(100),
                        state.speed.min(100),
                        state.direction.min(1),
                        state.palette_sel.min(7),
                    ];
                    if r[4..10] != exp {
                        self.write_lighting(*area, state)?;
                        self.save(scope::LIGHTING)?;
                    }
                }
                Ok(())
            }
            Change::Lighting { .. } => Err(HubError::Unsupported("lighting area")),
            Change::KeyPerf { keys, perf } => {
                let layout = registry::su66max_layout();
                for &i in keys {
                    let k = layout
                        .get(i)
                        .ok_or_else(|| HubError::Invalid("key index".into()))?;
                    let (row, col) = registry::su66max_matrix_pos(k.id);
                    self.write_perf(row, col, perf)?;
                }
                self.save(scope::PERFORMANCE)
            }
            Change::Advanced { key, binding, .. } => {
                self.write_higher(*key, binding.as_ref())?;
                self.save(scope::HIGHER_KEY)
            }
            Change::Macro { slot, def } => {
                self.write_macro(*slot as u8, def.as_ref())?;
                self.save(scope::MACRO)
            }
            Change::Setting { id, value } => self.write_setting(id, value),
            Change::Profile(i) => {
                if *i >= self.xs.configs {
                    return Err(HubError::Invalid("profile".into()));
                }
                // 退避した色は切替前の構成に属する。切替先をその色で上書きしない。
                self.direct_drive_stop(0)?;
                self.cmd(grp::GLOBAL, gfs::CONFIG_SWITCH, &[2, *i])?;
                Ok(())
            }
            Change::ProfileName { index, name } => {
                if *index >= self.xs.configs {
                    return Err(HubError::Invalid("profile".into()));
                }
                let mut args = vec![4, *index];
                let bytes = name.as_bytes();
                let n = bytes.len().min(32);
                args.extend_from_slice(&bytes[..n]);
                self.cmd(grp::GLOBAL, gfs::CONFIG_SWITCH, &args)?;
                Ok(())
            }
            _ => Err(HubError::Unsupported("change")),
        }
    }

    fn run_action(&self, id: &str, file: Option<&std::path::Path>) -> HubResult<ActionOutcome> {
        match id {
            // ブラックアウト (公式 CustomCommand): 状態 [10,1,0] → r[3]
            // (255=消灯中/0=通常)、切替は [10,1,1,{255|0}]。
            "blackout" => {
                let r = self.cmd(grp::CUSTOM, sub::READ, &[0])?;
                let active = r[3] == 255;
                self.cmd(grp::CUSTOM, sub::READ, &[1, if active { 0 } else { 255 }])?;
                Ok(ActionOutcome {
                    message: Some(
                        if active {
                            "ブラックアウトを解除しました"
                        } else {
                            "ブラックアウトを有効にしました"
                        }
                        .into(),
                    ),
                    reload: false,
                })
            }
            // キャリブレーション開始 [2,6,0]。キーを数回押した後、
            // 再度同じ action を呼ぶと [2,6,1] で終了する想定 — UI 側の導線に合わせ、
            // ここでは開始のみ (終了は calibrate_finish)。
            "calibrate" => {
                self.cmd(grp::GLOBAL, gfs::CALIBRATION, &[0])?;
                Ok(ActionOutcome {
                    message: Some(
                        "キャリブレーションを開始しました。全キーを数回強く押してから \
                         「キャリブレーション終了」を実行してください"
                            .into(),
                    ),
                    reload: false,
                })
            }
            "calibrate_finish" => {
                self.cmd(grp::GLOBAL, gfs::CALIBRATION, &[1])?;
                self.save(scope::CALIBRATION)?;
                Ok(ActionOutcome {
                    message: Some("キャリブレーションを終了しました".into()),
                    reload: true,
                })
            }
            "reset_keymap" => {
                for layer in 0..self.xs.layers {
                    let stream = self
                        .factory_stream(layer)
                        .unwrap_or_else(|| default_stream(layer));
                    self.write_stream(layer, &stream)?;
                }
                self.save(scope::LAYOUT)?;
                Ok(ActionOutcome {
                    message: Some("キー割り当てを初期値に戻しました".into()),
                    reload: true,
                })
            }
            "reset_lighting" => {
                self.write_lighting(
                    0,
                    &LightingState {
                        on: true,
                        effect: 0,
                        color: Rgb::new(255, 255, 255),
                        brightness: 50,
                        speed: 50,
                        direction: 0,
                        per_key: BTreeMap::new(),
                        zone_on: vec![true, true],
                        palette: vec![Rgb::new(255, 255, 255)],
                        palette_sel: 0,
                        color_correction: None,
                    },
                )?;
                self.save(scope::LIGHTING)?;
                Ok(ActionOutcome {
                    message: Some("ライティングを初期値に戻しました".into()),
                    reload: true,
                })
            }
            "factory_reset" => {
                self.cmd(grp::GLOBAL, gfs::RESET_FACTORY, &[scope::ALL])?;
                Ok(ActionOutcome {
                    message: Some("工場出荷時設定に戻しました".into()),
                    reload: true,
                })
            }
            // 設定の書き出し/読み込み (自前 JSON — 公式の暗号化形式とは非互換)。
            // {app, model, name, state: DeviceState} か DeviceState 単体を受け付ける。
            "export_config" => {
                let path = file.ok_or_else(|| HubError::Invalid("保存先".into()))?;
                let state = self.read()?;
                let doc = serde_json::json!({
                    "app": "device-hub",
                    "model": self.info.model,
                    "name": self.info.name,
                    "state": state,
                });
                let json = serde_json::to_string_pretty(&doc)
                    .map_err(|e| HubError::Invalid(format!("JSON: {e}")))?;
                std::fs::write(path, json)
                    .map_err(|e| HubError::Io(format!("ファイル書き出し: {e}")))?;
                Ok(ActionOutcome {
                    message: Some(format!("設定を {} に書き出しました", path.display())),
                    reload: false,
                })
            }
            "import_config" => {
                let path = file.ok_or_else(|| HubError::Invalid("ファイル".into()))?;
                let text = std::fs::read_to_string(path)
                    .map_err(|e| HubError::Io(format!("ファイル読み込み: {e}")))?;
                let v: serde_json::Value = serde_json::from_str(&text)
                    .map_err(|e| HubError::Invalid(format!("JSON: {e}")))?;
                let st: DeviceState = if v.get("keyboards").is_some() || v.get("data").is_some() {
                    // 公式 configurator のエクスポート (keyboard_config.json)。
                    let plain = self.official_plain(&v)?;
                    self.official_to_state(&plain)?
                } else {
                    serde_json::from_value(v.get("state").cloned().unwrap_or(v))
                        .map_err(|e| HubError::Invalid(format!("設定データ: {e}")))?
                };
                self.import_state(&st)?;
                Ok(ActionOutcome {
                    message: Some("設定をインポートしました".into()),
                    reload: true,
                })
            }
            "reset_perf" => self.reset_scope(scope::PERFORMANCE, "性能設定"),
            "reset_advanced" => self.reset_scope(scope::HIGHER_KEY, "高度キー"),
            "reset_macro" => self.reset_scope(scope::MACRO, "マクロ"),
            _ => Err(HubError::Unsupported("action")),
        }
    }

    fn live_travel(&self) -> HubResult<Vec<f32>> {
        self.route_mm()
    }

    /// `[4,3,kind,row]` × ADC/Route/Calibrate/KeyStatus を行 1-5 で取得し layout 順にまとめる。
    /// まずキューをドレインして流れているフレームをキャッシュし、不足セルには
    /// まとめて要求を送って一括収集する (セルごと往復では 25ms 枠を超え、
    /// 更新が飛び飛びになる)。公式 HUB 等の別プロセスが会話中ならその応答に
    /// 相乗りして自分は送信を休む (受動モード)。
    fn live_axis(&self) -> HubResult<Vec<AxisSample>> {
        const KINDS: [u8; 4] = [axis::ADC, axis::ROUTE, axis::CALIBRATE, axis::KEY_STATUS];
        // 視覚的な滑らかさに効く 2 種は ~ポーリング毎、変化の少ない 2 種は間引く。
        let fresh_for = |kind: u8| match kind {
            k if k == axis::ROUTE || k == axis::KEY_STATUS => Duration::from_millis(60),
            _ => Duration::from_millis(2500),
        };
        let cell_age = |kind: u8, row: u8| -> Option<(Instant, [u16; 30])> {
            self.axis_cache
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get(&(kind, row))
                .copied()
        };
        let stale = |kind: u8, row: u8| -> bool {
            match cell_age(kind, row) {
                Some((t, _)) => t.elapsed() >= fresh_for(kind),
                None => true,
            }
        };
        let is_passive =
            || *self.axis_passive.lock().unwrap_or_else(|p| p.into_inner()) > Instant::now();

        self.drain_axis(4);

        // 不足セルを洗い出して一括要求 → エコーをまとめて収集する。
        let mut wanted = Vec::with_capacity(20);
        for row in 1..=5u8 {
            for kind in KINDS {
                if stale(kind, row) {
                    wanted.push((kind, row));
                }
            }
        }
        if !wanted.is_empty() && !is_passive() {
            for &(kind, row) in &wanted {
                self.ch
                    .send(&[grp::PERFORMANCE, sub::AXIS_DATA, kind, row])?;
                self.axis_self
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert((kind, row), Instant::now());
            }
            // 短く待って残りは次ポーリングの drain に回す — ここで長くブロック
            // すると OBS ループ周期 (25ms) を大きく超えて表示 FPS が落ちる。
            let deadline = Instant::now() + Duration::from_millis(14);
            let mut saw_axis = false;
            let mut foreign = false;
            while Instant::now() < deadline {
                match self.ch.recv(5) {
                    Ok(b) => {
                        if self.note_axis_frame(&b) {
                            saw_axis = true;
                        } else if b.first().copied() == Some(grp::PERFORMANCE)
                            && b.get(1).copied() == Some(0xFF)
                        {
                            // 未対応の (kind,row) — 0 埋めで進める。
                            self.axis_cache
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .insert((b[2], b[3]), (Instant::now(), [0; 30]));
                        } else if self.is_foreign_cmd(&b) {
                            self.note_foreign();
                            foreign = true;
                            break;
                        }
                        if wanted.iter().all(|&(k, r)| !stale(k, r)) {
                            break;
                        }
                    }
                    Err(HubError::Protocol(_)) => {}
                    Err(e) => return Err(e),
                }
            }
            // 軸フレームが 1 枚も来ず外部応答でもない → 切断とみなす
            // (従来通り失敗として伝播し、切断検知を保つ)。
            if !saw_axis && !foreign {
                return Err(HubError::Protocol("axis: no frames".into()));
            }
        }

        let layout = registry::su66max_layout();
        let mut out = vec![AxisSample::default(); layout.len()];
        let cell = |kind: u8, row: u8| -> [u16; 30] {
            cell_age(kind, row).map(|(_, v)| v).unwrap_or([0; 30])
        };
        for (i, k) in layout.iter().enumerate() {
            let (kr, kc) = registry::su66max_matrix_pos(k.id);
            if (1..=5).contains(&kr) && (kc as usize) < 30 {
                let c = kc as usize;
                out[i] = AxisSample {
                    route: cell(axis::ROUTE, kr)[c] as f32 / 1000.0,
                    adc: cell(axis::ADC, kr)[c],
                    status: cell(axis::KEY_STATUS, kr)[c] as u8,
                    calibrated: cell(axis::CALIBRATE, kr)[c] as u8,
                };
            }
        }
        Ok(out)
    }

    /// キーごとの作動点 (layout 順、mm)。RT キーは First touch、通常はアクチュエーション。
    /// デバイス発見時に 1 度だけ呼ぶ (66 キー分の read_perf = 66 round-trip)。
    fn live_key_perf(&self) -> HubResult<Vec<KeyPerf>> {
        let layout = registry::su66max_layout();
        let mut out = Vec::with_capacity(layout.len());
        for k in &layout {
            self.alive()?;
            let (row, col) = registry::su66max_matrix_pos(k.id);
            let (p, _) = self.read_perf(row, col)?;
            out.push(p);
        }
        Ok(out)
    }

    fn live_actuation(&self) -> HubResult<Vec<f32>> {
        Ok(self
            .live_key_perf()?
            .into_iter()
            .map(|p| match p.mode {
                TriggerMode::Rapid => p.rt_first,
                TriggerMode::Normal => p.actuation,
            })
            .collect())
    }

    fn battery(&self) -> HubResult<Battery> {
        // [13,1] → [2]=mode [3]=charge [4]=percent。SU66 は有線 → 未対応返しのはず。
        let r = self.cmd(grp::THREE_MODE, sub::GET_BASIC_INFO, &[])?;
        Ok(Battery {
            percent: Some(r[4]),
            charging: r[3] == 1,
        })
    }

    /// DirectDrive `[5,5,area,chunk,…15セル×4B(B,G,R,255)]` — 応答なし (SDK
    /// `setLightingDirectDrive` = sendDataNoResponse)。キー index → LED セル展開は
    /// `led_cells` (スペースバー等は複数セル)。
    /// **flag=255 のセルは isCustom としてピン留めされる — flag=0 は「触らない」で
    /// あり解放ではない (実機確認)**: 未指定セルはファームのアニメーションが
    /// そのまま駆動し続ける (P2-b ハイブリッド — 実機で紫色エフェクト上に
    /// 赤 DD セルが重なって動作することを確認)。
    /// そのため:
    /// - flag=0 しか含まないチャンクは送らない (空振り削減 — ハイブリッドでは
    ///   5キーの強調に 1-2 チャンクで済む)。
    /// - 前フレームでピン留めしたが今回指定が無いセルは、`[5,4]` で isCustom を
    ///   落として解放する (ドライバが pin 集合を追跡 — `direct_drive` の呼ぶたびの
    ///   指定集合が「いま光らせたい集合」の絶対指定として扱える)。
    fn direct_drive(&self, area: u8, cells: &BTreeMap<usize, Rgb>) -> HubResult<()> {
        self.alive()?;
        let layout = registry::su66max_layout();
        let mut buf = [[0u8; 4]; 135];
        let mut new_map = BTreeMap::new();
        for (&idx, rgb) in cells {
            if let Some(k) = layout.get(idx) {
                for cell in led_cells(k) {
                    if cell < 135 {
                        buf[cell] = [rgb.b, rgb.g, rgb.r, 255];
                        new_map.insert(cell, [rgb.b, rgb.g, rgb.r]);
                    }
                }
            }
        }
        let old_map = {
            let pins = self.dd_pins.lock().unwrap_or_else(|p| p.into_inner());
            pins.get(&area).cloned().unwrap_or_default()
        };
        // セッション開始: 最初のピン設定の前に、ユーザーカスタムのセル+色を
        // 退避する — DD が上書きした後でも解放時に元のカスタムへ戻せる。
        if !new_map.is_empty() {
            let needs = self
                .dd_pre
                .lock()
                .map(|p| !p.contains_key(&area))
                .unwrap_or(true);
            if needs {
                self.capture_dd_pre(area)?;
            }
        }
        let released: Vec<usize> = old_map
            .keys()
            .filter(|c| !new_map.contains_key(*c))
            .copied()
            .collect();
        // 新規ピンまたは色が変わったセルを含むチャンクだけ送る。
        // 未変化ピンは残るので再送不要 (実機: ストリーム停止後も固まる)。
        let touched: std::collections::BTreeSet<u8> = new_map
            .iter()
            .filter(|(cell, rgb)| old_map.get(*cell).copied().flatten() != Some(**rgb))
            .map(|(cell, _)| (cell / 15) as u8)
            .collect();
        for c in touched {
            let mut w = [0u8; 64];
            w[0] = grp::LIGHTING;
            w[1] = sub::LIGHT_DIRECTDRIVE;
            w[2] = area;
            w[3] = c;
            for (i, cell) in buf[c as usize * 15..c as usize * 15 + 15]
                .iter()
                .enumerate()
            {
                w[4 + i * 4..8 + i * 4].copy_from_slice(cell);
            }
            let result = self.ch.send(&w);
            // 途中で送信に失敗しても、送信を試みたチャンクは停止時に復元する。
            // エラーでも機器側に届いた可能性があるため保守的に追跡する。
            self.dd_pins
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .entry(area)
                .or_default()
                .extend(
                    new_map
                        .range(c as usize * 15..(c as usize + 1) * 15)
                        .map(|(&cell, &rgb)| (cell, result.is_ok().then_some(rgb))),
                );
            result?;
            // 公式は await の USB 転送時間 (~1ms) で自然ペース。133µs 連打でも
            // デバイスは生存するが (probe9)、OUT エンドポイントの消費に合わせて 1ms 開ける。
            std::thread::sleep(Duration::from_millis(1));
        }
        // 外れたセルを `[5,4]` で解放 — チャンクごとに読み戻して対象セルだけ
        // 直す (ユーザーのカスタムや他のピン留めは温存。DD 前にカスタムだった
        // セルは dd_pre の色+flag=255 で復帰する)。
        let rel_failed = self.release_cells(area, &released.iter().copied().collect());
        // 解放に失敗したチャンクのセルは追跡に残し、次フレームで解放を再試行する。
        let mut pending: BTreeMap<_, _> = new_map
            .into_iter()
            .map(|(cell, rgb)| (cell, Some(rgb)))
            .collect();
        for cell in released {
            if rel_failed.contains(&((cell / 15) as u8)) {
                if let Some(rgb) = old_map.get(&cell) {
                    pending.insert(cell, *rgb);
                }
            }
        }
        let mut pins = self.dd_pins.lock().unwrap_or_else(|p| p.into_inner());
        pins.insert(area, pending);
        Ok(())
    }

    /// DirectDrive 解除 — DD フレームの flag=255 は isCustom としてセルを
    /// ピン留めする (flag=0 は「触らない」であり解放ではない — probe14)。
    /// 追跡中の DD ピンだけ `[5,4]` で解放し (ユーザーのカスタムは温存、
    /// DD 前にカスタムだったセルは `dd_pre` の色で復帰)、その後現在の照明
    /// 設定を `[5,2]` 再送してファーム側アニメーションに戻す。
    /// フラグ操作は save しない — flash 上のユーザーカスタム色は温存される。
    fn direct_drive_stop(&self, area: u8) -> HubResult<()> {
        if !self
            .dd_pre
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains_key(&area)
        {
            return Ok(());
        }
        // 空集合の direct_drive = 追跡ピンのみ解放 (未追跡セルは触らない)。
        self.direct_drive(area, &BTreeMap::new())?;
        if self
            .dd_pins
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&area)
            .is_some_and(|pins| !pins.is_empty())
        {
            return Err(HubError::Protocol(
                "DirectDrive: LED の復元が完了していません".into(),
            ));
        }
        let r = self.cmd(grp::LIGHTING, sub::READ, &[area, 0])?;
        self.cmd(
            grp::LIGHTING,
            sub::SETTING,
            &[area, 0, r[4], r[5], r[6], r[7], r[8], r[9]],
        )?;
        self.dd_pre
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&area);
        Ok(())
    }

    /// `[5,3]` の応答は**全セルの現在描画色**を返す (BGR は常に現在フレーム)。
    /// flag はマーカーであって点灯判定ではない — 0xFF=ユーザー/DD カスタム、
    /// 0xF0=ファームがマークしたセル、0=無印 (色は来る)。公式 HUB のプレビューは
    /// これを全セルそのまま表示している (キー色 = 実機の今の色。シミュレーション無し)。
    fn live_lighting(&self, area: u8) -> HubResult<BTreeMap<usize, Rgb>> {
        if area != 0 {
            return Err(HubError::Unsupported("live_lighting"));
        }
        self.alive()?;
        let layout = registry::su66max_layout();
        // cell → layout index。Space 等の複数セルは最初のセルの色を採用。
        let mut cell_key = [usize::MAX; 135];
        for (i, k) in layout.iter().enumerate() {
            for cell in led_cells(k) {
                if cell < 135 && cell_key[cell] == usize::MAX {
                    cell_key[cell] = i;
                }
            }
        }
        let mut out = BTreeMap::new();
        for c in 0..9u8 {
            let rb = self.cmd(grp::LIGHTING, sub::LIGHT_READBACK, &[0, c])?;
            for i in 0..15usize {
                let cell = c as usize * 15 + i;
                // 「点灯しているか」は flag ではなく色レジスタで見る:
                // flag (0xF0/0xFF) はファーム/ユーザーがマークしたセルにだけ立ち、
                // 無印のまま色だけ出るセルもある (litpoll で実機確認 — 書き込み無しの
                // セッションでは全セル flag=0 でも色は流れてくる)。
                // 色 000 の消灯セルはマップに入れない (UI 側は無印=消灯として描く)。
                let (b, g, r, f) = (rb[4 + i * 4], rb[5 + i * 4], rb[6 + i * 4], rb[7 + i * 4]);
                if (f != 0 || r != 0 || g != 0 || b != 0) && cell_key[cell] != usize::MAX {
                    out.insert(cell_key[cell], Rgb::new(r, g, b));
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keycode_roundtrip() {
        assert_eq!(code_to_action(0), KeyAction::None);
        assert_eq!(code_to_action(1), KeyAction::Transparent);
        assert_eq!(code_to_action(0x04), KeyAction::Code(4));
        assert_eq!(action_to_code(&KeyAction::Transparent).unwrap(), 1);
        assert_eq!(action_to_code(&KeyAction::Code(0xF101)).unwrap(), 0xF101);
        // combo: 0x1000 | mods<<8 | usage
        assert_eq!(
            action_to_code(&KeyAction::Combo { mods: 1, code: 4 }).unwrap(),
            0x1104
        );
        assert_eq!(
            code_to_action(0x1104),
            KeyAction::Combo { mods: 1, code: 4 }
        );
        assert!(action_to_code(&KeyAction::Combo {
            mods: 1,
            code: 0x104
        })
        .is_err());
    }

    #[test]
    fn default_stream_layer0() {
        let s = default_stream(0);
        // Esc は (1,0) → stream[30]
        assert_eq!(s[30], 0x29);
        // A は (3,1) → stream[3*30+1]
        assert_eq!(s[91], 0x04);
        // Space (5,4) → stream[154]
        assert_eq!(s[5 * 30 + 4], 0x2C);
        // Fn (5,10) → 0xF101
        assert_eq!(s[5 * 30 + 10], 0xF101);
        // レイヤー 1 はすべて透過
        let s1 = default_stream(1);
        assert_eq!(s1[30], 1);
    }

    #[test]
    fn fw_parse() {
        let mut r = [0u8; 64];
        r[2..9].copy_from_slice(b" 49142 ");
        r[40..56].copy_from_slice(b"2026101216:41:03");
        assert_eq!(parse_fw(&r).as_deref(), Some("2026101216:41:03"));
    }

    // ---- live_axis の相乗り/受動モード ----

    /// `[4,3,kind,row]` 軸フレーム。cell0 に mark を入れる。
    fn axis_frame(kind: u8, row: u8, mark: u16) -> Vec<u8> {
        let mut f = vec![4u8, 3, kind, row];
        for i in 0..30 {
            let v = if i == 0 { mark } else { 0 };
            f.extend_from_slice(&v.to_le_bytes());
        }
        f
    }

    /// 全 kind×row (20 枚) をキューに積む。
    fn seed_all(d: &XsydDriver, mark: u16) {
        for row in 1..=5 {
            for kind in 0..=3 {
                d.ch.mock_reply(&axis_frame(kind, row, mark));
            }
        }
    }

    #[test]
    fn live_axis_piggyback_sends_no_requests() {
        let d = mock_driver();
        // 公式 HUB 等の要求が起こしたフレームだけが流れている状況。
        seed_all(&d, 1500);
        let out = d.live_axis().unwrap();
        // 相乗りで全部キャッシュから取れるので要求は 0。
        assert!(d.ch.mock_sent().is_empty());
        // 値はキャッシュから来ている (route は /1000 → 1.5mm)。
        assert!(out.iter().any(|s| (s.route - 1.5).abs() < 0.001));
        // 外部トラフィック検出で受動モードが立っている。
        assert!(
            *d.axis_passive.lock().unwrap() > Instant::now(),
            "外部フレームだけのはずなので受動モードになる"
        );
    }

    #[test]
    fn live_axis_passive_never_requests() {
        let d = mock_driver();
        // 外部フレーム 1 枚で受動モードへ。
        d.ch.mock_reply(&axis_frame(1, 1, 1000));
        d.drain_axis(5);
        assert!(*d.axis_passive.lock().unwrap() > Instant::now());
        // キューが空でも要求を出さず Ok で返す (欠損セルは 0)。
        let out = d.live_axis().unwrap();
        assert!(d.ch.mock_sent().is_empty());
        assert_eq!(out.len(), registry::su66max_layout().len());
    }

    #[test]
    fn drain_foreign_command_frame_triggers_passive() {
        let d = mock_driver();
        // 公式の設定応答 ([3,1,...] 等) が流れている状況 — OBS は
        // 軸要求しか出さないので非軸フレーム = 他人のトラフィック。
        let mut f = vec![3u8, 1];
        f.resize(64, 0);
        d.ch.mock_reply(&f);
        assert!(d.drain_axis(5));
        assert!(*d.axis_passive.lock().unwrap() > Instant::now());
        // 他人のコマンドがデバイス側ストリームを解除するので、
        // こちらからは何も送らない。
        assert!(d.ch.mock_sent().is_empty());
    }

    #[test]
    fn drain_ignores_own_command_responses() {
        let d = mock_driver();
        // 自分が [3,1] を直前に送っていた状況 → その応答は外部ではない。
        d.cmd_self.lock().unwrap().insert((3, 1), Instant::now());
        let mut f = vec![3u8, 1];
        f.resize(64, 0);
        d.ch.mock_reply(&f);
        assert!(!d.drain_axis(5));
        assert!(*d.axis_passive.lock().unwrap() < Instant::now());
    }

    #[test]
    fn axis_row_ignores_foreign_frames_and_caches_them() {
        let d = mock_driver();
        // 他 kind/他行のフレーム → 自分のエコーの順で届く。
        d.ch.mock_reply(&axis_frame(3, 5, 777));
        d.ch.mock_reply(&axis_frame(1, 1, 2048));
        let v = d.axis_row(1, 1).unwrap();
        assert_eq!(v[0], 2048);
        // エコー外のフレームも捨てずキャッシュに入る (相乗りに使う)。
        let c = d.axis_cache.lock().unwrap();
        assert_eq!(c.get(&(3, 5)).unwrap().1[0], 777);
        // 自分の要求が記録されている。
        assert!(d.axis_self.lock().unwrap().contains_key(&(1, 1)));
    }

    #[test]
    fn live_axis_requests_when_cache_cold() {
        let d = mock_driver();
        // キャッシュも受動モードも無い → 要求を出して応答待ち → モックは
        // 応答を返さないのでエラー。切断検知に必要な挙動は残っている。
        assert!(d.live_axis().is_err());
        let sent = d.ch.mock_sent();
        assert!(sent.iter().any(|s| s.len() >= 4 && s[0] == 4 && s[1] == 3));
    }

    /// 照明 wire レイアウト (公式 lightingBaseData と一致) の境界値。
    #[test]
    fn lighting_scale() {
        // caps の範囲 = wire の範囲 = 0..100 生値。方向は Forward=0/Backward=1 のみ。
        let xs = XsydInfo {
            layers: 2,
            rt_step: 0.01,
            travel: 4.0,
            polling: vec![],
            configs: 1,
            light_areas: vec![0],
            macro_space: None,
            axes: vec![],
            double_lighting: true,
        };
        let caps = build_caps(&xs).lighting;
        assert_eq!(caps[0].brightness_max, 100);
        assert_eq!(caps[0].speed_max, 100);
        assert_eq!(caps[0].directions.len(), 2);
    }

    /// 実機 fixture: `fixtures/su66max/perf.resp.bin` (r1c0, 2026-11-15 採取)。
    /// mode=0, nP=nR=2.0, rtFirst=0.5, rtP=rtR=0.3, pDead=rDead=0.2, axis=0
    #[test]
    fn perf_decode_fixture() {
        let bin = include_bytes!("../fixtures/su66max/perf.resp.bin");
        let mut r = [0u8; 64];
        r.copy_from_slice(&bin[..64]);
        let p = XsydDriver::decode_perf(&r);
        assert_eq!(p.mode, TriggerMode::Normal);
        assert_eq!(p.actuation, 2.0);
        assert_eq!(p.release, Some(2.0));
        assert_eq!(p.rt_first, 0.5);
        assert_eq!(p.rt_press, 0.3);
        assert_eq!(p.rt_release, 0.3);
        assert_eq!(p.top_deadzone, 0.2);
        assert_eq!(p.bottom_deadzone, 0.2);
        assert_eq!(p.switch_type, 0);
    }

    /// rapid mode=1 → TriggerMode::Rapid + continuous (公式 vn: RT=1)。
    /// mode=2 (IntervalRT) → Rapid + 非連続。
    #[test]
    fn perf_decode_rapid() {
        let mut r = [0u8; 64];
        r[4] = 1;
        let p = XsydDriver::decode_perf(&r);
        assert_eq!(p.mode, TriggerMode::Rapid);
        assert!(p.continuous);
        r[4] = 2;
        let p = XsydDriver::decode_perf(&r);
        assert_eq!(p.mode, TriggerMode::Rapid);
        assert!(!p.continuous);
    }

    /// DKS trps ビットマスク ↔ フェーズの往復。
    #[test]
    fn trp_roundtrip() {
        use DksPhase::*;
        for (trp, expect) in [
            (0u8, [Off, Off, Off, Off]),
            (1, [Tap, Off, Off, Off]),
            (2, [Hold, Off, Off, Off]),
            (4, [Off, Tap, Off, Off]),
            (8 | 16, [Off, Hold, Off, Off]),
            (32, [Off, Off, Tap, Off]),
            (64, [Off, Off, Hold, Off]),
            (128, [Off, Off, Off, Tap]),
        ] {
            assert_eq!(trp_to_phases(trp), expect, "trp={trp:#04x}");
            assert_eq!(phases_to_trp(&expect), trp);
        }
        // Hold 優先: b0+b1 両立て → Hold
        assert_eq!(trp_to_phases(3)[0], Hold);
    }

    /// マクロイベント u32 パック (SDK と同じ: bit31=down, bits16-30=delay, low16=keycode)。
    #[test]
    fn macro_event_pack() {
        let w = (1u32 << 31) | (100u32 << 16) | 0x04;
        let b = w.to_le_bytes();
        let back = u32::from_le_bytes(b);
        assert_eq!(back & 0xFFFF, 0x04);
        assert_eq!((back >> 16) & 0x7FFF, 100);
        assert_eq!(back >> 31, 1);
    }

    /// ポーリング wire code のラベル。
    #[test]
    fn polling_labels() {
        assert_eq!(polling_label(0), "8000 Hz");
        assert_eq!(polling_label(3), "1000 Hz");
        assert_eq!(polling_label(6), "125 Hz");
    }

    /// LED セル対応 — 実機目視で確定した座標 (cell = 行*21 + 列)。
    #[test]
    fn led_cell_map() {
        let layout = registry::su66max_layout();
        let at = |label: &str| layout.iter().find(|k| k.label == label).unwrap().clone();
        assert_eq!(led_cells(&at("Esc")), vec![21]);
        assert_eq!(led_cells(&at("9")), vec![30]);
        assert_eq!(led_cells(&at("W")), vec![44]);
        // 右端は実機プローブ確定値 — LED 列 = マトリクス列。' は右端の死pad
        // (cell75) も自分の下に持つ。LShift も同様 (cell85)。
        assert_eq!(led_cells(&at("'")), vec![74, 75]);
        assert_eq!(led_cells(&at("Enter")), vec![76]);
        assert_eq!(led_cells(&at("↑")), vec![97]);
        assert_eq!(led_cells(&at("Del")), vec![98]);
        assert_eq!(led_cells(&at("LShift")), vec![84, 85]);
        assert_eq!(led_cells(&at("V")), vec![89]);
        assert_eq!(led_cells(&at("B")), vec![90]);
        // 行 5 は実測テーブル (右側圧縮 + スペースバー複数 LED)。
        assert_eq!(led_cells(&at("LCtrl")), vec![105]);
        assert_eq!(led_cells(&at("RAlt")), vec![114]);
        assert_eq!(led_cells(&at("Fn")), vec![115]);
        assert_eq!(led_cells(&at("←")), vec![117]);
        assert_eq!(led_cells(&at("→")), vec![119]);
        let spaces: Vec<usize> = layout
            .iter()
            .filter(|k| k.label == "Space")
            .flat_map(led_cells)
            .collect();
        assert_eq!(spaces, vec![109, 110, 111, 112, 113]);
    }

    /// 高度キー wire 往復 — `higher_packets` で書いた内容を `[6,1]` 応答
    /// として `decode_higher` で読み戻せるか (遅延フィールドを含む)。
    #[test]
    fn higher_wire_roundtrip() {
        let layout = registry::su66max_layout();
        let idx = |label: &str| {
            layout
                .iter()
                .position(|k| k.label == label)
                .unwrap_or_else(|| panic!("{label} not in layout"))
        };
        let roundtrip = |key_label: &str, b: Advanced| {
            let (row, col) = {
                let k = layout.iter().find(|k| k.label == key_label).unwrap();
                registry::su66max_matrix_pos(k.id)
            };
            let packets = higher_packets(row, col, Some(&b)).unwrap();
            // 応答 r[2..]=row,col,mode,... と同レイアウトで合成 (r[4]=mode)。
            let mut r = [0u8; 64];
            r[2..2 + packets[0].len()].copy_from_slice(&packets[0]);
            decode_higher(&r).unwrap_or_else(|| panic!("{key_label} decode failed"))
        };

        // TGL: kc@5..6, time@7..8
        match roundtrip(
            "A",
            Advanced::Tgl {
                code: 0x04,
                delay_ms: 120,
            },
        ) {
            Advanced::Tgl { code, delay_ms } => {
                assert_eq!(code, 0x04);
                assert_eq!(delay_ms, 120);
            }
            other => panic!("TGL → {other:?}"),
        }
        // END: kcs@5..8, delay@9..10
        match roundtrip(
            "A",
            Advanced::End {
                code: 0x04,
                code_release: 0x1E,
                delay_ms: 80,
            },
        ) {
            Advanced::End {
                code,
                code_release,
                delay_ms,
            } => {
                assert_eq!((code, code_release, delay_ms), (0x04, 0x1E, 80));
            }
            other => panic!("END → {other:?}"),
        }
        // MT: kcs@5..8, time@9..10
        match roundtrip(
            "A",
            Advanced::Mt {
                tap: 0x04,
                hold: 0xE1,
                hold_ms: 200,
            },
        ) {
            Advanced::Mt { tap, hold, hold_ms } => {
                assert_eq!((tap, hold, hold_ms), (0x04, 0xE1, 200));
            }
            other => panic!("MT → {other:?}"),
        }
        // SOCD: pair@5..6, kcs@7..10, delay@11..12, socdMode@13 (2 パケット目も構造を確認)
        for (mode, x0, x1) in [
            (SocdMode::Last, 0u8, 0u8),
            (SocdMode::PriorityA, 1, 2),
            (SocdMode::PriorityB, 2, 1),
            (SocdMode::Neutral, 3, 3),
        ] {
            let b = Advanced::Socd {
                other: idx("D"),
                code_a: 0x04,
                code_b: 0x07,
                mode,
                delay_ms: 33,
            };
            let (row, col) = registry::su66max_matrix_pos(layout[idx("A")].id);
            let packets = higher_packets(row, col, Some(&b)).unwrap();
            assert_eq!(packets.len(), 2);
            // 書き込みパケット = [row,col,mode,r2,c2,kcA,kcB,delay,x] → socdMode は index 11
            assert_eq!(packets[0][11], x0);
            assert_eq!(packets[1][11], x1);
            // パケット 2 はペアとキーコードが反転 (kcB@5..6, kcA@7..8)
            assert_eq!(u16::from_le_bytes([packets[1][5], packets[1][6]]), 0x07);
            assert_eq!(u16::from_le_bytes([packets[1][7], packets[1][8]]), 0x04);
            let mut r = [0u8; 64];
            r[2..2 + packets[0].len()].copy_from_slice(&packets[0]);
            match decode_higher(&r) {
                Some(Advanced::Socd {
                    other,
                    code_a,
                    code_b,
                    mode: m,
                    delay_ms,
                }) => {
                    assert_eq!(other, idx("D"));
                    assert_eq!((code_a, code_b, delay_ms), (0x04, 0x07, 33));
                    assert_eq!(m, mode);
                }
                other => panic!("SOCD {mode:?} → {other:?}"),
            }
        }
        // RS: SOCD と同構造 (モードバイトなし)
        match roundtrip(
            "A",
            Advanced::Rs {
                other: idx("S"),
                code_a: 0x04,
                code_b: 0x16,
                delay_ms: 17,
            },
        ) {
            Advanced::Rs {
                other,
                code_a,
                code_b,
                delay_ms,
            } => {
                assert_eq!(other, idx("S"));
                assert_eq!((code_a, code_b, delay_ms), (0x04, 0x16, 17));
            }
            other => panic!("RS → {other:?}"),
        }
        // DKS: 4×kc@5..12, 4×trp@13..16, dbs@17..20 — bottom は db2
        match roundtrip(
            "A",
            Advanced::Dks {
                rows: vec![DksRow {
                    code: 0x04,
                    phases: [DksPhase::Hold, DksPhase::Off, DksPhase::Off, DksPhase::Tap],
                }],
                bottom: 3.0,
            },
        ) {
            Advanced::Dks { rows, bottom } => {
                assert_eq!(rows[0].code, 0x04);
                assert!((bottom - 3.0).abs() < 0.001);
            }
            other => panic!("DKS → {other:?}"),
        }
        // MPT: 3×kc@5..10, 3×db@11..16
        match roundtrip(
            "A",
            Advanced::Mpt {
                depths: [0.8, 2.0, 3.2],
                codes: [0x04, 0x16, 0x07],
            },
        ) {
            Advanced::Mpt { depths, codes } => {
                assert_eq!(codes, [0x04, 0x16, 0x07]);
                assert!((depths[0] - 0.8).abs() < 0.001);
                assert!((depths[2] - 3.2).abs() < 0.001);
            }
            other => panic!("MPT → {other:?}"),
        }
        // NONE (解除) → decode_higher は None
        let packets = higher_packets(3, 1, None).unwrap();
        assert_eq!(packets[0][2], hk::NONE);
    }

    // ---- モック Channel の fixture テスト (実機応答バイトの再生) ----

    /// `[group, ...tail]` の 64B 応答を組み立てる (tail には sub/引数エコーを含める)。
    fn reply(group: u8, tail: &[u8]) -> Vec<u8> {
        let mut v = Vec::with_capacity(64);
        v.push(group);
        v.extend_from_slice(tail);
        v.resize(64, 0);
        v
    }

    /// DD セッション開始時の `capture_dd_pre` (全 9 チャンクのカスタム読み出し)
    /// への応答を積む — 全セル isCustom=0 (ユーザー塗りなし)。
    fn mock_dd_pre_empty(d: &XsydDriver) {
        for c in 0..9u8 {
            d.ch.mock_reply(&reply(5, &[3, 0, c]));
        }
    }

    /// DoubleLighting + areas=[0,1] の最小 XsydDriver。
    fn mock_driver() -> XsydDriver {
        XsydDriver {
            info: DeviceInfo {
                id: DeviceId("test".into()),
                kind: DeviceKind::Keyboard,
                name: "mock".into(),
                vendor: "test".into(),
                model: "test".into(),
                vid: 0,
                pid: 0,
                transport: Transport::Wired,
                firmware: None,
                dongle_firmware: None,
                serial: None,
                driver: DriverKind::Xsyd,
                battery: None,
                supported: true,
                asleep: false,
                board_id: None,
            },
            ch: Channel::mock(),
            xs: XsydInfo {
                layers: 2,
                rt_step: 0.01,
                travel: 4.0,
                polling: vec![],
                configs: 1,
                light_areas: vec![0, 1],
                macro_space: None,
                axes: vec![],
                double_lighting: true,
            },
            caps: Capabilities::empty(),
            fails: AtomicU32::new(0),
            dead_since: Mutex::new(None),
            dd_pins: Mutex::new(BTreeMap::new()),
            dd_pre: Mutex::new(BTreeMap::new()),
            axis_cache: Mutex::new(HashMap::new()),
            axis_self: Mutex::new(HashMap::new()),
            cmd_self: Mutex::new(HashMap::new()),
            axis_passive: Mutex::new(Instant::now() - Duration::from_secs(1)),
        }
    }

    /// SU66 実機 (area0) と同じ応答列: base=Open/mode15/#slot2/100/75 →
    /// LightingState へのデコードを検証。
    #[test]
    fn lighting_read_fixture() {
        let d = mock_driver();
        // base [5,1,0,0] → r[4]=open=3 r[5]=15 r[6]=100 r[7]=75 r[8]=0 r[9]=sel=2
        d.ch.mock_reply(&reply(5, &[1, 0, 0, 3, 15, 100, 75, 0, 2]));
        // palette [5,1,0,1] → r[4..36]=[B,G,R,H]×8、slot2 = #204080
        let mut tail = vec![1, 0, 1];
        tail.resize(3 + 32, 0);
        tail[3 + 8] = 0x80; // B
        tail[3 + 9] = 0x40; // G
        tail[3 + 10] = 0x20; // R
        d.ch.mock_reply(&reply(5, &tail));
        // cc [5,1,0,2] → r[4,5,6] = R,G,B
        d.ch.mock_reply(&reply(5, &[1, 0, 2, 38, 35, 40]));
        // per-key [5,3,0,0..8]: chunk1 (cells 15..29) の i=6 = cell21 (Esc) にカスタム
        for c in 0..9u8 {
            let mut t = vec![3, 0, c];
            t.resize(3 + 60, 0);
            if c == 1 {
                t[3 + 6 * 4] = 3; // B
                t[4 + 6 * 4] = 2; // G
                t[5 + 6 * 4] = 1; // R
                t[6 + 6 * 4] = 255; // isCustom
            }
            d.ch.mock_reply(&reply(5, &t));
        }
        let s = d.read_lighting(0).unwrap();
        assert!(s.on);
        assert_eq!(s.zone_on, vec![true, true]);
        assert_eq!(s.effect, 15);
        assert_eq!((s.brightness, s.speed, s.direction), (100, 75, 0));
        assert_eq!(s.palette_sel, 2);
        assert_eq!(s.color, Rgb::new(0x20, 0x40, 0x80));
        assert_eq!(s.color_correction, Some(Rgb::new(38, 35, 40)));
        assert_eq!(s.per_key.len(), 1);
        assert_eq!(*s.per_key.values().next().unwrap(), Rgb::new(1, 2, 3));
    }

    /// DirectDrive: `[5,5,area,chunk,…15セル×4B(B,G,R,255)]` を応答待ちせず送る。
    /// flag=0 しか無いチャンクは送らない (flag=0 は「触らない」)。
    /// キー index → LED セル展開 (Esc=21、スペースバー=複数セル) を検証。
    #[test]
    fn direct_drive_wire() {
        let d = mock_driver();
        let layout = registry::su66max_layout();
        let esc = layout.iter().position(|k| k.label == "Esc").unwrap();
        let spc = layout
            .iter()
            .position(|k| led_cells(k).contains(&111))
            .unwrap();
        let mut cells = BTreeMap::new();
        cells.insert(esc, Rgb::new(255, 0, 0)); // 赤
        cells.insert(spc, Rgb::new(0, 0, 255)); // 青
        mock_dd_pre_empty(&d);
        d.direct_drive(0, &cells).unwrap();
        let sent = d.ch.mock_sent();
        // Esc=cell21 (chunk1) と Space=cell111 (chunk7) だけが含まれる
        // (capture_dd_pre の 9 read も sent に混ざるので DD だけ拾う)。
        let dd: Vec<_> = sent
            .iter()
            .filter(|f| f[1] == sub::LIGHT_DIRECTDRIVE)
            .collect();
        assert_eq!(dd.len(), 2);
        for f in &dd {
            assert_eq!(f[0], grp::LIGHTING);
            assert_eq!(f[2], 0); // area
            assert_eq!(f.len(), 64);
        }
        let c1 = dd.iter().find(|f| f[3] == 1).unwrap();
        // Esc → cell21 = chunk1 の 7 番目セル → [B,G,R,255]
        assert_eq!(&c1[4 + 6 * 4..8 + 6 * 4], &[0, 0, 255, 255]);
        let c7 = dd.iter().find(|f| f[3] == 7).unwrap();
        // スペースバー中 cell111 = chunk7 (105-119) の 7 番目セル
        assert_eq!(&c7[4 + 6 * 4..8 + 6 * 4], &[255, 0, 0, 255]);
    }

    /// DirectDrive のハイブリッド解放: 前フレームでピン留めしたセルが
    /// 次フレームの指定集合から外れたら `[5,4]` で isCustom を落とす。
    /// 解放書き込みは readback した現行値で、対象セルの flag だけ 0。
    #[test]
    fn direct_drive_releases_dropped_cells() {
        let d = mock_driver();
        let layout = registry::su66max_layout();
        let esc = layout.iter().position(|k| k.label == "Esc").unwrap();
        let mut cells = BTreeMap::new();
        cells.insert(esc, Rgb::new(255, 0, 0));
        mock_dd_pre_empty(&d);
        d.direct_drive(0, &cells).unwrap();
        // Esc=cell21 → chunk1 にピン。次は空 → 全解放。
        // 解放は chunk1 だけ — readback 応答 (cell21 は isCustom=255, 他は 0) を用意。
        let mut t = vec![3, 0, 1u8];
        t.resize(3 + 60, 0);
        t[3 + 6 * 4] = 0; // B
        t[4 + 6 * 4] = 0;
        t[5 + 6 * 4] = 255; // R
        t[6 + 6 * 4] = 255; // isCustom (ピン留め中)
        t[3 + 7 * 4 + 3] = 255; // 隣の cell22 は別途ユーザーがピン留め → 温存すべき
        d.ch.mock_reply(&reply(5, &t));
        // [5,4] への ack
        d.ch.mock_reply(&reply(5, &[4, 0, 1]));
        d.direct_drive(0, &BTreeMap::new()).unwrap();
        let sent = d.ch.mock_sent();
        // [5,4] chunk1 だけ出る (DD フレームは送られない — 指定セル無し)。
        let custom: Vec<_> = sent.iter().filter(|f| f[1] == sub::LIGHT_CUSTOM).collect();
        assert_eq!(custom.len(), 1, "frames: {sent:?}");
        let f = custom[0];
        assert_eq!(f[3], 1); // chunk1
                             // 解放対象の cell21 (chunk 内 index6) → flag=0、色は readback 値を保持。
        assert_eq!(&f[4 + 6 * 4..8 + 6 * 4], &[0, 0, 255, 0]);
        // ユーザーのピン cell22 (index7) → flag=255 のまま温存。
        assert_eq!(f[7 + 7 * 4], 255);
    }

    /// 同じ DirectDrive フレームを連続で渡しても、ピン済み・同色のセルは
    /// 再送しない (実機でピンが持続するため静止時の輻輳を避ける)。
    #[test]
    fn direct_drive_skips_unchanged_frames() {
        let d = mock_driver();
        let layout = registry::su66max_layout();
        let esc = layout.iter().position(|k| k.label == "Esc").unwrap();
        let cells = BTreeMap::from([(esc, Rgb::new(255, 0, 0))]);
        mock_dd_pre_empty(&d);
        d.direct_drive(0, &cells).unwrap();
        let n = d.ch.mock_sent().len();
        d.direct_drive(0, &cells).unwrap();
        assert_eq!(d.ch.mock_sent().len(), n);
        // 色を変えたら同じセルでも chunk を再送する。
        d.direct_drive(0, &BTreeMap::from([(esc, Rgb::new(0, 255, 0))]))
            .unwrap();
        let sent = d.ch.mock_sent();
        assert_eq!(sent.len(), n + 1);
        assert_eq!(sent.last().unwrap()[1], sub::LIGHT_DIRECTDRIVE);
    }

    #[test]
    fn direct_drive_never_writes_with_an_incomplete_backup() {
        let d = mock_driver();
        // チャンク 1 の応答としてチャンク 2 が届いたら退避を確定しない。
        d.ch.mock_reply(&reply(5, &[3, 0, 0]));
        d.ch.mock_reply(&reply(5, &[3, 0, 2]));
        let esc = registry::su66max_layout()
            .iter()
            .position(|k| k.label == "Esc")
            .unwrap();
        assert!(d
            .direct_drive(0, &BTreeMap::from([(esc, Rgb::new(255, 0, 0))]))
            .is_err());
        d.direct_drive_stop(0).unwrap();
        assert_eq!(
            d.ch.mock_sent().iter().map(|f| f[1]).collect::<Vec<_>>(),
            [3, 3]
        );
    }

    #[test]
    fn direct_drive_stop_retries_with_the_original_custom_color() {
        let mut d = mock_driver();
        d.xs.configs = 2;
        for c in 0..9 {
            let mut rb = reply(5, &[3, 0, c]);
            if c == 1 {
                rb[28..32].copy_from_slice(&[3, 2, 1, 255]); // Esc のユーザー色
            }
            d.ch.mock_reply(&rb);
        }
        let esc = registry::su66max_layout()
            .iter()
            .position(|k| k.label == "Esc")
            .unwrap();
        d.direct_drive(0, &BTreeMap::from([(esc, Rgb::new(255, 0, 0))]))
            .unwrap();
        let mut lit = reply(5, &[3, 0, 1]);
        lit[28..32].copy_from_slice(&[0, 0, 255, 255]);
        d.ch.mock_reply(&lit);
        d.ch.mock_fail_send(&[5, 4, 0, 1]);
        assert!(d.apply(&Change::Profile(1)).is_err());
        assert!(d.ch.mock_sent().iter().all(|f| f[0] != grp::GLOBAL));

        d.ch.mock_reply(&lit);
        d.ch.mock_reply(&reply(5, &[4, 0, 1]));
        d.ch.mock_reply(&reply(5, &[1, 0, 0, 1, 1, 50, 50, 0, 0]));
        d.ch.mock_reply(&reply(5, &[2, 0, 0]));
        d.direct_drive_stop(0).unwrap();
        let sent = d.ch.mock_sent();
        assert_eq!(sent.iter().filter(|f| f[1] == 2).count(), 1);
        assert_eq!(
            &sent.iter().rfind(|f| f[1] == 4).unwrap()[28..32],
            &[3, 2, 1, 255]
        );
        d.direct_drive_stop(0).unwrap();
        assert_eq!(d.ch.mock_sent().len(), sent.len());
    }

    #[test]
    fn direct_drive_stop_restores_chunks_after_a_partial_frame() {
        let d = mock_driver();
        let layout = registry::su66max_layout();
        let esc = layout.iter().position(|k| k.label == "Esc").unwrap();
        let space = layout
            .iter()
            .position(|k| led_cells(k).contains(&111))
            .unwrap();
        let frame = BTreeMap::from([(esc, Rgb::new(1, 2, 3)), (space, Rgb::new(4, 5, 6))]);
        mock_dd_pre_empty(&d);
        d.ch.mock_fail_send(&[5, 5, 0, 7]);
        assert!(d.direct_drive(0, &frame).is_err());
        for c in [1, 7] {
            d.ch.mock_reply(&reply(5, &[3, 0, c]));
            d.ch.mock_reply(&reply(5, &[4, 0, c]));
        }
        d.ch.mock_reply(&reply(5, &[1, 0, 0, 1, 1, 50, 50, 0, 0]));
        d.ch.mock_reply(&reply(5, &[2, 0, 0]));
        d.direct_drive_stop(0).unwrap();
        let restored: Vec<_> =
            d.ch.mock_sent()
                .iter()
                .filter(|f| f[1] == 4)
                .map(|f| f[3])
                .collect();
        assert_eq!(restored, [1, 7]);
    }

    /// ベース書き込みの wire: `[5,2,area,0,open,mode,lum,spd,dir,sel]` と
    /// 「現行と同じ部分は送らない」ことを検証。
    #[test]
    fn lighting_write_wire() {
        let d = mock_driver();
        // cur base (変更あり → base フレームだけ出る)
        d.ch.mock_reply(&reply(5, &[1, 0, 0, 1, 1, 50, 50, 0, 0]));
        // base SET への ack エコー
        d.ch.mock_reply(&reply(5, &[2, 0, 0]));
        // cur palette: slot2 が目標色と同じ → パレット書き込み無し
        let mut tail = vec![1, 0, 1];
        tail.resize(3 + 32, 0);
        tail[3 + 8] = 0x80;
        tail[3 + 9] = 0x40;
        tail[3 + 10] = 0x20;
        d.ch.mock_reply(&reply(5, &tail));
        // cur cc = 目標と同じ → cc 書き込み無し
        d.ch.mock_reply(&reply(5, &[1, 0, 2, 38, 35, 40]));
        let s = LightingState {
            on: true,
            effect: 15,
            color: Rgb::new(0x20, 0x40, 0x80),
            brightness: 100,
            speed: 75,
            direction: 0,
            per_key: BTreeMap::new(),
            zone_on: vec![true, true],
            palette: vec![],
            palette_sel: 2,
            color_correction: Some(Rgb::new(38, 35, 40)),
        };
        d.write_lighting(0, &s).unwrap();
        let sent = d.ch.mock_sent();
        // sent には READ も混ざる — SET フレームだけ取り出して検証。
        let sets: Vec<_> = sent.iter().filter(|f| f[1] == sub::SETTING).collect();
        assert_eq!(sets.len(), 1, "frames: {sent:?}");
        assert_eq!(&sets[0][..10], &[5, 2, 0, 0, 3, 15, 100, 75, 0, 2]);
    }

    /// jo enum 合成: 両方=3 / 上のみ=2 / 下のみ=1 / 閉=0。非 DoubleLighting
    /// エリアは真偽 (on→1)。
    #[test]
    fn lighting_zone_open() {
        let d = mock_driver();
        let mk = |zone_on: Vec<bool>, on: bool| LightingState {
            on,
            effect: 0,
            color: Rgb::new(0, 0, 0),
            brightness: 0,
            speed: 0,
            direction: 0,
            per_key: BTreeMap::new(),
            zone_on,
            palette: vec![],
            palette_sel: 0,
            color_correction: None,
        };
        assert_eq!(d.compose_open(0, &mk(vec![true, true], true)), 3);
        assert_eq!(d.compose_open(0, &mk(vec![true, false], true)), 2);
        assert_eq!(d.compose_open(0, &mk(vec![false, true], true)), 1);
        assert_eq!(d.compose_open(0, &mk(vec![false, false], false)), 0);
        // zone_on が無い時は on→3 (DoubleLighting)
        assert_eq!(d.compose_open(0, &mk(vec![], true)), 3);
        // SingleLighting エリアは真偽
        assert_eq!(d.compose_open(1, &mk(vec![], true)), 1);
        assert_eq!(d.compose_open(1, &mk(vec![], false)), 0);
    }
}
