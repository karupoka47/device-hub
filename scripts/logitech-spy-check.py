"""Temporary G9 spy probe. Restores the exact profile bytes in finally."""
import importlib.util
import json
import os
import time
import tkinter as tk
from pathlib import Path

spec = importlib.util.spec_from_file_location("probe", Path(__file__).with_name("probe-logitech.py"))
probe = importlib.util.module_from_spec(spec)
spec.loader.exec_module(probe)
p = probe.Hidpp()
om = p.feature(0x8100)
spy = p.feature(0x8110)
assert p.call(om, 2)[0] == 1
sector = int.from_bytes(p.call(om, 4)[:2], "big")
size = int.from_bytes(p.call(om, 0)[7:9], "big")
assert size == 255


def read():
    out = bytearray(size)
    for start in range(0, size, 16):
        at = min(start, size - 16)
        out[at:at + 16] = p.call(om, 5, [0, sector, at >> 8, at & 255])[:16]
    return out


def write(data):
    p.call(om, 6, [0, sector, 0, 0, size >> 8, size & 255])
    for start in range(0, size, 16):
        p.call(om, 7, bytes(data[start:start + 16]).ljust(16, b"\xff"))
    p.call(om, 8)
    assert read() == data
    p.call(om, 3, [0, sector])


def crc(data):
    value = 0xffff
    for b in data:
        value ^= b << 8
        for _ in range(8):
            value = ((value << 1) ^ (0x1021 if value & 0x8000 else 0)) & 0xffff
    return value


original = read()
root = Path(os.environ["LOCALAPPDATA"]) / "DeviceHub/migration"
backup = root / f"spy-probe-{int(time.time())}.bin"
backup.write_bytes(original)
results = {"count": p.call(spy, 0).hex(), "map": p.call(spy, 3).hex(), "events": []}
modified = False
window = tk.Tk()
window.title("Deka Hub — G9 ボタン通知の確認")
window.geometry("720x330")
window.attributes("-topmost", True)
status = tk.StringVar(value="開始を押してから G9 を3回押してください。\n終了時に元の割り当てへ自動復元します。")
tk.Label(window, textvariable=status, font=("", 15), wraplength=680).pack(pady=30)
active = False


def begin():
    global modified, active
    if active:
        return
    data = original.copy()
    data[64:68] = bytes([0x80, 1, 0x80, 0])  # G9 -> HID mouse button 16
    data[-2:] = crc(data[:-2]).to_bytes(2, "big")
    modified = True
    write(data)
    p.call(spy, 1)
    active = True
    status.set("G9 を3回押して離してください。\n確認後「終了」を押してください。")


def tick():
    if active:
        while True:
            r = bytes(p.device.read(64, 1))
            if not r:
                break
            if len(r) >= 6 and r[2:4] == bytes([spy, 0]):
                mask = int.from_bytes(r[4:6], "big")
                results["events"].append({"mask": mask, "raw": r.hex()})
                status.set(f"ボタン通知: 0x{mask:04X} / {len(results['events'])}件\nG9 を3回押したら「終了」を押してください。")
    window.after(10, tick)


tk.Button(window, text="開始", command=begin, font=("", 16), width=18).pack(pady=10)
tk.Button(window, text="終了（元の設定へ戻す）", command=window.destroy, font=("", 14)).pack(pady=10)
try:
    window.after(10, tick)
    window.after(180000, window.destroy)
    window.mainloop()
finally:
    p.call(spy, 2)
    if modified:
        write(original)
    results["restored"] = read() == original
    (root / "spy-check.json").write_text(json.dumps(results, indent=2), encoding="utf-8")
    p.device.close()
    print(json.dumps(results), flush=True)
