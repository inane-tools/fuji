"""Tighten a font's letter spacing by taking `units` off every advance.

GPUI has no letter-spacing setting, so the tracking goes into the font.
Usage: track.py <in.ttf> <out.ttf> <units per 1000 em>
"""
import sys

from fontTools.ttLib import TTFont

src, dst, per_mille = sys.argv[1], sys.argv[2], float(sys.argv[3])
font = TTFont(src)
upem = font["head"].unitsPerEm
units = round(upem * per_mille / 1000)
hmtx = font["hmtx"].metrics
changed = 0
for name, (advance, lsb) in hmtx.items():
    if advance > 0:
        hmtx[name] = (max(0, advance - units), lsb)
        changed += 1
font["hhea"].advanceWidthMax = max(a for a, _ in hmtx.values())
font["OS/2"].recalcAvgCharWidth(font)
font.save(dst)
print(f"upem {upem}, -{units} units on {changed} glyphs")
