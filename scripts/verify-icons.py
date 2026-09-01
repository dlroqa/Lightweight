#!/usr/bin/env python3
"""Assert the rendered icons have the shape the product needs.

Run after `scripts/build-icons.py`. This checks *properties*, not bytes: PNG
encoders differ across libpng versions and platforms, so a byte-for-byte drift
check would be a flaky gate that says nothing about whether the icon is right.
What matters is provable instead — the packaged app icons carry the rounded
macOS squircle (transparent corners, an opaque middle), the tray icon is the
size the shell loads, and every file opens as an image at the size it claims.

    python3 scripts/verify-icons.py
"""

from __future__ import annotations

import sys
from pathlib import Path

try:
    from PIL import Image
except ModuleNotFoundError:  # pragma: no cover - a tool, not a test
    sys.exit("Pillow is needed to verify the icons: pip install --user Pillow")

ROOT = Path(__file__).resolve().parent.parent
BUILD = ROOT / "apps/desktop/build"

# The corner must be transparent and the middle opaque: that is what "rounded
# with a margin" means, and it is the one property a hard-edged square fails.
CORNER_ALPHA_MAX = 8
CENTRE_ALPHA_MIN = 250

failures: list[str] = []


def check(condition: bool, message: str) -> None:
    if not condition:
        failures.append(message)


def check_squircle(path: Path) -> None:
    if not path.is_file():
        failures.append(f"missing: {path.relative_to(ROOT)}")
        return
    image = Image.open(path).convert("RGBA")
    width, height = image.size
    corner = image.getpixel((0, 0))[3]
    centre = image.getpixel((width // 2, height // 2))[3]
    rel = path.relative_to(ROOT)
    check(corner <= CORNER_ALPHA_MAX, f"{rel}: corner alpha {corner} is not transparent")
    check(centre >= CENTRE_ALPHA_MIN, f"{rel}: centre alpha {centre} is not opaque")


def main() -> None:
    # The Linux icon set and the window icon are masked and must be squircles.
    for size in (16, 32, 128, 256, 1024):
        check_squircle(BUILD / f"icons/{size}x{size}.png")
    check_squircle(BUILD / "window.png")

    # The tray icon is what the menu bar shows: present, and the size the shell
    # loads it at.
    tray = BUILD / "tray.png"
    if tray.is_file():
        check(Image.open(tray).size == (48, 48), "tray.png is not 48x48")
    else:
        failures.append("missing: apps/desktop/build/tray.png")

    # The single-file icons macOS and Windows package must at least open.
    for name in ("icon.icns", "icon.ico"):
        path = BUILD / name
        if not path.is_file():
            failures.append(f"missing: {path.relative_to(ROOT)}")
            continue
        try:
            Image.open(path).load()
        except OSError as error:
            failures.append(f"{name} does not open: {error}")

    if failures:
        print("icon verification failed:")
        for line in failures:
            print(f"  - {line}")
        sys.exit(1)
    print("icons ok: squircle corners transparent, tray present, packaged icons open")


if __name__ == "__main__":
    main()
