"""Read-only HID++ discovery for the wired G502 HERO. Private captures stay local."""
import argparse
import hid
import json
import os
import time
from datetime import datetime
from pathlib import Path


class Hidpp:
    def __init__(self):
        info = next(d for d in hid.enumerate(0x046D, 0xC08B)
                    if d["usage_page"] == 0xFF00 and d["usage"] == 2)
        self.device = hid.device()
        self.device.open_path(info["path"])
        self.trace = []

    def call(self, feature, function, args=()):
        request = bytes([0x11, 0xFF, feature, (function << 4) | 0x0D, *args]).ljust(20, b"\0")
        self.device.write(request)
        deadline = time.monotonic() + 2
        while time.monotonic() < deadline:
            response = bytes(self.device.read(64, 200))
            if not response:
                continue
            self.trace.append({"tx": request.hex(), "rx": response.hex()})
            if response[1] != 0xFF:
                continue
            if response[2:4] == request[2:4]:
                return response[4:]
            if response[2] == 0xFF and response[3:5] == request[2:4]:
                raise RuntimeError(f"HID++ error {response.hex()}")
        raise TimeoutError(request.hex())

    def feature(self, feature):
        return self.call(0, 0, feature.to_bytes(2, "big"))[0]


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--profiles", action="store_true")
    args = parser.parse_args()
    p = Hidpp()
    print("Protocol", p.call(0, 1, [0, 0, 0x5A]).hex())
    fs = p.feature(1)
    count = p.call(fs, 0)[0]
    features = {}
    for i in range(1, count + 1):
        r = p.call(fs, 1, [i])
        feature = int.from_bytes(r[:2], "big")
        features[feature] = i
        print(f"Feature {feature:04X}: index={i:02X}, flags/version={r[2:4].hex()}")
    for feature, function, params in [
        (0x2201, 0, []), (0x2201, 1, [0]), (0x2201, 2, [0]),
        (0x8060, 0, []), (0x8060, 1, []),
        (0x8100, 0, []), (0x8100, 2, []),
        (0x8100, 4, []), (0x8100, 11, []),
        (0x8080, 0, []), (0x1B04, 0, []),
    ]:
        if feature in features:
            try:
                print(f"{feature:04X}/{function:X}", p.call(features[feature], function, params).hex())
            except RuntimeError as e:
                print(e)
    root = Path(os.environ["LOCALAPPDATA"]) / "DeviceHub" / "migration" / datetime.now().strftime("probe-%Y%m%d-%H%M%S")
    root.mkdir(parents=True, exist_ok=True)
    if args.profiles and 0x8100 in features:
        # 0x8100 getInfo supplies the sector size. Only read, never startWrite.
        info = p.call(features[0x8100], 0)
        size = int.from_bytes(info[7:9], "big")
        print("Profile memory info", list(info), "sector size", size)
        if size not in (255, 256, 512):
            raise RuntimeError("Unexpected sector size")
        for sector in range(info[6]):
            buf = bytearray(size)
            for offset in range(0, size, 16):
                offset = min(offset, size - 16)
                buf[offset:offset + 16] = p.call(features[0x8100], 5, [
                    sector >> 8, sector & 255, offset >> 8, offset & 255
                ])
            (root / f"g502-sector-{sector}.bin").write_bytes(buf[:size])
            print(f"Sector {sector}:", buf[:64].hex())
    (root / "hidpp-discovery.json").write_text(json.dumps(p.trace, indent=2), encoding="utf-8")
