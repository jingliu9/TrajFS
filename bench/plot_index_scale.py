#!/usr/bin/env python3
"""Chart for git_index_scale.py: seconds of `git add` for one 1M-file run against the number of paths
the repository already tracks, with the fitted quadratic and its extrapolation to a repository that
already holds several runs. Writes docs/benchmarks/git-index-scale.png.

    venv/bin/python bench/plot_index_scale.py [results.json]
"""

import glob
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from plot import C_GIT, C_TRAJ, INK2, MUTED, DPI, fmt_secs, plt, REPO  # noqa: E402

import numpy as np  # noqa: E402


def load(path=None):
    if path is None:
        cands = sorted(glob.glob(os.path.join(REPO, "bench", "results", "index-scale-*.json")))
        if not cands:
            sys.exit("no bench/results/index-scale-*.json; run bench/git_index_scale.py first")
        path = cands[-1]
    return json.load(open(path)), path


def fit(prior, secs):
    """Least-squares quadratic add_seconds = a + b*M + c*M^2 in the paths already tracked (M).
    Inserting N new paths into a sorted index of M entries shifts on average M + N/2 entries per
    insertion, so the cost is at least linear in M; the measured points bend upward beyond that
    (the index no longer fits in cache), which the M^2 term absorbs. Extrapolation is indicative."""
    c, b, a = np.polyfit(prior, secs, 2)
    return a, b, c


def main():
    data, path = load(sys.argv[1] if len(sys.argv) > 1 else None)
    st = data["stages"]
    prior = np.array([s["prior_paths"] for s in st], dtype=float)
    secs = np.array([s["add_seconds"] for s in st], dtype=float)
    n_real = st[0]["real_files"]
    a, b, c = fit(prior, secs)
    # extrapolate to a repository that already holds several such runs
    xs = np.linspace(0, 12e6, 200)
    ys = a + b * xs + c * xs**2

    fig, ax = plt.subplots(figsize=(9, 4.4), dpi=DPI)
    ax.plot(xs / 1e6, ys / 60, color=C_GIT, lw=1.4, ls="--", alpha=0.7, label="fit: quadratic in paths already tracked")
    ax.plot(prior / 1e6, secs / 60, "o", color=C_GIT, ms=6, label="measured: git add of the 1M-file run")
    for p, s in zip(prior, secs):
        ax.annotate(fmt_secs(s), (p / 1e6, s / 60), textcoords="offset points", xytext=(8, -3), fontsize=8.5, color=INK2)
    for m in (5e6, 11e6):
        y = (a + b * m + c * m**2) / 60
        ax.plot([m / 1e6], [y], "o", mfc="white", mec=C_GIT, ms=6)
        ax.annotate(f"{m / 1e6:.0f} M paths tracked: ~{y / 60:.1f} h" if y >= 60 else f"{m / 1e6:.0f} M paths tracked: ~{y:.0f} min",
                    (m / 1e6, y), textcoords="offset points", xytext=(-6, 8), ha="right", fontsize=8.5, color=INK2)
    ax.axhline(0, color="none")
    ax.set_xlabel("paths already tracked by the repository (millions)")
    ax.set_ylabel("git add of one more run")
    ax.yaxis.set_major_formatter(plt.FuncFormatter(lambda v, _: f"{v / 60:.0f} h" if v >= 60 else f"{v:.0f} min"))
    ax.set_xlim(-0.2, 12.4)
    ax.set_ylim(0, max(ys.max() / 60 * 1.08, 1))
    fig.text(0.075, 0.95, "git add of one 1,000,000-file run, by repository size", fontsize=11, weight="bold")
    fig.text(0.075, 0.905,
             f"The same run added to a fresh repository and to repositories already tracking up to "
             f"{prior.max() / 1e6:.1f} M paths from earlier runs.",
             fontsize=8.3, color=MUTED)
    fig.text(0.075, 0.87,
             "Filled points are measured; open points extrapolate the fit. traj pack of the same run does not depend "
             "on the repository (dotted).",
             fontsize=8.3, color=MUTED)
    ax.axhline(11.5 / 60, color=C_TRAJ, lw=1.2, ls=":", label=f"traj pack of the same run: {fmt_secs(11.5)}")
    ax.legend(loc="upper left")
    fig.subplots_adjust(left=0.075, right=0.98, top=0.83, bottom=0.14)
    out = os.path.join(REPO, "docs", "benchmarks", "git-index-scale.png")
    fig.savefig(out)
    print(f"wrote {os.path.relpath(out, REPO)} from {os.path.relpath(path, REPO)}: fit add_s = {a:.0f} + {b * 1e6:.0f}*M + {c * 1e12:.0f}*M^2 (M = paths already tracked, millions)")


if __name__ == "__main__":
    main()
