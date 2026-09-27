#!/usr/bin/env bash
# Render the demo to a GIF with agg (https://github.com/asciinema/agg).
#
#   demo/render.sh                                   # trajfs-demo{,-title,-end}.cast -> trajfs-demo.gif
#   demo/render.sh demo/trajfs-demo.small.cast demo/trajfs-demo.small.gif
#
# record.py writes three casts: the body (100x30, font 16) and the title and end cards (67x20,
# font 24, so their text is larger). Each is rendered on its own and the GIFs are joined with
# Pillow (POSTER_PY selects a python that has it; without Pillow only the body GIF is produced).
# The theme comes from the cast header (record.py); pass AGG_THEME=dracula etc. to override.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
cast="${1:-$here/trajfs-demo.cast}"
gif="${2:-$here/trajfs-demo.gif}"
stem="${cast%.cast}"
agg="${AGG:-$(command -v agg || echo "$HOME/.cargo/bin/agg")}"
font_dir="${FONT_DIR:-/usr/share/fonts/truetype/dejavu}"
py="${POSTER_PY:-python3}"

render() {  # cast gif font-size
  local args=(
    --font-dir "$font_dir"
    --font-family "DejaVu Sans Mono"
    --font-size "$3"
    --line-height 1.3
    --fps-cap "${FPS:-20}"
    --idle-time-limit 12
    --last-frame-duration "${4:-0.5}"
    --speed "${SPEED:-1}"
  )
  [[ -n "${AGG_THEME:-}" ]] && args+=(--theme "$AGG_THEME")
  "$agg" -q "${args[@]}" "$1" "$2"
}

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
render "$cast" "$tmp/body.gif" "${FONT_SIZE:-16}" 4
parts=("$tmp/body.gif")
if [[ -f "$stem-title.cast" && -f "$stem-end.cast" ]]; then
  render "$stem-title.cast" "$tmp/title.gif" "${CARD_FONT_SIZE:-24}" 0.6
  render "$stem-end.cast" "$tmp/end.gif" "${CARD_FONT_SIZE:-24}" 4
  parts=("$tmp/title.gif" "$tmp/body.gif" "$tmp/end.gif")
fi

# join the parts; card frames are scaled to the body's pixel size when the fonts do not line up
if "$py" - "$gif" "${parts[@]}" <<'PY'
import sys
try:
    from PIL import Image
except ImportError:
    sys.exit(1)
out, parts = sys.argv[1], sys.argv[2:]
frames, durations = [], []
size = None
for path in parts:
    im = Image.open(path)
    if size is None:
        size = Image.open(parts[1] if len(parts) > 1 else parts[0]).size
    try:
        while True:
            f = im.convert("RGB")
            if f.size != size:
                f = f.resize(size, Image.LANCZOS)
            frames.append(f)
            durations.append(max(20, int(im.info.get("duration", 50))))
            im.seek(im.tell() + 1)
    except EOFError:
        pass
frames[0].save(out, save_all=True, append_images=frames[1:], duration=durations, loop=0, optimize=True)
print(f"{out}: {len(frames)} frames, {sum(durations) / 1000:.1f} s")
PY
then :; else
  cp "$tmp/body.gif" "$gif"
  echo "Pillow not available for $py: wrote the body only to $gif"
fi
ls -l "$gif" | awk '{printf "%s  %.1f MB\n", $NF, $5/1048576}'

# Optional poster frame for places that cannot animate: the git-vs-TrajFS comparison table, i.e.
# the last body frame before the section-4 screen clear (POSTER_CLEAR=3: the 3rd full-screen change
# in the body cast). Needs Pillow; skipped otherwise.
png="${gif%.gif}.png"
"$py" - "$tmp/body.gif" "$png" "${POSTER_CLEAR:-3}" <<'PY' 2>/dev/null || true
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
