#!/usr/bin/env python3
"""Turn the "TrajFS" wordmark of docs/trajfs-logo.png into a coverage grid for the terminal demo.

The demo draws the wordmark with quadrant block cells (each terminal cell is 2x2 subpixels), so the
serif shape and the logo blue survive in a terminal. This script needs Pillow and is
run once; record.py only reads the text file it writes (demo/wordmark.txt: one row per subpixel row,
one hex digit 0-f per column giving the ink coverage).

    venv/bin/python demo/make_wordmark.py --cols 80
"""

import argparse
import os

from PIL import Image

HERE = os.path.dirname(os.path.abspath(__file__))
LOGO = os.path.join(os.path.dirname(HERE), "docs", "trajfs-logo.png")
BOX = (40, 205, 700, 455)  # the "TrajFS" text in the 1298x578 logo


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--cols", type=int, default=80, help="width in subpixel columns")
    ap.add_argument("--aspect", type=float, default=2.0,
                    help="subpixel width:height as 1:aspect; 2 for quadrant cells (2x2 per terminal cell), 1 for half-blocks")
    ap.add_argument("--out", default=os.path.join(HERE, "wordmark.txt"))
    a = ap.parse_args()
    im = Image.open(LOGO).convert("RGB").crop(BOX)
    w, h = im.size
    rows = round(h / w * a.cols / a.aspect)   # subpixel rows; each terminal row shows two of them
    rows += rows % 2
    # ink coverage: how far a pixel is from the white background towards the logo blue
    small = im.resize((a.cols, rows), Image.LANCZOS)
    px = small.load()
    lines = []
    for y in range(rows):
        digits = ""
        for x in range(a.cols):
            r, g, b = px[x, y]
            cov = 1.0 - ((r + g) / 2.0) / 255.0   # white -> 0, the blue (low red/green) -> ~0.9
            cov = min(1.0, max(0.0, cov * 1.15))
            digits += format(round(cov * 15), "x")
        lines.append(digits)
    with open(a.out, "w") as f:
        f.write("\n".join(lines) + "\n")
    print(f"wrote {a.out}: {a.cols} x {rows} subpixels ({a.cols // 2} x {rows // 2} terminal cells with quadrants)")


if __name__ == "__main__":
    main()
