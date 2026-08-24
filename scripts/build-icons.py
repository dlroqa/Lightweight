#!/usr/bin/env python3
"""Derive every icon this product ships from the one artwork in `icon/`.

The artwork is a 2000x2000 render with the mark small and high in a large
field of background. That framing is right for a poster and wrong for an app
icon: at 32px in a tray or a tab, a mark occupying a third of the canvas is a
coloured square with a smudge in it. So the mark's bounding box is measured
here and the canvas is re-cut around it, rather than a fixed crop being
guessed at once and then quietly drifting when the artwork is redrawn.

Nothing at build or run time depends on this script - it writes files that are
committed. It exists so that replacing `icon/source.png` and re-running is the
whole of "change the icon", and so the numbers behind each size are written
down somewhere other than an image editor's history.

Requires Pillow, which is a tool-time dependency and not one of the product's.

    python3 scripts/build-icons.py
"""

from __future__ import annotations

import sys
from pathlib import Path

try:
    from PIL import Image, ImageChops, ImageFilter
except ModuleNotFoundError:  # pragma: no cover - a tool, not a test
    sys.exit("Pillow is needed to rebuild the icons: pip install --user Pillow")

ROOT = Path(__file__).resolve().parent.parent
SOURCE = ROOT / "icon" / "source.png"

# The share of the icon's width the mark should span. Platform icon guidance
# lands between roughly 0.75 and 0.82 for a mark on its own plate; below that
# the icon reads as empty, above it the mark collides with the rounded mask
# macOS and Android apply.
FILL = 0.78

# How far a pixel must sit from the background before it counts as artwork.
# The render carries film grain, so a low threshold measures the noise instead
# of the feather. A median filter removes single-pixel speckle first, so this
# only has to clear the grain that survives it.
INK = 30


def background(image: Image.Image) -> tuple[int, int, int]:
    """The plate colour, read from the corners rather than assumed.

    Every corner is background by construction, and the median of the four
    tolerates one that a stray glow has reached.
    """
    corners = [
        image.getpixel((0, 0)),
        image.getpixel((image.width - 1, 0)),
        image.getpixel((0, image.height - 1)),
        image.getpixel((image.width - 1, image.height - 1)),
    ]
    return tuple(sorted(channel)[1] for channel in zip(*corners))  # type: ignore[return-value]


def ink_mask(image: Image.Image, plate: tuple[int, int, int]) -> Image.Image:
    """A greyscale map of how far each pixel is from the plate."""
    plain = Image.new("RGB", image.size, plate)
    return ImageChops.difference(image, plain).convert("L")


def mark_box(image: Image.Image, plate: tuple[int, int, int]) -> tuple[int, int, int, int]:
    """The mark's bounding box, grain excluded."""
    mask = ink_mask(image, plate).filter(ImageFilter.MedianFilter(5))
    box = mask.point(lambda value: 255 if value > INK else 0).getbbox()
    if box is None:
        sys.exit(f"{SOURCE} looks like a single flat colour: no mark to find")
    return box


def plate_icon(image: Image.Image, plate: tuple[int, int, int]) -> Image.Image:
    """The artwork re-cut so the mark fills `FILL` of a square canvas.

    The crop is taken from the source rather than the mark being lifted out and
    re-composited: the plate carries a vignette and a grain that are part of the
    artwork, and pasting the mark onto a flat fill would throw both away.
    """
    left, top, right, bottom = mark_box(image, plate)
    side = round(max(right - left, bottom - top) / FILL)
    centre_x, centre_y = (left + right) / 2, (top + bottom) / 2
    box = (
        round(centre_x - side / 2),
        round(centre_y - side / 2),
        round(centre_x + side / 2),
        round(centre_y + side / 2),
    )

    # A crop that runs off the source would be filled with black by Pillow, and
    # a black band down one edge of an app icon is worse than a tighter mark.
    # Slide the window back inside instead, and only then give up.
    if side > min(image.size):
        sys.exit("the mark is too large in frame to cut a plate around it")
    dx = max(0, -box[0]) - max(0, box[2] - image.width)
    dy = max(0, -box[1]) - max(0, box[3] - image.height)
    box = (box[0] + dx, box[1] + dy, box[2] + dx, box[3] + dy)

    return image.crop(box)


# There is deliberately no "mark on transparency" output here.
#
# It was tried: alpha taken from each pixel's distance to the plate. The
# feather's vane is a dark teal that sits about as far from the plate as the
# grain does, so keying it out deletes half the mark and leaves a quill and a
# bolt. This artwork is drawn *on* its plate, so everything below ships the
# plate with it - which is also why the panel's rail chip is the app icon
# itself rather than a recolourable glyph.


def save(image: Image.Image, path: Path, size: int | None = None) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    out = image if size is None else image.resize((size, size), Image.LANCZOS)
    out.save(path)
    print(f"{path.relative_to(ROOT)}  {out.width}x{out.height}")


def main() -> None:
    if not SOURCE.is_file():
        sys.exit(f"the artwork is missing: {SOURCE}")

    source = Image.open(SOURCE).convert("RGB")
    plate = background(source)
    icon = plate_icon(source, plate)

    # The desktop shell's packaged icon, as a *set* rather than one large PNG.
    # electron-builder reads a directory of `<n>x<n>.png` files as an icon set
    # and installs each size where the desktop looks for it, so a 24px taskbar
    # icon is a 24px render and not a 1024px one resampled by whoever draws it.
    # macOS and Windows take the largest of the set and convert it, so 1024 is
    # here and nothing is ever upscaled.
    icons = ROOT / "apps/desktop/build/icons"
    for size in (16, 24, 32, 48, 64, 128, 256, 512, 1024):
        save(icon, icons / f"{size}x{size}.png", size)
    # The two the shell loads at run time. `scripts-build.mjs` copies them into
    # `dist/`, which is the directory electron-builder packages - `build/` is
    # electron-builder's own resources directory and is not shipped inside the
    # app, so an icon read from there would be present in a checkout and
    # missing from the AppImage.
    #
    # The tray is drawn at 16-24px on Linux and Windows; at 2x so a HiDPI
    # display has real pixels to use.
    save(icon, ROOT / "apps/desktop/build/tray.png", 48)
    # The window, which is also what a Linux taskbar and an alt-tab switcher
    # show. 256 is the largest either asks for.
    save(icon, ROOT / "apps/desktop/build/window.png", 256)

    # The panel. `public/` is copied to the web root verbatim, so these keep
    # stable names - unlike the bundler's hashed assets, a favicon is asked for
    # by a fixed path. `icon.png` is both the tab icon and the mark the rail
    # draws, so a browser fetches it once and uses it twice.
    public = ROOT / "frontend/public"
    save(icon, public / "icon.png", 256)
    save(icon, public / "apple-touch-icon.png", 180)
    # Windows and older browsers still ask for `/favicon.ico` by reflex, and an
    # answer of "no such file" costs a 404 on every page load. Only the sizes an
    # `.ico` is actually asked for: a 256px frame in here would double the file
    # for a size `icon.png` already serves better.
    icon.resize((64, 64), Image.LANCZOS).save(
        public / "favicon.ico", sizes=[(16, 16), (32, 32), (48, 48)]
    )
    print("frontend/public/favicon.ico  16,32,48")


if __name__ == "__main__":
    main()
