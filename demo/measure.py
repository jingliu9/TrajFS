#!/usr/bin/env python3
"""Measure the numbers the TrajFS demo GIF shows. Stdlib only.

Generates a synthetic trajectory tree with bench/synthetic_tree.py, times the REAL git commands
on the raw files, then the REAL traj commands on the same tree, and writes everything the
recording needs to a JSON file (default demo/measurements.json).

Layout under --root (default /workspace/trajfs-demo):

    runs/task-a                 the raw trajectory tree (kept; record.py does not need it)
    git/raw                     git repo whose work tree is runs/ (removed unless --keep-git)
    git/remote.git, git/clone   push target and clone (removed unless --keep-git)
    exp/                        the "experiment repo": a git repo holding stores/task-a.trajstore (kept)
    exp-remote.git, exp-clone   push target and clone of the experiment repo (kept, tiny)
    mnt/task-a                  mountpoint used to capture the FUSE outputs (unmounted afterwards)

Usage:
    python3 demo/measure.py                              # 1,000,000 files, 40 rounds -> demo/measurements.json
    python3 demo/measure.py --files 20000 --rounds 10 --out demo/measurements.small.json
"""

import argparse
import json
import os
import shutil
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)
GIT_ENV = {
    "GIT_AUTHOR_NAME": "demo", "GIT_AUTHOR_EMAIL": "demo@example.com",
    "GIT_COMMITTER_NAME": "demo", "GIT_COMMITTER_EMAIL": "demo@example.com",
}


def log(msg):
    print(f"[measure] {msg}", flush=True)


def run(cmd, cwd=None, env=None, check=True, timeout=None):
    """Run a command, return (seconds, stdout, stderr)."""
    e = dict(os.environ)
    e.update(GIT_ENV)
    if env:
        e.update(env)
    log("$ " + " ".join(cmd))
    t0 = time.perf_counter()
    p = subprocess.run(cmd, cwd=cwd, env=e, text=True, capture_output=True, timeout=timeout)
    dt = time.perf_counter() - t0
    if check and p.returncode != 0:
        sys.exit(f"command failed ({p.returncode}) after {dt:.1f}s: {' '.join(cmd)}\n{p.stderr}")
    log(f"  {dt:.2f}s")
    return dt, p.stdout, p.stderr


def du_bytes(path):
    """Disk usage in bytes (what `du -sh` reports), and apparent bytes."""
    used = int(subprocess.check_output(["du", "-s", "-B1", path], text=True).split()[0])
    apparent = int(subprocess.check_output(["du", "-s", "-b", path], text=True).split()[0])
    return used, apparent


def du_human(path):
    return subprocess.check_output(["du", "-sh", path], text=True).split()[0]


def count_files(path):
    dt, out, _ = run(["bash", "-c", f"find {path} -type f | wc -l"])
    return int(out.strip()), dt


def head(text, n):
    lines = text.splitlines()
    return lines[:n], len(lines)


def settle_and_warm(tree):
    """Make both sides start from the same state: flush dirty pages left by generation (writeback
    would otherwise slow whichever side runs first), then read every file once so the page cache
    is warm. Returns the seconds spent, which are not part of any measurement."""
    t0 = time.time()
    os.sync()
    for _ in range(120):
        dirty_kb = 0
        with open("/proc/meminfo") as f:
            for line in f:
                if line.startswith("Dirty:"):
                    dirty_kb = int(line.split()[1])
        if dirty_kb < 50_000:
            break
        time.sleep(0.5)
    run(["tar", "-cf", "/dev/null", "-C", os.path.dirname(tree), os.path.basename(tree)], timeout=None)
    return round(time.time() - t0, 1)


def cpu_model():
    """A neutral host description (CPU model and core count), never the machine's name."""
    model = "unknown CPU"
    try:
        with open("/proc/cpuinfo") as f:
            for line in f:
                if line.startswith("model name"):
                    model = line.split(":", 1)[1].strip()
                    break
    except OSError:
        pass
    return f"{model}, {os.cpu_count()} cores"


def measure_git(root, runs_dir, keep):
    """Time git on the raw tree. The git dir lives outside the tree so `traj pack` sees a plain
    directory; `gc.auto=0` keeps a background auto-gc from overlapping the later timings."""
    gdir = os.path.join(root, "git")
    shutil.rmtree(gdir, ignore_errors=True)
    os.makedirs(gdir)
    raw = os.path.join(gdir, "raw")
    run(["git", "init", "-q", "-b", "main", raw])
    g = ["git", f"--git-dir={raw}/.git", f"--work-tree={runs_dir}", "-c", "gc.auto=0"]
    res = {}
    res["add"] = {"cmd": "git add -A", "seconds": run(g + ["add", "-A"])[0]}
    res["commit"] = {"cmd": 'git commit -m "round 40"', "seconds": run(g + ["commit", "-q", "-m", "round 40"])[0]}
    res["status"] = {"cmd": "git status", "seconds": run(g + ["status", "--short"])[0]}
    remote = os.path.join(gdir, "remote.git")
    run(["git", "init", "-q", "--bare", "-b", "main", remote])
    res["push"] = {"cmd": "git push origin main", "seconds": run(g + ["push", "-q", remote, "main"])[0]}
    git_dir_bytes = du_bytes(os.path.join(raw, ".git"))[0]
    remote_bytes = du_bytes(remote)[0]
    clone = os.path.join(gdir, "clone")
    # file:// forces a real pack transfer instead of hardlinking the object store.
    res["clone"] = {"cmd": "git clone --no-checkout", "seconds": run(["git", "clone", "-q", "--no-checkout", f"file://{remote}", clone])[0]}
    res["checkout"] = {"cmd": "git checkout main", "seconds": run(["git", "-C", clone, "checkout", "-q", "main"])[0]}
    out = {"commands": res, "git_dir_bytes": git_dir_bytes, "remote_git_bytes": remote_bytes,
           "clone_git_bytes": du_bytes(os.path.join(clone, ".git"))[0]}
    if not keep:
        log("removing git/ (use --keep-git to keep it)")
        shutil.rmtree(gdir, ignore_errors=True)
    return out


def measure_traj(root, tree, traj):
    exp = os.path.join(root, "exp")
    for p in (exp, os.path.join(root, "exp-remote.git"), os.path.join(root, "exp-clone")):
        shutil.rmtree(p, ignore_errors=True)
    os.makedirs(os.path.join(exp, "stores"))
    run(["git", "init", "-q", "-b", "main", exp])
    store_rel = "stores/task-a.trajstore"
    store = os.path.join(exp, store_rel)
    res = {"exp_dir": exp, "store": store, "store_rel": store_rel}

    dt, out, err = run([traj, "pack", tree, "--out", store_rel, "--adapter", "copilot-cli"], cwd=exp)
    summary = [l for l in out.splitlines() if l.startswith("batch ")]
    res["pack"] = {"cmd": f"traj pack runs/task-a --out {store_rel} --adapter copilot-cli",
                   "seconds": dt, "summary": summary[-1].replace(store, store_rel) if summary else out.strip()}
    res["store_bytes"], res["store_apparent_bytes"] = du_bytes(store)
    res["store_files"] = sum(len(f) for _, _, f in os.walk(store))

    dt, out, err = run([traj, "commit", store_rel], cwd=exp)
    res["commit"] = {"cmd": f"traj commit {store_rel}", "seconds": dt, "output": (out + err).strip().splitlines()[-1]}
    res["exp_git_bytes"] = du_bytes(os.path.join(exp, ".git"))[0]
    res["status"] = {"cmd": "git status", "seconds": run(["git", "status", "--short"], cwd=exp)[0]}
    remote = os.path.join(root, "exp-remote.git")
    run(["git", "init", "-q", "--bare", "-b", "main", remote])
    res["push"] = {"cmd": "git push origin main", "seconds": run(["git", "push", "-q", remote, "main"], cwd=exp)[0]}
    clone = os.path.join(root, "exp-clone")
    res["clone"] = {"cmd": "git clone", "seconds": run(["git", "clone", "-q", f"file://{remote}", clone])[0]}

    env = {"TRAJ_STORE": store}
    reads = {}

    def cap(key, args, cwd=exp, keep=None):
        dt, out, err = run([traj] + args, cwd=cwd, env=env)
        lines = out.splitlines()
        reads[key] = {"cmd": "traj " + " ".join(args), "seconds": dt, "lines": lines if keep is None else lines[:keep],
                      "total_lines": len(lines)}

    cap("ls", ["ls", "-l"])
    cap("ls_rounds", ["ls", "rounds"], keep=6)
    cap("du", ["du", "--depth", "1"])
    cap("grep", ["grep", "-l", "-e", "Traceback", "--name", "*.stderr"], keep=8)
    first_hit = reads["grep"]["lines"][0] if reads["grep"]["lines"] else "rounds/round-0001/builder/logs/call-00037.stderr"
    rnd = first_hit.split("/")[1]
    res["first_traceback_path"] = first_hit
    res["first_traceback_round"] = rnd
    sql = ("select round, verdict, tests_passed from (select json_extract_string(text(sha),'$.round')::int as round, "
           "json_extract_string(text(sha),'$.verdict') as verdict, json_extract_string(text(sha),'$.tests_passed')::int "
           "as tests_passed from files where name = 'review.json') order by round limit 4")
    cap("sql", ["sql", sql])
    cap("cat", ["cat", f"rounds/{rnd}/reviewer/review.json"])
    cap("tree", ["tree", "--depth", "2", f"rounds/{rnd}"], keep=12)
    res["reads"] = reads

    # FUSE view: mount, list, read, unmount. Captured so record.py can replay it without FUSE.
    mnt = os.path.join(root, "mnt", "task-a")
    subprocess.run([traj, "umount", mnt], capture_output=True)
    os.makedirs(mnt, exist_ok=True)
    mount = {"mountpoint": mnt}
    dt, out, err = run([traj, "mount", mnt, "--daemon", "--no-vscode"], env=env, check=False)
    if "mounted" in out + err:
        mount["seconds"] = dt
        mount["output"] = (out + err).strip().splitlines()[-1]
        dt, out, _ = run(["bash", "-c", f"ls {mnt}/rounds | head -4"])
        mount["ls_rounds"] = out.splitlines()
        dt, out, _ = run(["cat", f"{mnt}/rounds/{rnd}/reviewer/review.json"])
        mount["cat"] = out.splitlines()
        mount["cat_seconds"] = dt
        run([traj, "umount", mnt], env=env, check=False)
    else:
        mount["error"] = (out + err).strip()
        log("mount unavailable: " + mount["error"])
    res["mount"] = mount
    return res


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--files", type=int, default=1_000_000)
    ap.add_argument("--rounds", type=int, default=40)
    ap.add_argument("--root", default="/workspace/trajfs-demo")
    ap.add_argument("--out", default=os.path.join(HERE, "measurements.json"))
    ap.add_argument("--traj", default=os.path.join(REPO, "target", "release", "traj"))
    ap.add_argument("--force", action="store_true", help="re-measure even if --out exists")
    ap.add_argument("--keep-git", action="store_true", help="keep the multi-GB raw-tree git repos")
    ap.add_argument("--regen", action="store_true", help="delete and regenerate the tree even if its size matches")
    a = ap.parse_args()

    if os.path.exists(a.out) and not a.force:
        log(f"{a.out} exists; use --force to re-measure")
        return
    if not os.access(a.traj, os.X_OK):
        sys.exit(f"traj binary not found at {a.traj}; run: cargo build --release -p traj")
    os.makedirs(a.root, exist_ok=True)
    runs_dir = os.path.join(a.root, "runs")
    tree = os.path.join(runs_dir, "task-a")
    t_all = time.time()
    if a.regen:
        shutil.rmtree(tree, ignore_errors=True)
    if os.path.exists(tree):
        have, _ = count_files(tree)
        if abs(have - a.files) > max(1000, a.files // 50):
            log(f"{tree} has {have:,} files, not ~{a.files:,}: regenerating")
            shutil.rmtree(tree)
        else:
            log(f"reusing existing tree {tree} ({have:,} files)")
            gen = {"reused": True}
    if not os.path.exists(tree):
        os.makedirs(runs_dir, exist_ok=True)
        dt, out, _ = run([sys.executable, os.path.join(REPO, "bench", "synthetic_tree.py"), tree,
                          "--files", str(a.files), "--rounds", str(a.rounds)], timeout=None)
        gen = json.loads(out.strip().splitlines()[-1])
        gen["seconds"] = dt

    m = {"created": time.strftime("%Y-%m-%d %H:%M:%S"), "host": cpu_model(),
         "cpus": os.cpu_count(), "root": a.root, "tree": tree, "requested_files": a.files, "rounds": a.rounds,
         "generate": gen}
    m["file_count"], m["find_seconds"] = count_files(tree)
    m["tree_bytes"], m["tree_apparent_bytes"] = du_bytes(tree)
    m["du_sh"] = du_human(tree)
    m["round_dirs"] = sorted(d for d in os.listdir(os.path.join(tree, "rounds")))
    dt, out, _ = run(["tree", "-L", "2", "--noreport", f"{tree}/rounds/{m['round_dirs'][len(m['round_dirs']) // 2]}"], check=False)
    m["tree_listing"] = out.replace(tree, "runs/task-a").replace("\u00a0", " ").splitlines()
    git_v = run(["git", "--version"])[1].strip()
    traj_v = run([a.traj, "--version"])[1].strip()
    m["versions"] = {"git": git_v, "traj": traj_v}

    log("settling writeback and warming the page cache before timing anything")
    m["warm_seconds_before_git"] = settle_and_warm(tree)
    m["git"] = measure_git(a.root, runs_dir, a.keep_git)
    m["warm_seconds_before_traj"] = settle_and_warm(tree)
    m["traj"] = measure_traj(a.root, tree, a.traj)
    m["cache"] = "warm: both sides measured after sync + a full read of the tree"
    m["total_seconds"] = round(time.time() - t_all, 1)

    os.makedirs(os.path.dirname(a.out), exist_ok=True)
    with open(a.out, "w") as f:
        json.dump(m, f, indent=2)
    log(f"wrote {a.out} ({m['total_seconds']} s total)")


if __name__ == "__main__":
    main()
