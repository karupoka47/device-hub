//! LAMZU Aurora (www.lamzu.net) 系マウス — 64B feature report プロトコル。
//!
//! 公式 Web HUB の JS 解析 + Maya X V2 / LM20 有線 (37b0:0044) 実機検証
//! (docs/PROTOCOL_NOTES.md §5)。
//!
//! - 設定チャネル: if2 `FFFF:00`、64B feature report のみ (output report 無し)。
//! - 送信 `[0,0,2,sub,f,cmd,devID,args…]` (devID = プロファイル番号 1..4、
//!   ライト系と一部の devID 無しコマンドは別レイアウト)。
//! - 応答 `a[1]`=ステータス (A1=ACK)、`a[4]`=ペイロード長、`a[6]`=cmd エコー、
//!   `a[7]`=devID、`a[8..]`=値。書き込みは即時適用・flash 永続。
//! - 自律通知 `[4,type,…]` は if1 `FFA0:01` の input report。
//! - 未初期化プロファイル枠は走査しない (p4 が HID エラーを起こす場合あり)。

use devhub_core::backend::*;
use devhub_core::lamzu::{caps as lamzu_caps, model_by_pid, LamzuModel, MODELS};
use devhub_core::model::*;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::sleep;
use std::time::Duration;

/// feature report 往復の待ち。有線は ~10-30ms で済むが、ドングル中継は
/// 応答バッファの更新が ~200ms 遅れることがある。
const CMD_DELAY: Duration = Duration::from_millis(120);

// ---------------------------------------------------------------- コマンド

/// feature report 往復: `[0,0,b2,sub,f,cmd,args…]` を送り応答を返す。
/// 応答は report ID 込み 65B。`None` は a1 でない応答/失敗。
fn cmd(
    dev: &hidapi::HidDevice,
    b2: u8,
    sub: u8,
    f: u8,
    command: u8,
    args: &[u8],
) -> HubResult<Vec<u8>> {
    // 初期化系 (cmd 13 = resetProfile) は再送しない — 二重実行を避ける。
    cmd_ex(dev, b2, sub, f, command, args, command != 13)
}

/// 送信だけして応答を待たない (公式 `defaultDevice` と同じ。完全初期化は応答を返さない)。
fn send_only(dev: &hidapi::HidDevice, b2: u8, sub: u8, f: u8, command: u8, args: &[u8]) -> HubResult<()> {
    let mut w = [0u8; 65];
    w[3] = b2;
    w[4] = sub;
    w[5] = f;
    w[6] = command;
    let n = args.len().min(56);
    w[7..7 + n].copy_from_slice(&args[..n]);
    dev.send_feature_report(&w)
        .map(|_| ())
        .map_err(|e| HubError::Io(format!("lamzu feature write: {e}")))
}

fn cmd_ex(
    dev: &hidapi::HidDevice,
    b2: u8,
    sub: u8,
    f: u8,
    command: u8,
    args: &[u8],
    resend: bool,
) -> HubResult<Vec<u8>> {
    let mut w = [0u8; 65];
    w[3] = b2;
    w[4] = sub;
    w[5] = f;
    w[6] = command;
    let n = args.len().min(56);
    w[7..7 + n].copy_from_slice(&args[..n]);
    // 送信は 1 回 — ドングル中継では応答生成が遅れるので、get を
    // 繰り返して a[6]==cmd の新しい応答が来るのを待つ。
    // (再送信するとデバイス側の処理タイマーがリセットされて余計遅れる。)
    dev.send_feature_report(&w)
        .map_err(|e| HubError::Io(format!("lamzu feature write: {e}")))?;
    let mut last = HubError::Protocol("lamzu: response timeout".into());
    // ドングル経由の一部書込み (ポーリング変更等) は RF 再ネゴシエーションで
    // 応答生成まで数秒かかることがある — get は余裕を持って繰り返す。
    // 送信自体が取りこぼされると応答が再生成されず stale バッファのまま
    // 固まるので、公式 retrySetGet と同じく途中で 1 度だけ再送する。
    for i in 0..25 {
        // フラッシュ書込みのコミット中は次コマンドを取りこぼすことがある —
        // 2 回まで再送 (retrySetGet 相当)。
        if resend && (i == 8 || i == 16) {
            dev.send_feature_report(&w).ok();
        }
        sleep(CMD_DELAY);
        let mut r = [0u8; 65];
        match dev.get_feature_report(&mut r) {
            Ok(len) if len >= 8 => {
                // 応答レイアウト (fixtures/lamzu/lm20-wired-active.txt):
                // r[3]=target エコー, r[4]=宣言ペイロード長, r[5]=f エコー,
                // r[6]=cmd エコー, r[7..7+r[4]]=ペイロード。
                // cmd だけでは f 違いの残りバッファ (ボタン get f3 と
                // ポーリング get f1 は共に cmd=0x80) と衝突するので全照合する。
                if r[6] == command && r[5] == f && r[3] == b2 {
                    // GET 系 (cmd>=0x80) はペイロード先頭に args[0]
                    // (dev/profile) をエコー。ボタン get はスロット番号
                    // args[1] も r[8] にエコー — 連続スロット読みで隣の
                    // 古い応答を誤採用しないための識別。
                    let arg_ok = command < 0x80 || args.first().is_none_or(|a| r[7] == *a);
                    let btn_ok = !(f == 3 && command == 0x80)
                        || args.get(1).is_none_or(|a| r[8] == *a);
                    let len_ok = (r[4] as usize) <= len.saturating_sub(7);
                    if arg_ok && btn_ok && len_ok {
                        return if r[1] == 0xA1 {
                            Ok(r[..len].to_vec())
                        } else {
                            // このコマンドへの正式な応答だが ACK ではない (a2/a3)。
                            Err(HubError::Protocol(format!(
                                "lamzu: cmd {command:#04x} -> tag {:02x}",
                                r[1]
                            )))
                        };
                    }
                    last = HubError::Protocol(format!(
                        "lamzu: cmd {command:#04x} -> mismatched payload f{:02x}/len{}/arg{:02x}",
                        r[5], r[4], r[7]
                    ));
                    continue;
                }
                last = HubError::Protocol(format!(
                    "lamzu: cmd {command:#04x} -> stale f{:02x} echo {:02x}",
                    r[5], r[6]
                ));
            }
            Ok(_) => {}
            Err(e) => last = HubError::Io(format!("lamzu feature read: {e}")),
        }
    }
    Err(last)
}

/// プロファイル対象コマンド (devID=profile 1..4 を args 先頭に置く) の値 `a[8]`。
fn u8_at(resp: &[u8], i: usize) -> HubResult<u8> {
    resp.get(i)
        .copied()
        .ok_or_else(|| HubError::Protocol("lamzu: short payload".into()))
}

// ---------------------------------------------------------------- ドライバ

pub struct LamzuDriver {
    dev: Mutex<hidapi::HidDevice>,
    info: Mutex<DeviceInfo>,
    /// 機種 (ペア先) とセンサーで決まる。ドングルでペア先が後から分かれば作り直す。
    caps: Mutex<Capabilities>,
    /// 公式機種定義の行。None = 未接続 / 未知のペア先。
    model: Mutex<Option<&'static LamzuModel>>,
    /// センサー型番コマンドの応答 (4 = PAW3955)。
    sensor: Mutex<u8>,
    stop: Arc<AtomicBool>,
}

pub fn open(
    dev: hidapi::HidDevice,
    info: DeviceInfo,
    events: Option<Arc<dyn Fn(HubEvent) + Send + Sync>>,
) -> HubResult<LamzuDriver> {
    dev.set_blocking_mode(false).ok();
    let stop = Arc::new(AtomicBool::new(false));
    // 通知チャネル if1 FFA0:01 — 自分で開いて input report を読む。
    if let Some(ev) = events {
        spawn_notify(info.clone(), ev, stop.clone());
    }
    let d = LamzuDriver {
        dev: Mutex::new(dev),
        info: Mutex::new(info.clone()),
        caps: Mutex::new(lamzu_caps(&MODELS[0], info.transport, 4)),
        model: Mutex::new(None),
        sensor: Mutex::new(4),
        stop,
    };
    d.detect();
        Ok(d)
}

impl Drop for LamzuDriver {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// `if1 FFA0:01` の input report を読み、`[4,type,…]` を HubEvent に変換する。
fn spawn_notify(info: DeviceInfo, ev: Arc<dyn Fn(HubEvent) + Send + Sync>, stop: Arc<AtomicBool>) {
    let (vid, pid) = (info.vid, info.pid);
    let serial = info.serial.clone();
    std::thread::spawn(move || {
        let Ok(api) = hidapi::HidApi::new() else {
            return;
        };
        // 同じ VID/PID の受信機が複数あると先頭一致で別個体に紐付く —
        // シリアルが取れていれば一致を要求する。
        let Some(d) = api
            .device_list()
            .find(|d| {
                d.vendor_id() == vid
                    && d.product_id() == pid
                    && d.interface_number() == 1
                    && d.usage_page() == 0xFFA0
                    && d.usage() == 0x01
                    && serial
                        .as_deref()
                        .is_none_or(|s| s.is_empty() || d.serial_number() == Some(s))
            })
            .and_then(|i| i.open_device(&api).ok())
        else {
            return;
        };
        d.set_blocking_mode(false).ok();
        let mut buf = [0u8; 64];
        while !stop.load(Ordering::Relaxed) {
            match d.read_timeout(&mut buf, 250) {
                Ok(n) if n >= 3 && buf[0] == 4 => match buf[1] {
                    // type3 = 電池 (b[2]=%, b[3]=充電)
                    3 => {
                        let id = info.id.clone();
                        ev(HubEvent::Battery(
                            id,
                            Battery {
                                percent: Some(buf[2].min(100)),
                                charging: buf[3] == 1,
                            },
                        ));
                    }
                    // type1=DPI, 2=プロファイル, 6=接続/切断, 7=LOD,
                    // 8=ポーリング, 13=モード — UI に再読させる。
                    1 | 2 | 6 | 7 | 8 | 13 => {
                        ev(HubEvent::StateChanged(info.id.clone()));
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    });
}

impl LamzuDriver {
    /// 機種 (有線 = 自分の PID、ドングル = GetDevicePID のペア先) とセンサーを判定し、
    /// 製品名・モデル・caps を確定する。未知のペア先は `supported=false`。
    fn detect(&self) {
        let (transport, own_pid) = {
            let i = self.info.lock().unwrap_or_else(|p| p.into_inner());
            (i.transport, i.pid)
        };
        let pid = if transport == Transport::Wireless24G {
            match self.run(1, 6, 0, 139, &[2]) {
                Ok(r) if r.len() >= 13 => {
                    let vid = u16::from_be_bytes([r[9], r[10]]);
                    let pid = u16::from_be_bytes([r[11], r[12]]);
                    (vid == 0x37B0).then_some(pid)
                }
                _ => {
                    // マウスが眠っている/未接続 — 受信機名のまま。
                    self.info.lock().unwrap_or_else(|p| p.into_inner()).asleep = true;
                    return;
                }
            }
        } else {
            Some(own_pid)
        };
        let model = pid.and_then(model_by_pid);
        // センサー型番 [2,1,1,143] → a[7] (1=3395, 2=3950, 4=3955)。
        let sensor = self
            .run(2, 1, 1, 143, &[])
            .ok()
            .and_then(|r| r.get(7).copied())
            .unwrap_or(4);
        *self.sensor.lock().unwrap_or_else(|p| p.into_inner()) = sensor;
        *self.model.lock().unwrap_or_else(|p| p.into_inner()) = model;
        let m = model.unwrap_or(&MODELS[0]);
        *self.caps.lock().unwrap_or_else(|p| p.into_inner()) = lamzu_caps(m, transport, sensor);
        let mut i = self.info.lock().unwrap_or_else(|p| p.into_inner());
        i.asleep = false;
        i.supported = model.is_some();
        if let Some(m) = model {
            i.name = m.name.into();
            i.model = m.model.into();
        } else if transport == Transport::Wireless24G {
            i.name = "LAMZU 8K レシーバー (未対応のマウス)".into();
            i.model = "lamzu-dongle".into();
        }
    }

    fn caps_now(&self) -> Capabilities {
        self.caps.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn has(&self, id: &str) -> bool {
        self.caps.lock().unwrap_or_else(|p| p.into_inner()).setting(id).is_some()
    }

    fn run(&self, b2: u8, sub: u8, f: u8, command: u8, args: &[u8]) -> HubResult<Vec<u8>> {
        let dev = self.dev.lock().unwrap_or_else(|p| p.into_inner());
        cmd(&dev, b2, sub, f, command, args)
    }

    /// get: `[2,sub,f,cmd|0x80,dev]` → `a[8]` 値。
    fn get(&self, sub: u8, f: u8, command: u8, dev: u8) -> HubResult<u8> {
        let r = self.run(2, sub, f, command, &[dev])?;
        u8_at(&r, 8)
    }

    /// set: `[2,sub,f,cmd,dev,val]`。
    /// 同値書込みは FW が応答を生成しない (応答バッファが前コマンドのまま
    /// 固まる) ので、まず現在値を読んで同じなら送らない。
    /// ドングル経由の応答遅延で失敗したら get (cmd|0x80) で読み戻し検証。
    fn set(&self, sub: u8, f: u8, command: u8, dev: u8, val: u8) -> HubResult<()> {
        if self.get(sub, f, command | 0x80, dev).ok() == Some(val) {
            return Ok(());
        }
        match self.run(2, sub, f, command, &[dev, val]) {
            Ok(_) => Ok(()),
            Err(e) => {
                let back = self.get(sub, f, command | 0x80, dev);
                if back.as_ref().ok() == Some(&val) {
                    return Ok(());
                }
                Err(HubError::Protocol(format!("{e} [verify {back:?}]")))
            }
        }
    }

    /// ドングル経由か (37b0:0048 Orbital 8K)。
    fn is_dongle(&self) -> bool {
        self.info
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .transport
            == Transport::Wireless24G
    }

    /// 現在のプロファイル (1..4)。不明なときは Err —
    /// 勝手に 1 に落とすと別プロファイルを編集中に 1 番を書き潰し得る。
    fn active_profile(&self) -> HubResult<u8> {
        let r = self.run(2, 1, 0, 133, &[])?;
        let p = u8_at(&r, 7)?;
        if (1..=4).contains(&p) {
            Ok(p)
        } else {
            Err(HubError::Protocol(format!(
                "lamzu: bad active profile {p:#04x}"
            )))
        }
    }

    /// ボタン 1 個読み出し。空エントリは echo が返るので Disabled。
    fn read_button(&self, profile: u8, btn: u8) -> ButtonAction {
        let Ok(r) = self.run(2, 15, 3, 128, &[profile, btn, 0, 255, 10]) else {
            return ButtonAction::Disabled;
        };
        if r.len() < 12 {
            return ButtonAction::Disabled;
        }
        let (ty, len) = (
            u8_at(&r, 10).unwrap_or(0),
            u8_at(&r, 11).unwrap_or(0) as usize,
        );
        if ty == 0xFF || len == 0x0A {
            // リクエスト echo (未初期化エントリ)。
            return ButtonAction::Disabled;
        }
        let p = &r[12..(12 + len).min(r.len())];
        decode_button(ty, p)
    }

    fn write_button(&self, profile: u8, btn: u8, a: &ButtonAction) -> HubResult<()> {
        let (ty, params) = encode_button(a)?;
        let mut args = vec![profile, btn, 0, ty, params.len() as u8];
        args.extend_from_slice(&params);
        self.run(2, (5 + params.len()) as u8, 3, 0, &args)
            .map(|_| ())
    }

    /// ステージ値+色の読み出し。count=0・範囲外・短い応答は Err
    /// (異常値を正常データに丸めない)。
    /// 速度連動 DPI カーブ (公式 getAdaptiveDPICurve)。
    fn read_adaptive(&self, dev: u8) -> HubResult<AdaptiveCurve> {
        let r = self.run(2, 9, 1, 154, &[dev])?;
        let enabled = u8_at(&r, 8)? == 1;
        let n = u8_at(&r, 12)? as usize;
        if n > 13 || r.len() < 13 + n * 4 {
            return Err(HubError::Protocol(format!("lamzu: bad adaptive curve n={n}")));
        }
        let points = (0..n)
            .map(|i| {
                let b = &r[13 + i * 4..13 + i * 4 + 4];
                AdaptivePoint {
                    speed: u16::from_be_bytes([b[0], b[1]]),
                    ratio: u16::from_be_bytes([b[2], b[3]]) as f32 / 100.,
                }
            })
            .collect();
        Ok(AdaptiveCurve { enabled, points })
    }

    /// 速度連動 DPI カーブの書き込み (公式 setAdaptiveDPICurve:
    /// `[2,4+4N,1,26,prof,en,0,N,(速度 u16BE, 倍率×100 u16BE)×N]`)。入力は先に検証する。
    fn write_adaptive(&self, prof: u8, c: &AdaptiveCurve) -> HubResult<()> {
        let Some(ac) = self.caps_now().mouse.and_then(|m| m.adaptive_dpi) else {
            return Err(HubError::Unsupported("adaptive dpi"));
        };
        let n = c.points.len();
        if n < ac.min_points || n > ac.max_points {
            return Err(HubError::Invalid("速度連動 DPI の点の数".into()));
        }
        let mut last = None;
        for p in &c.points {
            if p.speed > ac.speed_max
                || !(0.0..=ac.ratio_max + 1e-3).contains(&p.ratio)
                || last.is_some_and(|l| p.speed <= l)
            {
                return Err(HubError::Invalid("速度連動 DPI の点の値".into()));
            }
            last = Some(p.speed);
        }
        let mut args = vec![prof, c.enabled as u8, 0, n as u8];
        for p in &c.points {
            args.extend_from_slice(&p.speed.to_be_bytes());
            args.extend_from_slice(&((p.ratio * 100.).round() as u16).to_be_bytes());
        }
        match self.run(2, (4 + n * 4) as u8, 1, 26, &args) {
            Ok(_) => Ok(()),
            // 同値書込み等で応答が来ないときは読み戻しで判定。
            Err(e) => match self.read_adaptive(prof) {
                Ok(back) if back.enabled == c.enabled && back.points.len() == n => Ok(()),
                _ => Err(e),
            },
        }
    }

    fn read_stages(&self, dev: u8) -> HubResult<Vec<DpiStage>> {
        let r = self.run(2, 10, 1, 129, &[dev, 6])?;
        let n = u8_at(&r, 8)? as usize;
        if !(1..=5).contains(&n) || r.len() < 9 + n * 4 {
            return Err(HubError::Protocol(format!("lamzu: bad dpi table n={n}")));
        }
        let colors = self.run(2, 19, 2, 129, &[dev]).ok();
        Ok((0..n)
            .map(|i| {
                let b = &r[9 + i * 4..9 + i * 4 + 4];
                let (x, y) = (
                    u16::from_be_bytes([b[0], b[1]]) as u32,
                    u16::from_be_bytes([b[2], b[3]]) as u32,
                );
                let (cr, cg, cb) = colors
                    .as_ref()
                    .and_then(|c| c.get(8 + i * 3..8 + i * 3 + 3).map(|s| (s[0], s[1], s[2])))
                    .unwrap_or((255, 255, 255));
                DpiStage {
                    x,
                    y: if y == x { None } else { Some(y) },
                    color: Rgb {
                        r: cr,
                        g: cg,
                        b: cb,
                    },
                    enabled: true,
                }
            })
            .collect())
    }

    /// ステージ値+色の書き込み (アクティブ段は変えない)。
    /// 値→色→有効段の順に別コマンドなので、入力検証を先に済ませる。
    fn write_stages(&self, prof: u8, stages: &[DpiStage]) -> HubResult<()> {
        if stages.is_empty() || stages.len() > 5 {
            return Err(HubError::Invalid("dpi stage count".into()));
        }
        for s in stages {
            let ok = (50..=50000).contains(&s.x)
                && s.y.is_none_or(|y| (50..=50000).contains(&y));
            if !ok {
                return Err(HubError::Invalid("dpi value".into()));
            }
        }
        let mut args = vec![prof, stages.len() as u8];
        for s in stages {
            args.extend_from_slice(&(s.x as u16).to_be_bytes());
            args.extend_from_slice(&(s.y.unwrap_or(s.x) as u16).to_be_bytes());
        }
        self.run(2, 26, 1, 1, &args)?;
        // 色: [2,19,2,1,dev,RGB×6] — 6枠分パディング。
        let mut cargs = vec![prof];
        for s in stages {
            cargs.extend_from_slice(&[s.color.r, s.color.g, s.color.b]);
        }
        cargs.resize(1 + 18, 0);
        self.run(2, 19, 2, 1, &cargs)?;
        // フラッシュコミット待ち — 直後のコマンドが取りこぼされるのを防ぐ。
        sleep(Duration::from_millis(300));
        Ok(())
    }
}

// ---------------------------------------------------------------- ボタン

fn decode_button(ty: u8, p: &[u8]) -> ButtonAction {
    match ty {
        1 => ButtonAction::Mouse(match p.first().copied().unwrap_or(0) {
            1 => MouseFn::Left,
            2 => MouseFn::Right,
            3 => MouseFn::Middle,
            4 => MouseFn::Back,
            5 => MouseFn::Forward,
            16 => MouseFn::ScrollUp,
            17 => MouseFn::ScrollDown,
            // due enum の旧 DPI コード (公式 jM は type7 を使うが、過去の
            // 書き込みで残っている可能性があるので読みだけは対応)。
            18 => return ButtonAction::Dpi(DpiFn::Up),
            19 => return ButtonAction::Dpi(DpiFn::Down),
            20 | 21 => return ButtonAction::Dpi(DpiFn::Cycle),
            _ => {
                return ButtonAction::Unsupported {
                    ty,
                    params: p.to_vec(),
                }
            }
        }),
        2 if p.len() >= 5 => ButtonAction::Fire {
            button: match p[1] {
                2 => MouseFn::Right,
                3 => MouseFn::Middle,
                4 => MouseFn::Back,
                5 => MouseFn::Forward,
                _ => MouseFn::Left,
            },
            count: p[2],
            interval_ms: u16::from_be_bytes([p[3], p[4]]),
        },
        4 if p.len() >= 2 => ButtonAction::Key(KeyStroke {
            mods: p[0],
            usage: p[1] as u16,
        }),
        5 if p.len() >= 2 => {
            let usage = u16::from_be_bytes([p[0], p[1]]);
            ButtonAction::Media(match usage {
                205 => MediaFn::PlayPause,
                183 => MediaFn::Stop,
                181 => MediaFn::Next,
                182 => MediaFn::Prev,
                233 => MediaFn::VolumeUp,
                234 => MediaFn::VolumeDown,
                226 => MediaFn::Mute,
                402 => MediaFn::Calculator,
                404 => MediaFn::MyComputer,
                406 => MediaFn::Browser,
                394 => MediaFn::Mail,
                111 => MediaFn::BrightnessUp,
                112 => MediaFn::BrightnessDown,
                551 => MediaFn::Refresh,
                _ => {
                    return ButtonAction::Unsupported {
                        ty,
                        params: p.to_vec(),
                    }
                }
            })
        }
        // DPI 系は公式 jM() と同じ type7 + Er enum
        // (CycleUp=6/CycleDown=7/Up=1/Down=2/Lock=5+[Xhi,Xlo,Xhi,Xlo])。
        7 => ButtonAction::Dpi(match p.first().copied().unwrap_or(0) {
            1 => DpiFn::Up,
            2 => DpiFn::Down,
            5 if p.len() >= 5 => DpiFn::Shift(u32::from(p[1]) << 8 | u32::from(p[2])),
            6 | 7 => DpiFn::Cycle,
            _ => {
                return ButtonAction::Unsupported {
                    ty,
                    params: p.to_vec(),
                }
            }
        }),
        8 => ButtonAction::ProfileCycle,
        13 => ButtonAction::PollingCycle,
        // 公式 MacroI/II/III: type16=回数 [slotHi,slotLo,count]、
        // 17=離すまで、18=再押下まで (params は slot のみ)。
        16 if p.len() >= 3 => ButtonAction::Macro {
            slot: u16::from_be_bytes([p[0], p[1]]) as u8,
            run: MacroRun::Times(p[2] as u16),
        },
        17 if p.len() >= 2 => ButtonAction::Macro {
            slot: u16::from_be_bytes([p[0], p[1]]) as u8,
            run: MacroRun::WhileHeld,
        },
        18 if p.len() >= 2 => ButtonAction::Macro {
            slot: u16::from_be_bytes([p[0], p[1]]) as u8,
            run: MacroRun::Toggle,
        },
        // 解釈できない type (14=LOD循環, 20=競技モード切替 等) は
        // Disabled に潰さず Unsupported で保持 — 往復書き戻しで消えない。
        _ => ButtonAction::Unsupported {
            ty,
            params: p.to_vec(),
        },
    }
}

fn encode_button(a: &ButtonAction) -> HubResult<(u8, Vec<u8>)> {
    use ButtonAction::*;
    Ok(match a {
        Disabled => (0, vec![]),
        Mouse(f) => {
            let c = match f {
                MouseFn::Left => 1,
                MouseFn::Right => 2,
                MouseFn::Middle => 3,
                MouseFn::Back => 4,
                MouseFn::Forward => 5,
                MouseFn::ScrollUp => 16,
                MouseFn::ScrollDown => 17,
                _ => return Err(HubError::Unsupported("mouse fn")),
            };
            (1, vec![c])
        }
        // 公式 jM(): DPI は type7 + Er enum、ロックは [5,Xhi,Xlo,Xhi,Xlo]。
        Dpi(DpiFn::Cycle) => (7, vec![6]),
        Dpi(DpiFn::Up) => (7, vec![1]),
        Dpi(DpiFn::Down) => (7, vec![2]),
        Dpi(DpiFn::Shift(d)) => (
            7,
            vec![5, (*d >> 8) as u8, *d as u8, (*d >> 8) as u8, *d as u8],
        ),
        Fire {
            button,
            count,
            interval_ms,
        } => {
            // 公式の連射 (Xクリック) は左クリック固定。他のボタンは実機未検証なので書かない。
            if *button != MouseFn::Left {
                return Err(HubError::Unsupported("連射は左クリックのみ (公式と同じ)"));
            }
            let b = 1;
            let [hi, lo] = interval_ms.to_be_bytes();
            (2, vec![1, b, *count, hi, lo])
        }
        Key(k) => (4, vec![k.mods, (k.usage & 0xff) as u8]),
        Media(m) => {
            let usage: u16 = match m {
                MediaFn::PlayPause => 205,
                MediaFn::Stop => 183,
                MediaFn::Next => 181,
                MediaFn::Prev => 182,
                MediaFn::VolumeUp => 233,
                MediaFn::VolumeDown => 234,
                MediaFn::Mute => 226,
                MediaFn::Calculator => 402,
                MediaFn::MyComputer => 404,
                MediaFn::Browser => 406,
                MediaFn::Mail => 394,
                MediaFn::BrightnessUp => 111,
                MediaFn::BrightnessDown => 112,
                MediaFn::Refresh => 551,
            };
            (5, usage.to_be_bytes().to_vec())
        }
        // 公式 MacroI/II/III: 回数=type16 [slot,count]、離すまで=17、
        // 再押下まで=18 (params は slot のみ、回数フィールドなし)。
        Macro { slot, run } => {
            let [hi, lo] = (*slot as u16).to_be_bytes();
            match run {
                MacroRun::Times(n) => (16, vec![hi, lo, (*n).min(255) as u8]),
                MacroRun::WhileHeld => (17, vec![hi, lo]),
                MacroRun::Toggle => (18, vec![hi, lo]),
            }
        }
        // 公式は循環パラメータ LoopUp=3 を送る。
        PollingCycle => (13, vec![3]),
        ProfileCycle => (8, vec![3]),
        ScrollModeToggle | GShift => return Err(HubError::Unsupported("scroll-mode toggle / G-shift")),
        // デバイス側の未解釈割当はそのまま往復で書き戻せる。
        Unsupported { ty, params } => (*ty, params.clone()),
    })
}

// ---------------------------------------------------------------- caps

// ---------------------------------------------------------------- DeviceDriver

impl DeviceDriver for LamzuDriver {
    fn info(&self) -> DeviceInfo {
        // ドングルで開いた時にマウスが寝ていた/未判定なら、ここで判定し直す。
        if self.model.lock().unwrap_or_else(|p| p.into_inner()).is_none() {
            self.detect();
        }
        let mut i = self.info.lock().unwrap_or_else(|p| p.into_inner()).clone();
        // マウス FW: [2,16,0,129] → a[7..11] = 4B バージョン (監査: 4B目固定0は誤り)。
        match self.run(2, 16, 0, 129, &[]) {
            Ok(r) => {
                i.asleep = false;
                if let Some(v) = r.get(7..11) {
                    i.firmware = Some(format!("{}.{}.{}.{}", v[0], v[1], v[2], v[3]));
                }
            }
            // ドングル経由でマウスが応答しない = スリープ/未接続。
            Err(_) if i.transport == Transport::Wireless24G => i.asleep = true,
            Err(_) => {}
        }
        // 受信機 FW: [0,16,0,129] (target 0)。
        if i.transport == Transport::Wireless24G {
            if let Ok(r) = self.run(0, 16, 0, 129, &[]) {
                if let Some(v) = r.get(7..11) {
                    i.dongle_firmware = Some(format!("{}.{}.{}.{}", v[0], v[1], v[2], v[3]));
                }
            }
        }
        if let Ok(b) = self.battery() {
            i.battery = Some(b);
        }
        // シリアル: EID 応答 a[9..] ASCII
        if let Ok(r) = self.run(2, 1, 0, 130, &[]) {
            if let Some(tail) = r.get(9..) {
                if let Some(end) = tail.iter().position(|&b| b == 0 || b == 0xFF) {
                    if end > 0 {
                        i.serial = Some(String::from_utf8_lossy(&tail[..end]).into_owned());
                    }
                }
            }
        }
        // supported は detect() が機種表で決めた値のまま (未知のペア先は false)。
        i
    }

    fn caps(&self) -> Capabilities {
        self.caps_now()
    }

    fn read(&self) -> HubResult<DeviceState> {
        // プロファイル不明のまま続行すると、読み取った値がどの
        // プロファイルのものか分からず書き込み先も誤る — 失敗を伝える。
        let prof = self.active_profile()?;
        let dev = prof;
        let mut settings = BTreeMap::new();

        // ポーリング: 8,4,2,1,32,64,128 → Hz。
        // 読めなければ入れない (既定値で埋めると 1000Hz と誤表示する)。
        if let Ok(code) = self.get(2, 1, 128, dev) {
            let rate = match code {
                8 => 125,
                4 => 250,
                2 => 500,
                // 公式 getPollingRate は 16 を 1 (1000Hz) として扱う。
                1 | 16 => 1000,
                32 => 2000,
                64 => 4000,
                128 => 8000,
                v => v as i64,
            };
            settings.insert("polling_wireless".into(), SettingValue::Choice(rate));
        }

        // LOD: >=128 は (v-128)/10 mm、BCD 0x1x は 1.xmm、1=1mm、2=2mm。
        if let Ok(v) = self.get(2, 1, 136, dev) {
            let mm10 = if v >= 128 {
                (v - 128) as i64
            } else if (0x10..=0x19).contains(&v) {
                (v >> 4) as i64 * 10 + (v & 0x0f) as i64
            } else if v == 3 {
                7
            } else {
                v as i64 * 10
            };
            settings.insert("lod".into(), SettingValue::Choice(mm10.clamp(1, 30)));
        }

        for (sub, f, cmdg, id) in [
            (2u8, 1u8, 137u8, "motion_sync"),
            (2, 1, 132, "angle_snap"),
            (2, 1, 138, "ripple"),
            (2, 1, 139, "hyper_mode"),
            (2, 1, 147, "tracking_mode"),
            (2, 1, 141, "dpi_xy"),
        ] {
            if let Ok(v) = self.get(sub, f, cmdg, dev) {
                settings.insert(id.into(), SettingValue::Bool(v == 1));
            }
        }
        if let Ok(v) = self.get(2, 1, 148, dev) {
            let signed = if v > 100 { v as i64 - 256 } else { v as i64 };
            settings.insert("angle_tune".into(), SettingValue::Num(signed as f32));
        }
        // SensorOffset は値が a[9] (a[8] は空き)。
        if let Ok(r) = self.run(2, 3, 1, 149, &[dev]) {
            let v = u8_at(&r, 9).unwrap_or(0);
            let signed = if v > 100 { v as i64 - 256 } else { v as i64 };
            settings.insert("sensor_offset".into(), SettingValue::Num(signed as f32));
        }
        // ドングル経由のみ有効な項目 (有線 FW では a3/無視): 受信機 3 灯、
        // ガラスモード。DPI インジケーターは無線でも a3 なので読まない。
        if self.is_dongle() {
            // マスタースイッチ: light_idx=255 のモード — 公式 K0() は
            // 既知設定があり値が 0 以外なら ON (実機は 0xFF を返す)。
            if let Ok(r) = self.run(1, 5, 2, 128, &[dev, 255]) {
                settings.insert(
                    "dongle_led".into(),
                    SettingValue::Bool(u8_at(&r, 9).unwrap_or(0) != 0),
                );
            }
            for (id, idx) in [("dongle_led1", 5u8), ("dongle_led2", 3), ("dongle_led3", 4)] {
                if let Ok(r) = self.run(1, 5, 2, 128, &[dev, idx]) {
                    settings.insert(
                        id.into(),
                        SettingValue::Choice(u8_at(&r, 9).unwrap_or(0) as i64),
                    );
                }
            }
        }
        if let Ok(v) = self.get(2, 0, 136, dev) {
            settings.insert("debounce".into(), SettingValue::Num(v as f32));
        }
        if let Ok(r) = self.run(2, 3, 0, 135, &[dev]) {
            let secs = u16::from_be_bytes([u8_at(&r, 8).unwrap_or(0), u8_at(&r, 9).unwrap_or(0)]);
            // 公式は 0/0xFF00/0xFFFF を「無効」と表示する (書き込みの選択肢には無い)。
            // その場合は値を入れない — UI は選択なしで表示し、選べば有効な分数を書く。
            if !matches!(secs, 0 | 0xFF00 | 0xFFFF) {
                settings.insert(
                    "sleep_min".into(),
                    SettingValue::Choice(((secs / 60).max(1)) as i64),
                );
            }
        }
        // SpeedEnable: a[8]=L, a[9]=R。
        if let Ok(r) = self.run(2, 3, 0, 154, &[dev]) {
            settings.insert(
                "rapid_left".into(),
                SettingValue::Bool(u8_at(&r, 8).unwrap_or(0) == 1),
            );
            settings.insert(
                "rapid_right".into(),
                SettingValue::Bool(u8_at(&r, 9).unwrap_or(0) == 1),
            );
        }
        // ScrollHP (B-hop): a[8]=方向, a[9..11]=u16BE ms。
        if let Ok(r) = self.run(2, 4, 0, 153, &[dev]) {
            settings.insert(
                "bhop_dir".into(),
                SettingValue::Choice(u8_at(&r, 8).unwrap_or(0) as i64),
            );
            let ms = u16::from_be_bytes([u8_at(&r, 9).unwrap_or(0), u8_at(&r, 10).unwrap_or(0)]);
            settings.insert("bhop_ms".into(), SettingValue::Num(ms as f32));
        }
        // 速度連動 DPI (AdaDPI): a[8]=有効, a[12]=点数, a[13..]=(速度 u16BE, 倍率×100 u16BE)×N。
        let adaptive = if self
            .caps_now()
            .mouse
            .as_ref()
            .is_some_and(|m| m.adaptive_dpi.is_some())
        {
            self.read_adaptive(dev).ok()
        } else {
            None
        };
        // ガラスモード: [2,1,1,142] (プロファイル引数なし) → a[7]。
        if self.has("glass_mode") {
            if let Ok(r) = self.run(2, 1, 1, 142, &[]) {
                settings.insert(
                    "glass_mode".into(),
                    SettingValue::Bool(u8_at(&r, 7).unwrap_or(0) == 1),
                );
            }
        }

        // DPI ステージ: get [dev,6] → a[8]=数, a[9..]=4B×N (X,Y u16BE)。
        // 読めなければ空のまま (異常値を丸めない)。
        let stages = self.read_stages(dev).unwrap_or_default();
        let mut active = 0usize;
        if let Ok(v) = self.get(2, 1, 130, dev) {
            active = (v as usize)
                .saturating_sub(1)
                .min(stages.len().saturating_sub(1));
        }

        let buttons = (1..=5u8).map(|b| self.read_button(dev, b)).collect();

        Ok(DeviceState {
            profile: ProfileState {
                active: prof.saturating_sub(1),
                names: vec![],
            },
            settings,
            mouse: Some(MouseState {
                stages,
                active_stage: active,
                buttons,
                macros: vec![],
                adaptive,
            }),
            keyboard: None,
            lighting: BTreeMap::new(),
            peq: None,
        })
    }

    fn apply(&self, change: &Change) -> HubResult<()> {
        // プロファイル切替は active 不明でも通す (修正手段になり得る)。
        if let Change::Profile(i) = change {
            return self
                .run(2, 1, 0, 5, &[i.saturating_add(1)])
                .map(|_| ());
        }
        let prof = self.active_profile()?;
        match change {
            Change::Setting { id, value } => match id.as_str() {
                "polling_wireless" => {
                    let hz = value.as_choice();
                    let allowed = self
                        .caps_now()
                        .setting("polling_wireless")
                        .is_some_and(|sp| match &sp.kind {
                            SettingKind::Choice { options } => options.iter().any(|o| o.value == hz),
                            _ => false,
                        });
                    if !allowed {
                        return Err(HubError::Invalid("この接続では選べないポーリングレートです".into()));
                    }
                    let code = match hz {
                        125 => 8,
                        250 => 4,
                        500 => 2,
                        1000 => 1,
                        2000 => 32,
                        4000 => 64,
                        8000 => 128,
                        _ => return Err(HubError::Invalid("polling rate".into())),
                    };
                    self.set(2, 1, 0, prof, code)
                }
                "lod" => {
                    // 公式 setLOD: <1mm → v*10|0x80、1.xmm → BCD 0x1x、その他 → mm。
                    let mm10 = value.as_choice();
                    let v = match mm10 {
                        1..=9 => (mm10 | 0x80) as u8,
                        10 | 20 => (mm10 / 10) as u8,
                        11..=19 => (((mm10 / 10) << 4) | (mm10 % 10)) as u8,
                        _ => return Err(HubError::Invalid("lod".into())),
                    };
                    self.set(2, 1, 8, prof, v)
                }
                "motion_sync" => self.set(2, 1, 9, prof, value.as_bool() as u8),
                "angle_snap" => self.set(2, 1, 4, prof, value.as_bool() as u8),
                "ripple" => self.set(2, 1, 10, prof, value.as_bool() as u8),
                "hyper_mode" => {
                    let on = value.as_bool();
                    self.set(2, 1, 11, prof, on as u8)?;
                    // 公式 s2(): 競技モード OFF なら 20K FPS も OFF にする。
                    if !on {
                        self.set(2, 1, 19, prof, 0)?;
                    }
                    Ok(())
                }
                "tracking_mode" => {
                    let on = value.as_bool();
                    // 公式: 20K FPS は競技モードが ON のときだけ有効にできる。
                    if on && self.get(2, 1, 139, prof).ok() != Some(1) {
                        return Err(HubError::Invalid(
                            "20K FPS は先に競技モードを ON にしてください".into(),
                        ));
                    }
                    self.set(2, 1, 19, prof, on as u8)
                }
                "dpi_xy" => {
                    // 公式 $2(): 切替時に段の X/Y も書き直す (OFF 時は Y=X)。
                    let on = value.as_bool();
                    self.set(2, 1, 13, prof, on as u8)?;
                    let stages = self.read_stages(prof)?;
                    if !stages.is_empty() {
                        let fixed: Vec<DpiStage> = stages
                            .iter()
                            .map(|s| DpiStage {
                                y: if on { Some(s.y.unwrap_or(s.x)) } else { None },
                                ..*s
                            })
                            .collect();
                        self.write_stages(prof, &fixed)?;
                    }
                    Ok(())
                }
                "angle_tune" => {
                    let v = value.as_num() as i32;
                    self.set(
                        2,
                        1,
                        20,
                        prof,
                        if v < 0 { (255 + v + 1) as u8 } else { v as u8 },
                    )
                }
                "sensor_offset" => {
                    // 値は o[8] (o[7] は空き) — 公式 setSensorOffsetValue と同じ。
                    let v = value.as_num().clamp(-5., 5.) as i32;
                    let enc = if v < 0 { (255 + v + 1) as u8 } else { v as u8 };
                    let read_back = || {
                        self.run(2, 3, 1, 149, &[prof])
                            .ok()
                            .and_then(|r| u8_at(&r, 9).ok())
                    };
                    if read_back() == Some(enc) {
                        return Ok(());
                    }
                    match self.run(2, 3, 1, 21, &[prof, 0, enc]) {
                        Ok(_) => Ok(()),
                        Err(e) => {
                            if read_back() == Some(enc) {
                                return Ok(());
                            }
                            Err(e)
                        }
                    }
                }
                "dongle_led" => {
                    // 公式: ON=SetLightness(1,prof,0,255)、
                    // OFF=SetLightEffect idx255 mode0。モード値が 0 以外なら ON。
                    if value.as_bool() {
                        self.run(1, 3, 2, 2, &[prof, 0, 255]).map(|_| ())
                    } else {
                        self.run(1, 5, 2, 0, &[prof, 255, 0, 0, 0]).map(|_| ())
                    }
                }
                "dongle_led1" | "dongle_led2" | "dongle_led3" => {
                    let idx = match id.as_str() {
                        "dongle_led1" => 5,
                        "dongle_led2" => 3,
                        _ => 4,
                    };
                    let mode = value.as_choice() as u8;
                    self.run(1, 5, 2, 0, &[prof, idx, mode, 0, 0]).map(|_| ())
                }
                // ガラスモード: 公式 setGlassModeDetect [2,1,1,14,v] (プロファイル引数なし)。
                "glass_mode" => {
                    let v = value.as_bool() as u8;
                    let read = || {
                        self.run(2, 1, 1, 142, &[])
                            .ok()
                            .and_then(|r| u8_at(&r, 7).ok())
                    };
                    if read() == Some(v) {
                        return Ok(());
                    }
                    let res = self.run(2, 1, 1, 14, &[v]).map(|_| ());
                    match read() {
                        Some(b) if b == v => Ok(()),
                        Some(_) => Err(HubError::Protocol(
                            "ガラスモードが反映されませんでした (このファームウェアでは未対応の可能性)".into(),
                        )),
                        None => res,
                    }
                }
                "debounce" => self.set(2, 0, 8, prof, value.as_num().clamp(0., 255.) as u8),
                "sleep_min" => {
                    // 公式の選択肢は 1/5/10/15/20/25 分のみ (OFF は書けない)。
                    let min = value.as_choice();
                    if !matches!(min, 1 | 5 | 10 | 15 | 20 | 25) {
                        return Err(HubError::Invalid("スリープ時間は 1〜25 分から選んでください".into()));
                    }
                    let secs = (min * 60) as u16;
                    let [hi, lo] = secs.to_be_bytes();
                    let read_back = || {
                        self.run(2, 3, 0, 135, &[prof]).ok().map(|r| {
                            u16::from_be_bytes([
                                u8_at(&r, 8).unwrap_or(0),
                                u8_at(&r, 9).unwrap_or(0),
                            ])
                        })
                    };
                    if read_back() == Some(secs) {
                        return Ok(());
                    }
                    match self.run(2, 3, 0, 7, &[prof, hi, lo]) {
                        Ok(_) => Ok(()),
                        Err(_) if read_back() == Some(secs) => Ok(()),
                        Err(e) => Err(e),
                    }
                }
                "rapid_left" | "rapid_right" => {
                    let left = id == "rapid_left";
                    let read_lr = || {
                        self.run(2, 3, 0, 154, &[prof])
                            .ok()
                            .map(|r| (u8_at(&r, 8).unwrap_or(0), u8_at(&r, 9).unwrap_or(0)))
                    };
                    let (cur_l, cur_r) =
                        read_lr().ok_or(HubError::Protocol("speedEnable".into()))?;
                    let (nl, nr) = if left {
                        (value.as_bool() as u8, cur_r)
                    } else {
                        (cur_l, value.as_bool() as u8)
                    };
                    if (nl, nr) == (cur_l, cur_r) {
                        return Ok(());
                    }
                    match self.run(2, 3, 0, 26, &[prof, nl, nr]) {
                        Ok(_) => Ok(()),
                        Err(_) if read_lr() == Some((nl, nr)) => Ok(()),
                        Err(e) => Err(e),
                    }
                }
                "bhop_dir" | "bhop_ms" => {
                    let read_bp = || {
                        self.run(2, 4, 0, 153, &[prof]).ok().map(|r| {
                            (
                                u8_at(&r, 8).unwrap_or(0),
                                u16::from_be_bytes([
                                    u8_at(&r, 9).unwrap_or(0),
                                    u8_at(&r, 10).unwrap_or(200),
                                ]),
                            )
                        })
                    };
                    let (cur_dir, cur_ms) = read_bp().unwrap_or((0, 200));
                    let dir = if id == "bhop_dir" {
                        value.as_choice() as u8
                    } else {
                        cur_dir
                    };
                    let ms = if id == "bhop_ms" {
                        (value.as_num().clamp(200., 1000.) as u16 / 200) * 200
                    } else {
                        cur_ms
                    };
                    if (dir, ms) == (cur_dir, cur_ms) {
                        return Ok(());
                    }
                    let [hi, lo] = ms.to_be_bytes();
                    match self.run(2, 4, 0, 25, &[prof, dir, hi, lo]) {
                        Ok(_) => Ok(()),
                        Err(_) if read_bp() == Some((dir, ms)) => Ok(()),
                        Err(e) => Err(e),
                    }
                }
                _ => Err(HubError::Unsupported("setting")),
            },
            Change::DpiStages { stages, active } => {
                // 値→色→有効段の 3 コマンド — 先に入力検証を済ませてから書く。
                self.write_stages(prof, stages)?;
                self.set(2, 1, 2, prof, (*active as u8) + 1)?;
                // X/Y 個別フラグはステージ内容と同期 (UI トグルは段だけを
                // 書き換えるので、ここで公式 $2() 相当の整合を取る)。
                self.set(
                    2,
                    1,
                    13,
                    prof,
                    stages.iter().any(|s| s.y.is_some()) as u8,
                )
            }
            Change::ActiveStage(a) => self.set(2, 1, 2, prof, (*a as u8) + 1),
            Change::Adaptive(c) => self.write_adaptive(prof, c),
            Change::Button { slot, action } => {
                if *slot >= 5 {
                    return Err(HubError::Invalid("button slot".into()));
                }
                self.write_button(prof, (*slot as u8) + 1, action)
            }
            _ => Err(HubError::Unsupported("change")),
        }
    }

    fn run_action(&self, id: &str, _file: Option<&std::path::Path>) -> HubResult<ActionOutcome> {
        match id {
            "reset_profile" => {
                // active 不明のまま 1 番を消去しない — 失敗を伝える。
                let prof = self.active_profile()?;
                self.run(2, 1, 0, 13, &[prof])?;
                sleep(Duration::from_millis(500));
                Ok(ActionOutcome {
                    message: Some("プロファイルを初期化しました".into()),
                    reload: true,
                })
            }
            "factory_reset" => {
                // 公式 Xv(): defaultDevice は送るだけ (応答なし)。無線は 1 秒後に
                // 受信機 (target 0) も初期化する。再送しない (二重初期化を避ける)。
                {
                    let dev = self.dev.lock().unwrap_or_else(|p| p.into_inner());
                    send_only(&dev, 2, 2, 0, 0, &[192, 1])?;
                }
                if self.is_dongle() {
                    sleep(Duration::from_millis(1000));
                    let dev = self.dev.lock().unwrap_or_else(|p| p.into_inner());
                    send_only(&dev, 0, 2, 0, 0, &[192, 1])?;
                }
                sleep(Duration::from_millis(1500));
                Ok(ActionOutcome {
                    message: Some("工場出荷状態に戻しました".into()),
                    reload: true,
                })
            }
            _ => Err(HubError::Unsupported("action")),
        }
    }

    fn battery(&self) -> HubResult<Battery> {
        let r = self.run(2, 2, 0, 131, &[])?;
        Ok(Battery {
            percent: Some(u8_at(&r, 8).unwrap_or(0).min(100)),
            charging: r.get(7).copied().unwrap_or(0) == 1,
        })
    }
}
