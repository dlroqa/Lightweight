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
    from PIL import Image, ImageChops, ImageDraw, ImageFilter
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


# The tray artwork, kept apart from the brand plate above.
#
# A menu-bar icon wants the mark on transparency, not on a plate, and the reason
# the plate icons ship *with* their plate — that keying the old teal feather out
# of navy deleted half the mark — does not apply to this one: it is a saturated
# feather on pure white, which keys cleanly. So the tray gets its own source and
# its own path through this script.
TRAY_SOURCE = ROOT / "icon" / "tray-source.png"

# How much of the macOS app-icon canvas the rounded body fills. Apple's grid
# puts an 824px body in a 1024px canvas — a hair over 80% — with the rest a
# transparent margin the dock and Launchpad expect to see.
MACOS_BODY = 0.805

# The superellipse exponent that matches the macOS "squircle". Two gives an
# ellipse and infinity a square; the platform corner sits around five.
SQUIRCLE_N = 5.0


def superellipse_mask(side: int, n: float = SQUIRCLE_N, supersample: int = 4) -> Image.Image:
    """An anti-aliased squircle alpha mask, `side` x `side`.

    The macOS corner is a superellipse, not a circular round-rect, and matching
    it is what makes the icon sit on the dock exactly like every other app
    rather than almost like them. Pillow has no superellipse primitive, so the
    boundary is sampled parametrically, filled as a polygon at `supersample`x,
    and box-averaged down — which is where the smooth edge comes from.
    """
    import math

    high = side * supersample
    half = high / 2.0
    steps = 720
    points = []
    for index in range(steps):
        angle = 2.0 * math.pi * index / steps
        cos_a, sin_a = math.cos(angle), math.sin(angle)
        # |cos|^(2/n)·sign(cos), scaled from the centre to the half-extent.
        x = half + math.copysign(abs(cos_a) ** (2.0 / n), cos_a) * half
        y = half + math.copysign(abs(sin_a) ** (2.0 / n), sin_a) * half
        points.append((x, y))
    big = Image.new("L", (high, high), 0)
    ImageDraw.Draw(big).polygon(points, fill=255)
    return big.resize((side, side), Image.LANCZOS)


def macos_masked(icon: Image.Image, size: int) -> Image.Image:
    """`icon` re-cut to the macOS app-icon shape at `size` x `size`.

    The plate is scaled to Apple's content square, centred on a transparent
    canvas, and its corners taken off by the squircle mask — so the packaged
    icon carries its own rounded corners and margin. macOS, unlike iOS, does not
    add them for an app, which is why a full-bleed square reads as a hard-edged
    tile next to everything else on the dock.
    """
    body = round(size * MACOS_BODY)
    plate = icon.resize((body, body), Image.LANCZOS).convert("RGBA")
    # Fold the squircle into the plate's own alpha rather than overwriting it,
    # so a plate that ever carried transparency of its own keeps it.
    combined = ImageChops.multiply(plate.getchannel("A"), superellipse_mask(body))
    plate.putalpha(combined)
    canvas = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    edge = (size - body) // 2
    canvas.alpha_composite(plate, (edge, edge))
    return canvas


def tray_icon(fallback: Image.Image) -> Image.Image:
    """The menu-bar icon: the feather on transparency.

    Two source shapes are accepted. A source that already carries an alpha
    channel — a feather drawn on transparency — is used as it is. A fully opaque
    source (the feather on a white field) is keyed by distance from white:
    `255 - min(r, g, b)` per pixel, so the white goes transparent, the saturated
    feather stays opaque, and its soft wisps keep a soft edge. Either way the
    mark is cropped, centred in a square with a little padding, and returned at
    48px RGBA — the size the shell loads.

    When `icon/tray-source.png` is absent the plated brand mark is used instead,
    so the build still produces a tray icon; drop the feather in and re-run to
    get the intended one.
    """
    if not TRAY_SOURCE.is_file():
        print(f"note: {TRAY_SOURCE.relative_to(ROOT)} is absent; tray uses the plated brand mark")
        return fallback.resize((48, 48), Image.LANCZOS)
    art = Image.open(TRAY_SOURCE).convert("RGBA")
    lowest_alpha, _ = art.getchannel("A").getextrema()
    if lowest_alpha == 255:
        # Opaque source: the background is a white field to key out. (A source
        # that already has transparency keeps the alpha it came with; keying it
        # from white would paint its transparent, RGB-zero background solid.)
        red, green, blue = art.convert("RGB").split()
        lowest = ImageChops.darker(ImageChops.darker(red, green), blue)
        art.putalpha(ImageChops.invert(lowest))
    box = art.getchannel("A").getbbox()
    if box is None:
        print("note: the tray source keyed to nothing; tray uses the plated brand mark")
        return fallback.resize((48, 48), Image.LANCZOS)
    mark = art.crop(box)
    # A little air around the feather so it is not wall-to-wall in the bar.
    side = round(max(mark.size) / 0.82)
    square = Image.new("RGBA", (side, side), (0, 0, 0, 0))
    square.alpha_composite(mark, ((side - mark.width) // 2, (side - mark.height) // 2))
    return square.resize((48, 48), Image.LANCZOS)


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
    # The packaging icons carry the rounded macOS squircle and its transparent
    # margin; built once at full size and resized down, so every size shares one
    # shape. The web assets below stay full-bleed on `icon`, because a browser
    # favicon and the iOS home-screen icon are masked by their own host.
    masked = macos_masked(icon, 1024)

    # The desktop shell's packaged icon, as a *set* rather than one large PNG.
    # electron-builder reads a directory of `<n>x<n>.png` files as an icon set
    # and installs each size where the desktop looks for it, so a 24px taskbar
    # icon is a 24px render and not a 1024px one resampled by whoever draws it.
    # macOS and Windows take the largest of the set and convert it, so 1024 is
    # here and nothing is ever upscaled.
    icons = ROOT / "apps/desktop/build/icons"
    for size in (16, 24, 32, 48, 64, 128, 256, 512, 1024):
        save(masked, icons / f"{size}x{size}.png", size)
    # The two the shell loads at run time. `scripts-build.mjs` copies them into
    # `dist/`, which is the directory electron-builder packages - `build/` is
    # electron-builder's own resources directory and is not shipped inside the
    # app, so an icon read from there would be present in a checkout and
    # missing from the AppImage.
    #
    # The tray gets the mark on transparency from its own source; the menu bar
    # wants a glyph, not a plated tile.
    save(tray_icon(icon), ROOT / "apps/desktop/build/tray.png")
    # The window, which is also what a Linux taskbar and an alt-tab switcher
    # show. 256 is the largest either asks for.
    save(masked, ROOT / "apps/desktop/build/window.png", 256)

    # macOS and Windows want a single multi-resolution file rather than the
    # directory of PNGs the Linux target reads. They are generated here and
    # committed like every other icon, so no build step needs Pillow - the
    # dependency policy keeps tool-time requirements out of the build.
    desktop_build = ROOT / "apps/desktop/build"
    masked.save(
        desktop_build / "icon.icns",
        sizes=[(16, 16), (32, 32), (64, 64), (128, 128), (256, 256), (512, 512), (1024, 1024)],
    )
    print("apps/desktop/build/icon.icns  16-1024")
    # `.ico` tops out at 256; Windows scales that for anything larger.
    masked.resize((256, 256), Image.LANCZOS).save(
        desktop_build / "icon.ico",
        sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)],
    )
    print("apps/desktop/build/icon.ico   16-256")

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
