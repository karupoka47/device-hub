//! G502 read/backup and explicit reversible integration check.
//! cargo run -p devhub-hid --example logitech -- [--verify-writes | --apply-json FILE]
use devhub_core::{backend::*, model::*};
use std::{path::PathBuf, sync::Arc};

fn roundtrip(
    d: &Arc<dyn DeviceDriver>,
    change: Change,
    restore: Change,
    check: impl Fn(&DeviceState) -> bool,
) -> HubResult<()> {
    let result = d.apply(&change).and_then(|_| d.read()).and_then(|s| {
        if check(&s) {
            Ok(())
        } else {
            Err(HubError::Protocol("Readback differs".into()))
        }
    });
    let restored = d.apply(&restore);
    result.and(restored)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let hub = devhub_hid::connect()?;
    let info = hub
        .scan()?
        .into_iter()
        .find(|d| d.driver == DriverKind::Logitech)
        .ok_or("G502 not found")?;
    let driver = hub.driver(&info.id)?;
    let initial = driver.read()?;
    let root = PathBuf::from(std::env::var_os("LOCALAPPDATA").ok_or("LOCALAPPDATA missing")?)
        .join("DeviceHub/migration");
    std::fs::create_dir_all(&root)?;
    let backup = root.join("rust-initial-memory.json");
    if !backup.exists() {
        driver.run_action("backup", Some(&backup))?;
    }
    println!("Device: {} / {:?}", info.name, driver.info().firmware);
    println!("State: {}", serde_json::to_string_pretty(&initial)?);
    if std::env::args().any(|a| a == "--verify-writes") {
        let mouse = initial.mouse.as_ref().unwrap();
        let original = initial.settings["polling"].clone();
        let rate = if original.as_choice() == 1000 {
            500
        } else {
            1000
        };
        roundtrip(
            &driver,
            Change::Setting {
                id: "polling".into(),
                value: SettingValue::Choice(rate),
            },
            Change::Setting {
                id: "polling".into(),
                value: original,
            },
            |s| s.settings["polling"].as_choice() == rate,
        )?;
        println!("PASS polling write/read/restore");
        let mut stages = mouse.stages.clone();
        stages[0].x += 50;
        let expected = stages[0].x;
        roundtrip(
            &driver,
            Change::DpiStages {
                stages,
                active: mouse.active_stage,
            },
            Change::DpiStages {
                stages: mouse.stages.clone(),
                active: mouse.active_stage,
            },
            |s| s.mouse.as_ref().unwrap().stages[0].x == expected,
        )?;
        println!("PASS DPI write/read/restore");
        let action = ButtonAction::Key(KeyStroke {
            mods: 0,
            usage: 0x73,
        }); // F24
        for slot in [6, 17] {
            roundtrip(
                &driver,
                Change::Button {
                    slot,
                    action: action.clone(),
                },
                Change::Button {
                    slot,
                    action: mouse.buttons[slot].clone(),
                },
                |s| s.mouse.as_ref().unwrap().buttons[slot] == action,
            )?;
        }
        println!("PASS normal + G-shift mapping write/read/restore");
        let last = driver.read()?;
        if last != initial {
            return Err("State not restored exactly".into());
        }
        println!("PASS complete state restored");
    }
    let args: Vec<String> = std::env::args().collect();
    if let Some(pos) = args.iter().position(|a| a == "--apply-json") {
        let path = args.get(pos + 1).ok_or("Missing JSON path")?;
        let changes: Vec<Change> = serde_json::from_slice(&std::fs::read(path)?)?;
        // Refuse to replace occupied macro slots during migration.
        for change in &changes {
            if let Change::Macro { slot, def: Some(_) } = change {
                if initial
                    .mouse
                    .as_ref()
                    .unwrap()
                    .macros
                    .get(*slot)
                    .is_none_or(Option::is_some)
                {
                    return Err("Migration would replace an existing macro slot".into());
                }
            }
        }
        for change in &changes {
            if let Err(error) = driver.apply(change) {
                let _ = driver.apply(&Change::Profile(initial.profile.active));
                return Err(format!("Migration stopped at {change:?}: {error}; original profile reselected; backup: {}", backup.display()).into());
            }
        }
        let actual = driver.read()?;
        for change in &changes {
            let valid = match change {
                Change::Macro { slot, def } => actual.mouse.as_ref().unwrap().macros[*slot] == *def,
                Change::Button { slot, action } => {
                    actual.mouse.as_ref().unwrap().buttons[*slot] == *action
                }
                Change::Setting { id, value } => actual.settings.get(id) == Some(value),
                Change::DpiStages { stages, active } => {
                    let m = actual.mouse.as_ref().unwrap();
                    m.stages == *stages && m.active_stage == *active
                }
                Change::Profile(index) => actual.profile.active == *index,
                Change::ProfileName { index, name } => {
                    actual.profile.names[*index as usize] == *name
                }
                _ => false,
            };
            if !valid {
                return Err(format!("Import readback mismatch: {change:?}").into());
            }
        }
        println!(
            "PASS imported {} changes, all verified by readback",
            changes.len()
        );
    }
    Ok(())
}
