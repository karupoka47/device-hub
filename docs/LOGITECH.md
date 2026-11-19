# Logicool G502 HERO — HID++ integration

## Device and transport

Windows wired `046D:C08B`, interface 1, usage page `FF00`, usage `02`.
The mouse/keyboard input collections are never opened for configuration.
HID++ long output/input reports are 20 bytes:
`11 FF feature-index function<<4|software-id payload[16]`.
Feature indices are resolved through root `0000`; replies match device, feature,
function and software ID. HID++ errors are correlated to the originating request.

Observed protocol 4.2. Features used:

| Feature | Functions |
|---|---|
| `0003` | Firmware information |
| `2201` | DPI range (100–25600, 50 step on the tested firmware) |
| `8100` | Profile metadata, mode, profile/DPI selection, sector reads/writes |
| `8110` | Physical button count, spy start/stop and button-mask notifications |

Feature `8100` metadata: memory model 1, profile format 2, macro format 1,
5 profiles, 11 buttons, 16 sectors, **255 bytes per sector**.
Directory is sector 0; profiles are sectors 1–5. The driver validates that all
five directory entries are enabled and map to sectors 1–5 before allowing edits.

The final read starts at offset 239 (overlaps the preceding chunk).
Writes use begin-write fn6 (`sector:u16be, offset:u16be, count:u16be`),
16-byte fn7 blocks (last padded FF), then fn8. The final two bytes are
CRC-16/CCITT-FALSE, big endian, over the preceding 253 bytes.
Every write is read back and compared. Original sectors are saved once under
`%APPDATA%\DeviceHub\logitech\<device-id>\original`.

## Profile layout

| Offset | Meaning |
|---|---|
| 0 | Report interval in ms (1/2/4/8) |
| 1 / 2 | Default / DPI-shift index |
| 3–12 | Five DPI values, little endian, zero terminates |
| 32–75 | 11 normal button bindings, four bytes each |
| 96–139 | 11 G-shift bindings |
| 160–207 | UTF-16LE name, at most 24 code units |
| 208 onward | Existing lighting data (preserved) |
| 253–254 | CRC |

Only edited fields and CRC are changed; all other bytes are retained.
Both layers must retain a left-click binding. Profile and physical DPI changes
are observed every 750 ms and forwarded to the UI.
An explicitly selected profile is remembered in `selected-profile.json` and
reselected when the driver opens in onboard mode.

Button forms: `80 01 mask:u16be` mouse; `80 02 modifiers key` keyboard;
`80 03 usage:u16be` consumer; `90 special xx xx` special actions.
Special values: 1/2 horizontal scroll, 3/4 DPI up/down, 5 DPI cycle,
7 DPI shift, 10 profile cycle, 11 G-shift. Unknown bindings remain opaque.
The second set of 11 slots is shown as “Gシフト: …”; the UI uses capabilities.

## Onboard macros

Ten shared macro slots map to sectors 6–15, offset 0; bindings use
`00 sector 00 00`. Unknown/noncanonical existing programs cannot be overwritten.
All five profiles are checked before deleting or changing a shared binding mode.
Names are local metadata in `macros.json`; actual execution is in mouse firmware.

Independent format-1 codec:

* `41/42 mask:u16be`: mouse down/up
* `43/44 modifiers key`: keyboard down/up
* `40 delay:u16be`: delay in ms
* `02`: repeat from start while held
* `FF`: end

Supports single, held and bounded unrolled repeat. Does not advertise toggle:
opcode `03` can create an unbounded firmware loop and is deliberately unused.
Programs must be balanced (all pressed inputs released), at most 40 events and
253 bytes including termination. Held loops require at least 10 ms per cycle.
Unrecognised macros are preserved. Lua scripts and complex host-side G HUB
sequence constructs are not executed by this driver.

## PC coordinate macros

Windows adds PC slots 11–20 (zero-based 10–19); native slots 1–10 keep their
original storage and codecs. `MacroCaps.coordinate_slots` drives the UI.
`MacroInput::ClickAt { x, y, button }` is one complete click (`edge: Down`),
followed by `delay_ms`; it never needs an additional release event.
Definitions and per-profile/per-layer button bindings are atomically persisted in
`%APPDATA%\DeviceHub\logitech\<device-id>\pc-macros.json`.

The onboard binding marker is `80 01 80 00` (HID mouse button 16). Windows does
not turn that button into a standard click/key. While the app runs, `8110` fn1
enables event 0: its first two payload bytes are a **big-endian** held-button mask.
Verified G9 frames are `11 FF <8110-index> 00 01 00 ...` and release `00 00`.
The mouse stays in onboard mode. This avoids fake keyboard hotkeys and allows
native assignments to run alongside PC macros. A stopped app leaves PC bindings
inactive. Do not copy this marker to other models without hardware verification.

All command reads dispatch spy notifications; the idle reader drains every 5 ms
under the same transport lock. Physical release is routed by button identity,
including when G-shift changes while held. Profile changes invalidate routing.
One serialized player per device supports single, counted and held execution.
New playback cancels previous playback; Esc, disconnect, profile changes, edits,
driver drop and graceful shutdown release injected holds.
Screen clicks use a DPI-aware `SendInput` batch (virtual-desktop absolute move,
down, up), including negative monitor origins; points outside real monitors fail.
Playback errors are reported to the UI. Elevated targets require matching input
permissions. Forced OS process termination cannot execute release cleanup.

## Resident application

### Physical-button identification in the assignment page

On Windows, the existing `8110` reader also emits `HubEvent::ButtonActivity`
for changes to the 11-bit physical mask, including a final empty snapshot on
disconnect/driver drop. Duplicate masks, foreign-device reports and command replies
do not count as new presses. PC macro triggering still receives the same physical
events independently.

`MouseCaps.button_activity` groups normal slot `i` and G-shift slot `i+11`.
Both assignment rows therefore display the same physical number and light together.
The UI shows held inputs, an 850 ms release glow, and the last detected name.
All 11 numbers appear below the generic mouse illustration; clicking one reveals
its assignment row. This observes existing onboard input without remapping buttons
or entering host mode. A firmware-disabled/unbound button may not produce an
onboard spy event; no keyboard/hook inference is used as a fallback.

### Tray and startup

Windows tray icon, close-to-tray, right-click exit, `--background`, single-instance
activation, and icon re-registration after Explorer restart.
`scripts/install-resident.ps1` installs per-user to
`%LOCALAPPDATA%\Programs\DekaHub\deka-hub.exe`, adds the **Deka Hub** Start-menu
shortcut and HKCU Run `DekaHub` entry. Upgrading removes the old startup/shortcut
and binary, retaining `%APPDATA%\DeviceHub` preferences and local backups.
Native keyboard/mouse mappings work onboard even when the app exits.

## Verification commands

```powershell
python scripts/probe-logitech.py --profiles  # read-only, captures stay outside repo
cargo test -p devhub-core -p devhub-hid --lib
cargo run -p devhub-hid --example logitech
cargo run -p devhub-hid --example logitech -- --verify-writes
cargo build --release -p device-hub
cargo build -p devhub-hid --example coordinate_check
python scripts/coordinate-input-check.py # temporary G9 binding, restored afterward
```

The write check backs up memory, temporarily changes DPI/report rate and one
binding in each layer, restores them, and compares the entire returned state.
Physical input tests and reboot/startup checks are recorded separately; a
successful memory readback alone is not proof of macro playback.

### Hardware results (2026-11-18)

* G502 HERO, firmware U1 69.00.0009, Windows x64.
* DPI, report rate and normal/G-shift key bindings: changed, read back and
  restored; live `2201`/`8060` values also checked.
* Desktop import: 27 changes compared against complete readback.
* Held click macro: physical G7 produced 28 down / 28 up events in 1.420 s;
  physical G9 produced 32 down / 32 up events in 1.629 s, without G HUB.
* Original profiles and SQLite settings backed up locally.
* PC coordinate playback through physical G9: 15 left clicks at `(340,408)`,
  13 right clicks at `(755,408)`, all 56 down/up edges at the exact targets.
  User confirmed held-button release and Esc both stop playback.
  Temporary PC slot and G9 assignment removed; complete device state compared
  equal to the pre-test snapshot. Multi-monitor negative origins are implemented
  but were not part of this physical-button test.
* Release build installed with matching SHA-256; hidden background launch,
  tray presence, second-launch activation, single-instance count and close-to-tray
  verified. Login startup entry registered (no Windows reboot performed).
* G HUB uninstall entry, program folder, processes, updater service and startup
  entry absent after official uninstall; device readback still succeeds.
* Deka Hub upgrade: executable metadata, window/sidebar name, Start-menu target,
  renamed startup entry and installed SHA-256 verified. Close-to-tray and
  second-launch restoration keep exactly one resident process.
  Native UI created/saved/deleted a single ClickAt event without a spurious Up.
  Final unit suite: 61 passed; workspace Clippy has only pre-existing OBS/audio warnings.

### Button feedback verification (2026-11-19)

* 62 HID tests and 2 transient-feedback tests passed; command-read interleaving,
  duplicate masks, simultaneous buttons, release/drop and short-tap afterglow covered.
* Workspace Clippy passed with the existing OBS/audio warnings; release build
  installed and its SHA-256 matched the build.
* Native assignment page displayed a received G1 press as the last detected input,
  with its row outlined. Clicking number 11 scrolled to G11's assignment row.
  Physical hold/release feedback for every button has not been manually checked.

### G HUB desktop migration

1. `python scripts/inspect-ghub.py` creates a consistent SQLite backup using the
   SQLite backup API, and extracts JSON into `%LOCALAPPDATA%\DeviceHub\migration`.
2. `python scripts/import-ghub-desktop.py --profile 5` converts the active desktop
   profile into a local Change list. It fails on unsupported assignment/sequence
   forms rather than silently dropping them.
3. With other configurators closed, apply using
   `cargo run -p devhub-hid --example logitech -- --apply-json <generated-json>`.
   Occupied macro slots are rejected, every imported field is read back, and a
   failed write reselects the original profile.
4. `python scripts/logitech-input-check.py` checks physical G7/G9 macro playback
   inside a dedicated test window (no global input recording).

`mouseMacro BUTTON` commands represent complete clicks; recorded `mouse.button`
and `keyboard` entries use explicit press/release edges. For a held sequence
ending in a zero-delay click, the converter inserts the source default delay
(50 ms if absent) at the loop boundary and reports it, so adjacent clicks remain
distinct. Other slots retain their original onboard configuration.

## Protocol sources

Field/opcode research, followed by device discovery:

* https://github.com/libratbag/libratbag/blob/master/src/hidpp20.h
* https://github.com/libratbag/libratbag/blob/master/src/hidpp20.c
* https://github.com/cvuchener/hidpp/blob/master/profile_format.md
* https://github.com/cvuchener/hidpp/blob/master/src/libhidpp/hidpp20/MacroFormat.cpp
* https://openlogi.org/hidpp/features/x8100-onboard-profiles
* https://docs.rs/openlogi-hidpp/0.8.20/src/hidpp/feature/mouse_button_spy.rs.html

This implementation was written independently from the observed protocol fields.
Downloaded research material and private HID/profile captures are not distributed.
