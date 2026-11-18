//! Format-1 onboard macros: bounded, balanced input sequences.
//! Opcode facts are documented in docs/LOGITECH.md; this is an independent codec.
use super::*;

pub(super) fn encode(def: &MacroDef, size: usize) -> HubResult<Vec<u8>> {
    if def.events.is_empty() || def.events.len() > 40 || def.mode > 2 {
        return Err(invalid(
            "マクロは1～40イベント（1回・押している間・回数指定）",
        ));
    }
    let mut pressed = Vec::new();
    let mut body = Vec::new();
    for e in &def.events {
        match e.edge {
            MacroEdge::Down => {
                if pressed.contains(&e.input) {
                    return Err(invalid("マクロで同じキーを二重に押しています"));
                }
                pressed.push(e.input);
            }
            MacroEdge::Up => {
                let i = pressed
                    .iter()
                    .position(|x| *x == e.input)
                    .ok_or_else(|| invalid("押されていないキーのリリース"))?;
                pressed.remove(i);
            }
        }
        let down = e.edge == MacroEdge::Down;
        match e.input {
            MacroInput::Key(usage @ 0xe0..=0xe7) => {
                body.extend_from_slice(&[if down { 0x43 } else { 0x44 }, 1 << (usage - 0xe0), 0]);
            }
            MacroInput::Key(usage @ 1..=0xdf) => {
                body.extend_from_slice(&[if down { 0x43 } else { 0x44 }, 0, usage as u8]);
            }
            MacroInput::Mouse(m) => {
                let mask =
                    mouse_mask(m).ok_or_else(|| invalid("マクロ内のホイールは非対応です"))?;
                body.extend_from_slice(&[
                    if down { 0x41 } else { 0x42 },
                    (mask >> 8) as u8,
                    mask as u8,
                ]);
            }
            _ => return Err(invalid("Unsupported macro key")),
        }
        if e.delay_ms > u16::MAX as u32 {
            return Err(invalid("Macro delay exceeds 65535 ms"));
        }
        if e.delay_ms != 0 {
            body.extend_from_slice(&[0x40, (e.delay_ms >> 8) as u8, e.delay_ms as u8]);
        }
    }
    if !pressed.is_empty() {
        return Err(invalid("マクロの最後で全キーを離してください"));
    }
    if def.mode == 1 && def.events.iter().map(|e| e.delay_ms).sum::<u32>() < 10 {
        return Err(invalid("繰り返しマクロには1周10ms以上の待ち時間が必要です"));
    }
    let repeat = if def.mode == 2 { def.repeat } else { 1 };
    if repeat == 0
        || def.events.len() * repeat as usize > 40
        || body.len() * repeat as usize + 2 > size - 2
    {
        return Err(invalid("オンボードマクロの253バイト上限を超えます"));
    }
    let mut out = body.repeat(repeat as usize);
    if def.mode == 1 {
        out.push(0x02);
    }
    out.push(0xff);
    out.resize(size, 0xff);
    let crc = crc16(&out[..size - 2]);
    out[size - 2..].copy_from_slice(&crc.to_be_bytes());
    Ok(out)
}

pub(super) fn decode(bytes: &[u8], name: String) -> HubResult<MacroDef> {
    let end = bytes
        .len()
        .checked_sub(2)
        .ok_or_else(|| protocol("Short macro sector"))?;
    if crc16(&bytes[..end]) != be(&bytes[end..]) {
        return Err(protocol("Macro CRC mismatch"));
    }
    let mut events: Vec<MacroEvent> = Vec::new();
    let mut mode = 0;
    let mut i = 0;
    while i < end {
        let opcode = bytes[i];
        i += 1;
        match opcode {
            0xff => {
                if bytes[i..end].iter().any(|b| *b != 0xff) {
                    return Err(protocol("Foreign macro sector layout"));
                }
                let def = MacroDef {
                    name,
                    events,
                    mode,
                    repeat: 1,
                };
                // Canonical round-trip also rejects unbalanced or unrecognised programs.
                if encode(&def, bytes.len())? != bytes {
                    return Err(protocol("Noncanonical macro program"));
                }
                return Ok(def);
            }
            2 if mode == 0 => {
                mode = 1;
            }
            0x40..=0x44 if i + 2 <= end && mode == 0 => {
                let a = bytes[i];
                let b = bytes[i + 1];
                i += 2;
                if opcode == 0x40 {
                    let event = events
                        .last_mut()
                        .ok_or_else(|| protocol("Leading macro delay"))?;
                    event.delay_ms += u16::from_be_bytes([a, b]) as u32;
                    continue;
                }
                let input = if opcode <= 0x42 {
                    let mask = u16::from_be_bytes([a, b]);
                    let m = [
                        MouseFn::Left,
                        MouseFn::Right,
                        MouseFn::Middle,
                        MouseFn::Back,
                        MouseFn::Forward,
                    ]
                    .into_iter()
                    .find(|m| mouse_mask(*m) == Some(mask))
                    .ok_or_else(|| protocol("Unknown macro mouse mask"))?;
                    MacroInput::Mouse(m)
                } else if a == 0 && b > 0 {
                    MacroInput::Key(b as u16)
                } else if a.is_power_of_two() && b == 0 {
                    MacroInput::Key(0xe0 + a.trailing_zeros() as u16)
                } else {
                    return Err(protocol("Unknown macro keyboard chord"));
                };
                events.push(MacroEvent {
                    input,
                    edge: if matches!(opcode, 0x41 | 0x43) {
                        MacroEdge::Down
                    } else {
                        MacroEdge::Up
                    },
                    delay_ms: 0,
                });
            }
            _ => return Err(protocol("Unsupported macro opcode")),
        }
    }
    Err(protocol("Missing macro terminator"))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn click() -> MacroDef {
        MacroDef {
            name: "click".into(),
            mode: 1,
            repeat: 1,
            events: vec![
                MacroEvent {
                    input: MacroInput::Mouse(MouseFn::Left),
                    edge: MacroEdge::Down,
                    delay_ms: 50,
                },
                MacroEvent {
                    input: MacroInput::Mouse(MouseFn::Left),
                    edge: MacroEdge::Up,
                    delay_ms: 50,
                },
            ],
        }
    }
    #[test]
    fn held_macro_protocol_bytes() {
        let d = click();
        let b = encode(&d, 255).unwrap();
        assert_eq!(
            &b[..14],
            &[0x41, 0, 1, 0x40, 0, 50, 0x42, 0, 1, 0x40, 0, 50, 2, 0xff]
        );
        assert_eq!(decode(&b, d.name.clone()).unwrap(), d);
    }
    #[test]
    fn rejects_stuck_keys_overflow_and_unsafe_loop() {
        let mut d = click();
        d.events.pop();
        assert!(encode(&d, 255).is_err());
        let mut d = click();
        d.mode = 2;
        d.repeat = 255;
        assert!(encode(&d, 255).is_err());
        let mut d = click();
        d.events.iter_mut().for_each(|e| e.delay_ms = 0);
        assert!(encode(&d, 255).is_err());
    }
    #[test]
    fn foreign_or_corrupt_macro_is_not_rewritten() {
        let mut b = encode(&click(), 255).unwrap();
        b[1] ^= 1;
        assert!(decode(&b, "".into()).is_err());
    }
}
