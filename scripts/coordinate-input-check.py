"""G9 -> real driver -> resident playback -> physical-coordinate click receiver."""
import ctypes
import json
import os
from pathlib import Path
import queue
import subprocess
import threading
import time
import tkinter as tk

ctypes.windll.user32.SetProcessDpiAwarenessContext(ctypes.c_void_p(-4))
window = tk.Tk()
window.title("Deka Hub — 座標クリックの実機テスト")
window.geometry("880x480+100+100")
window.attributes("-topmost", True)
window.resizable(False, False)
status = tk.StringVar(value="準備中…")
tk.Label(window, textvariable=status, font=("", 16), wraplength=850).pack(pady=24)
pads = tk.Frame(window)
pads.pack(fill="both", expand=True, padx=25, pady=15)
events = []
targets = []
started = time.monotonic()
for index, title in enumerate(("ここへ左クリック", "ここへ右クリック")):
    pad = tk.Label(pads, text=title, bg=("#164a7c", "#6c326a")[index], fg="white", font=("", 19))
    pad.pack(side="left", fill="both", expand=True, padx=10)
    for button in (1, 3):
        for edge in ("Press", "Release"):
            def record(e, i=index, b=button, edge=edge):
                events.append({"pad": i, "button": b, "edge": edge, "x": e.x_root, "y": e.y_root,
                               "time": round(time.monotonic() - started, 4)})
            pad.bind(f"<Button{edge}-{button}>", record)
    targets.append(pad)
window.update()
coords = [(p.winfo_rootx() + p.winfo_width() // 2, p.winfo_rooty() + p.winfo_height() // 2) for p in targets]
binary = Path(__file__).resolve().parent.parent / "target/debug/examples/coordinate_check.exe"
process = subprocess.Popen([str(binary), *map(str, sum((list(c) for c in coords), []))],
                           stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                           text=True, encoding="utf-8")
messages = queue.Queue()
log = []


def reader():
    for line in process.stdout:
        messages.put(line.strip())


threading.Thread(target=reader, daemon=True).start()
finishing = False
ready = False


def done():
    global finishing
    if finishing:
        return
    finishing = True
    status.set("元のG9割り当てとマクロを復元しています…")
    try:
        process.stdin.write("done\n")
        process.stdin.flush()
    except (BrokenPipeError, OSError):
        pass


def tick():
    global ready
    while not messages.empty():
        line = messages.get()
        log.append(line)
        print(line, flush=True)
        if line == "READY":
            ready = True
            status.set("G9を1秒ほど長押し → 離すと停止。\nもう一度長押ししながらEsc → 停止を確認してください。")
        if line == "RESTORED":
            report = {"targets": coords, "events": events, "restored": True, "log": log}
            path = Path(os.environ["LOCALAPPDATA"]) / "DeviceHub/migration/coordinate-input-check.json"
            path.write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
            print(json.dumps({"targets": coords, "events": len(events), "restored": True}), flush=True)
            window.destroy()
            return
    if process.poll() is not None and not finishing:
        status.set("確認プログラムが終了しました。ログを確認します。")
    window.after(20, tick)


tk.Button(window, text="確認完了（元の設定へ戻す）", command=done, font=("", 15)).pack(pady=20)
window.protocol("WM_DELETE_WINDOW", done)
window.after(20, tick)
window.after(180000, done)
window.mainloop()
process.wait(timeout=15)
