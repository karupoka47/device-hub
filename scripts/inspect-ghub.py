"""Back up G HUB's SQLite store and summarize device/profile configuration locally."""
import json
import os
import sqlite3
from datetime import datetime
from pathlib import Path

root = Path(os.environ["LOCALAPPDATA"]) / "DeviceHub" / "migration" / datetime.now().strftime("%Y%m%d-%H%M%S")
root.mkdir(parents=True, exist_ok=True)
source = Path(os.environ["LOCALAPPDATA"]) / "LGHUB" / "settings.db"
with sqlite3.connect(f"{source.as_uri()}?mode=ro", uri=True) as src:
    with sqlite3.connect(root / "settings.db") as dst:
        src.backup(dst)
    tables = src.execute("SELECT name, sql FROM sqlite_master WHERE type='table'").fetchall()
    print("Backup:", root)
    print("Tables:", tables)
    for name, _ in tables:
        if name.startswith("sqlite_"):
            continue
        for i, row in enumerate(src.execute('SELECT * FROM "' + name.replace('"', '""') + '"')):
            for j, value in enumerate(row):
                if isinstance(value, bytes):
                    value = value.decode("utf-8", errors="replace")
                if isinstance(value, str):
                    try:
                        data = json.loads(value)
                    except ValueError:
                        continue
                    out = root / f"{name}-{i}-{j}.json"
                    out.write_text(json.dumps(data, ensure_ascii=False, indent=2), encoding="utf-8")
                    print("JSON:", out)
                    print("Root keys:", list(data) if isinstance(data, dict) else type(data).__name__)
