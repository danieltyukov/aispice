"""Draws every brand file from one description of the mark.

    python3 assets/brand/tools/build_fonts.py ... --static-dir /tmp/aispice-draw
    python3 assets/brand/tools/build_brand.py --fonts /tmp/aispice-draw

Needs inkscape (to turn text into outlines, so no file depends on a font being
installed), rsvg-convert (to rasterise) and fontTools. The fonts directory holds
the static "Draw" instances build_fonts.py writes; a private fontconfig file
points Inkscape at them, so nothing is installed system-wide.

Writes:
    assets/brand/mark.svg            the tile, 32-unit grid
    assets/brand/mark-mono.svg       the trace alone in currentColor, for inline use
    assets/brand/wordmark.svg        "aispice" in aispice Sans SemiBold, as outlines
    assets/brand/lockup.svg          mark and wordmark, for light grounds
    assets/brand/lockup-dark.svg     the same for dark grounds
    assets/brand/og.svg, og.png      1200x630 social card
    assets/brand/mark-1024.png       input for `tauri icon`
    app/public/favicon.svg           the tile, for browser tabs
    app/public/apple-touch-icon.png  180 px, square (iOS rounds the corners itself)
    app/src-tauri/icons/             every desktop size, .ico and .icns, through
                                     `tauri icon` (needs `npm install` at the root)

`tauri icon` downscales the 1024 px PNG for every size. For the sizes a Windows
taskbar and title bar use, the .ico and 32x32.png are then redrawn straight from
the vector, which keeps the flat runs of the trace on whole pixels at 16 px. The
Android and iOS sets it also writes are removed: there is no mobile build.
"""

import argparse
import math
import os
import re
import shutil
import subprocess
import tempfile
from pathlib import Path

from fontTools.pens.boundsPen import BoundsPen
from fontTools.pens.svgPathPen import SVGPathPen
from fontTools.pens.transformPen import TransformPen
from fontTools.svgLib.path import parse_path

REPO = Path(__file__).resolve().parents[3]
BRAND = REPO / "assets" / "brand"
PUBLIC = REPO / "app" / "public"

# The mark, on a 32-unit grid. A unit step response: flat from the probe dot,
# a rise, one overshoot, a damped ring, and the settled level. The baseline
# (y 21) and the settled level (y 11) sit on pixel centres at 16 px, so the two
# flat runs render as one crisp row each in a favicon.
TRACE = (
    "M6 21H10.2C12.4 21 12.6 7.8 15.2 7.8C17.6 7.8 17.5 13.6 19.8 13.6"
    "C21.7 13.6 21.8 10.1 23.5 10.1C24.9 10.1 25.3 11 26.5 11"
)
DOT = (6, 21, 2.25)
STROKE = 2.4
TILE_RX = 7.5
TEAL = "#0d7a68"

# Mirrors of tokens.css, for files that cannot read CSS variables.
INK = "#17181a"
INK_DARK = "#e9e7e2"
MUTED = "#5e6065"
PAPER = "#f8f7f4"

# The wordmark is set at 100 units per em: Instrument Sans has 1000 units per
# em, so every outline coordinate lands on a tenth of a unit. Tracking -2.5 %.
WORD_SIZE = 100
WORD_TRACKING = -2.5
X_HEIGHT = 51  # Instrument Sans: 510 of 1000


def fmt(v: float) -> str:
    s = f"{v:.2f}".rstrip("0").rstrip(".")
    return "0" if s in ("-0", "") else s


def trace_elements(color: str = "#fff") -> str:
    cx, cy, r = DOT
    return (
        f'<path d="{TRACE}" fill="none" stroke="{color}" stroke-width="{STROKE}" '
        f'stroke-linecap="round" stroke-linejoin="round"/>'
        f'<circle cx="{fmt(cx)}" cy="{fmt(cy)}" r="{fmt(r)}" fill="{color}"/>'
    )


def tile(rx: float = TILE_RX) -> str:
    return f'<rect width="32" height="32" rx="{fmt(rx)}" fill="{TEAL}"/>' + trace_elements()


def outline(fonts_conf: Path, text: str, family: str, size: float, tracking: float = 0) -> tuple[str, tuple]:
    """Text as one path, baseline at y=0, origin at x=0. Returns (d, bounds)."""
    with tempfile.TemporaryDirectory() as tmp:
        src, dst = Path(tmp) / "in.svg", Path(tmp) / "out.svg"
        src.write_text(
            '<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10">'
            f'<text x="0" y="0" font-family="{family}" font-size="{size}" '
            f'letter-spacing="{tracking}">{text}</text></svg>',
            encoding="utf-8",
        )
        subprocess.run(
            ["inkscape", str(src), "--export-text-to-path", "--export-plain-svg",
             "--export-type=svg", f"--export-filename={dst}"],
            check=True, capture_output=True, env={**os.environ, "FONTCONFIG_FILE": str(fonts_conf)},
        )
        out = dst.read_text(encoding="utf-8")
    ds = re.findall(r'\sd="([^"]+)"', out)
    if not ds:
        raise SystemExit(f"inkscape produced no outline for {text!r}; is {family!r} in the fonts directory?")
    return bake(" ".join(ds), 0, 0)


def bake(d: str, dx: float, dy: float) -> tuple[str, tuple]:
    """Translate a path, round it to hundredths, and measure its exact bounds."""
    svg = SVGPathPen(None, ntos=fmt)
    bounds = BoundsPen(None)

    class Tee:
        def __getattr__(self, name):
            return lambda *a: (getattr(svg, name)(*a), getattr(bounds, name)(*a))

    parse_path(d, TransformPen(Tee(), (1, 0, 0, 1, dx, dy)))
    return svg.getCommands(), bounds.bounds


def write(path: Path, svg: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(svg.strip() + "\n", encoding="utf-8")
    if path.is_relative_to(REPO):
        print(f"  {path.relative_to(REPO)}")


def png(svg: Path, out: Path, w: int, h: int, fonts_conf: Path) -> None:
    subprocess.run(["rsvg-convert", "-w", str(w), "-h", str(h), str(svg), "-o", str(out)],
                   check=True, env={**os.environ, "FONTCONFIG_FILE": str(fonts_conf)})
    subprocess.run(["convert", str(out), "-strip", "-define", "png:compression-level=9", str(out)], check=True)
    print(f"  {out.relative_to(REPO)} ({out.stat().st_size // 1024} KiB)")


def step_response(x0: float, y0: float, w: float, h: float, zeta: float = 0.28, cycles: float = 4.2) -> str:
    """A second-order step response as a polyline: flat lead-in, then the response."""
    lead = 0.12 * w
    pts = [(x0, y0 + h)]
    wd = 2 * math.pi * cycles / (w - lead)
    wn = wd / math.sqrt(1 - zeta * zeta)
    phi = math.acos(zeta)
    for i in range(0, 241):
        t = (w - lead) * i / 240
        y = 1 - math.exp(-zeta * wn * t) / math.sqrt(1 - zeta * zeta) * math.sin(wd * t + phi)
        pts.append((x0 + lead + t, y0 + h - y * h * 0.72))
    return "M" + " L".join(f"{fmt(x)} {fmt(y)}" for x, y in pts)


def main() -> None:
    p = argparse.ArgumentParser()
    p.add_argument("--fonts", required=True, type=Path, help="directory with the static Draw instances")
    a = p.parse_args()

    with tempfile.TemporaryDirectory() as tmp:
        conf = Path(tmp) / "fonts.conf"
        conf.write_text(
            '<?xml version="1.0"?><!DOCTYPE fontconfig SYSTEM "fonts.dtd"><fontconfig>'
            '<include ignore_missing="yes">/etc/fonts/fonts.conf</include>'
            f"<dir>{a.fonts.resolve()}</dir><cachedir>{tmp}/cache</cachedir></fontconfig>",
            encoding="utf-8",
        )
        build(conf, Path(tmp))


def build(conf: Path, tmp: Path) -> None:
    print("mark")
    mark = (
        '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32" width="32" height="32">'
        f"<title>aispice</title>{tile()}</svg>"
    )
    write(BRAND / "mark.svg", mark)
    write(PUBLIC / "favicon.svg", mark.replace("<title>aispice</title>", ""))

    # The trace's ink, stroke included, is x 3.75 to 27.7 and y 6.6 to 23.25.
    write(BRAND / "mark-mono.svg",
          '<svg xmlns="http://www.w3.org/2000/svg" viewBox="3.5 6.4 24.5 17.1" width="24.5" height="17.1">'
          f'<title>aispice</title>{trace_elements("currentColor")}</svg>')

    print("wordmark")
    word, (x0, y0, x1, y1) = outline(conf, "aispice", "aispice Sans Draw SemiBold", WORD_SIZE, WORD_TRACKING)
    ww, wh = x1 - x0, y1 - y0
    word, _ = bake(word, -x0, -y0)  # ink box at the origin
    baseline = -y0  # where the baseline now sits inside the wordmark box
    write(BRAND / "wordmark.svg",
          f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {fmt(ww)} {fmt(wh)}" '
          f'width="{fmt(ww)}" height="{fmt(wh)}"><title>aispice</title>'
          f'<path d="{word}" fill="{INK}"/></svg>')

    # Lockup: a one-em tile centred on the x-height, 0.28 em of air, then the
    # wordmark. The tile spans the wordmark's ascender to descender almost
    # exactly, so the pair reads as one block.
    t = WORD_SIZE
    gap = 0.28 * WORD_SIZE
    lock_baseline = t / 2 + X_HEIGHT / 2
    word_top = lock_baseline - baseline
    lw = t + gap + ww
    lh = max(t, word_top + wh) - min(0, word_top)

    def lockup(word_fill: str) -> str:
        return (
            f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {fmt(lw)} {fmt(lh)}" '
            f'width="{fmt(lw)}" height="{fmt(lh)}"><title>aispice</title>'
            f'<g transform="scale({t / 32:g})">{tile()}</g>'
            f'<path transform="translate({fmt(t + gap)} {fmt(word_top)})" d="{word}" fill="{word_fill}"/></svg>'
        )

    print("lockups")
    write(BRAND / "lockup.svg", lockup(INK))
    write(BRAND / "lockup-dark.svg", lockup(INK_DARK))

    print("social card")
    lines = ["An agent that edits circuit schematics,", "simulates them and checks the results", "against your specs."]
    size, leading = 54, 68
    sentence = []
    for i, line in enumerate(lines):
        d, _ = outline(conf, line, "aispice Sans Draw Medium", size, -0.6)
        sentence.append(f'<path transform="translate(80 {340 + i * leading})" d="{d}"/>')
    url, _ = outline(conf, "danieltyukov.github.io/aispice", "aispice Mono Draw", 26)

    # Motif: an oscilloscope graticule with a damped step response, in the
    # accent at low contrast, top right where the lockup leaves the card empty.
    gx, gy, gw, gh, div = 680, 72, 440, 176, 44
    grid = []
    for i in range(0, gw // div + 1):
        x = gx + i * div
        grid.append(f"M{x} {gy}V{gy + gh}")
    for j in range(0, gh // div + 1):
        y = gy + j * div
        grid.append(f"M{gx} {y}H{gx + gw}")
    wave = step_response(gx + 6, gy + 30, gw - 12, gh - 48)
    og_scale = 0.8
    og = f"""
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1200 630" width="1200" height="630">
<title>aispice: an agent that edits circuit schematics, simulates them and checks the results against your specs</title>
<rect width="1200" height="630" fill="{PAPER}"/>
<path d="{''.join(grid)}" fill="none" stroke="{TEAL}" stroke-opacity="0.14" stroke-width="1.5"/>
<path d="{wave}" fill="none" stroke="{TEAL}" stroke-opacity="0.34" stroke-width="4" stroke-linecap="round" stroke-linejoin="round"/>
<circle cx="{gx + 6}" cy="{gy + gh - 18}" r="7" fill="{TEAL}" fill-opacity="0.34"/>
<g transform="translate(80 {gy}) scale({og_scale})">
<g transform="scale({t / 32:g})">{tile()}</g>
<path transform="translate({fmt(t + gap)} {fmt(word_top)})" d="{word}" fill="{INK}"/>
</g>
<g fill="{INK}">{''.join(sentence)}</g>
<path transform="translate(80 556)" d="{url}" fill="{TEAL}"/>
</svg>"""
    write(BRAND / "og.svg", og)
    png(BRAND / "og.svg", BRAND / "og.png", 1200, 630, conf)

    print("icons")
    write(tmp / "touch.svg",
          f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32">{tile(rx=0)}</svg>')
    png(tmp / "touch.svg", PUBLIC / "apple-touch-icon.png", 180, 180, conf)
    png(BRAND / "mark.svg", BRAND / "mark-1024.png", 1024, 1024, conf)
    tauri_icons(conf, tmp)


def tauri_icons(conf: Path, tmp: Path) -> None:
    icons = REPO / "app" / "src-tauri" / "icons"
    subprocess.run(["npx", "tauri", "icon", str(BRAND / "mark-1024.png"), "-o", str(icons)],
                   check=True, cwd=REPO / "app", capture_output=True)
    for mobile in ("android", "ios"):
        shutil.rmtree(icons / mobile, ignore_errors=True)
    sizes = [16, 24, 32, 48, 64, 256]
    frames = []
    for size in sizes:
        out = tmp / f"ico-{size}.png"
        subprocess.run(["rsvg-convert", "-w", str(size), "-h", str(size), str(BRAND / "mark.svg"), "-o", str(out)],
                       check=True)
        frames.append(str(out))
    subprocess.run(["convert", *frames, str(icons / "icon.ico")], check=True)
    shutil.copyfile(tmp / "ico-32.png", icons / "32x32.png")
    print(f"  {icons.relative_to(REPO)}/ (tauri icon, then .ico and 32x32.png from the vector)")


if __name__ == "__main__":
    main()
