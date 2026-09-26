#!/usr/bin/env bash
# Render the demo cast to a GIF with agg (https://github.com/asciinema/agg).
#
#   demo/render.sh                                   # trajfs-demo.cast -> trajfs-demo.gif
#   demo/render.sh demo/trajfs-demo.small.cast demo/trajfs-demo.small.gif
#
# The theme comes from the cast header (record.py); pass AGG_THEME=dracula etc. to override.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
cast="${1:-$here/trajfs-demo.cast}"
gif="${2:-$here/trajfs-demo.gif}"
agg="${AGG:-$(command -v agg || echo "$HOME/.cargo/bin/agg")}"
font_dir="${FONT_DIR:-/usr/share/fonts/truetype/dejavu}"

args=(
  --font-dir "$font_dir"
  --font-family "DejaVu Sans Mono"
  --font-size "${FONT_SIZE:-16}"
  --line-height 1.3
  --fps-cap "${FPS:-20}"
  --idle-time-limit 3
  --last-frame-duration 4
  --speed "${SPEED:-1}"
)
[[ -n "${AGG_THEME:-}" ]] && args+=(--theme "$AGG_THEME")

"$agg" -q "${args[@]}" "$cast" "$gif"
ls -l "$gif" | awk '{printf "%s  %.1f MB\n", $NF, $5/1048576}'

# Optional poster frame for places that cannot animate: the git-vs-TrajFS comparison table, i.e.
# the last frame before the section-4 screen clear (POSTER_CLEAR=4: the 4th full-screen change after
# the title card). Needs Pillow (POSTER_PY selects the interpreter); skipped otherwise.
png="${gif%.gif}.png"
"${POSTER_PY:-python3}" - "$gif" "$png" "${POSTER_CLEAR:-4}" <<'PY' 2>/dev/null || true
import sys
try:
    from PIL import Image, ImageChops
except ImportError:
    sys.exit(0)
gif, png, want = sys.argv[1], sys.argv[2], int(sys.argv[3])
im = Image.open(gif); prev = im.convert("RGB"); clears = 0; t = 0.0; poster = None
try:
    while True:
        t += im.info.get("duration", 0) / 1000.0
        im.seek(im.tell() + 1); cur = im.convert("RGB")
        box = ImageChops.difference(prev, cur).getbbox()
        if box and box[1] < 0.08 * cur.height and (box[3] - box[1]) > 0.25 * cur.height:  # screen clear
            clears += 1
            if clears == want:
                poster = prev; break
        prev = cur
except EOFError:
    pass
if poster is None:
    sys.exit(0)
poster.save(png); print(f"{png}  poster frame (before screen clear #{want}, t={t:.1f} s)")
PY
