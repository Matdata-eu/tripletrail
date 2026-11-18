#!/usr/bin/env python3
"""Writes the TripleTrail logo files in this folder (brand/).

The SVGs are written from the geometry below; the wordmark is set in Barlow SemiBold
(SIL OFL 1.1, the copy in brand/fonts with its licence) and turned into outlines, so the files
need no font. PNG and ICO files are rendered from the SVGs by render.mjs (Playwright).

    pip install fonttools brotli uharfbuzz
    python3 brand/build.py && node brand/render.mjs
"""
import io
import re
from pathlib import Path

import uharfbuzz as hb
from fontTools.pens.svgPathPen import SVGPathPen
from fontTools.pens.transformPen import TransformPen
from fontTools.ttLib import TTFont

HERE = Path(__file__).resolve().parent
FONT = HERE / "fonts/barlow-latin-600-normal.woff2"

BLUE, AMBER, INK = "#0563bb", "#f2b300", "#1c2633"
BLUE_ON_DARK, INK_ON_DARK = "#2f8cff", "#edf0f4"

# The mark, on a 64 x 64 grid: three stones that grow along a dotted trail, from the first
# triple to the destination. The large stone is amber, the colour the website uses for routes.
STONES = [(15, 49, 5), (28, 35, 7), (45, 20, 9.5)]
TRAIL = '<path d="M15 49L28 35L45 20" fill="none" stroke="#fff" stroke-width="2.5" stroke-dasharray="2 5" stroke-linecap="round" opacity=".7"/>'
# For 32 px and smaller: no trail, the stones larger and further apart, so they stay apart.
SMALL = [(16, 48, 6), (30, 33, 7.5), (47, 17, 10)]


def stones(spec, colours):
    return "".join(f'<circle cx="{x}" cy="{y}" r="{r}" fill="{c}"/>' for (x, y, r), c in zip(spec, colours))


def tile(spec, trail, x=0, y=0, size=64):
    body = f'<rect width="64" height="64" rx="14" fill="{BLUE}"/>{trail}{stones(spec, ["#fff", "#fff", AMBER])}'
    if (x, y, size) == (0, 0, 64):
        return body
    return f'<g transform="translate({x} {y}) scale({size / 64:g})">{body}</g>'


def svg(w, h, body, title):
    return (f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w:g} {h:g}" width="{w:g}" height="{h:g}" role="img">'
            f"<title>{title}</title>{body}</svg>\n")


def wordmark(text, size):
    """Outlines of `text` at `size` px, kerned by HarfBuzz. Returns (paths per word part, width)."""
    font = TTFont(FONT)
    font.flavor = None  # HarfBuzz reads TrueType, not WOFF2
    raw = io.BytesIO()
    font.save(raw)
    data = raw.getvalue()
    upem = font["head"].unitsPerEm
    glyphs = font.getGlyphSet()
    hbfont = hb.Font(hb.Face(hb.Blob(data)))
    buf = hb.Buffer()
    buf.add_str(text)
    buf.guess_segment_properties()
    hb.shape(hbfont, buf, {"kern": True, "liga": True})
    scale = size / upem
    order = font.getGlyphOrder()
    pens, x = [], 0
    for info, pos in zip(buf.glyph_infos, buf.glyph_positions):
        pen = SVGPathPen(glyphs)
        # Font units have y up; SVG has y down. The baseline is y = 0.
        glyphs[order[info.codepoint]].draw(TransformPen(pen, (scale, 0, 0, -scale, (x + pos.x_offset) * scale, 0)))
        pens.append((info.cluster, pen.getCommands()))
        x += pos.x_advance
    return pens, x * scale


def rounded(d):
    return re.sub(r"-?\d+\.\d+", lambda m: f"{float(m.group()):.2f}".rstrip("0").rstrip("."), d)


def lockup(text_colour, accent, title):
    """The mark with the wordmark beside it: "Triple" in the text colour, "Trail" in blue."""
    size = 46
    pens, width = wordmark("TripleTrail", size)
    split = "TripleTrail".index("Trail")
    first = "".join(d for c, d in pens if c < split)
    second = "".join(d for c, d in pens if c >= split)
    gap, base = 18, 48  # baseline 48 puts the cap height (about 32 px) on the mark's centre
    x0 = 64 + gap
    words = (f'<g transform="translate({x0} {base})">'
             f'<path fill="{text_colour}" d="{rounded(first)}"/><path fill="{accent}" d="{rounded(second)}"/></g>')
    return svg(round(x0 + width + 2), 64, tile(STONES, TRAIL) + words, title)


def main():
    files = {
        "tripletrail-mark.svg": svg(64, 64, tile(STONES, TRAIL), "TripleTrail"),
        "tripletrail-mark-small.svg": svg(64, 64, tile(SMALL, ""), "TripleTrail"),
        # One colour, no tile: for stamps, engraving and single-colour print. Uses the text colour.
        "tripletrail-mark-mono.svg": svg(64, 64, stones(STONES, ["currentColor"] * 3), "TripleTrail"),
        "tripletrail-logo.svg": lockup(INK, BLUE, "TripleTrail"),
        "tripletrail-logo-dark.svg": lockup(INK_ON_DARK, BLUE_ON_DARK, "TripleTrail"),
    }
    for name, text in files.items():
        (HERE / name).write_text(text)
        print(f"{name}: {len(text)} bytes")


if __name__ == "__main__":
    main()
