#!/usr/bin/env python3
"""Compose the TrajFS demo as an asciicast v2 file. Stdlib only; no asciinema needed.

Reads the numbers from demo/measurements.json (written by demo/measure.py), re-runs the fast
read commands (traj ls/cat/grep/sql) live against the measured store when it exists, and writes
demo/trajfs-demo.cast. Render it with demo/render.sh.

Usage:
    python3 demo/record.py                                   # measurements.json -> trajfs-demo.cast
    python3 demo/record.py --measurements demo/measurements.small.json --out demo/trajfs-demo.small.cast
    python3 demo/record.py --offline                         # use the outputs captured by measure.py
"""

import argparse
import json
import os
import random
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)
COLS, ROWS = 100, 30

# --- theme (GitHub-dark-ish); the 16-color palette lives in the cast header so agg picks it up ---
THEME = {
    "fg": "#c9d1d9", "bg": "#0d1117",
    "palette": ":".join([
        "#30363d", "#ff7b72", "#3fb950", "#d29922", "#58a6ff", "#bc8cff", "#39c5cf", "#b1bac4",
        "#6e7681", "#ffa198", "#56d364", "#e3b341", "#79c0ff", "#d2a8ff", "#56d4dd", "#f0f6fc",
    ]),
}
ESC = "\x1b["
RESET = ESC + "0m"
BOLD = ESC + "1m"
DIM = ESC + "90m"          # bright black from the palette: comments and rules
RED, GREEN, YELLOW, BLUE, MAGENTA, CYAN, WHITE = (ESC + f"{c}m" for c in (91, 92, 93, 94, 95, 96, 97))
GREEN_B, WHITE_B, BLUE_B = ESC + "1;92m", ESC + "1;97m", ESC + "1;94m"

PROMPT_DIR = "~/exp"
BULLET = "●"                 # Claude Code's "⏺" is not in DejaVu Sans Mono; ● is
ELBOW = "└"                  # stands in for "⎿"
SPINNER = "◐◓◑◒"  # ◐◓◑◒ (geometric shapes, in DejaVu Sans Mono)


# --- formatting helpers (shared with the README numbers) --------------------------------------


# ---------------------------------------------------------------------------- animated cards
# The title and end cards are separate casts rendered at a larger font (render.sh joins the GIFs),
# so their text is bigger than the body's. They share one look: the logo's own "TrajFS" wordmark
# (demo/wordmark.txt, cut from docs/trajfs-logo.png by make_wordmark.py) drawn with quadrant cells
# in the logo blue, over which one broad, soft band of grey light unfolds once, left to right, like
# silk laid across ink (浮光跃金); a pale-gold reflection lies still under it (静影沉璧). When the
# band has passed, the lines below fade in as a whole.

import math

CARD_COLS, CARD_ROWS = 67, 20           # font 24 px gives the body's pixel size at these dimensions
CARD_FPS = 15
BG = (13, 17, 23)                       # THEME["bg"]
LOGO_BLUE = (31, 111, 229)
WASH_GREY = (196, 204, 216)             # the silk band
GOLD_PALE = (236, 208, 150)             # the reflection
INK_WHITE = (236, 238, 242)
INK_SOFT = (150, 156, 168)
PILLAR_GREEN, PILLAR_BLUE, PILLAR_WHITE = (63, 185, 80), (88, 166, 255), (236, 238, 242)


def load_wordmark():
    """Rows of ink coverage (0.0-1.0) per subpixel; two subpixel rows per terminal row."""
    path = os.path.join(HERE, "wordmark.txt")
    rows = [[int(ch, 16) / 15.0 for ch in line.strip()] for line in open(path) if line.strip()]
    if len(rows) % 2:
        rows.append([0.0] * len(rows[0]))
    return rows


def rgb(fg):
    return f"{ESC}38;2;{fg[0]};{fg[1]};{fg[2]}m"


def rgb_bg(bg):
    return f"{ESC}48;2;{bg[0]};{bg[1]};{bg[2]}m"


def mix(a, b, t):
    t = max(0.0, min(1.0, t))
    return tuple(round(a[i] + (b[i] - a[i]) * t) for i in range(3))


def silk(x, t, width, t0=0.3, duration=2.6, band=14.0):
    """One soft band crossing `width` columns between t0 and t0 + duration; 0..1 at column x.
    Its centre eases in and out, and its profile is a wide gaussian with a softer trailing edge,
    which is what makes it read as cloth unfolding rather than a bar sweeping."""
    u = (t - t0) / duration
    if u <= 0 or u >= 1.25:
        return 0.0
    u = min(1.0, u)
    e = 0.5 - 0.5 * math.cos(math.pi * u)                    # ease in-out
    centre = -band + (width + 2 * band) * e
    d = (x - centre) / band
    if d > 0:
        return math.exp(-2.2 * d * d)                        # leading edge
    return math.exp(-0.9 * d * d)                            # trailing edge lingers


QUADRANTS = {  # (top-left, top-right, bottom-left, bottom-right) inked -> block element
    (0, 0, 0, 0): " ", (1, 0, 0, 0): "▘", (0, 1, 0, 0): "▝", (0, 0, 1, 0): "▖", (0, 0, 0, 1): "▗",
    (1, 1, 0, 0): "▀", (0, 0, 1, 1): "▄", (1, 0, 1, 0): "▌", (0, 1, 0, 1): "▐", (1, 0, 0, 1): "▚",
    (0, 1, 1, 0): "▞", (1, 1, 1, 0): "▛", (1, 1, 0, 1): "▜", (1, 0, 1, 1): "▙", (0, 1, 1, 1): "▟",
    (1, 1, 1, 1): "█",
}


def quadrant_rows(cells, col0, threshold=0.4):
    """cells: subpixel rows of (coverage, RGB) or None, two subpixels per terminal cell in each
    direction. A cell shows the quadrant block of its inked subpixels in their average colour, so
    edges keep twice the resolution of plain half-blocks."""
    out = []
    for y in range(0, len(cells), 2):
        line, last = " " * col0, None
        for x in range(0, len(cells[y]), 2):
            quad = [cells[y][x], cells[y][x + 1], cells[y + 1][x], cells[y + 1][x + 1]]
            inked = [q for q in quad if q is not None and q[0] >= threshold]
            if not inked:
                line += RESET + " "
                last = None
                continue
            key = tuple(1 if (q is not None and q[0] >= threshold) else 0 for q in quad)
            col = tuple(round(sum(q[1][i] for q in inked) / len(inked)) for i in range(3))
            code = rgb(col)
            if code != last:
                line += code
                last = code
            line += QUADRANTS[key]
        out.append(line + RESET)
    return out


def render_wordmark(grid, t, col0, reveal=1.0):
    """The wordmark in logo blue; where the silk band passes, the blue washes towards grey."""
    width = len(grid[0])
    cells = []
    for y, row in enumerate(grid):
        line = []
        for x, cov in enumerate(row):
            if cov <= 0.08:
                line.append(None)
            else:
                wash = silk((x + 0.15 * y) / 2.0, t, width / 2.0)
                colour = mix(LOGO_BLUE, WASH_GREY, 0.85 * wash)
                line.append((cov, mix(BG, colour, reveal)))
        cells.append(line)
    return quadrant_rows(cells, col0)


def render_reflection(grid, t, col0, reveal=1.0, depth_rows=4):
    """The wordmark mirrored under its baseline in pale gold: still water with a faint slow ripple
    and a softened echo of the band passing above."""
    # the baseline is the inked row where the letters stand (most ink in the lower half of the
    # grid), not the bottom of the j's descender, so the mirror image lines up under the letters
    width = len(grid[0])
    lower = range(len(grid) // 2, len(grid))
    baseline = max(lower, key=lambda y: sum(grid[y]))
    cells = []
    for d in range(depth_rows * 2):
        src = grid[baseline - d] if baseline - d >= 0 else [0.0] * width
        fade = (1.0 - d / (depth_rows * 2.0)) ** 1.2 * 0.62
        shift = round(2.0 * math.sin(t * 0.9 + d * 0.5))
        line = [None] * width
        for x, cov in enumerate(src):
            xx = x + shift
            if cov > 0.08 and 0 <= xx < width:
                glow = 0.7 + 0.3 * silk(x / 2.0, t - 0.35, width / 2.0, band=18.0)
                line[xx] = (cov, mix(BG, GOLD_PALE, fade * glow * reveal))
        cells.append(line)
    return quadrant_rows(cells, col0, threshold=0.35)


def centred(text, cols=CARD_COLS):
    return " " * max(0, (cols - len(text)) // 2)


def fade_line(text, t, t0, base, bold=False, dur=1.1):
    """A centred line fading in as a whole between t0 and t0 + dur."""
    a = min(1.0, max(0.0, (t - t0) / dur))
    return centred(text) + (BOLD if bold else "") + rgb(mix(BG, base, a)) + text + RESET


def fade_pillars(t, t0, dur=1.1, gap="   "):
    """The three pillars in their own colours, fading in together."""
    parts = [("Faster for Git.", PILLAR_GREEN), ("Friendly to agents.", PILLAR_BLUE), ("Still files for humans.", PILLAR_WHITE)]
    a = min(1.0, max(0.0, (t - t0) / dur))
    width = sum(len(p) for p, _ in parts) + len(gap) * (len(parts) - 1)
    line = " " * max(0, (CARD_COLS - width) // 2) + BOLD
    for k, (text, base) in enumerate(parts):
        line += rgb(mix(BG, base, a)) + text + (gap if k < len(parts) - 1 else "")
    return line + RESET


def play_card(c, seconds, draw):
    """Emit `seconds` of frames at CARD_FPS; `draw(t)` returns the list of (row, line) to paint."""
    n = int(seconds * CARD_FPS)
    for k in range(n):
        t = k / CARD_FPS
        frame = ESC + "H"
        for row, line in draw(t):
            frame += f"{ESC}{row};1H{ESC}2K" + line
        c.out(frame, 1.0 / CARD_FPS if k else 0.0)


def wordmark_card(seconds, lines_below):
    """A card cast: wordmark and reflection at the top, then `lines_below` as (row, t0, render)
    where render(t, t0) returns the line. Both cards use this so they stay consistent."""
    c = Cast()
    c.out(ESC + "?25l")
    c.clear()
    grid = load_wordmark()
    col0 = (CARD_COLS - len(grid[0]) // 2) // 2
    top = 2

    def draw(t):
        reveal = min(1.0, t / 0.7)
        wm = render_wordmark(grid, t, col0, reveal)
        out = [(top + i, l) for i, l in enumerate(wm)]
        out += [(top + len(wm) + i, l) for i, l in enumerate(render_reflection(grid, t, col0, reveal))]
        out += [(row, render(t, t0)) for row, t0, render in lines_below]
        return out

    play_card(c, seconds, draw)
    return c


def title_card():
    # the band has crossed by about 3.0 s; the lines follow it
    return wordmark_card(7.5, [
        (14, 3.0, lambda t, t0: fade_line("Make millions of AI-agent trajectory files gittable", t, t0, INK_WHITE, bold=True)),
        (16, 3.8, lambda t, t0: fade_pillars(t, t0)),
    ])


def end_card(url):
    return wordmark_card(8.5, [
        (13, 3.0, lambda t, t0: fade_line("Gittable version control for trajectories", t, t0, INK_WHITE, bold=True)),
        (14, 3.0, lambda t, t0: fade_line("ultra-fast, human-readable files, agent-native", t, t0, INK_SOFT)),
        (16, 3.8, lambda t, t0: fade_pillars(t, t0)),
        (18, 4.6, lambda t, t0: fade_line(url, t, t0, INK_SOFT)),
    ])


def index_scale():
    """The largest stage of the newest bench/results/index-scale-*.json, if any: what git add costs once the
    repository already tracks millions of paths, plus the cold-disk read rate. None when not measured."""
    import glob
    cands = sorted(glob.glob(os.path.join(REPO, "bench", "results", "index-scale-*.json")))
    if not cands:
        return None
    try:
        d = json.load(open(cands[-1]))
        st = max(d["stages"], key=lambda x: x["prior_paths"])
        return {"prior_paths": st["prior_paths"], "add_seconds": st["add_seconds"],
                "cold_files_per_second": (d.get("cold_cache") or {}).get("cold_files_per_second")}
    except (OSError, KeyError, ValueError):
        return None


def fmt_count(n):
    return f"{int(n):,}"


def fmt_time(s):
    s = float(s)
    if s >= 3600:
        return f"{int(s // 3600)}h {int((s % 3600) // 60):02d}m"
    if s >= 60:
        return f"{int(s // 60)}m {int(round(s % 60)):02d}s"
    if s >= 10:
        return f"{s:.0f} s"
    if s >= 1:
        return f"{s:.1f} s"
    if s >= 0.1:
        return f"{s:.1f} s"
    return f"{s:.2f} s"


def fmt_size(b):
    b = float(b)
    for unit, div in (("GB", 1024 ** 3), ("MB", 1024 ** 2), ("KB", 1024)):
        if b >= div:
            v = b / div
            return f"{v:.1f} {unit}" if v < 10 else f"{v:.0f} {unit}"
    return f"{b:.0f} B"


def fmt_factor(x):
    if x >= 100:
        return f"{x:,.0f}x"
    if x >= 10:
        return f"{x:.0f}x"
    return f"{x:.1f}x"


def vis_len(s):
    """Printable width of a string containing SGR sequences."""
    out, i = 0, 0
    while i < len(s):
        if s.startswith(ESC, i):
            j = s.find("m", i)
            i = j + 1 if j > 0 else i + 1
            continue
        out += 1
        i += 1
    return out


# --- the cast writer -----------------------------------------------------------------------
class Cast:
    def __init__(self, seed=7):
        self.t = 0.0
        self.events = []
        self.rng = random.Random(seed)
        self.markers = []

    def out(self, s, dt=0.0):
        self.t += dt
        self.events.append([round(self.t, 4), "o", s])

    def wait(self, s):
        self.t += s

    def marker(self, label):
        self.markers.append((self.t, label))
        self.events.append([round(self.t, 4), "m", label])

    def type(self, text, fast=False):
        """Type text char by char with jittered delays; long commands type faster."""
        n = vis_len(text)
        base = 0.02 if (fast or n > 45) else 0.04
        i = 0
        while i < len(text):
            if text.startswith(ESC, i):           # emit escape sequences instantly
                j = text.find("m", i)
                self.out(text[i:j + 1])
                i = j + 1
                continue
            ch = text[i]
            d = base + self.rng.uniform(-0.4, 0.8) * base
            if ch == " ":
                d += 0.03
            self.out(ch, d)
            i += 1

    def clear(self):
        self.out(ESC + "2J" + ESC + "H")

    def line(self, s="", dt=0.0):
        """Print a line; anything wider than the terminal is word-wrapped with a hanging indent."""
        for i, part in enumerate(self.wrap_line(s)):
            self.out(part + RESET + "\r\n", dt if i == 0 else 0.0)

    @staticmethod
    def wrap_line(s, width=COLS - 1, indent="  "):
        if vis_len(s) <= width:
            return [s]
        sgr, i = "", 0                       # leading SGR sequences are re-applied on continuation lines
        while s.startswith(ESC, i):
            j = s.find("m", i)
            sgr += s[i:j + 1]
            i = j + 1
        parts = wrap(s, width, indent)
        return [parts[0]] + [sgr + p for p in parts[1:]]

    def prompt(self, cmd, cwd=PROMPT_DIR, comment=None, gap=0.6):
        """Show a prompt, type the command, pause, press enter."""
        self.out(BLUE_B + cwd + RESET + " " + WHITE_B + "$ " + RESET, gap)
        self.type(cmd)
        if comment:
            self.type("   " + DIM + comment + RESET, fast=True)
        self.out("\r\n", 0.45)

    def comment(self, text, dt=0.3):
        self.line(DIM + text, dt)

    def title(self, n, text, pad=1):
        """Bold section title over a dim rule, then `pad` blank rows so short sections sit lower."""
        head = f"── {n} · {text} "
        self.line(BOLD + WHITE + head + RESET + DIM + "─" * (COLS - vis_len(head) - 1), 0.0)
        for _ in range(pad):
            self.line()

    def spinner(self, label, seconds, fps=8, suffix="", color=YELLOW):
        frames = int(seconds * fps)
        for k in range(frames):
            el = k / fps
            s = f"\r{color}{SPINNER[k % len(SPINNER)]}{RESET} {label}{DIM} {int(el // 60)}:{int(el % 60):02d}{suffix}{RESET}{ESC}K"
            self.out(s, 1 / fps if k else 0.0)
        self.out("\r" + ESC + "K")

    def write(self, path, width=COLS, height=ROWS):
        header = {"version": 2, "width": width, "height": height, "timestamp": int(time.time()),
                  "title": "TrajFS demo", "env": {"TERM": "xterm-256color", "SHELL": "/bin/bash"}, "theme": THEME}
        with open(path, "w") as f:
            f.write(json.dumps(header) + "\n")
            for ev in self.events:
                f.write(json.dumps(ev, ensure_ascii=False) + "\n")


class Pane:
    """A column inside the split-screen frame; writes with absolute cursor positioning."""

    def __init__(self, cast, top, left, width, height):
        self.c, self.top, self.left, self.width, self.height = cast, top, left, width, height
        self.row = 0

    def _goto(self, row, col=0):
        self.c.out(f"{ESC}{self.top + row};{self.left + col}H")

    def _clip(self, s):
        # clip on printable width; content is pre-wrapped so this rarely triggers
        return Pane._clip_to(s, self.width)

    def line(self, s="", dt=0.05):
        if self.row >= self.height:
            return
        self.c.wait(dt)
        self._goto(self.row)
        self.c.out(self._clip(s) + RESET)
        self.row += 1

    def lines(self, seq, dt=0.04):
        for s in seq:
            self.line(s, dt)

    def type(self, prefix, text, dt=0.3):
        """Prefix printed at once, text typed."""
        if self.row >= self.height:
            return
        self.c.wait(dt)
        self._goto(self.row)
        self.c.out(prefix)
        room = self.width - vis_len(prefix)
        self.c.type(self._clip(text) if room >= self.width else Pane._clip_to(text, room))
        self.c.out(RESET)
        self.row += 1

    @staticmethod
    def _clip_to(s, width):
        out, n, i = "", 0, 0
        while i < len(s):
            if s.startswith(ESC, i):
                j = s.find("m", i)
                out += s[i:j + 1]
                i = j + 1
                continue
            if n < width:
                out += s[i]
                n += 1
            i += 1
        return out

    def blank(self):
        self.row += 1


def wrap(text, width, indent=""):
    words, lines, cur = text.split(" "), [], ""
    for w in words:
        cand = (cur + " " + w) if cur else w
        if vis_len(cand) > width and cur:
            lines.append(cur)
            cur = indent + w
        else:
            cur = cand
    if cur:
        lines.append(cur)
    return lines


# --- live command runs --------------------------------------------------------------------------
def live(traj, store, args, keep=None):
    env = dict(os.environ, TRAJ_STORE=store)
    p = subprocess.run([traj] + args, env=env, text=True, capture_output=True)
    if p.returncode != 0:
        return None
    lines = p.stdout.splitlines()
    return {"lines": lines if keep is None else lines[:keep], "total_lines": len(lines)}


# --- the narrative --------------------------------------------------------------------------------
def build(m, traj, offline):
    c = Cast()
    g = m["git"]["commands"]
    tj = m["traj"]
    reads = tj["reads"]
    store = tj["store"]
    if not offline and os.path.isdir(store) and os.access(traj, os.X_OK):
        for key, args, keep in (("grep", ["grep", "-l", "-e", "Traceback", "--name", "*.stderr"], 8),
                                ("cat", ["cat", f"rounds/{tj['first_traceback_round']}/reviewer/review.json"], None),
                                ("ls_rounds", ["ls", "rounds"], 6)):
            r = live(traj, store, args, keep)
            if r:
                reads[key].update(r)
        r = live(traj, store, ["sql", reads["sql"]["cmd"][len("traj sql "):]])
        if r:
            reads["sql"].update(r)
    nfiles = m["file_count"]
    rounds = m["rounds"]
    rnd = tj["first_traceback_round"]
    first_hit = tj["first_traceback_path"]

    c.out(ESC + "?25l")   # hide the cursor; typing is visible enough and it keeps frames clean

    # 1. a million files --------------------------------------------------------------------------
    c.marker("1 million files")
    c.clear()
    c.title(1, f"An agent run is a million files ({rounds} rounds of build, review, test)", pad=2)
    c.prompt("find runs/task-a -type f | wc -l | numfmt --grouping")
    c.line(BOLD + fmt_count(nfiles), 0.5)
    c.prompt("du -sh runs/task-a")
    c.line(f"{m['du_sh']}\truns/task-a", 0.3)
    mid = m["round_dirs"][len(m["round_dirs"]) // 2]
    c.prompt(f"tree -L 2 runs/task-a/rounds/{mid}")
    for ln in m["tree_listing"]:
        c.line(ln, 0.03)
    c.line()
    c.comment(f"# {rounds} rounds × 3 agents × (events.jsonl + logs + a workspace snapshot)")
    c.comment("# tiny, duplicate-heavy files — the workspace is snapshotted again every round", 0.1)
    c.wait(2.6)

    # 2. git ---------------------------------------------------------------------------------------
    c.marker("2 git")
    c.clear()
    c.title(2, "Try to put it in Git", pad=5)
    c.prompt(f'git add -A && git commit -m "round {rounds}"')
    c.spinner(f"adding {fmt_count(nfiles)} files ...", min(3.6, max(1.0, g['add']['seconds'])), suffix="")
    c.line(DIM + f"# ... still running. On this host it takes {fmt_time(g['add']['seconds'] + g['commit']['seconds'])}. Skipping ahead.", 0.2)
    c.line()
    c.line(BOLD + f"  measured on this host" + RESET + DIM + f"  —  {fmt_count(nfiles)} files, {fmt_size(m['tree_bytes'])} on disk, {m['versions']['git']}", 0.8)
    c.line()
    rows = [("git add -A", g["add"]["seconds"], ""),
            (f'git commit -m "round {rounds}"', g["commit"]["seconds"], ""),
            ("git status", g["status"]["seconds"], ""),
            ("git push", g["push"]["seconds"], ""),
            ("git clone", g["clone"]["seconds"], "objects only"),
            ("git checkout", g["checkout"]["seconds"], f"writes {fmt_count(nfiles)} files")]
    for cmd, sec, note in rows:
        c.line(f"    {cmd:<32}{RED}{fmt_time(sec):>9}{RESET}   {DIM}{note}", 0.22)
    c.line(f"    {'.git':<32}{RED}{fmt_size(m['git']['git_dir_bytes']):>9}{RESET}   {DIM}after one commit", 0.22)
    scale = index_scale()
    if scale:
        c.line()
        c.line(DIM + "  and that was the easy case: a fresh repository, files already in the page cache", 0.5)
        c.line(f"    {'git add -A, repo already tracks ' + fmt_count(scale['prior_paths']) + ' paths':<44}{RED}{fmt_time(scale['add_seconds']):>9}{RESET}   {DIM}measured", 0.3)
        if scale.get("cold_files_per_second"):
            est = nfiles / scale["cold_files_per_second"]
            c.line(f"    {'first read of the files from a cold disk':<44}{RED}{'~' + fmt_time(est):>9}{RESET}   {DIM}{scale['cold_files_per_second']} files/s measured", 0.3)
    c.line()
    c.line(YELLOW + "  ✗ every command walks a million files     ✗ every clone and checkout pays again", 0.4)
    c.line(YELLOW + "  ✗ history grows by a whole tree per round  ✗ GitHub file-count and size limits", 0.6)
    c.wait(5.0)

    # 3. the TrajFS way ------------------------------------------------------------------------------
    c.marker("3 trajfs")
    c.clear()
    c.title(3, "The TrajFS way", pad=3)
    c.comment("# TrajFS packs the run into one deduplicated, compressed store that Git handles easily.")
    c.line()
    c.prompt("traj pack runs/task-a --out stores/task-a.trajstore --adapter copilot-cli")
    pack_s = tj["pack"]["seconds"]
    c.spinner("hashing, deduplicating, packing ...", min(2.0, max(0.8, pack_s)), suffix="")
    c.line(tj["pack"]["summary"], 0.1)
    c.line(DIM + f"# took {fmt_time(pack_s)}", 0.2)
    c.prompt("traj commit stores/task-a.trajstore")
    c.line(tj["commit"]["output"] + DIM + f"    # took {fmt_time(tj['commit']['seconds'])}", 0.5)
    c.prompt("du -sh stores/task-a.trajstore")
    c.line(f"{fmt_size(tj['store_bytes']).replace(' ', '')}\tstores/task-a.trajstore", 0.3)
    c.line()
    c.line()
    hdr = f"    {'':<22}{'git on raw files':>18}{'TrajFS':>16}"
    c.line(BOLD + hdr, 0.6)
    c.line(DIM + "    " + "─" * 76, 0.0)
    comp = [("add + commit", g["add"]["seconds"] + g["commit"]["seconds"], pack_s + tj["commit"]["seconds"], "pack + commit"),
            ("status", g["status"]["seconds"], tj["status"]["seconds"], ""),
            ("push", g["push"]["seconds"], tj["push"]["seconds"], ""),
            ("clone + checkout", g["clone"]["seconds"] + g["checkout"]["seconds"], tj["clone"]["seconds"], "")]
    for name, a, b, note in comp:
        c.line(f"    {name:<22}{fmt_time(a):>18}{fmt_time(b):>16}   {GREEN_B}{fmt_factor(a / max(b, 1e-3)):>7} faster{RESET}  {DIM}{note}", 0.3)
    a, b = m["git"]["git_dir_bytes"], tj["store_bytes"]
    c.line(f"    {'repository size':<22}{fmt_size(a):>18}{fmt_size(b):>16}   {GREEN_B}{fmt_factor(a / b):>7} smaller{RESET}  {DIM}.git vs store", 0.3)
    c.wait(7.0)

    # 4. split view ---------------------------------------------------------------------------------
    c.marker("4 read")
    c.clear()
    c.title(4, "Read it back: agents query the store, humans open it as files", pad=0)
    top, bottom = 3, ROWS - 1
    div = 57
    lw, rw = div - 3, COLS - div - 2          # inner text widths (1 col padding each side)
    lt = "─ " + BOLD + "agent view" + RESET + DIM + " (Claude Code) "
    rt = "─ " + BOLD + "human view" + RESET + DIM + " (FUSE mount) "
    c.out(f"{ESC}{top};1H{DIM}┌{lt}{'─' * (div - 2 - vis_len(lt))}┬{rt}{'─' * (COLS - div - 1 - vis_len(rt))}┐{RESET}")
    for r in range(top + 1, bottom):
        c.out(f"{ESC}{r};1H{DIM}│{ESC}{r};{div}H│{ESC}{r};{COLS}H│{RESET}")
    c.out(f"{ESC}{bottom};1H{DIM}└{'─' * (div - 2)}┴{'─' * (COLS - div - 1)}┘{RESET}")
    L = Pane(c, top + 1, 3, lw, bottom - top - 1)
    R = Pane(c, top + 1, div + 2, rw, bottom - top - 1)
    P = BLUE_B + "~/exp" + RESET + " " + WHITE_B + "$ " + RESET

    c.wait(0.6)
    q = "which round first hit a Traceback, and what did the reviewer say?"
    ql = wrap(q, lw - 2)
    L.type(WHITE_B + "> " + RESET, ql[0], 0.2)
    for extra in ql[1:]:
        L.line("  " + extra, 0.02)
    L.blank()

    R.type(P, "traj mount ~/mnt/task-a --daemon", 0.4)
    mnt = tj["mount"]
    R.line(f"mounted ~/mnt/task-a {DIM}(pid {mnt.get('output', 'pid 0').split('pid ')[-1].split(',')[0] if 'output' in mnt else '…'})", 0.25)
    R.blank()

    grep_cmd = "traj grep -l -e Traceback --name '*.stderr'"
    L.type(MAGENTA + BULLET + RESET + " " + BOLD + "Bash" + RESET + "(", grep_cmd + ")", 0.5)
    gl = reads["grep"]
    L.line(f"  {DIM}{ELBOW}{RESET}  {gl['lines'][0]}", 0.35)
    L.line(f"     {gl['lines'][1]}", 0.03)
    L.line(f"     {DIM}… +{fmt_count(gl['total_lines'] - 2)} lines", 0.03)
    L.blank()

    R.type(P, "ls ~/mnt/task-a/rounds | head -4", 0.5)
    R.lines(mnt.get("ls_rounds", [x.rstrip('/') for x in reads["ls_rounds"]["lines"][:4]]), 0.03)
    R.blank()

    sql_show = ["traj sql \"select round, verdict, tests_passed",
                " from … where name = 'review.json' order by 1\""]
    L.type(MAGENTA + BULLET + RESET + " " + BOLD + "Bash" + RESET + "(", sql_show[0], 0.5)
    L.line("      " + sql_show[1] + ")", 0.02)
    sq = reads["sql"]["lines"]
    body = [ln for ln in sq if ln.startswith("|")]
    L.line(f"  {DIM}{ELBOW}{RESET}  {body[0]}", 0.35)
    for ln in body[1:4]:
        L.line("     " + ln, 0.03)
    L.line(f"     {DIM}… ", 0.03)
    L.blank()

    cat_path = f"rounds/{rnd}/reviewer/review.json"
    R.type(P, "cat ~/mnt/task-a/\\", 0.5)
    R.line("  " + cat_path, 0.02)
    R.lines(mnt.get("cat", reads["cat"]["lines"]), 0.03)
    R.blank()

    cat_cmd = f"traj cat {cat_path})"
    if vis_len(cat_cmd) + 7 > lw:                      # break the path at a slash if it would not fit
        cut = cat_cmd.rfind("/", 0, lw - 8) + 1
        L.type(MAGENTA + BULLET + RESET + " " + BOLD + "Bash" + RESET + "(", cat_cmd[:cut], 0.5)
        L.line("      " + cat_cmd[cut:], 0.02)
    else:
        L.type(MAGENTA + BULLET + RESET + " " + BOLD + "Bash" + RESET + "(", cat_cmd, 0.5)
    cat = reads["cat"]["lines"]
    one = " ".join(x.strip() for x in cat)
    cl = wrap(one, lw - 5)
    L.line(f"  {DIM}{ELBOW}{RESET}  {cl[0]}", 0.35)
    for extra in cl[1:]:
        L.line("     " + extra, 0.02)
    L.blank()

    review = {}
    try:
        review = json.loads("\n".join(cat))
    except Exception:
        pass
    rnum = int(rnd.split("-")[-1])
    ans = (f"Round {rnum} hit the first Traceback "
           f"({'/'.join(first_hit.split('/')[2:])}). The reviewer's verdict: "
           f"\"{review.get('verdict', '?')}\", {review.get('tests_passed', '?')} tests passed.")
    al = wrap(ans, lw - 2)
    L.line(f"{MAGENTA}{BULLET}{RESET} {al[0]}", 0.6)
    for extra in al[1:]:
        L.line("  " + extra, 0.02)

    R.type(P, "code ~/mnt/task-a", 0.6)
    R.line(DIM + "# read-only files; any editor works", 0.15)
    c.wait(4.0)

    return c


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--measurements", default=os.path.join(HERE, "measurements.json"))
    ap.add_argument("--out", default=os.path.join(HERE, "trajfs-demo.cast"))
    ap.add_argument("--traj", default=os.path.join(REPO, "target", "release", "traj"))
    ap.add_argument("--offline", action="store_true", help="never run traj; use the outputs captured by measure.py")
    a = ap.parse_args()
    if not os.path.exists(a.measurements):
        sys.exit(f"{a.measurements} not found; run demo/measure.py first")
    with open(a.measurements) as f:
        m = json.load(f)
    c = build(m, a.traj, a.offline)
    c.write(a.out)
    print(f"wrote {a.out}: {len(c.events)} events, {c.t:.1f} s")
    # the cards: separate casts at card dimensions; render.sh draws them at a larger font
    stem = a.out[:-5] if a.out.endswith(".cast") else a.out
    for name, card in (("title", title_card()), ("end", end_card("github.com/jingliu9/TrajFS"))):
        path = f"{stem}-{name}.cast"
        card.write(path, width=CARD_COLS, height=CARD_ROWS)
        print(f"wrote {path}: {len(card.events)} frames, {card.t:.1f} s")
    prev = None
    for t, label in c.markers + [(c.t, "end")]:
        if prev:
            print(f"  {prev[1]:<18} {prev[0]:6.1f} s  ->  {t:6.1f} s   ({t - prev[0]:.1f} s)")
        prev = (t, label)


if __name__ == "__main__":
    main()
