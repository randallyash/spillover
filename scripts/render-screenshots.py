#!/usr/bin/env python3
"""Paint dump_screens JSON with Caskaydia Mono into the README PNGs."""

from __future__ import annotations

import json
import sys
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

FONT = "/usr/share/fonts/TTF/CaskaydiaMonoNerdFont-Regular.ttf"
FONT_BOLD = "/usr/share/fonts/TTF/CaskaydiaMonoNerdFont-Bold.ttf"
FONT_ITALIC = "/usr/share/fonts/TTF/CaskaydiaMonoNerdFont-Italic.ttf"
# Caskaydia has no shape for U+2717 (✗), the mark a spilled-past tier wears.
FALLBACK = "/usr/share/fonts/noto/NotoSansSymbols2-Regular.ttf"

PALETTE = {
    None: None,
    "black": (17, 17, 27),
    "red": (243, 139, 168),
    "green": (166, 227, 161),
    "yellow": (249, 226, 175),
    "blue": (137, 180, 250),
    "magenta": (203, 166, 247),
    "cyan": (137, 220, 235),
    "gray": (166, 173, 200),
    "darkgray": (108, 112, 134),
    "lightred": (243, 139, 168),
    "lightgreen": (166, 227, 161),
    "lightyellow": (249, 226, 175),
    "lightblue": (137, 180, 250),
    "lightmagenta": (203, 166, 247),
    "lightcyan": (148, 226, 213),
    "white": (205, 214, 244),
}

BG = (30, 30, 46)
FG = (205, 214, 244)
CHROME = (17, 17, 27)
CHROME_FG = (108, 112, 134)
SCALE = 2
CELL_W = 9
CELL_H = 18
PAD_X = 14
PAD_Y = 12
CHROME_H = 36
TITLES = {
    "hero": "spill — a cheap model stalls, and the turn moves on",
    "commands": "spill — type / for the chain commands",
    "plan": "spill — plan mode, so nothing writes",
    "approval": "spill — a write waits for y or n",
}


def parse_colour(value: str | None) -> tuple[int, int, int] | None:
    if value is None:
        return None
    if value.startswith("#") and len(value) == 7:
        return tuple(int(value[i : i + 2], 16) for i in (1, 3, 5))
    return PALETTE.get(value)


def font(size: int, *, bold: bool = False, italic: bool = False) -> ImageFont.FreeTypeFont:
    path = FONT_BOLD if bold else FONT_ITALIC if italic else FONT
    return ImageFont.truetype(path, size)


def missing_glyph(face: ImageFont.FreeTypeFont, ch: str) -> bool:
    return list(face.getmask(ch)) == list(face.getmask("\uFFFF"))


def round_rect(draw: ImageDraw.ImageDraw, box, radius: int, fill) -> None:
    draw.rounded_rectangle(box, radius=radius, fill=fill)


def render(name: str, frame: dict, dest: Path) -> None:
    rows = frame["rows"]
    cols = frame["width"]
    height = frame["height"]
    term_w = cols * CELL_W + PAD_X * 2
    term_h = height * CELL_H + PAD_Y * 2
    win_w = term_w
    win_h = CHROME_H + term_h
    outer = 24
    img_w = (win_w + outer * 2) * SCALE
    img_h = (win_h + outer * 2) * SCALE

    im = Image.new("RGB", (img_w, img_h), (12, 12, 16))
    draw = ImageDraw.Draw(im)

    ox = outer * SCALE
    oy = outer * SCALE
    ww = win_w * SCALE
    wh = win_h * SCALE
    round_rect(draw, (ox, oy, ox + ww, oy + wh), 18, CHROME)

    # traffic lights
    r = 5 * SCALE
    cy = oy + (CHROME_H * SCALE) // 2
    for i, colour in enumerate(((255, 95, 87), (254, 188, 46), (40, 200, 64))):
        cx = ox + 18 * SCALE + i * 16 * SCALE
        draw.ellipse((cx - r, cy - r, cx + r, cy + r), fill=colour)

    title = TITLES.get(name, "spill")
    tf = font(11 * SCALE)
    bbox = tf.getbbox(title)
    tw = bbox[2] - bbox[0]
    draw.text(
        (ox + (ww - tw) // 2, oy + 10 * SCALE),
        title,
        fill=CHROME_FG,
        font=tf,
    )

    term_x = ox
    term_y = oy + CHROME_H * SCALE
    draw.rectangle(
        (term_x, term_y, term_x + ww, oy + wh),
        fill=BG,
    )
    # clip the bottom corners by redrawing a rounded hole? skip — chrome already round

    regular = font(13 * SCALE)
    bold = font(13 * SCALE, bold=True)
    italic = font(13 * SCALE, italic=True)
    fallback = ImageFont.truetype(FALLBACK, 13 * SCALE)

    origin_x = term_x + PAD_X * SCALE
    origin_y = term_y + PAD_Y * SCALE
    cw = CELL_W * SCALE
    ch = CELL_H * SCALE

    for y, row in enumerate(rows):
        for x, cell in enumerate(row):
            glyph = cell.get("s") or " "
            fg = parse_colour(cell.get("fg")) or FG
            bg = parse_colour(cell.get("bg"))
            x0 = origin_x + x * cw
            y0 = origin_y + y * ch
            if bg:
                draw.rectangle((x0, y0, x0 + cw, y0 + ch), fill=bg)
            face = bold if cell.get("b") else italic if cell.get("i") else regular
            if glyph != " " and missing_glyph(face, glyph):
                face = fallback
            if cell.get("d"):
                fg = tuple(int(c * 0.65) for c in fg)
            draw.text((x0, y0 + SCALE), glyph, fill=fg, font=face)

    im.save(dest, "PNG", optimize=True)
    print(f"wrote {dest} {im.size}")


def main() -> int:
    src = Path(sys.argv[1] if len(sys.argv) > 1 else "/tmp/spill-shots")
    dest = Path(sys.argv[2] if len(sys.argv) > 2 else "assets")
    dest.mkdir(parents=True, exist_ok=True)
    for json_path in sorted(src.glob("*.json")):
        frame = json.loads(json_path.read_text())
        render(json_path.stem, frame, dest / f"{json_path.stem}.png")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
