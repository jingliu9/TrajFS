#!/usr/bin/env python3
"""How `git add` of one run scales with the paths a repository already holds.

An experiment repository accumulates runs. Git keeps every tracked path in one sorted index, and
adding N new paths to an index that already holds M is O(N * M) (each insertion shifts the array),
so the same `git add` gets slower the longer the repository lives. This script measures that on a
real 1,000,000-file run (generated with synthetic_tree.py) sitting next to K sibling runs of tiny
files (paths only, since the cost is per path, not per byte):

    for K in --prior:  fresh repo -> add + commit the K siblings -> time `git add` / `commit` /
                       `status` for the real run -> record index size

Output: one JSON line per K on stdout, and the same list written to --out. Plot with
`plot_index_scale.py`. Runs sequentially in one work tree; the siblings are kept between stages.
Wall time on a 40-core host: about 5 min for K=0, 13 min for K=2, 25 min for K=4 (the quadratic
term is the point).

Usage:
    python3 bench/git_index_scale.py --tree /data/synthetic-run --prior 0 2 4
"""

import argparse
import json
import os
import subprocess
import sys
import time
from multiprocessing import Pool

HERE = os.path.dirname(os.path.abspath(__file__))
SIBLING_ROUNDS = 40
SIBLING_FILES_PER_ROLE_DIR = 273  # 3 roles x 40 rounds x (1..40 carried log dirs) x 273 ~= 671k paths


def timed(cmd, **kw):
    t0 = time.perf_counter()
    r = subprocess.run(cmd, capture_output=True, text=True, **kw)
    return round(time.perf_counter() - t0, 1), r.returncode


def _sibling_round(args):
    root, r = args
    for role in ("builder", "reviewer", "tester"):
        for k in range(1, r + 1):
            d = os.path.join(root, f"rounds/round-{r:04d}/{role}/logs/round-{k:04d}")
            os.makedirs(d, exist_ok=True)
            for j in range(SIBLING_FILES_PER_ROLE_DIR):
                with open(os.path.join(d, f"call-{j:05d}.log"), "w") as f:
                    f.write("x\n")


def make_sibling(root):
    """A run-shaped tree of tiny files: the checkpoint layout of synthetic_tree.py, 2 bytes each."""
    if os.path.exists(root):
        return
    with Pool(min(32, os.cpu_count() or 4)) as p:
        p.map(_sibling_round, [(root, r) for r in range(1, SIBLING_ROUNDS + 1)])


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--tree", required=True, help="the real run to add (e.g. from synthetic_tree.py)")
    ap.add_argument("--prior", type=int, nargs="+", default=[0, 2, 4], help="numbers of sibling runs already committed")
    ap.add_argument("--work", default=None, help="scratch directory for the git dir and siblings (default: next to --tree)")
    ap.add_argument("--out", default=None, help="results JSON (default: bench/results/index-scale-<UTC>.json)")
    a = ap.parse_args()

    tree = os.path.abspath(a.tree.rstrip("/"))
    runs = os.path.dirname(tree)  # the work tree: the real run and its siblings side by side
    name = os.path.basename(tree)
    work = os.path.abspath(a.work or os.path.join(runs, "index-scale-work"))
    gitdir = os.path.join(work, "repo")
    out = a.out or os.path.join(HERE, "results", f"index-scale-{time.strftime('%Y-%m-%dT%H%M%SZ', time.gmtime())}.json")
    os.makedirs(os.path.dirname(out), exist_ok=True)

    def g(*args):
        return ["git", f"--git-dir={gitdir}/.git", f"--work-tree={runs}", "-c", "gc.auto=0",
                "-c", "core.fsmonitor=false", *args]

    real_files = int(subprocess.run(["bash", "-c", f"find {tree} -type f | wc -l"], capture_output=True, text=True).stdout)
    results = []
    for k in a.prior:
        subprocess.run(["rm", "-rf", f"{gitdir}/.git"], check=True)
        os.makedirs(gitdir, exist_ok=True)
        subprocess.run(["git", "init", "-q", "-b", "main", gitdir], check=True)
        subprocess.run(g("config", "user.email", "bench@example.com"), check=True)
        subprocess.run(g("config", "user.name", "bench"), check=True)
        siblings = [f"sibling-{i:02d}" for i in range(k)]
        t0 = time.perf_counter()
        for s in siblings:
            make_sibling(os.path.join(runs, s))
        rec = {"prior_runs": k, "real_files": real_files, "gen_seconds": round(time.perf_counter() - t0, 1)}
        # every sibling not in this stage must be invisible to git: add only the chosen ones and
        # exclude the rest through the work tree's info/exclude
        with open(os.path.join(gitdir, ".git", "info", "exclude"), "w") as f:
            for extra in sorted(d for d in os.listdir(runs) if d.startswith("sibling-") and d not in siblings):
                f.write(f"/{extra}/\n")
            f.write("/index-scale-work/\n")
        if siblings:
            rec["add_siblings_seconds"], _ = timed(g("add", "--", *siblings))
            rec["commit_siblings_seconds"], _ = timed(g("commit", "-q", "-m", "siblings"))
        rec["prior_paths"] = subprocess.run(g("ls-files"), capture_output=True, text=True).stdout.count("\n")
        rec["status_before_seconds"], _ = timed(g("status", "--short"))
        print(f"[index-scale] K={k}: {rec['prior_paths']:,} paths in the index; adding {name} ({real_files:,} files)",
              file=sys.stderr, flush=True)
        rec["add_seconds"], rc = timed(g("add", "--", name))
        rec["add_exit"] = rc
        rec["commit_seconds"], _ = timed(g("commit", "-q", "-m", name))
        rec["status_after_seconds"], _ = timed(g("status", "--short"))
        rec["paths_after"] = subprocess.run(g("ls-files"), capture_output=True, text=True).stdout.count("\n")
        rec["index_bytes"] = os.path.getsize(os.path.join(gitdir, ".git", "index"))
        results.append(rec)
        print(json.dumps(rec), flush=True)
        with open(out, "w") as f:
            json.dump({"tree": tree, "git_version": subprocess.run(["git", "--version"], capture_output=True, text=True).stdout.strip(),
                       "stages": results}, f, indent=2)
    print(f"[index-scale] wrote {out}", file=sys.stderr)


if __name__ == "__main__":
    main()
