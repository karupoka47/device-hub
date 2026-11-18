//! Resident PC macros. Cancellation releases all injected holds.
use devhub_core::*;

pub fn validate(def: &MacroDef) -> HubResult<()> {
    if def.events.is_empty()
        || def.events.len() > 40
        || def.mode > 2
        || def.repeat == 0
        || def.repeat > 255
    {
        return Err(HubError::Invalid(
            "PCマクロは1〜40イベント・1〜255回です".into(),
        ));
    }
    let mut held = Vec::new();
    for e in &def.events {
        if e.delay_ms > 65_535 {
            return Err(HubError::Invalid("待ち時間は65535ms以内です".into()));
        }
        match e.input {
            MacroInput::ClickAt { button, .. } => {
                if e.edge != MacroEdge::Down || !click_button(button) {
                    return Err(HubError::Invalid("座標クリックのボタンが不正です".into()));
                }
                if held
                    .iter()
                    .any(|i| matches!(i, MacroInput::Mouse(m) if *m == button))
                {
                    return Err(HubError::Invalid(
                        "押下中のボタンを座標クリックに使えません".into(),
                    ));
                }
                continue;
            }
            MacroInput::Key(u) if virtual_key(u).is_none() => {
                return Err(HubError::Invalid(format!("PCマクロで未対応のキー: {u:#x}")));
            }
            MacroInput::Mouse(m) if !click_button(m) => {
                return Err(HubError::Invalid(
                    "PCマクロはマウスの5ボタンに対応します".into(),
                ));
            }
            _ => {}
        }
        if e.edge == MacroEdge::Down {
            if held.contains(&e.input) {
                return Err(HubError::Invalid("同じ入力が二重に押されています".into()));
            }
            held.push(e.input);
        } else if let Some(at) = held.iter().position(|i| *i == e.input) {
            held.remove(at);
        } else {
            return Err(HubError::Invalid("押下のない解放イベントです".into()));
        }
    }
    if !held.is_empty() {
        return Err(HubError::Invalid(
            "マクロ末尾に解放イベントが必要です".into(),
        ));
    }
    Ok(())
}
fn click_button(m: MouseFn) -> bool {
    matches!(
        m,
        MouseFn::Left | MouseFn::Right | MouseFn::Middle | MouseFn::Back | MouseFn::Forward
    )
}
fn virtual_key(usage: u16) -> Option<u16> {
    Some(match usage {
        0x04..=0x1d => 0x41 + usage - 0x04,
        0x1e..=0x26 => 0x31 + usage - 0x1e,
        0x27 => 0x30,
        0x28 => 0x0d,
        0x29 => 0x1b,
        0x2a => 0x08,
        0x2b => 0x09,
        0x2c => 0x20,
        0x2d => 0xbd,
        0x2e => 0xbb,
        0x2f => 0xdb,
        0x30 => 0xdd,
        0x31 => 0xdc,
        0x33 => 0xba,
        0x34 => 0xde,
        0x35 => 0xc0,
        0x36 => 0xbc,
        0x37 => 0xbe,
        0x38 => 0xbf,
        0x39 => 0x14,
        0x3a..=0x45 => 0x70 + usage - 0x3a,
        0x46 => 0x2c,
        0x47 => 0x91,
        0x48 => 0x13,
        0x49 => 0x2d,
        0x4a => 0x24,
        0x4b => 0x21,
        0x4c => 0x2e,
        0x4d => 0x23,
        0x4e => 0x22,
        0x4f => 0x27,
        0x50 => 0x25,
        0x51 => 0x28,
        0x52 => 0x26,
        0x53 => 0x90,
        0x54 => 0x6f,
        0x55 => 0x6a,
        0x56 => 0x6d,
        0x57 => 0x6b,
        0x58 => 0x0d,
        0x59..=0x61 => 0x61 + usage - 0x59,
        0x62 => 0x60,
        0x63 => 0x6e,
        0x64 => 0xe2,
        0x65 => 0x5d,
        0x68..=0x73 => 0x7c + usage - 0x68,
        0xe0 => 0xa2,
        0xe1 => 0xa0,
        0xe2 => 0xa4,
        0xe3 => 0x5b,
        0xe4 => 0xa3,
        0xe5 => 0xa1,
        0xe6 => 0xa5,
        0xe7 => 0x5c,
        _ => return None,
    })
}

trait InputSink {
    fn send(&mut self, input: MacroInput, edge: MacroEdge) -> Result<(), String>;
}
struct Playback<S: InputSink> {
    sink: S,
    def: MacroDef,
    run: MacroRun,
    key: usize,
    index: usize,
    remaining: u16,
    held: Vec<MacroInput>,
}
impl<S: InputSink> Playback<S> {
    fn new(sink: S, def: MacroDef, run: MacroRun, key: usize) -> Self {
        let remaining = match run {
            MacroRun::Times(n) => n,
            _ => 1,
        };
        Self {
            sink,
            def,
            run,
            key,
            index: 0,
            remaining,
            held: vec![],
        }
    }
    fn step(&mut self) -> Result<Option<u32>, String> {
        if self.index == self.def.events.len() {
            if self.run != MacroRun::WhileHeld {
                self.remaining = self.remaining.saturating_sub(1);
                if self.remaining == 0 {
                    return Ok(None);
                }
            }
            self.index = 0;
        }
        let e = self.def.events[self.index];
        // Track before injection so partially failed SendInput also gets cleanup.
        if !matches!(e.input, MacroInput::ClickAt { .. }) && e.edge == MacroEdge::Down {
            self.held.push(e.input);
        }
        self.sink.send(e.input, e.edge)?;
        if e.edge == MacroEdge::Up {
            self.held.retain(|i| *i != e.input);
        }
        self.index += 1;
        // Bound zero-delay loops.
        Ok(Some(if self.index == self.def.events.len() {
            e.delay_ms.max(10)
        } else {
            e.delay_ms
        }))
    }
}
impl<S: InputSink> Drop for Playback<S> {
    fn drop(&mut self) {
        for input in self.held.drain(..).rev() {
            let _ = self.sink.send(input, MacroEdge::Up);
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::sync::{mpsc, Arc, Mutex, OnceLock, Weak};
    use std::time::{Duration, Instant};
    use windows_sys::Win32::{
        Foundation::POINT,
        Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTONULL},
        UI::{HiDpi::*, Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
    };
    const TAG: usize = 0x44454b41;
    pub fn cursor_position() -> Option<(i32, i32)> {
        let mut p = POINT { x: 0, y: 0 };
        (unsafe { GetPhysicalCursorPos(&mut p) } != 0).then_some((p.x, p.y))
    }
    pub fn valid_point(x: i32, y: i32) -> bool {
        unsafe {
            let previous = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            let valid = !MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONULL).is_null();
            if !previous.is_null() {
                SetThreadDpiAwarenessContext(previous);
            }
            valid
        }
    }
    fn mouse(flags: u32, data: u32, dx: i32, dy: i32) -> INPUT {
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy,
                    mouseData: data,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: TAG,
                },
            },
        }
    }
    fn button(m: MouseFn, edge: MacroEdge) -> INPUT {
        let down = edge == MacroEdge::Down;
        let (flags, data) = match m {
            MouseFn::Left => (
                if down {
                    MOUSEEVENTF_LEFTDOWN
                } else {
                    MOUSEEVENTF_LEFTUP
                },
                0,
            ),
            MouseFn::Right => (
                if down {
                    MOUSEEVENTF_RIGHTDOWN
                } else {
                    MOUSEEVENTF_RIGHTUP
                },
                0,
            ),
            MouseFn::Middle => (
                if down {
                    MOUSEEVENTF_MIDDLEDOWN
                } else {
                    MOUSEEVENTF_MIDDLEUP
                },
                0,
            ),
            MouseFn::Back => (
                if down {
                    MOUSEEVENTF_XDOWN
                } else {
                    MOUSEEVENTF_XUP
                },
                1,
            ),
            MouseFn::Forward => (
                if down {
                    MOUSEEVENTF_XDOWN
                } else {
                    MOUSEEVENTF_XUP
                },
                2,
            ),
            _ => unreachable!("validated"),
        };
        mouse(flags, data, 0, 0)
    }
    fn inject(inputs: &[INPUT]) -> Result<(), String> {
        let n = unsafe {
            SendInput(
                inputs.len() as u32,
                inputs.as_ptr(),
                std::mem::size_of::<INPUT>() as i32,
            )
        };
        if n != inputs.len() as u32 {
            Err("PCマクロの入力を送信できません。操作対象とDeka Hubの権限を確認してください".into())
        } else {
            Ok(())
        }
    }
    struct WindowsInput;
    impl InputSink for WindowsInput {
        fn send(&mut self, input: MacroInput, edge: MacroEdge) -> Result<(), String> {
            match input {
                MacroInput::Key(u) => {
                    let vk = virtual_key(u).ok_or("未対応のキー")?;
                    let scan = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC_EX) };
                    let extended = scan & 0xff00 != 0 || u == 0x58;
                    let use_scan = scan != 0 && u != 0x48;
                    inject(&[INPUT {
                        r#type: INPUT_KEYBOARD,
                        Anonymous: INPUT_0 {
                            ki: KEYBDINPUT {
                                wVk: if use_scan { 0 } else { vk },
                                wScan: if use_scan { (scan & 0xff) as u16 } else { 0 },
                                dwFlags: (if use_scan { KEYEVENTF_SCANCODE } else { 0 })
                                    | (if extended { KEYEVENTF_EXTENDEDKEY } else { 0 })
                                    | (if edge == MacroEdge::Up {
                                        KEYEVENTF_KEYUP
                                    } else {
                                        0
                                    }),
                                time: 0,
                                dwExtraInfo: TAG,
                            },
                        },
                    }])
                }
                MacroInput::Mouse(m) => inject(&[button(m, edge)]),
                MacroInput::ClickAt { x, y, button: m } => {
                    if !valid_point(x, y) {
                        return Err(format!("座標 ({x}, {y}) にモニターがありません"));
                    }
                    let (left, top, width, height) = unsafe {
                        (
                            GetSystemMetrics(SM_XVIRTUALSCREEN),
                            GetSystemMetrics(SM_YVIRTUALSCREEN),
                            GetSystemMetrics(SM_CXVIRTUALSCREEN),
                            GetSystemMetrics(SM_CYVIRTUALSCREEN),
                        )
                    };
                    // Physical pixel centres across the entire virtual desktop, including negative origins.
                    let nx = (((i64::from(x) - i64::from(left)) * 65536 + 32768)
                        / i64::from(width.max(1))) as i32;
                    let ny = (((i64::from(y) - i64::from(top)) * 65536 + 32768)
                        / i64::from(height.max(1))) as i32;
                    let result = inject(&[
                        mouse(
                            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                            0,
                            nx,
                            ny,
                        ),
                        button(m, MacroEdge::Down),
                        button(m, MacroEdge::Up),
                    ]);
                    if result.is_err() {
                        let _ = inject(&[button(m, MacroEdge::Up)]);
                    }
                    result
                }
            }
        }
    }
    enum Command {
        Start(usize, MacroDef, MacroRun),
        Release(usize),
        Cancel,
        Stop,
    }
    struct Control {
        tx: mpsc::Sender<Command>,
        thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    }
    impl Control {
        fn stop(&self) {
            let _ = self.tx.send(Command::Stop);
            if let Some(t) = self.thread.lock().unwrap_or_else(|p| p.into_inner()).take() {
                let _ = t.join();
            }
        }
    }
    static PLAYERS: OnceLock<Mutex<Vec<Weak<Control>>>> = OnceLock::new();
    pub struct Runner {
        control: Arc<Control>,
    }
    impl Runner {
        pub fn new(error: impl Fn(String) + Send + 'static) -> Self {
            let (tx, rx) = mpsc::channel();
            let thread = std::thread::spawn(move || {
                unsafe {
                    SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
                }
                let mut active: Option<Playback<WindowsInput>> = None;
                let mut due = Instant::now();
                loop {
                    let command = if active.is_some() {
                        rx.recv_timeout(
                            due.saturating_duration_since(Instant::now())
                                .min(Duration::from_millis(10)),
                        )
                    } else {
                        rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected)
                    };
                    match command {
                        Ok(Command::Start(key, def, run)) => {
                            active = None;
                            if let Err(e) = validate(&def) {
                                error(e.to_string());
                                continue;
                            }
                            if !matches!(run, MacroRun::WhileHeld | MacroRun::Times(1..=255)) {
                                error("PCマクロの再生回数が不正です".into());
                                continue;
                            }
                            active = Some(Playback::new(WindowsInput, def, run, key));
                            due = Instant::now();
                        }
                        Ok(Command::Release(key)) => {
                            if active
                                .as_ref()
                                .is_some_and(|p| p.key == key && p.run == MacroRun::WhileHeld)
                            {
                                active = None;
                            }
                        }
                        Ok(Command::Cancel) => active = None,
                        Ok(Command::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    if unsafe { GetAsyncKeyState(VK_ESCAPE as i32) } < 0 {
                        active = None;
                    }
                    if Instant::now() >= due {
                        if let Some(p) = active.as_mut() {
                            match p.step() {
                                Ok(Some(delay)) => {
                                    due = Instant::now() + Duration::from_millis(delay.into())
                                }
                                Ok(None) => active = None,
                                Err(e) => {
                                    active = None;
                                    error(e);
                                }
                            }
                        }
                    }
                }
            });
            let control = Arc::new(Control {
                tx,
                thread: Mutex::new(Some(thread)),
            });
            let mut players = PLAYERS
                .get_or_init(Default::default)
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            players.retain(|p| p.strong_count() > 0);
            players.push(Arc::downgrade(&control));
            Self { control }
        }
        pub fn start(&self, key: usize, def: MacroDef, run: MacroRun) {
            let _ = self.control.tx.send(Command::Start(key, def, run));
        }
        pub fn release(&self, key: usize) {
            let _ = self.control.tx.send(Command::Release(key));
        }
        pub fn cancel(&self) {
            let _ = self.control.tx.send(Command::Cancel);
        }
    }
    impl Drop for Runner {
        fn drop(&mut self) {
            self.control.stop();
        }
    }
    pub fn shutdown() {
        if let Some(players) = PLAYERS.get() {
            for p in players
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .iter()
                .filter_map(Weak::upgrade)
            {
                p.stop();
            }
        }
    }
}
#[cfg(windows)]
pub use platform::*;
#[cfg(not(windows))]
pub fn shutdown() {}
#[cfg(not(windows))]
pub fn cursor_position() -> Option<(i32, i32)> {
    None
}
#[cfg(not(windows))]
pub fn valid_point(_: i32, _: i32) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    struct Sink(Arc<Mutex<Vec<(MacroInput, MacroEdge)>>>);
    impl InputSink for Sink {
        fn send(&mut self, i: MacroInput, e: MacroEdge) -> Result<(), String> {
            self.0.lock().unwrap().push((i, e));
            Ok(())
        }
    }
    fn def() -> MacroDef {
        MacroDef {
            name: "test".into(),
            mode: 0,
            repeat: 1,
            events: vec![
                MacroEvent {
                    input: MacroInput::Key(0xe0),
                    edge: MacroEdge::Down,
                    delay_ms: 65000,
                },
                MacroEvent {
                    input: MacroInput::Key(0xe0),
                    edge: MacroEdge::Up,
                    delay_ms: 0,
                },
                MacroEvent {
                    input: MacroInput::ClickAt {
                        x: -123,
                        y: 456,
                        button: MouseFn::Right,
                    },
                    edge: MacroEdge::Down,
                    delay_ms: 0,
                },
            ],
        }
    }
    #[test]
    fn cancellation_releases_inputs_without_running_later_clicks() {
        let events = Arc::new(Mutex::new(vec![]));
        let mut p = Playback::new(Sink(events.clone()), def(), MacroRun::WhileHeld, 8);
        assert_eq!(p.step().unwrap(), Some(65000));
        drop(p);
        assert_eq!(
            *events.lock().unwrap(),
            vec![
                (MacroInput::Key(0xe0), MacroEdge::Down),
                (MacroInput::Key(0xe0), MacroEdge::Up),
            ]
        );
    }
    #[test]
    fn repeat_count_and_atomic_click_are_exact() {
        let events = Arc::new(Mutex::new(vec![]));
        let mut p = Playback::new(Sink(events.clone()), def(), MacroRun::Times(2), 8);
        for _ in 0..6 {
            assert!(p.step().unwrap().is_some());
        }
        assert_eq!(p.step().unwrap(), None);
        drop(p);
        assert_eq!(events.lock().unwrap().len(), 6);
    }
    #[test]
    fn validation_and_coordinate_json_roundtrip() {
        validate(&def()).unwrap();
        let mut d = def();
        d.events.remove(1);
        assert!(validate(&d).is_err());
        let mut d = def();
        d.events[2].edge = MacroEdge::Up;
        assert!(validate(&d).is_err());
        let d: MacroDef = serde_json::from_str(&serde_json::to_string(&def()).unwrap()).unwrap();
        assert_eq!(d, def());
    }
}
