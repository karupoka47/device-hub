//! Wired G502 HERO HID++ 2.0. Protocol findings and verification: docs/LOGITECH.md.
//! Read/modify/write onboard sectors, preserving unknown bytes and G-shift bindings.
use crate::transport::Channel;
use devhub_core::{backend::*, model::*};
mod host;
mod macros;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const BUTTONS: usize = 11;
const LABELS: [&str; BUTTONS] = [
    "G1 左クリック",
    "G2 右クリック",
    "G3 ホイールクリック",
    "G4 戻る",
    "G5 進む",
    "G6 サムボタン",
    "G7 DPI下",
    "G8 DPI上",
    "G9 プロファイル",
    "G10 ホイール右",
    "G11 ホイール左",
];

fn protocol(s: impl Into<String>) -> HubError {
    HubError::Protocol(s.into())
}
fn invalid(s: impl Into<String>) -> HubError {
    HubError::Invalid(s.into())
}
fn io(e: std::io::Error) -> HubError {
    HubError::Io(e.to_string())
}
fn be(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}
fn le(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}

/// CRC-16/CCITT-FALSE (init FFFF, polynomial 1021), stored big endian.
pub fn crc16(bytes: &[u8]) -> u16 {
    bytes.iter().fold(0xffff, |mut crc, b| {
        crc ^= (*b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
        crc
    })
}

struct Wire {
    spy: u8,
    host: Option<host::Host>,
    channel: Channel,
    software: u8,
    profiles: u8,
    dpi: u8,
    rate: u8,
    size: usize,
    count: u8,
    sectors: u8,
    notify: Option<crate::Watcher>,
    id: DeviceId,
    backup: PathBuf,
}

impl Wire {
    fn call(&mut self, feature: u8, function: u8, args: &[u8]) -> HubResult<[u8; 16]> {
        if args.len() > 16 {
            return Err(invalid("HID++ payload too large"));
        }
        self.software = self.software % 15 + 1;
        let address = (function << 4) | self.software;
        let mut body = [0; 19];
        body[..3].copy_from_slice(&[0xff, feature, address]);
        body[3..3 + args.len()].copy_from_slice(args);
        self.channel.send_report(0x11, &body)?;
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            let r = self.channel.recv(2000)?;
            if r.len() < 4 || !matches!(r[0], 0x10 | 0x11) || r[1] != 0xff {
                continue;
            }
            if r[2] == 0xff && r.len() >= 6 && r[3] == feature && r[4] == address {
                return Err(protocol(format!(
                    "HID++ {feature:02x}/{function:x}: error {:02x}",
                    r[5]
                )));
            }
            if r[2] == feature && r[3] == address {
                if r.len() != 20 {
                    return Err(protocol("HID++ truncated long response"));
                }
                return Ok(r[4..20].try_into().unwrap());
            }
            self.notification(&r);
        }
        Err(protocol("HID++ reply timeout"))
    }
    fn feature(&mut self, id: u16) -> HubResult<u8> {
        let r = self.call(0, 0, &id.to_be_bytes())?;
        if r[0] == 0 {
            return Err(protocol(format!("Missing HID++ feature {id:04x}")));
        }
        Ok(r[0])
    }
    fn notification(&mut self, r: &[u8]) {
        if r.len() < 6 || r[1] != 0xff || !matches!(r[0], 0x10 | 0x11) {
            return;
        }
        if self.spy != 0 && r[2..4] == [self.spy, 0] {
            if let Some(host) = &mut self.host {
                host.buttons(be(&r[4..]));
            }
        } else if r[2] == self.profiles && r[3] & 0x0f == 0 {
            // Only profile notifications invalidate routing; DPI events do not stop playback.
            if r[3] == 0 {
                if let Some(host) = &mut self.host {
                    host.invalidate();
                }
            }
            if let Some(cb) = &self.notify {
                cb(HubEvent::StateChanged(self.id.clone()));
            }
        }
    }
    fn refresh_host(&mut self, profile: u8) -> HubResult<()> {
        if self.host.is_some() {
            let bytes = self.valid_sector(profile)?;
            self.host.as_mut().unwrap().refresh(profile, &bytes);
        }
        Ok(())
    }
    fn onboard(&mut self, function: u8, args: &[u8]) -> HubResult<[u8; 16]> {
        self.call(self.profiles, function, args)
    }
    fn sector(&mut self, sector: u8) -> HubResult<Vec<u8>> {
        if sector >= self.sectors {
            return Err(invalid("Sector out of range"));
        }
        let mut data = vec![0; self.size];
        for start in (0..self.size).step_by(16) {
            // G502 HERO has 255-byte sectors, not 256. Final read overlaps by one.
            let offset = start.min(self.size - 16);
            let r = self.onboard(5, &[0, sector, (offset >> 8) as u8, offset as u8])?;
            data[offset..offset + 16].copy_from_slice(&r);
        }
        Ok(data)
    }
    fn valid_sector(&mut self, sector: u8) -> HubResult<Vec<u8>> {
        let data = self.sector(sector)?;
        if crc16(&data[..self.size - 2]) != be(&data[self.size - 2..]) {
            return Err(protocol(format!("Profile sector {sector} CRC mismatch")));
        }
        Ok(data)
    }
    fn write(&mut self, sector: u8, mut data: Vec<u8>) -> HubResult<()> {
        if data.len() != self.size || sector == 0 || sector >= self.sectors {
            return Err(invalid("Invalid onboard write"));
        }
        let crc = crc16(&data[..self.size - 2]);
        data[self.size - 2..].copy_from_slice(&crc.to_be_bytes());
        let previous = self.sector(sector)?;
        if data == previous {
            return Ok(());
        }
        fs::create_dir_all(&self.backup).map_err(io)?;
        // Each device's original sectors are saved once, before the first write.
        let backup = self.backup.join(format!("sector-{sector}.bin"));
        if !backup.exists() {
            fs::write(&backup, &previous).map_err(io)?;
        }
        self.onboard(
            6,
            &[0, sector, 0, 0, (self.size >> 8) as u8, self.size as u8],
        )?;
        for chunk in data.chunks(16) {
            let mut block = [0xff; 16];
            block[..chunk.len()].copy_from_slice(chunk);
            self.onboard(7, &block)?;
        }
        self.onboard(8, &[])?;
        if self.sector(sector)? != data {
            return Err(protocol(format!(
                "Sector {sector} write verification failed; backup: {}",
                backup.display()
            )));
        }
        Ok(())
    }
    fn current(&mut self) -> HubResult<u8> {
        if self.onboard(2, &[])?[0] != 1 {
            return Err(invalid(
                "オンボードモードに切り替えてください（一般 → オンボードモード）",
            ));
        }
        let sector = be(&self.onboard(4, &[])?);
        if !(1..=u16::from(self.count)).contains(&sector) {
            return Err(protocol("Unknown active profile"));
        }
        Ok(sector as u8)
    }
    fn select(&mut self, sector: u8) -> HubResult<()> {
        self.onboard(3, &[0, sector])?;
        if self.current()? != sector {
            return Err(protocol("Profile selection did not persist"));
        }
        Ok(())
    }
}

impl Drop for Wire {
    fn drop(&mut self) {
        if let Some(host) = &mut self.host {
            host.invalidate();
        }
        if self.spy != 0 {
            let _ = self.call(self.spy, 2, &[]);
        }
    }
}

pub struct Logitech {
    info: DeviceInfo,
    wire: Arc<Mutex<Wire>>,
    caps: Capabilities,
    metadata: PathBuf,
}

pub fn open(
    dev: hidapi::HidDevice,
    mut info: DeviceInfo,
    events: Option<crate::Watcher>,
) -> HubResult<Logitech> {
    let root = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .ok_or_else(|| invalid("APPDATA unavailable"))?
        .join("DeviceHub")
        .join("logitech");
    let safe_id: String = info
        .id
        .0
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let mut wire = Wire {
        spy: 0,
        host: None,
        channel: Channel::open(dev, 20, 20),
        software: 12,
        profiles: 0,
        dpi: 0,
        rate: 0,
        size: 0,
        count: 0,
        sectors: 0,
        notify: events,
        id: info.id.clone(),
        backup: root.join(&safe_id).join("original"),
    };
    let version = wire.call(0, 1, &[0, 0, 0x5a])?;
    if version[0] < 2 || version[2] != 0x5a {
        return Err(protocol("HID++ version handshake failed"));
    }
    wire.profiles = wire.feature(0x8100)?;
    wire.dpi = wire.feature(0x2201)?;
    wire.rate = wire.feature(0x8060)?;
    let desc = wire.onboard(0, &[])?;
    wire.size = be(&desc[7..]) as usize;
    wire.count = desc[3];
    wire.sectors = desc[6];
    // Deliberately restrict writes to the layout verified on this device.
    if desc[..3] != [1, 2, 1]
        || wire.count != 5
        || desc[5] != 11
        || wire.sectors != 16
        || wire.size != 255
    {
        return Err(protocol(format!(
            "Unverified Logitech memory layout: {desc:02x?}"
        )));
    }
    let directory = wire.valid_sector(0)?;
    for i in 0..wire.count as usize {
        if be(&directory[i * 4..]) != (i + 1) as u16 || directory[i * 4 + 2] != 1 {
            return Err(protocol(
                "Unsupported onboard directory (disabled/reordered profile)",
            ));
        }
    }
    let dpi_feature = wire.dpi;
    let dpi = wire.call(dpi_feature, 1, &[0])?;
    let dpi_min = be(&dpi[1..]) as u32;
    let dpi_step = (be(&dpi[3..]) & 0x1fff) as u32;
    let dpi_max = be(&dpi[5..]) as u32;
    if dpi_min == 0 || dpi_step == 0 || dpi_max < dpi_min {
        return Err(protocol("Invalid DPI range"));
    }
    if let Ok(f) = wire.feature(3) {
        if let Ok(v) = wire.call(f, 1, &[0]) {
            info.firmware = Some(format!(
                "{} {:02x}.{:02x}.{:04x}",
                String::from_utf8_lossy(&v[1..4]).trim_end_matches('\0'),
                v[4],
                v[5],
                be(&v[6..])
            ));
        }
    }
    let mut caps = Capabilities::empty();
    caps.profiles = Some(ProfileCaps {
        count: wire.count,
        renamable: true,
    });
    caps.mouse = Some(MouseCaps {
        dpi_min,
        dpi_max,
        dpi_step,
        min_stages: 1,
        max_stages: 5,
        xy_split: false,
        stage_colors: false,
        stage_toggle: false,
        buttons: (0..BUTTONS * 2)
            .map(|i| ButtonSlot {
                name: format!(
                    "{}{}",
                    if i >= BUTTONS { "Gシフト: " } else { "" },
                    LABELS[i % BUTTONS]
                ),
            })
            .collect(),
        button_kinds: vec![
            ButtonActionKind::Disabled,
            ButtonActionKind::Mouse,
            ButtonActionKind::Key,
            ButtonActionKind::Media,
            ButtonActionKind::Dpi,
            ButtonActionKind::ProfileCycle,
            ButtonActionKind::GShift,
            ButtonActionKind::Macro,
        ],
        require_left_click: true,
        macros: Some(MacroCaps {
            coordinate_slots: if cfg!(windows) {
                (10..20).collect()
            } else {
                vec![]
            },
            button_toggle: false,
            slots: if cfg!(windows) { 20 } else { 10 },
            max_events: 40,
            max_delay_ms: 65535,
            modes: vec![
                ChoiceOpt::new(0, "1回"),
                ChoiceOpt::new(1, "押している間"),
                ChoiceOpt::new(2, "回数指定（メモリ上限内）"),
            ],
            repeat_modes: vec![2],
            names: true,
            mouse_events: true,
        }),
        live_buttons: vec![],
        adaptive_dpi: None,
    });
    caps.settings = vec![
        SettingSpec::choice(
            "polling",
            section::PERFORMANCE,
            "ポーリングレート",
            [125, 250, 500, 1000]
                .map(|v| ChoiceOpt::new(v, format!("{v} Hz")))
                .to_vec(),
        ),
        SettingSpec::choice(
            "gshift",
            section::CLICK,
            "Gシフトボタン",
            std::iter::once(ChoiceOpt::new(-1, "割り当てなし"))
                .chain(
                    LABELS
                        .iter()
                        .enumerate()
                        .map(|(i, n)| ChoiceOpt::new(i as i64, *n)),
                )
                .collect(),
        )
        .help("押している間、ボタン一覧の「Gシフト」側の割り当てをマウス本体で実行します。"),
    ];
    caps.actions.push(
        ActionSpec::new("onboard", section::SYSTEM, "オンボードモードに切り替える")
            .help("マウス本体に保存された設定を使用します。"),
    );
    caps.actions.push(
        ActionSpec::new("backup", section::SYSTEM, "オンボードメモリを書き出す")
            .save_file(&["json"]),
    );
    let selection = root.join(&safe_id).join("selected-profile.json");
    if let Some(index) = fs::read(&selection)
        .ok()
        .and_then(|b| serde_json::from_slice::<u8>(&b).ok())
    {
        if index < wire.count && wire.onboard(2, &[])?[0] == 1 {
            wire.select(index + 1)?;
        }
    }
    #[cfg(windows)]
    {
        wire.host = Some(host::Host::load(
            root.join(&safe_id).join("pc-macros.json"),
            info.id.clone(),
            wire.notify.clone(),
        )?);
        wire.spy = wire.feature(0x8110)?;
        let spy = wire.spy;
        if wire.call(spy, 0, &[])?[0] != BUTTONS as u8 {
            return Err(protocol("Unexpected button spy layout"));
        }
        wire.call(spy, 1, &[])?;
        let current = wire.current()?;
        wire.refresh_host(current)?;
    }
    let wire = Arc::new(Mutex::new(wire));
    // Hardware DPI/profile buttons keep working while the UI is hidden.
    // Observe them without holding the device open after the driver is dropped.
    let weak = Arc::downgrade(&wire);
    std::thread::spawn(move || {
        let mut previous = None;
        let mut next_poll = Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(5));
            let Some(shared) = weak.upgrade() else { break };
            let Ok(mut w) = shared.try_lock() else {
                continue;
            };
            // The same lock owns every read. Requests also dispatch notifications,
            // so a release arriving during a configuration read cannot get lost.
            for _ in 0..64 {
                match w.channel.recv(0) {
                    Ok(r) => w.notification(&r),
                    Err(HubError::Io(_)) => {
                        if let Some(h) = &mut w.host {
                            h.invalidate();
                        }
                        return;
                    }
                    Err(_) => break,
                }
            }
            if Instant::now() < next_poll {
                continue;
            }
            next_poll = Instant::now() + Duration::from_millis(750);
            let value = w
                .onboard(4, &[])
                .and_then(|p| w.onboard(11, &[]).map(|d| (be(&p), d[0])));
            match value {
                Ok(value) => {
                    if w.host.as_ref().is_some_and(|h| h.profile != value.0 as u8)
                        && w.refresh_host(value.0 as u8).is_err() {
                        if let Some(h) = &mut w.host {
                            h.invalidate();
                        }
                    }
                    if previous.is_some_and(|old| old != value) {
                        if let Some(cb) = &w.notify {
                            cb(HubEvent::StateChanged(w.id.clone()));
                        }
                    }
                    previous = Some(value);
                }
                Err(_) => {
                    if let Some(h) = &mut w.host {
                        h.invalidate();
                    }
                    if let Some(cb) = &w.notify {
                        cb(HubEvent::StateChanged(w.id.clone()));
                    }
                    break;
                }
            }
        }
    });
    Ok(Logitech {
        info,
        metadata: root.join(safe_id).join("macros.json"),
        wire,
        caps,
    })
}

fn offset(slot: usize) -> HubResult<usize> {
    if slot >= BUTTONS * 2 {
        return Err(invalid("Button out of range"));
    }
    Ok(if slot < BUTTONS {
        32 + slot * 4
    } else {
        96 + (slot - BUTTONS) * 4
    })
}

fn mouse_mask(m: MouseFn) -> Option<u16> {
    Some(match m {
        MouseFn::Left => 1,
        MouseFn::Right => 2,
        MouseFn::Middle => 4,
        MouseFn::Back => 8,
        MouseFn::Forward => 16,
        _ => return None,
    })
}
fn media_code(m: MediaFn) -> u16 {
    match m {
        MediaFn::PlayPause => 0xcd,
        MediaFn::Stop => 0xb7,
        MediaFn::Next => 0xb5,
        MediaFn::Prev => 0xb6,
        MediaFn::VolumeUp => 0xe9,
        MediaFn::VolumeDown => 0xea,
        MediaFn::Mute => 0xe2,
        MediaFn::Calculator => 0x192,
        MediaFn::MyComputer => 0x194,
        MediaFn::Browser => 0x196,
        MediaFn::Mail => 0x18a,
        MediaFn::BrightnessUp => 0x6f,
        MediaFn::BrightnessDown => 0x70,
        MediaFn::Refresh => 0x227,
    }
}
fn decode_button(b: &[u8], shift_dpi: u32) -> ButtonAction {
    match (b[0], b[1]) {
        (0xff, _) | (0x80, 0) | (0x90, 0) => ButtonAction::Disabled,
        (0x80, 1) => {
            for m in [
                MouseFn::Left,
                MouseFn::Right,
                MouseFn::Middle,
                MouseFn::Back,
                MouseFn::Forward,
            ] {
                if mouse_mask(m) == Some(be(&b[2..])) {
                    return ButtonAction::Mouse(m);
                }
            }
            ButtonAction::Unsupported {
                ty: b[0],
                params: b[1..4].to_vec(),
            }
        }
        (0x80, 2) => ButtonAction::Key(KeyStroke {
            mods: b[2],
            usage: b[3] as u16,
        }),
        (0x80, 3) => {
            for m in [
                MediaFn::PlayPause,
                MediaFn::Stop,
                MediaFn::Next,
                MediaFn::Prev,
                MediaFn::VolumeUp,
                MediaFn::VolumeDown,
                MediaFn::Mute,
                MediaFn::Calculator,
                MediaFn::MyComputer,
                MediaFn::Browser,
                MediaFn::Mail,
                MediaFn::BrightnessUp,
                MediaFn::BrightnessDown,
                MediaFn::Refresh,
            ] {
                if media_code(m) == be(&b[2..]) {
                    return ButtonAction::Media(m);
                }
            }
            ButtonAction::Unsupported {
                ty: b[0],
                params: b[1..4].to_vec(),
            }
        }
        (0x90, 1) => ButtonAction::Mouse(MouseFn::ScrollLeft),
        (0x90, 2) => ButtonAction::Mouse(MouseFn::ScrollRight),
        (0x90, 3) => ButtonAction::Dpi(DpiFn::Up),
        (0x90, 4) => ButtonAction::Dpi(DpiFn::Down),
        (0x90, 5) => ButtonAction::Dpi(DpiFn::Cycle),
        (0x90, 7) => ButtonAction::Dpi(DpiFn::Shift(shift_dpi)),
        (0x90, 10) => ButtonAction::ProfileCycle,
        (0x90, 11) => ButtonAction::GShift,
        _ => ButtonAction::Unsupported {
            ty: b[0],
            params: b[1..4].to_vec(),
        },
    }
}
fn encode_button(a: &ButtonAction) -> HubResult<[u8; 4]> {
    let value = match a {
        ButtonAction::Disabled => [0xff; 4],
        ButtonAction::Mouse(m) => match m {
            MouseFn::ScrollLeft => [0x90, 1, 0, 0],
            MouseFn::ScrollRight => [0x90, 2, 0, 0],
            _ => {
                let bits = mouse_mask(*m)
                    .ok_or_else(|| invalid("このオンボード割当は非対応です"))?
                    .to_be_bytes();
                [0x80, 1, bits[0], bits[1]]
            }
        },
        ButtonAction::Key(k) if k.usage <= 255 => [0x80, 2, k.mods, k.usage as u8],
        ButtonAction::Media(m) => {
            let v = media_code(*m).to_be_bytes();
            [0x80, 3, v[0], v[1]]
        }
        ButtonAction::Dpi(d) => [
            0x90,
            match d {
                DpiFn::Up => 3,
                DpiFn::Down => 4,
                DpiFn::Cycle => 5,
                DpiFn::Shift(_) => 7,
            },
            0,
            0,
        ],
        ButtonAction::ProfileCycle => [0x90, 10, 0, 0],
        ButtonAction::GShift => [0x90, 11, 0, 0],
        ButtonAction::Unsupported { ty, params } if params.len() == 3 => {
            [*ty, params[0], params[1], params[2]]
        }
        _ => return Err(HubError::Unsupported("Logitech onboard button action")),
    };
    Ok(value)
}
fn name(data: &[u8]) -> String {
    let units: Vec<u16> = data[160..208]
        .chunks_exact(2)
        .map(le)
        .take_while(|c| *c != 0 && *c != 0xffff)
        .collect();
    String::from_utf16_lossy(&units)
}
fn validate_left(data: &[u8]) -> HubResult<()> {
    for start in [32, 96] {
        if !(0..BUTTONS).any(|i| data[start + i * 4..start + i * 4 + 4] == [0x80, 1, 0, 1]) {
            return Err(invalid(
                "通常側・Gシフト側それぞれに左クリックを1つ残してください",
            ));
        }
    }
    Ok(())
}

impl Logitech {
    fn macro_names(&self) -> BTreeMap<usize, String> {
        fs::read(&self.metadata)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }
    fn save_macro_name(&self, slot: usize, name: Option<String>) -> HubResult<()> {
        let mut names = self.macro_names();
        if let Some(name) = name {
            names.insert(slot, name);
        } else {
            names.remove(&slot);
        }
        fs::create_dir_all(self.metadata.parent().unwrap()).map_err(io)?;
        let temp = self.metadata.with_extension("tmp");
        fs::write(
            &temp,
            serde_json::to_vec_pretty(&names).map_err(|e| protocol(e.to_string()))?,
        )
        .map_err(io)?;
        fs::rename(temp, &self.metadata).map_err(io)
    }
    fn read_macros(&self, w: &mut Wire) -> HubResult<Vec<Option<MacroDef>>> {
        let names = self.macro_names();
        let mut result: Vec<_> = (0..10)
            .map(|slot| {
                let data = w.sector(slot as u8 + 6)?;
                Ok(macros::decode(
                    &data,
                    names
                        .get(&slot)
                        .cloned()
                        .unwrap_or_else(|| format!("Macro {}", slot + 1)),
                )
                .ok())
            })
            .collect::<HubResult<_>>()?;
        if let Some(host) = &w.host {
            result.extend(host.macros());
        }
        Ok(result)
    }
    fn macro_references(w: &mut Wire, slot: usize) -> HubResult<Vec<(u8, usize)>> {
        let sector = slot as u8 + 6;
        let mut references = Vec::new();
        for profile in 1..=w.count {
            let data = w.valid_sector(profile)?;
            for button in 0..BUTTONS * 2 {
                let at = offset(button)?;
                if data[at..at + 2] == [0, sector] {
                    if data[at + 2..at + 4] != [0, 0] {
                        return Err(invalid("Foreign macro references this sector"));
                    }
                    references.push((profile, button));
                }
            }
        }
        Ok(references)
    }
    fn write_macro(&self, w: &mut Wire, slot: usize, def: Option<&MacroDef>) -> HubResult<()> {
        if slot >= 10 {
            return Err(invalid("Macro slot out of range"));
        }
        let old = w.sector(slot as u8 + 6)?;
        let blank = old.iter().all(|b| *b == 0xff) || old[..w.size - 2].iter().all(|b| *b == 0xff);
        if !blank && macros::decode(&old, String::new()).is_err() {
            return Err(invalid(
                "このスロットは未対応の既存マクロに使用されています",
            ));
        }
        let references = Self::macro_references(w, slot)?;
        if def.is_none() && !references.is_empty() {
            return Err(invalid(
                "先に全プロファイルのマクロ割り当てを解除してください",
            ));
        }
        let data = match def {
            Some(def) => macros::encode(def, w.size)?,
            None => vec![0xff; w.size],
        };
        w.write(slot as u8 + 6, data)?;
        self.save_macro_name(slot, def.map(|d| d.name.clone()))
    }
}

impl DeviceDriver for Logitech {
    fn info(&self) -> DeviceInfo {
        self.info.clone()
    }
    fn caps(&self) -> Capabilities {
        self.caps.clone()
    }
    fn read(&self) -> HubResult<DeviceState> {
        let mut w = self.wire.lock().unwrap_or_else(|p| p.into_inner());
        let active = w.current()?;
        let mut profiles = Vec::new();
        for i in 1..=w.count {
            profiles.push(w.valid_sector(i)?);
        }
        let data = &profiles[active as usize - 1];
        let stages: Vec<DpiStage> = data[3..13]
            .chunks_exact(2)
            .map(le)
            .take_while(|v| *v != 0)
            .map(|v| DpiStage {
                x: v as u32,
                y: None,
                color: Rgb::default(),
                enabled: true,
            })
            .collect();
        if stages.is_empty() {
            return Err(protocol("Empty DPI table"));
        }
        let stage = w.onboard(11, &[])?[0] as usize;
        let shift = stages.get(data[2] as usize).unwrap_or(&stages[0]).x;
        let macros = self.read_macros(&mut w)?;
        if let Some(host) = &mut w.host {
            host.refresh(active, data);
        }
        let buttons = (0..BUTTONS * 2)
            .map(|slot| {
                let at = offset(slot).unwrap();
                let b = &data[at..at + 4];
                if b == host::MARKER {
                    if let Some(binding) = w.host.as_ref().and_then(|h| h.binding(active, slot)) {
                        return ButtonAction::Macro {
                            slot: binding.slot,
                            run: binding.run,
                        };
                    }
                }
                if b[0] == 0 && (6..16).contains(&b[1]) && b[2..] == [0, 0] {
                    let macro_slot = b[1] - 6;
                    if let Some(def) = &macros[macro_slot as usize] {
                        return ButtonAction::Macro {
                            slot: macro_slot,
                            run: if def.mode == 1 {
                                MacroRun::WhileHeld
                            } else {
                                MacroRun::Times(1)
                            },
                        };
                    }
                }
                decode_button(b, shift)
            })
            .collect();
        let gshift = (0..BUTTONS)
            .find(|i| data[32 + i * 4..34 + i * 4] == [0x90, 0x0b])
            .map_or(-1, |i| i as i64);
        let mut settings = BTreeMap::new();
        settings.insert(
            "polling".into(),
            SettingValue::Choice(1000 / i64::from(data[0].max(1))),
        );
        settings.insert("gshift".into(), SettingValue::Choice(gshift));
        Ok(DeviceState {
            profile: ProfileState {
                active: active - 1,
                names: profiles
                    .iter()
                    .enumerate()
                    .map(|(i, d)| {
                        let n = name(d);
                        if n.is_empty() {
                            format!("Profile {}", i + 1)
                        } else {
                            n
                        }
                    })
                    .collect(),
            },
            settings,
            mouse: Some(MouseState {
                active_stage: stage.min(stages.len() - 1),
                stages,
                buttons,
                macros,
                adaptive: None,
            }),
            keyboard: None,
            lighting: BTreeMap::new(),
            peq: None,
        })
    }
    fn apply(&self, change: &Change) -> HubResult<()> {
        let mut w = self.wire.lock().unwrap_or_else(|p| p.into_inner());
        let current = w.current()?;
        if let Some(h) = &mut w.host {
            h.invalidate();
        }
        if let Change::Macro { slot, def } = change {
            if *slot >= 10 {
                if def.is_none() {
                    for profile in 1..=w.count {
                        let data = w.valid_sector(profile)?;
                        for button in 0..BUTTONS * 2 {
                            let at = offset(button)?;
                            if data[at..at + 4] == host::MARKER
                                && w.host
                                    .as_ref()
                                    .and_then(|h| h.binding(profile, button))
                                    .is_some_and(|b| b.slot as usize == *slot)
                            {
                                return Err(invalid(
                                    "先に全プロファイルのPCマクロ割り当てを解除してください",
                                ));
                            }
                        }
                    }
                }
                w.host
                    .as_mut()
                    .ok_or(HubError::Unsupported("PC macros"))?
                    .set_macro(*slot, def.as_ref())?;
                w.refresh_host(current)?;
                return Ok(());
            }
            self.write_macro(&mut w, *slot, def.as_ref())?;
            w.select(current)?;
            w.refresh_host(current)?;
            return Ok(());
        }
        if let Change::Profile(index) = change {
            if *index >= w.count {
                return Err(invalid("Profile out of range"));
            }
            w.select(index + 1)?;
            w.refresh_host(index + 1)?;
            fs::create_dir_all(self.metadata.parent().unwrap()).map_err(io)?;
            fs::write(
                self.metadata.with_file_name("selected-profile.json"),
                index.to_string(),
            )
            .map_err(io)?;
            return Ok(());
        }
        if let Change::ActiveStage(index) = change {
            let data = w.valid_sector(current)?;
            if *index >= 5 || le(&data[3 + index * 2..]) == 0 {
                return Err(invalid("DPI stage out of range"));
            }
            w.onboard(12, &[*index as u8])?;
            if w.onboard(11, &[])?[0] as usize != *index {
                return Err(protocol("DPI stage verification failed"));
            }
            let feature = w.dpi;
            let live = w.call(feature, 2, &[0])?;
            if be(&live[1..]) != le(&data[3 + index * 2..]) {
                return Err(protocol("Live DPI verification failed"));
            }
            w.refresh_host(current)?;
            return Ok(());
        }
        let sector = match change {
            Change::ProfileName { index, .. } if *index < w.count => index + 1,
            _ => current,
        };
        let mut data = w.valid_sector(sector)?;
        // PC metadata is committed before installing its marker. Failed device
        // writes restore the previous intent; reads trust only verified markers.
        let mut host_binding = None;
        let mut active = w.onboard(11, &[])?[0];
        if active >= 5 {
            return Err(protocol("Invalid active DPI index"));
        }
        match change {
            Change::Setting { id, value } if id == "polling" => {
                let rate = value.as_choice();
                if ![125, 250, 500, 1000].contains(&rate) {
                    return Err(invalid("Invalid polling rate"));
                }
                data[0] = (1000 / rate) as u8;
            }
            Change::Setting { id, value } if id == "gshift" => {
                let slot = value.as_choice();
                if !(-1..BUTTONS as i64).contains(&slot) {
                    return Err(invalid("G-shift slot out of range"));
                }
                for i in 0..BUTTONS {
                    let at = 32 + i * 4;
                    if data[at..at + 2] == [0x90, 0x0b] {
                        data[at..at + 4].fill(0xff);
                    }
                }
                if slot >= 0 {
                    let at = 32 + slot as usize * 4;
                    data[at..at + 4].copy_from_slice(&[0x90, 0x0b, 0, 0]);
                }
            }
            Change::DpiStages {
                stages,
                active: selected,
            } => {
                let cap = self.caps.mouse.as_ref().unwrap();
                if stages.is_empty()
                    || stages.len() > 5
                    || *selected >= stages.len()
                    || stages.iter().any(|s| {
                        s.x < cap.dpi_min
                            || s.x > cap.dpi_max
                            || !(s.x - cap.dpi_min).is_multiple_of(cap.dpi_step)
                            || s.y.is_some_and(|y| y != s.x)
                            || !s.enabled
                    })
                {
                    return Err(invalid("DPI must match the device range/step, 1–5 stages"));
                }
                data[3..13].fill(0);
                for (i, s) in stages.iter().enumerate() {
                    data[3 + i * 2..5 + i * 2].copy_from_slice(&(s.x as u16).to_le_bytes());
                }
                data[1] = *selected as u8;
                data[2] = data[2].min(stages.len() as u8 - 1);
                active = *selected as u8;
            }
            Change::Button { slot, action } => {
                let at = offset(*slot)?;
                if let ButtonAction::Dpi(DpiFn::Shift(dpi)) = action {
                    let i = data[3..13]
                        .chunks_exact(2)
                        .position(|b| le(b) as u32 == *dpi)
                        .ok_or_else(|| invalid("DPIシフト値を先にDPIステージへ追加してください"))?;
                    data[2] = i as u8;
                }
                if let ButtonAction::Macro {
                    slot: macro_slot @ 10..=19,
                    run,
                } = action
                {
                    data[at..at + 4].copy_from_slice(&host::MARKER);
                    host_binding = Some((
                        *slot,
                        Some(host::Binding {
                            slot: *macro_slot,
                            run: *run,
                        }),
                    ));
                } else if let ButtonAction::Macro {
                    slot: macro_slot,
                    run,
                } = action
                {
                    let slot_index = *macro_slot as usize;
                    let mut def = self
                        .read_macros(&mut w)?
                        .get(slot_index)
                        .and_then(|d| d.clone())
                        .ok_or_else(|| invalid("先にマクロを作成してください"))?;
                    let original = def.clone();
                    match run {
                        MacroRun::Times(n) if *n > 0 => {
                            def.mode = if *n == 1 { 0 } else { 2 };
                            def.repeat = *n;
                        }
                        MacroRun::WhileHeld => {
                            def.mode = 1;
                            def.repeat = 1;
                        }
                        _ => {
                            return Err(invalid(
                                "G502オンボードは1回・回数指定・押している間に対応",
                            ))
                        }
                    }
                    if def.mode != original.mode || def.repeat != original.repeat {
                        let others = Self::macro_references(&mut w, slot_index)?;
                        if others.iter().any(|r| *r != (current, *slot)) {
                            return Err(invalid("同じマクロを使う他のボタンがあります。別スロットへ複製して再生方法を変更してください"));
                        }
                        // Validate the resulting mapping before committing macro memory.
                        data[at..at + 4].copy_from_slice(&[0, *macro_slot + 6, 0, 0]);
                        validate_left(&data)?;
                        self.write_macro(&mut w, slot_index, Some(&def))?;
                    }
                    data[at..at + 4].copy_from_slice(&[0, *macro_slot + 6, 0, 0]);
                } else {
                    data[at..at + 4].copy_from_slice(&encode_button(action)?);
                }
                if host_binding.is_none() && w.host.is_some() {
                    host_binding = Some((*slot, None));
                }
            }
            Change::ProfileName { index, name } if *index < w.count => {
                let utf16: Vec<u16> = name.encode_utf16().collect();
                if utf16.len() > 24 {
                    return Err(invalid("Profile name: maximum 24 UTF-16 units"));
                }
                data[160..208].fill(0);
                for (i, c) in utf16.iter().enumerate() {
                    data[160 + i * 2..162 + i * 2].copy_from_slice(&c.to_le_bytes());
                }
            }
            _ => return Err(HubError::Unsupported("Logitech change")),
        }
        validate_left(&data)?;
        let expected_rate = data[0];
        let expected_dpi = le(&data[3 + active as usize * 2..]);
        let previous_binding = host_binding
            .as_ref()
            .and_then(|(button, _)| w.host.as_ref().and_then(|h| h.binding(current, *button)));
        if let Some((button, binding)) = &host_binding {
            w.host
                .as_mut()
                .ok_or(HubError::Unsupported("PC macros"))?
                .set_binding(current, *button, binding.clone())?;
        }
        if let Err(e) = w.write(sector, data) {
            if let Some((button, _)) = host_binding {
                let _ = w
                    .host
                    .as_mut()
                    .unwrap()
                    .set_binding(current, button, previous_binding);
            }
            return Err(e);
        }
        if sector == current {
            w.select(current)?;
            w.onboard(12, &[active])?;
            let rate = w.rate;
            let dpi = w.dpi;
            if w.call(rate, 1, &[])?[0] != expected_rate
                || be(&w.call(dpi, 2, &[0])?[1..]) != expected_dpi
            {
                return Err(protocol(
                    "Onboard data saved but live DPI/rate differ; reselect profile",
                ));
            }
        }
        w.refresh_host(current)?;
        Ok(())
    }
    fn run_action(&self, id: &str, file: Option<&Path>) -> HubResult<ActionOutcome> {
        let mut w = self.wire.lock().unwrap_or_else(|p| p.into_inner());
        match id {
            "onboard" => {
                w.onboard(1, &[1])?;
                w.current()?;
            }
            "backup" => {
                let file = file.ok_or_else(|| invalid("Choose a backup path"))?;
                let mut sectors = Vec::new();
                for sector in 0..w.sectors {
                    sectors.push(w.sector(sector)?);
                }
                let json =
                    serde_json::to_vec_pretty(&sectors).map_err(|e| protocol(e.to_string()))?;
                fs::write(file, json).map_err(io)?;
            }
            _ => return Err(HubError::Unsupported("Logitech action")),
        }
        Ok(ActionOutcome {
            message: None,
            reload: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn crc_known_vector() {
        assert_eq!(crc16(b"123456789"), 0x29b1);
    }
    #[test]
    fn gshift_offsets_do_not_overlap_name() {
        assert_eq!(offset(10).unwrap(), 72);
        assert_eq!(offset(11).unwrap(), 96);
        assert_eq!(offset(21).unwrap(), 136);
        assert!(offset(22).is_err());
    }
    #[test]
    fn captured_copy_binding_and_unknown_roundtrip() {
        assert_eq!(
            decode_button(&[0x80, 2, 1, 6], 1200),
            ButtonAction::Key(KeyStroke { mods: 1, usage: 6 })
        );
        let raw = [0x91, 0x0b, 0xff, 0xff];
        assert_eq!(encode_button(&decode_button(&raw, 1200)).unwrap(), raw);
    }
    #[test]
    fn enforce_left_click_on_both_layers() {
        let mut p = [0xff; 255];
        p[32..36].copy_from_slice(&[0x80, 1, 0, 1]);
        assert!(validate_left(&p).is_err());
        p[96..100].copy_from_slice(&[0x80, 1, 0, 1]);
        assert!(validate_left(&p).is_ok());
    }
    fn mock_wire() -> Wire {
        Wire {
            spy: 0,
            host: None,
            channel: Channel::mock(),
            software: 12,
            profiles: 12,
            dpi: 10,
            rate: 11,
            size: 255,
            count: 5,
            sectors: 16,
            notify: None,
            id: DeviceId("test".into()),
            backup: PathBuf::new(),
        }
    }
    #[test]
    fn ignores_notifications_and_other_software_responses() {
        let mut w = mock_wire();
        for (address, value) in [(0x20, 1), (0x2e, 2), (0x2d, 3)] {
            let mut reply = [0; 20];
            reply[..5].copy_from_slice(&[0x11, 0xff, 12, address, value]);
            w.channel.mock_reply(&reply);
        }
        assert_eq!(w.onboard(2, &[]).unwrap()[0], 3);
    }
    #[test]
    fn matches_hidpp_error_to_own_request() {
        let mut w = mock_wire();
        w.channel.mock_reply(&[
            0x11, 0xff, 0xff, 12, 0x2d, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ]);
        assert!(w
            .onboard(2, &[])
            .unwrap_err()
            .to_string()
            .contains("error 02"));
    }
    #[test]
    #[cfg(windows)]
    fn spy_press_and_release_are_dispatched_during_command_reads() {
        let mut w = mock_wire();
        w.spy = 13;
        w.host = Some(host::Host::test_empty());
        for mask in [0x0100u16, 0] {
            let mut notification = [0u8; 20];
            notification[..4].copy_from_slice(&[0x11, 0xff, 13, 0]);
            notification[4..6].copy_from_slice(&mask.to_be_bytes());
            w.channel.mock_reply(&notification);
            let mut response = [0u8; 20];
            response[..5].copy_from_slice(&[0x11, 0xff, 12, 0x20 | (w.software % 15 + 1), 1]);
            w.channel.mock_reply(&response);
            w.onboard(2, &[]).unwrap();
            assert_eq!(w.host.as_ref().unwrap().held_mask(), mask);
        }
        // Avoid an irrelevant stop-spy request against an empty mock queue.
        w.spy = 0;
    }
}
