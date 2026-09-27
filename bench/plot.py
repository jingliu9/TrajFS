#!/usr/bin/env python3
"""Plot a bench/run.py results file into docs/benchmarks/ (PNG at 2x DPI + README.md).

    python3 bench/plot.py                     # newest bench/results/*.json
    python3 bench/plot.py bench/results/2026-09-26T140700Z.json

Needs matplotlib (numpy comes with it).  Writes:
    docs/benchmarks/git-commands.png   git-raw vs TrajFS, seconds per command, per size
    docs/benchmarks/space.png          raw tree vs packed git objects vs TrajFS store, GB
    docs/benchmarks/read-path.png      ls/find/grep/cat/sql: raw files vs traj vs FUSE mount, ms
    docs/benchmarks/README.md          host block, reproduction commands, results tables
"""

import glob
import json
import os
import sys

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt  # noqa: E402
from matplotlib.ticker import FuncFormatter, LogLocator, NullFormatter  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)
OUT_DIR = os.path.join(REPO, "docs", "benchmarks")

# Palette: validated categorical slots (dataviz reference palette, light mode).
# Color follows the entity across all three charts.
C_GIT = "#2a78d6"      # slot 1 blue: git / raw files on disk
C_TRAJ = "#eb6834"     # slot 2 orange: TrajFS (store, traj commands)
C_THIRD = "#1baf7a"    # slot 3 aqua: the third entity in a chart (FUSE mount, raw tree)
INK = "#0b0b0b"
INK2 = "#52514e"
MUTED = "#898781"
GRID = "#e1e0d9"
AXIS = "#c3c2b7"
SURFACE = "#ffffff"
DPI = 200               # 2x of a 100-dpi README render

plt.rcParams.update({
    "font.family": "sans-serif",
    "font.sans-serif": ["DejaVu Sans", "Helvetica", "Arial"],
    "font.size": 9,
    "text.color": INK,
    "axes.edgecolor": AXIS,
    "axes.labelcolor": INK2,
    "axes.titlecolor": INK,
    "axes.titleweight": "semibold",
    "axes.titlesize": 9.5,
    "axes.linewidth": 0.6,
    "axes.spines.top": False,
    "axes.spines.right": False,
    "axes.grid": True,
    "axes.grid.axis": "y",
    "grid.color": GRID,
    "grid.linewidth": 0.6,
    "grid.linestyle": "-",
    "xtick.color": INK2,
    "ytick.color": INK2,
    "xtick.labelcolor": INK2,
    "ytick.labelcolor": INK2,
    "xtick.major.size": 0,
    "ytick.major.size": 0,
    "xtick.minor.size": 0,
    "ytick.minor.size": 0,
    "legend.frameon": False,
    "legend.fontsize": 8.5,
    "figure.facecolor": SURFACE,
    "axes.facecolor": SURFACE,
    "savefig.facecolor": SURFACE,
})


# ------------------------------------------------------------------------------- data access

def load(path=None):
    if path is None:
        files = sorted(glob.glob(os.path.join(HERE, "results", "*.json")))
        if not files:
            sys.exit("no results in bench/results/; run bench/run.py first")
        path = files[-1]
    with open(path) as f:
        return json.load(f), path


def secs(rec):
    """Seconds of one timed record, or None when missing/aborted/failed."""
    if not isinstance(rec, dict) or rec.get("aborted") or rec.get("exit") not in (0, None):
        return None
    return rec.get("seconds")


def add(*vals):
    if any(v is None for v in vals):
        return None
    return sum(vals)


def size_label(n):
    return f"{n / 1e6:g}M" if n >= 1_000_000 else f"{n // 1000}k"


# The five compared operations: (title, git-raw seconds, TrajFS seconds).
def operations(run):
    g, t = run.get("git", {}), run.get("trajfs", {})
    return [
        ("add + commit", add(secs(g.get("add")), secs(g.get("commit"))),
         add(secs(t.get("pack")), secs(t.get("commit")))),
        ("status", secs(g.get("status")), secs(t.get("status"))),
        ("push", secs(g.get("push")), secs(t.get("push"))),
        ("clone", secs(g.get("clone")), secs(t.get("clone"))),
        ("checkout, round trip", add(secs(g.get("checkout_back")), secs(g.get("checkout_forward"))),
         add(secs(t.get("checkout_back")), secs(t.get("checkout_forward")))),
    ]


def fmt_secs(s):
    if s is None:
        return "aborted"
    if s < 0.1:
        return f"{s * 1000:.0f} ms"
    if s < 10:
        return f"{s:.2f} s"
    if s < 100:
        return f"{s:.1f} s"
    if s < 600:
        return f"{s:.0f} s"
    return f"{s / 60:.1f} min"


def fmt_bytes(b):
    if b is None:
        return "n/a"
    if b >= 1e9:
        return f"{b / 1e9:.2f} GB"
    if b >= 1e6:
        return f"{b / 1e6:.0f} MB"
    return f"{b / 1e3:.0f} kB"


def fmt_x(a, b):
    if a is None or b is None or b == 0:
        return ""
    r = a / b
    return f"{r:.0f}x" if r >= 10 else f"{r:.1f}x"


def fmt_ms(ms):
    if ms is None:
        return "n/a"
    if ms >= 10000:
        return f"{ms / 1000:.1f} s"
    if ms >= 1000:
        return f"{ms / 1000:.2f} s"
    return f"{ms:.0f} ms"


def thin_bars(ax, bars, y_fraction=None):
    for b in bars:
        b.set_linewidth(0)


# ------------------------------------------------------------------------------ chart 1: git

def plot_git_commands(data, out):
    runs = [r for r in data["runs"] if "git" in r and "trajfs" in r]
    ops = [operations(r) for r in runs]
    n_ops = 5
    fig, axes = plt.subplots(1, n_ops, figsize=(9.2, 4.0), sharey=True, dpi=DPI)
    fig.subplots_adjust(left=0.075, right=0.995, top=0.71, bottom=0.19, wspace=0.10)
    xs = list(range(len(runs)))
    w = 0.30
    ymin = 1e-3
    all_vals = [v for o in ops for _, a, b in o for v in (a, b) if v]
    ymax = 10 ** (int(__import__("math").log10(max(all_vals))) + 1.6)
    for k, ax in enumerate(axes):
        title = ops[0][k][0] if ops else ""
        gv = [o[k][1] for o in ops]
        tv = [o[k][2] for o in ops]
        ax.set_yscale("log")
        ax.set_ylim(ymin, ymax)
        gb = ax.bar([x - w / 2 - 0.02 for x in xs], [(v or ymin) - ymin for v in gv], w, bottom=ymin,
                    color=C_GIT, label="git on the raw tree", zorder=3)
        tb = ax.bar([x + w / 2 + 0.02 for x in xs], [(v or ymin) - ymin for v in tv], w, bottom=ymin,
                    color=C_TRAJ, label="TrajFS store", zorder=3)
        thin_bars(ax, list(gb) + list(tb))
        for x, a, b in zip(xs, gv, tv):
            top = max(v for v in (a, b) if v) if (a or b) else ymin
            if a is None:
                ax.text(x - w / 2, ymin * 1.5, "aborted", rotation=90, ha="center", va="bottom",
                        fontsize=7, color=INK2)
            label = fmt_x(a, b)
            if label:
                ax.text(x, top * 1.8, label, ha="center", va="bottom", fontsize=8.5, color=INK,
                        fontweight="semibold")
        ax.set_title(title, pad=6)
        ax.set_xticks(xs)
        ax.set_xticklabels([size_label(r["files_requested"]) for r in runs])
        ax.set_xlim(-0.6, len(runs) - 0.4)
        ax.tick_params(axis="x", length=0, pad=3)
        ax.set_xlabel("files in the run", color=INK2, fontsize=8, labelpad=4)
        ax.grid(True, axis="y", which="major")
        ax.grid(False, axis="y", which="minor")
        ax.yaxis.set_major_locator(LogLocator(base=10, numticks=8))
        ax.yaxis.set_minor_formatter(NullFormatter())
        ax.yaxis.set_major_formatter(FuncFormatter(lambda v, _: fmt_secs(v).replace(".00", "").replace(".0 ", " ")))
        if k == 0:
            ax.set_ylabel("seconds, log scale", color=INK2)
        ax.spines["left"].set_visible(k == 0)
    fig.text(0.075, 0.965, "Git commands: raw trajectory tree vs TrajFS store", fontsize=11,
             fontweight="semibold", color=INK, ha="left", va="top")
    fig.text(0.075, 0.905, "Whole-process wall time on runs of 100k, 300k, and 1M files; the number above each pair is the speedup.\n"
             "add + commit is git add -A + git commit versus traj pack + traj commit; both read every byte of the tree\n"
             "(traj pack re-reads unchanged files as its change check). checkout is the round trip between the two commits.",
             fontsize=7.8, color=INK2, ha="left", va="top", linespacing=1.4)
    h, l = axes[0].get_legend_handles_labels()
    fig.legend(h, l, loc="upper right", bbox_to_anchor=(0.995, 0.995), ncol=2, handlelength=1.0,
               handleheight=1.0, columnspacing=1.2)
    fig.savefig(out)
    plt.close(fig)


# ---------------------------------------------------------------------------- chart 2: space

def plot_space(data, out):
    runs = [r for r in data["runs"] if "git" in r and "trajfs" in r]
    raw = [r["raw"].get("bytes_after_extra_round") or r["raw"]["bytes"] for r in runs]
    gitb = [r["git"].get("bare_repo_bytes") for r in runs]
    store = [r["trajfs"].get("store_bytes") for r in runs]
    gb = lambda b: (b or 0) / 1e9  # noqa: E731
    fig, ax = plt.subplots(figsize=(9.2, 3.7), dpi=DPI)
    fig.subplots_adjust(left=0.075, right=0.995, top=0.76, bottom=0.15)
    xs = list(range(len(runs)))
    w = 0.22
    series = [("raw tree on disk", raw, C_THIRD, -1), ("git objects, packed", gitb, C_GIT, 0),
              ("TrajFS store", store, C_TRAJ, 1)]
    ymax = max(gb(b) for b in raw + gitb + store) * 1.5
    for label, vals, color, off in series:
        bars = ax.bar([x + off * (w + 0.02) for x in xs], [gb(v) for v in vals], w, color=color, label=label, zorder=3)
        thin_bars(ax, bars)
        for x, v in zip(xs, vals):
            ax.text(x + off * (w + 0.02), gb(v) + ymax * 0.015, fmt_bytes(v), ha="center", va="bottom",
                    fontsize=7.5, color=INK2)
    def rel(a, b):
        if not a or not b:
            return "n/a"
        return f"{fmt_x(a, b)} smaller" if a >= b else f"{fmt_x(b, a)} larger"
    for x, r, rb, gbytes, sb in zip(xs, runs, raw, gitb, store):
        if sb:
            facts = r["trajfs"].get("store_facts") or {}
            share = (f"\n{100 * facts['distinct_contents'] / facts['paths']:.0f}% of paths have distinct contents"
                     if facts.get("paths") and facts.get("distinct_contents") else "")
            ax.text(x, ymax * 0.985, f"store: {rel(rb, sb)} than the tree,\n{rel(gbytes, sb)} than packed git objects"
                    + share, ha="center", va="top", fontsize=7.8, color=INK, linespacing=1.3)
    ax.set_ylim(0, ymax)
    ax.set_xticks(xs)
    ax.set_xticklabels([f"{size_label(r['files_requested'])} files, {r['rounds'] + 1} rounds" for r in runs])
    ax.set_xlim(-0.6, len(runs) - 0.4)
    ax.tick_params(axis="x", length=0, pad=4)
    ax.set_ylabel("GB", color=INK2)
    ax.yaxis.set_major_formatter(FuncFormatter(lambda v, _: f"{v:g}"))
    fig.text(0.075, 0.965, "Space: raw tree, packed git objects, TrajFS store", fontsize=11,
             fontweight="semibold", color=INK, ha="left", va="top")
    fig.text(0.075, 0.905, "Bytes after both commits (du -sb). Git objects are the bare repository after push, so they "
             "are packed and delta-compressed.\nThe store includes catalog, packs and the parsed event tables.",
             fontsize=7.8, color=INK2, ha="left", va="top", linespacing=1.4)
    fig.legend(loc="upper right", bbox_to_anchor=(0.995, 0.995), ncol=3, handlelength=1.0, handleheight=1.0,
               columnspacing=1.2)
    fig.savefig(out)
    plt.close(fig)


# -------------------------------------------------------------------------- chart 3: read path

def plot_read_path(data, out):
    run = next((r for r in reversed(data["runs"]) if "read_path" in r), None)
    if run is None:
        print("no read_path in results; skipping read-path.png", file=sys.stderr)
        return None
    rp = run["read_path"]
    mount = rp.get("mount", {})
    mount_ok = "skipped" not in mount

    def ms(side, key):
        v = rp.get(side, {}).get(key)
        return v.get("median_ms") if isinstance(v, dict) else None

    def single(side, key):
        v = rp.get(side, {}).get(key)
        return isinstance(v, dict) and v.get("single_shot")
    rows = [  # (label, raw, traj, mount)
        ("ls one directory", ms("raw", "ls"), ms("traj", "ls"), ms("mount", "ls")),
        ("cat one file", ms("raw", "cat"), ms("traj", "cat"), ms("mount", "cat")),
        ("find -name review.json, one round", ms("raw", "find_round"), ms("traj", "find_round"), ms("mount", "find_round")),
        ("grep -rl Traceback in *.stderr, one round", ms("raw", "grep_round"), ms("traj", "grep_round"), ms("mount", "grep_round")),
        ("find -name review.json, whole tree" + (" *" if single("mount", "find") else ""),
         ms("raw", "find"), ms("traj", "find"), ms("mount", "find")),
        ("grep -rl Traceback in *.stderr, whole tree" + (" *" if single("mount", "grep") else ""),
         ms("raw", "grep"), ms("traj", "grep"), ms("mount", "grep")),
        ("sql: count(*) from files", None, ms("traj", "sql_count"), None),
        ("sql: tool calls from events", None, ms("traj", "sql_tool_calls"), None),
    ]
    any_single = any(single("mount", k) for k in ("find", "grep"))
    series = [("raw files on disk", 1, C_GIT), ("traj command", 2, C_TRAJ), ("FUSE mount", 3, C_THIRD)]
    fig, ax = plt.subplots(figsize=(9.2, 5.4), dpi=DPI)
    fig.subplots_adjust(left=0.30, right=0.985, top=0.865, bottom=0.09)
    h = 0.24
    ys = list(range(len(rows)))[::-1]
    vals_all = [v for r in rows for v in r[1:] if v]
    xmin, xmax = 1, 10 ** (int(__import__("math").log10(max(vals_all))) + 1.5)
    ax.set_xscale("log")
    ax.set_xlim(xmin, xmax)
    for label, idx, color in series:
        vals = [r[idx] for r in rows]
        yy = [y + (2 - idx) * (h + 0.02) for y in ys]
        bars = ax.barh(yy, [(v or xmin) - xmin for v in vals], h, left=xmin, color=color, label=label, zorder=3)
        thin_bars(ax, bars)
        for y, v in zip(yy, vals):
            if v:
                ax.text(v * 1.15, y, fmt_ms(v), va="center", ha="left", fontsize=7.5, color=INK2)
    if not mount_ok:
        ax.text(xmin * 1.15, ys[0] - (h + 0.02), "mount skipped: " + str(mount.get("skipped"))[:60],
                va="center", ha="left", fontsize=7, color=MUTED)
    ax.set_yticks(ys)
    ax.set_yticklabels([r[0] for r in rows])
    ax.set_ylim(-0.6, len(rows) - 0.4)
    ax.tick_params(axis="y", length=0, pad=6)
    ax.grid(False, axis="y")
    ax.grid(True, axis="x", which="major")
    ax.xaxis.set_major_locator(LogLocator(base=10, numticks=10))
    ax.xaxis.set_major_formatter(FuncFormatter(lambda v, _: fmt_ms(v)))
    ax.xaxis.set_minor_formatter(NullFormatter())
    ax.spines["left"].set_visible(False)
    ax.set_xlabel("median whole-process wall time, warm cache, log scale"
                  + ("   (* mount walk measured once, no warm-up)" if any_single else ""), color=INK2)
    n = run["raw"]["file_count"] + run.get("extra_round", {}).get("files", 0)
    fig.text(0.02, 0.965, f"Read path on the {size_label(run['files_requested'])}-file tree", fontsize=11,
             fontweight="semibold", color=INK, ha="left", va="top")
    fig.text(0.02, 0.92, f"{n:,} files, {run['rounds'] + 1} rounds; median of {rp['runs']} runs after one warm-up. "
             "Raw files are read in place; traj commands and the mount read the store.\n"
             f"One round = {rp.get('round_dir', 'one round directory')}, the largest checkpoint.",
             fontsize=7.8, color=INK2, ha="left", va="top", linespacing=1.4)
    ax.legend(loc="lower right", ncol=3, handlelength=1.0, handleheight=1.0, columnspacing=1.2, borderaxespad=0.3)
    fig.savefig(out)
    plt.close(fig)
    return run


# ---------------------------------------------------------------------------------- README

def write_readme(data, path_json, out, read_run):
    h = data["host"]
    p = data["params"]
    runs = [r for r in data["runs"] if "git" in r and "trajfs" in r]
    rel_json = os.path.relpath(path_json, REPO)
    L = []
    L.append("# Benchmarks on synthetic trajectory trees\n")
    L.append("Read [git-at-scale.md](git-at-scale.md) first: it measures the case that hurts, a repository that already "
             "holds runs (git add of one more run grows from 118 s to 30 min with 2.7 M paths tracked, hours beyond) and a "
             "cold page cache. The numbers below are Git's best case, a fresh repository with a warm cache.\n")
    L.append("Every number here comes from [`bench/run.py`](../../bench/run.py) on generated data, so anyone can "
             "reproduce it without access to private trajectories. The source file for this page is "
             f"[`{rel_json}`](../../{rel_json}); [`bench/plot.py`](../../bench/plot.py) renders the charts and this "
             "table from it.\n")
    L.append("## Host\n")
    L.append("| | |\n|---|---|")
    L.append(f"| CPU | {h.get('cpu')} ({h.get('cores')} logical cores) |")
    L.append(f"| Memory | {h.get('memory_gb')} GB |")
    L.append(f"| Kernel | {h.get('kernel')} |")
    L.append(f"| Filesystem | {h.get('workspace_fs')} |")
    L.append(f"| traj | {h.get('traj_version')} (release build) |")
    L.append(f"| git | {h.get('git_version')} |")
    L.append(f"| Run | started {data.get('started_utc')}, total {data.get('total_seconds', 0) / 60:.0f} min |\n")
    L.append("## Reproduce\n")
    L.append("```bash\ncargo build --release -p traj\n"
             f"python3 bench/run.py --sizes {','.join(str(s) for s in p['sizes'])} --rounds "
             f"{','.join(str(r) for r in p['rounds'])} --read-runs {p['read_runs']}\n"
             "python3 bench/plot.py            # newest bench/results/*.json -> docs/benchmarks/\n```\n")
    L.append("`run.py` needs only the Python standard library; `plot.py` needs matplotlib. Git runs with an "
             "isolated configuration (`" + ", ".join(f"{k}={v}" for k, v in p["git_config"].items()) +
             "`), a local bare repository as the remote, and `git clone` from that bare repository. "
             "The raw tree's repository keeps its `GIT_DIR` outside the tree so `traj pack` never sees a `.git` "
             "directory. TrajFS packs with `--adapter copilot-cli` and the default retention rules, commits with "
             "`traj commit <store>` inside a plain git repository that holds only the store, and adds the extra "
             "round as a second batch (`--label`). Any single command over "
             f"{p['git_timeout_min']:g} minutes is recorded as aborted and larger sizes are skipped.\n")
    if p.get("cache"):
        L.append("All measurements are **warm-cache**: " + p["cache"].split(": ", 1)[1] + ". Both sides therefore "
                 "start from an identical page-cache state.\n")
    L.append("## Git commands\n")
    L.append("![git commands](git-commands.png)\n")
    L.append("Whole-process wall time. *add + commit* is `git add -A` + `git commit` versus `traj pack` + "
             "`traj commit`; the ingest comparison is apples to apples because both read every byte of the tree "
             "(`traj pack` includes a full re-read of unchanged files as its change check, which is also why the "
             "second, one-round batch is not free). *checkout* is the round trip from the second commit to the "
             "first and back.\n")
    hdr = "| Files | Rounds | Command | git on raw tree | TrajFS | Speedup |\n|---:|---:|---|---:|---:|---:|"
    L.append(hdr)
    for r in runs:
        for title, a, b in operations(r):
            L.append(f"| {r['raw']['file_count']:,} | {r['rounds']}+1 | {title} | {fmt_secs(a)} | {fmt_secs(b)} | {fmt_x(a, b)} |")
    L.append("")
    L.append("Individual steps:\n")
    L.append("| Files | git add | git commit | git add (2nd) | git commit (2nd) | traj pack | traj commit | traj pack (2nd) | traj commit (2nd) |\n"
             "|---:|---:|---:|---:|---:|---:|---:|---:|---:|")
    for r in runs:
        g, t = r["git"], r["trajfs"]
        L.append(f"| {r['raw']['file_count']:,} | {fmt_secs(secs(g.get('add')))} | {fmt_secs(secs(g.get('commit')))} | "
                 f"{fmt_secs(secs(g.get('add2')))} | {fmt_secs(secs(g.get('commit2')))} | {fmt_secs(secs(t.get('pack')))} | "
                 f"{fmt_secs(secs(t.get('commit')))} | {fmt_secs(secs(t.get('pack2')))} | {fmt_secs(secs(t.get('commit2')))} |")
    L.append("")
    L.append("## Space\n")
    L.append("![space](space.png)\n")
    L.append("| Files | Raw tree | `.git` working repo (loose) | git objects, packed (bare) | TrajFS store | of which events tables | Store repo `.git` | Distinct contents |\n"
             "|---:|---:|---:|---:|---:|---:|---:|---:|")
    for r in runs:
        g, t = r["git"], r["trajfs"]
        facts = t.get("store_facts") or {}
        L.append(f"| {r['raw']['file_count']:,} | {fmt_bytes(r['raw'].get('bytes_after_extra_round'))} | "
                 f"{fmt_bytes(g.get('git_dir_bytes'))} | {fmt_bytes(g.get('bare_repo_bytes'))} | "
                 f"{fmt_bytes(t.get('store_bytes'))} | {fmt_bytes(t.get('store_derived_bytes'))} | {fmt_bytes(t.get('git_dir_bytes'))} | "
                 + (f"{facts['distinct_contents']:,} of {facts['paths']:,} paths |" if facts.get("paths") else "n/a |"))
    L.append("\nSizes are `du -sb` after both commits. The store size includes the parsed `events` tables "
             "(`derived/`), which `--no-derive` would omit.\n")
    L.append("Deduplication, as reported by the `traj pack` summary line (first batch; sizes as traj prints them) "
             "and by `traj sql` after both batches:\n")
    L.append("| Files | Batch-1 paths | New blobs | Blob bytes raw | Blob bytes packed | Distinct contents after both batches |\n"
             "|---:|---:|---:|---:|---:|---:|")
    for r in runs:
        t = r["trajfs"]
        ps = t.get("pack_summary") or {}
        facts = t.get("store_facts") or {}
        share = (f"{facts['distinct_contents']:,} of {facts['paths']:,} ({100 * facts['distinct_contents'] / facts['paths']:.1f}%)"
                 if facts.get("paths") and facts.get("distinct_contents") else "n/a")
        L.append(f"| {r['raw']['file_count']:,} | {ps.get('paths', 0):,} ({ps.get('paths_size', 'n/a')}) | "
                 f"{ps.get('new_blobs', 0):,} | {ps.get('new_blobs_raw', 'n/a')} | {ps.get('new_blobs_packed', 'n/a')} | {share} |")
    L.append("")
    if read_run is not None:
        rp = read_run["read_path"]
        L.append("## Read path\n")
        L.append("![read path](read-path.png)\n")
        n = read_run["raw"]["file_count"] + read_run.get("extra_round", {}).get("files", 0)
        L.append(f"On the {n:,}-file tree; median of {rp['runs']} whole-process runs after one warm-up, warm page cache, "
                 "except that whole-tree walks through the FUSE mount are measured once with no warm-up (marked). "
                 "`ls` lists `" + rp["ls_dir"] + "`; `cat` reads `" + rp["cat_file"] + "`; one round is `"
                 + rp.get("round_dir", "") + "`, the largest checkpoint. A recursive walk through the mount visits "
                 "every duplicate path, which is why `traj grep`/`traj find` (one pass over distinct contents and "
                 "the catalog) are the recommended whole-store tools.\n")
        L.append("| Operation | Raw files | `traj` command | FUSE mount |\n|---|---:|---:|---:|")

        def cell(side, key):
            v = rp.get(side, {}).get(key)
            if side == "mount" and "skipped" in rp.get("mount", {}):
                return "skipped"
            return fmt_ms(v.get("median_ms")) if isinstance(v, dict) else "-"
        for key, label in (("ls", "ls one directory"), ("cat", "cat one file"),
                           ("find_round", "find -name review.json, one round"),
                           ("grep_round", "grep -rl Traceback --include='*.stderr', one round"),
                           ("find", "find -name review.json, whole tree"),
                           ("grep", "grep -rl Traceback --include='*.stderr', whole tree")):
            star = " (1 run)" if (rp.get("mount", {}).get(key) or {}).get("single_shot") else ""
            L.append(f"| {label} | {cell('raw', key)} | {cell('traj', key)} | {cell('mount', key)}{star} |")
        L.append(f"| sql: `select count(*) from files` | - | {cell('traj', 'sql_count')} | - |")
        L.append(f"| sql: tool-call counts over events | - | {cell('traj', 'sql_tool_calls')} | - |")
        facts = read_run["trajfs"].get("store_facts") or {}
        if facts.get("event_rows"):
            L.append(f"\nThe events query groups {facts['event_rows']:,} parsed event rows by tool name.")
        L.append("")
    L.append("## What synthetic data does and does not capture\n")
    facts = (runs[-1]["trajfs"].get("store_facts") or {}) if runs else {}
    if facts.get("paths") and facts.get("distinct_contents"):
        dup = (f"In the largest tree here, {facts['distinct_contents']:,} of {facts['paths']:,} recorded paths "
               f"({100 * facts['distinct_contents'] / facts['paths']:.0f}%) have distinct contents; the real Task A "
               "recorded in the main README had 23,685 distinct contents among 2.1 million paths (about 1%), so "
               "the model is conservative and real trees deduplicate better still. ")
    else:
        dup = ""
    L.append("The generator (`bench/synthetic_tree.py`) models what makes real trajectory trees hard for Git: "
             "hundreds of thousands to millions of small files in a rounds layout where every round is a checkpoint "
             "of the whole task directory, so round *r* carries the workspace again (about 2% of its files edited "
             "that round) plus the logs of every round up to *r*, together with Copilot-CLI-style `events.jsonl` "
             "streams and a few build products for the retention rules to exclude. Most paths are therefore "
             "byte-for-byte copies of files in earlier rounds, and later rounds are larger than earlier ones. " + dup +
             "Contents are log-like and code-like text drawn from a small vocabulary, which compresses in the normal "
             "range for text but not exactly like real tool output, and the size distribution (log-normal, median "
             "near 1 KB, long tail to a few hundred KB) is a guess rather than a measurement. The tree has no binary "
             "data, no very large files and one task with three roles, and every command ran on a local ext4 disk "
             "with a warm page cache, so treat these results as a controlled comparison of the two workflows on the "
             "same tree, not as a prediction of any particular dataset's numbers.\n")
    with open(out, "w") as f:
        f.write("\n".join(L))


def main():
    import argparse
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("results", nargs="?", default=None, help="results JSON [default: newest in bench/results/]")
    ap.add_argument("--out-dir", default=OUT_DIR, help="where to write the PNGs and README.md")
    a = ap.parse_args()
    data, path = load(a.results)
    os.makedirs(a.out_dir, exist_ok=True)
    plot_git_commands(data, os.path.join(a.out_dir, "git-commands.png"))
    plot_space(data, os.path.join(a.out_dir, "space.png"))
    read_run = plot_read_path(data, os.path.join(a.out_dir, "read-path.png"))
    write_readme(data, path, os.path.join(a.out_dir, "README.md"), read_run)
    print(f"wrote {os.path.relpath(a.out_dir, REPO)}/ from {os.path.relpath(path, REPO)}")


if __name__ == "__main__":
    main()
