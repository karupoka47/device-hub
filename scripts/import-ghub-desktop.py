"""Convert G502 G HUB desktop bindings into a local Device HUB Change list.

No device writes. Review the output, then use the logitech example --apply-json.
Settings and personal profile names are written outside the repository.
"""
import argparse
import json
import os
from pathlib import Path


def convert(store, target):
    cards = {c["id"]: c for c in store["cards"]["cards"]}
    app = next(a for a in store["applications"]["applications"]
               if a["name"] == "APPLICATION_NAME_DESKTOP")
    profile = next(p for p in store["profiles"]["profiles"]
                   if p["applicationId"] == app["applicationId"] and p.get("activeForApplication"))
    assignments = {a["slotId"]: a["cardId"] for a in profile["assignments"]}
    mouse = cards[assignments["g502hero_mouse_settings"]]["mouseSettings"]
    table = mouse["dpiTable"]
    macros = {}
    definitions = []
    reports = []
    mouse_buttons = ["Left", "Right", "Middle", "Back", "Forward"]

    def macro(card):
        if card["id"] in macros:
            return macros[card["id"]]
        value = card["macro"]
        if value["type"] != "SEQUENCE":
            raise ValueError(f"Unsupported macro type: {value['type']}")
        seq = value["sequence"]
        nonempty = {k: v["components"] for k, v in seq.items()
                    if k.endswith("Sequence") and isinstance(v, dict) and v.get("components")}
        if set(nonempty) not in ({"heldSequence"}, {"simpleSequence"}, {"pressSequence"}):
            raise ValueError("Complex sequence requires manual migration")
        key = next(iter(nonempty))
        mode = 1 if key == "heldSequence" else 0
        events = []
        for component in nonempty[key]:
            if "delay" in component:
                if not events:
                    raise ValueError("Leading macro delay unsupported")
                events[-1]["delay_ms"] += component["delay"]["durationMs"]
            elif "mouseMacro" in component:
                # G HUB mouseMacro BUTTON is a complete click, unlike
                # mouse.button {isDown,...}, which is a recorded edge.
                m = component["mouseMacro"]
                if m["action"] != "BUTTON" or m["hidUsage"] not in range(1, 6):
                    raise ValueError("Unsupported mouse command")
                inp = {"Mouse": mouse_buttons[m["hidUsage"] - 1]}
                events += [{"input": inp, "edge": "Down", "delay_ms": 0},
                           {"input": inp, "edge": "Up", "delay_ms": 0}]
            elif "mouse" in component and "button" in component["mouse"]:
                m = component["mouse"]["button"]
                events.append({"input": {"Mouse": mouse_buttons[m["hidUsage"] - 1]},
                               "edge": "Down" if m.get("isDown") else "Up", "delay_ms": 0})
            elif "keyboard" in component:
                k = component["keyboard"]
                events.append({"input": {"Key": k["hidUsage"]},
                               "edge": "Down" if k.get("isDown") else "Up", "delay_ms": 0})
            else:
                raise ValueError(f"Unsupported macro component: {list(component)}")
        if mode == 1 and events[-1]["delay_ms"] == 0:
            # Explicit cycle pause prevents two adjacent boundary clicks from
            # collapsing into one host input report.
            events[-1]["delay_ms"] = seq.get("defaultDelay", 50)
            reports.append(f"{card['name']}: cycle-end pause {events[-1]['delay_ms']} ms")
        slot = len(macros)
        macros[card["id"]] = (slot, mode)
        definitions.append({"Macro": {"slot": slot, "def": {
            "name": card["name"], "events": events, "mode": mode, "repeat": 1}}})
        return slot, mode

    def action(card_id):
        if card_id in cards and cards[card_id].get("attribute") == "MACRO_PLAYBACK":
            slot, mode = macro(cards[card_id])
            return {"Macro": {"slot": slot, "run": "WhileHeld" if mode else {"Times": 1}}}
        if not card_id.startswith("0f82f693-5b78-4cf5-867e-"):
            raise ValueError(f"Unsupported assignment card: {card_id}")
        suffix = card_id.rsplit("-", 1)[1]
        kind, code = int(suffix[:2], 16), int(suffix[2:4], 16)
        if kind == 1:
            if 0xE0 <= code <= 0xE7:
                return {"Key": {"mods": 1 << (code - 0xE0), "usage": 0}}
            return {"Key": {"mods": 0, "usage": code}}
        if kind == 2 and code in range(1, 6):
            return {"Mouse": mouse_buttons[code - 1]}
        if kind == 4:
            return {1: {"Dpi": "Up"}, 2: {"Dpi": "Down"},
                    3: {"Dpi": {"Shift": table["shiftDpi"]}},
                    11: {"Mouse": "ScrollLeft"}, 12: {"Mouse": "ScrollRight"}}[code]
        if kind == 9:
            return {1: "ProfileCycle", 7: None}[code]
        raise ValueError(f"Unsupported built-in card {kind:02x}/{code:02x}")

    normal = [action(assignments[f"g502hero_g{i}_m1"]) for i in range(1, 12)]
    shifted = [action(assignments[f"g502hero_g{i}_m1_shifted"]) for i in range(1, 12)]
    shifted = [normal[i] if a is None else a for i, a in enumerate(shifted)]
    stages = [{"x": dpi, "y": None, "color": {"r": 0, "g": 0, "b": 0}, "enabled": True}
              for dpi in table["levels"]]
    changes = definitions + [
        {"Profile": target},
        {"DpiStages": {"stages": stages, "active": table["levels"].index(table["activeDpi"])}},
        {"Setting": {"id": "polling", "value": {"Choice": mouse["reportRate"]["value"]}}},
        {"ProfileName": {"index": target, "name": "G HUB Desktop"}},
    ]
    changes += [{"Button": {"slot": i, "action": a}}
                for i, a in enumerate(normal + shifted)]
    return changes, reports


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--source", type=Path)
    parser.add_argument("--profile", type=int, default=5, choices=range(1, 6))
    args = parser.parse_args()
    root = Path(os.environ["LOCALAPPDATA"]) / "DeviceHub" / "migration"
    source = args.source or sorted(root.glob("*/data-0-2.json"))[-1]
    changes, reports = convert(json.loads(source.read_text(encoding="utf-8")), args.profile - 1)
    dest = root / "ghub-desktop-changes.json"
    dest.write_text(json.dumps(changes, ensure_ascii=False, indent=2), encoding="utf-8")
    print("Converted:", dest)
    print("Target profile:", args.profile, "Changes:", len(changes))
    for report in reports:
        print(report)
