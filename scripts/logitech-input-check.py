"""Interactive, window-scoped input check; records only clicks inside its test pad."""
import json
import os
import time
import tkinter as tk
from pathlib import Path

root = tk.Tk()
root.title("Device HUB — G7 / G9 macro check")
root.geometry("780x440")
root.attributes("-topmost", True)
results = {}
current = None
downs = 0
ups = 0
started = None
last = None
status = tk.StringVar(value="G7またはG9のテストを選び、青い領域でそのボタンを2秒ほど押してください。")


def finish_phase():
    if current:
        results[current] = {"downs": downs, "ups": ups,
                            "duration_seconds": round((last - started), 3) if started and last else 0}


def begin(button):
    global current, downs, ups, started, last
    finish_phase()
    current, downs, ups, started, last = button, 0, 0, None, None
    status.set(f"{button}: 青い領域にポインターを置き、マウスの{button}を押し続けて離してください。")


def click(down):
    global downs, ups, started, last
    if not current:
        return
    now = time.monotonic()
    started = started or now
    last = now
    if down:
        downs += 1
    else:
        ups += 1
    status.set(f"{current}: 押下 {downs} / 解放 {ups} — ボタンを離すとカウントが止まれば正常")


def done():
    finish_phase()
    path = Path(os.environ["LOCALAPPDATA"]) / "DeviceHub/migration/input-check.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(results, indent=2), encoding="utf-8")
    print(json.dumps(results), flush=True)
    root.destroy()


tk.Label(root, text="G HUBを起動せず、G7 / G9 の連打を確認", font=("", 18)).pack(pady=16)
bar = tk.Frame(root)
bar.pack()
for button in ("G7", "G9"):
    tk.Button(bar, text=f"{button}をテスト", command=lambda b=button: begin(b),
              width=20, font=("", 13)).pack(side="left", padx=10)
pad = tk.Label(root, text="ここにポインターを置いて G7 / G9 を長押し",
               bg="#174e8c", fg="white", font=("", 18), height=7)
pad.pack(fill="both", expand=True, padx=20, pady=16)
pad.bind("<ButtonPress-1>", lambda _: click(True))
pad.bind("<ButtonRelease-1>", lambda _: click(False))
tk.Label(root, textvariable=status, wraplength=740, font=("", 11)).pack(pady=5)
tk.Button(root, text="結果を保存して終了", command=done, font=("", 13)).pack(pady=12)
root.protocol("WM_DELETE_WINDOW", done)
root.mainloop()
