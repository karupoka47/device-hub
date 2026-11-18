//! PC slots use an otherwise unhandled HID mouse button 16 as their onboard marker.
//! Physical button identities come from 0x8110, never from synthetic keyboard keys.
use super::*;
use serde::{Deserialize, Serialize};

pub(super) const MARKER: [u8; 4] = [0x80, 1, 0x80, 0];
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(super) struct Binding {
    pub slot: u8,
    pub run: MacroRun,
}
#[derive(Clone, Default, Serialize, Deserialize)]
struct Data {
    macros: BTreeMap<usize, MacroDef>,
    bindings: BTreeMap<String, Binding>,
}
pub(super) struct Host {
    path: PathBuf,
    data: Data,
    pub profile: u8,
    active: Vec<Option<Binding>>,
    shift_mask: u16,
    held: u16,
    #[cfg(windows)]
    runner: crate::pc_macro::Runner,
}
impl Host {
    #[cfg(all(test, windows))]
    pub(super) fn test_empty() -> Self {
        Self {
            path: PathBuf::new(), data: Data::default(), profile: 0,
            active: vec![None; BUTTONS * 2], shift_mask: 0, held: 0,
            runner: crate::pc_macro::Runner::new(|_| {}),
        }
    }
    #[cfg(test)]
    pub(super) fn held_mask(&self) -> u16 { self.held }
    #[cfg(windows)]
    pub fn load(path: PathBuf, id: DeviceId, notify: Option<crate::Watcher>) -> HubResult<Self> {
        let data: Data = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| protocol(format!("PC macro file: {e}")))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Data::default(),
            Err(e) => return Err(io(e)),
        };
        for (slot, def) in &data.macros {
            if !(10..20).contains(slot) {
                return Err(invalid("Invalid PC macro slot"));
            }
            crate::pc_macro::validate(def)?;
        }
        Ok(Self {
            path,
            data,
            profile: 0,
            active: vec![None; BUTTONS * 2],
            shift_mask: 0,
            held: 0,
            runner: crate::pc_macro::Runner::new(move |message| {
                if let Some(cb) = &notify {
                    cb(HubEvent::RuntimeError {
                        id: id.clone(),
                        message,
                    });
                }
            }),
        })
    }
    fn save(&mut self, data: Data) -> HubResult<()> {
        fs::create_dir_all(self.path.parent().unwrap()).map_err(io)?;
        let temp = self.path.with_extension("tmp");
        fs::write(
            &temp,
            serde_json::to_vec_pretty(&data).map_err(|e| protocol(e.to_string()))?,
        )
        .map_err(io)?;
        fs::rename(temp, &self.path).map_err(io)?;
        self.invalidate();
        self.data = data;
        Ok(())
    }
    pub fn macros(&self) -> Vec<Option<MacroDef>> {
        (10..20)
            .map(|i| self.data.macros.get(&i).cloned())
            .collect()
    }
    pub fn set_macro(&mut self, slot: usize, def: Option<&MacroDef>) -> HubResult<()> {
        if !(10..20).contains(&slot) {
            return Err(invalid("PC macro slot out of range"));
        }
        let mut data = self.data.clone();
        if let Some(def) = def {
            crate::pc_macro::validate(def)?;
            data.macros.insert(slot, def.clone());
        } else {
            // Caller has checked actual markers in every onboard profile.
            data.bindings.retain(|_, b| b.slot as usize != slot);
            data.macros.remove(&slot);
        }
        self.save(data)
    }
    pub fn binding(&self, profile: u8, button: usize) -> Option<Binding> {
        self.data
            .bindings
            .get(&format!("{profile}:{button}"))
            .cloned()
    }
    pub fn set_binding(
        &mut self,
        profile: u8,
        button: usize,
        binding: Option<Binding>,
    ) -> HubResult<()> {
        if self.binding(profile, button) == binding {
            return Ok(());
        }
        let mut data = self.data.clone();
        let key = format!("{profile}:{button}");
        if let Some(b) = binding {
            if !self.data.macros.contains_key(&(b.slot as usize))
                || !matches!(b.run, MacroRun::Times(1..=255) | MacroRun::WhileHeld)
            {
                return Err(invalid("PCマクロまたは再生方法が不正です"));
            }
            data.bindings.insert(key, b);
        } else {
            data.bindings.remove(&key);
        }
        self.save(data)
    }
    pub fn invalidate(&mut self) {
        #[cfg(windows)]
        self.runner.cancel();
        self.profile = 0;
        self.active.fill(None);
    }
    pub fn refresh(&mut self, profile: u8, bytes: &[u8]) {
        let active: Vec<_> = (0..BUTTONS * 2)
            .map(|button| {
                let at = offset(button).unwrap();
                (bytes[at..at + 4] == MARKER)
                    .then(|| self.binding(profile, button))
                    .flatten()
            })
            .collect();
        if self.profile != profile || self.active != active {
            self.invalidate();
        }
        self.profile = profile;
        self.active = active;
        self.shift_mask = (0..BUTTONS)
            .filter(|i| {
                let at = offset(*i).unwrap();
                bytes[at..at + 2] == [0x90, 11]
            })
            .fold(0, |mask, i| mask | (1 << i));
    }
    pub fn buttons(&mut self, mask: u16) {
        let pressed = mask & !self.held;
        let released = self.held & !mask;
        self.held = mask;
        // Releases refer to physical buttons, even if G-shift changed mid-press.
        for button in 0..BUTTONS {
            if released & (1 << button) != 0 {
                #[cfg(windows)]
                self.runner.release(button);
            }
            if pressed & (1 << button) != 0 {
                let slot = button
                    + if mask & self.shift_mask != 0 {
                        BUTTONS
                    } else {
                        0
                    };
                if let Some(b) = &self.active[slot] {
                    if let Some(def) = self.data.macros.get(&(b.slot as usize)) {
                        #[cfg(windows)]
                        self.runner.start(button, def.clone(), b.run);
                    }
                }
            }
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    #[test]
    fn stale_metadata_cannot_override_native_bindings_or_other_profiles() {
        let mut host = Host::test_empty();
        let binding = Binding { slot: 10, run: MacroRun::WhileHeld };
        host.data.bindings.insert("5:8".into(), binding.clone());
        let mut bytes = vec![0xff; 255];
        host.refresh(5, &bytes);
        assert!(host.active.iter().all(Option::is_none));
        let at = offset(8).unwrap();
        bytes[at..at + 4].copy_from_slice(&MARKER);
        host.refresh(5, &bytes);
        assert_eq!(host.active[8], Some(binding));
        host.refresh(4, &bytes);
        assert!(host.active.iter().all(Option::is_none));
        host.refresh(5, &bytes);
        host.invalidate();
        assert!(host.active.iter().all(Option::is_none));
    }
}
