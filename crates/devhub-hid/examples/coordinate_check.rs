//! Reversible PC-macro/G9 integration check. Used by scripts/coordinate-input-check.py.
use devhub_core::*;
use std::{
    io::{self, Write},
    sync::Arc,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let coords: Vec<i32> = std::env::args()
        .skip(1)
        .map(|s| s.parse())
        .collect::<Result<_, _>>()?;
    if coords.len() != 4 {
        return Err("expected x1 y1 x2 y2".into());
    }
    let hub = devhub_hid::connect()?;
    hub.watch(Box::new(|e| {
        if let HubEvent::RuntimeError { message, .. } = e {
            eprintln!("RUNTIME ERROR: {message}");
        }
    }));
    let info = hub
        .scan()?
        .into_iter()
        .find(|d| d.driver == DriverKind::Logitech)
        .ok_or("G502 not found")?;
    let driver = hub.driver(&info.id)?;
    let original = driver.read()?;
    let mouse = original.mouse.as_ref().unwrap();
    let slot = (10..20)
        .find(|i| mouse.macros.get(*i).is_some_and(Option::is_none))
        .ok_or("no free PC slot")?;
    let restore = Change::Button {
        slot: 8,
        action: mouse.buttons[8].clone(),
    };
    let backup = std::path::PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap())
        .join("DeviceHub/migration/coordinate-before.json");
    std::fs::write(&backup, serde_json::to_vec_pretty(&original)?)?;
    let result = run(&driver, slot, &coords);
    devhub_hid::pc_macro::shutdown();
    let a = driver.apply(&restore);
    let b = driver.apply(&Change::Macro { slot, def: None });
    a?;
    b?;
    if driver.read()? != original {
        return Err("restored state differs".into());
    }
    println!("RESTORED");
    result
}

fn run(
    driver: &Arc<dyn DeviceDriver>,
    slot: usize,
    coords: &[i32],
) -> Result<(), Box<dyn std::error::Error>> {
    let def = MacroDef {
        name: "Coordinate verification".into(),
        mode: 1,
        repeat: 1,
        events: vec![
            MacroEvent {
                input: MacroInput::ClickAt {
                    x: coords[0],
                    y: coords[1],
                    button: MouseFn::Left,
                },
                edge: MacroEdge::Down,
                delay_ms: 150,
            },
            MacroEvent {
                input: MacroInput::ClickAt {
                    x: coords[2],
                    y: coords[3],
                    button: MouseFn::Right,
                },
                edge: MacroEdge::Down,
                delay_ms: 150,
            },
        ],
    };
    driver.apply(&Change::Macro {
        slot,
        def: Some(def.clone()),
    })?;
    let binding = ButtonAction::Macro {
        slot: slot as u8,
        run: MacroRun::WhileHeld,
    };
    driver.apply(&Change::Button {
        slot: 8,
        action: binding.clone(),
    })?;
    let state = driver.read()?.mouse.unwrap();
    if state.macros[slot] != Some(def) || state.buttons[8] != binding {
        return Err("PC macro readback differs".into());
    }
    println!("READY");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(())
}
