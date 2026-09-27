#!/usr/bin/env python3
"""Reproducible TrajFS-vs-Git benchmark on synthetic trajectory trees. Standard library only.

For each size, the runner generates a deterministic rounds-style tree with bench/synthetic_tree.py,
then times (whole-process wall clock, one subprocess per command):

  git-raw   the tree committed directly: git add -A, git commit, git status, git push to a local
            bare repo, git clone from it, a second commit that adds one round, and git checkout
            back to the first commit and forward again.
  trajfs    the same tree packed with `traj pack --adapter copilot-cli` into a store that lives in
            its own small git repo: traj pack, traj commit, git status, git push, git clone, a
            second batch for the added round (`--label`), and the same checkout round trip.
  read path (largest size only) ls / cat / find / grep on the raw files, on `traj` commands, and
            through a `traj mount` FUSE mount, plus two `traj sql` queries; each measured
            --read-runs times after one warm-up, median reported.  find and grep are run both over
            the whole tree and bounded to one round; whole-tree walks through the mount take minutes
            at a million files and are measured once (flagged single_shot in the JSON).

Every measured side starts from the same warm page cache: after generating the tree, and again
after appending the extra round, the runner syncs, waits for dirty pages to drain and reads every
file once (timed, never counted) before the TrajFS side and again before the git side.

Results are written incrementally to bench/results/<UTC timestamp>.json.  Trees and repositories
are removed after each size unless --keep is given.

    python3 bench/run.py --sizes 100000                 # quick run, ~a few minutes
    python3 bench/run.py                                # 100k, 300k, 1M files (default)
"""

import argparse
import datetime as dt
import json
import os
import platform
import re
import signal
import statistics
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)
sys.path.insert(0, HERE)
import synthetic_tree  # noqa: E402

DEFAULT_SIZES = "100000,300000,1000000"
DEFAULT_ROUNDS = {100_000: 20, 300_000: 30, 1_000_000: 40}
LS_DIR = "rounds/round-0001/builder/logs/round-0001"
CAT_FILE = "rounds/round-0001/reviewer/review.json"
SQL_COUNT = "select count(*) from files"
SQL_TOOLS = ("select tool_name, count(*) as calls from events "
             "where type = 'tool.execution_complete' group by tool_name order by calls desc")


# --------------------------------------------------------------------------------------- helpers

def log(msg):
    ts = dt.datetime.now(dt.timezone.utc).strftime("%H:%M:%S")
    print(f"[{ts}] {msg}", file=sys.stderr, flush=True)


def timed(cmd, cwd=None, env=None, timeout=None, capture_stdout=False):
    """Run one command; return its wall-clock seconds and exit code.

    Only the subprocess is timed.  stdout is discarded unless capture_stdout (then its tail is
    kept), stderr's tail is kept for diagnostics.  On timeout the whole process group is killed
    and the record says so instead of a time.
    """
    shown = " ".join(cmd)
    rec = {"cmd": shown}
    out = subprocess.PIPE if capture_stdout else subprocess.DEVNULL
    t0 = time.perf_counter()
    p = subprocess.Popen(cmd, cwd=cwd, env=env, stdout=out, stderr=subprocess.PIPE,
                         start_new_session=True)
    try:
        so, se = p.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(p.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        p.wait()
        rec.update(seconds=None, exit=None, aborted=f">{int(timeout // 60)} min")
        log(f"  ABORTED after {timeout / 60:.0f} min: {shown}")
        return rec
    secs = time.perf_counter() - t0
    rec.update(seconds=round(secs, 3), exit=p.returncode)
    if se:
        rec["stderr_tail"] = se.decode("utf-8", "replace")[-400:]
    if capture_stdout and so:
        rec["stdout_tail"] = so.decode("utf-8", "replace")[-600:]
    flag = "" if p.returncode == 0 else f"  (exit {p.returncode})"
    log(f"  {secs:9.3f} s  {shown[:110]}{flag}")
    return rec


def run_out(cmd, cwd=None, env=None, check=True):
    p = subprocess.run(cmd, cwd=cwd, env=env, capture_output=True, text=True)
    if check and p.returncode != 0:
        raise RuntimeError(f"{' '.join(cmd)} failed ({p.returncode}): {p.stderr.strip()[-400:]}")
    return p.stdout


def du_bytes(path):
    return int(run_out(["du", "-sb", path]).split()[0])


def count_files(root):
    n = 0
    for _, _, files in os.walk(root):
        n += len(files)
    return n


def rm_rf(path):
    if os.path.lexists(path):
        subprocess.run(["rm", "-rf", path], check=False)


def dirty_mb():
    try:
        with open("/proc/meminfo") as f:
            for line in f:
                if line.startswith("Dirty:"):
                    return int(line.split()[1]) / 1024
    except OSError:
        pass
    return None


def settle_and_warm(tree, stage, dirty_limit_mb=50, max_wait_s=60):
    """Put the page cache in the same state before every measured side.

    Right after generation (or after appending a round) the kernel is still writing back gigabytes
    of dirty pages, which slows whatever runs first.  So: sync, wait until Dirty: in /proc/meminfo
    is below `dirty_limit_mb` (or `max_wait_s`), then read every file once (tar to /dev/null) so
    each side starts from an identical warm cache.  The read pass is timed but never counted."""
    t0 = time.perf_counter()
    d0 = dirty_mb()
    os.sync()
    while (dirty_mb() or 0) > dirty_limit_mb and time.perf_counter() - t0 < max_wait_s:
        time.sleep(0.5)
    d1 = dirty_mb()
    waited = time.perf_counter() - t0
    t1 = time.perf_counter()
    # GNU tar skips file contents when its output is /dev/null, so read for real: every file through cat.
    p = subprocess.run(f"find {tree} -type f -print0 | xargs -0 -n 2000 -P 8 cat > /dev/null", shell=True,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    read_s = time.perf_counter() - t1
    log(f"  cache settle ({stage}): dirty {d0:.0f} -> {d1:.0f} MB in {waited:.1f} s, warm read pass {read_s:.1f} s")
    return {"stage": stage, "dirty_mb_before": round(d0 or 0), "dirty_mb_after": round(d1 or 0),
            "settle_seconds": round(waited, 1), "settled": (d1 or 0) <= dirty_limit_mb,
            "warm_read_seconds": round(read_s, 1), "warm_read_exit": p.returncode}


def host_info(traj):
    cpu = ""
    try:
        with open("/proc/cpuinfo") as f:
            for line in f:
                if line.startswith("model name"):
                    cpu = line.split(":", 1)[1].strip()
                    break
    except OSError:
        pass
    mem_gb = None
    try:
        with open("/proc/meminfo") as f:
            for line in f:
                if line.startswith("MemTotal"):
                    mem_gb = round(int(line.split()[1]) / 1024 / 1024, 1)
                    break
    except OSError:
        pass
    fs = run_out(["findmnt", "-n", "-o", "FSTYPE", "--target", "/workspace"], check=False).strip() or \
        run_out(["stat", "-f", "-c", "%T", "/workspace"], check=False).strip()
    return {
        "cpu": cpu, "cores": os.cpu_count(), "memory_gb": mem_gb,
        "kernel": platform.release(), "os": platform.platform(),
        "workspace_fs": fs,
        "traj_version": run_out([traj, "--version"], check=False).strip(),
        "git_version": run_out(["git", "--version"], check=False).strip(),
        "python": platform.python_version(),
    }


class Results:
    def __init__(self, path):
        self.path = path
        self.data = None

    def save(self):
        tmp = self.path + ".tmp"
        with open(tmp, "w") as f:
            json.dump(self.data, f, indent=1)
        os.replace(tmp, self.path)


# ------------------------------------------------------------------------------------- git setup

def git_env(gitdir=None, worktree=None):
    """A git environment isolated from the user's configuration.

    The raw-tree repository keeps its GIT_DIR *outside* the tree so that no `.git` directory is
    ever inside the tree that `traj pack` reads.
    """
    env = dict(os.environ)
    env["GIT_CONFIG_GLOBAL"] = os.path.join(WORK, "gitconfig")
    env["GIT_CONFIG_NOSYSTEM"] = "1"
    env.pop("GIT_DIR", None)
    env.pop("GIT_WORK_TREE", None)
    if gitdir:
        env["GIT_DIR"] = gitdir
    if worktree:
        env["GIT_WORK_TREE"] = worktree
    return env


GIT_CONFIG = [
    ("user.name", "trajfs-bench"),
    ("user.email", "bench@example.invalid"),
    ("core.fsmonitor", "false"),
    ("gc.auto", "0"),          # no background repack during the measurements
    ("init.defaultBranch", "main"),
    ("advice.detachedHead", "false"),
]


def write_global_gitconfig():
    with open(os.path.join(WORK, "gitconfig"), "w") as f:
        section = None
        for k, v in GIT_CONFIG:
            sec, key = k.split(".", 1)
            if sec != section:
                f.write(f"[{sec}]\n")
                section = sec
            f.write(f"\t{key} = {v}\n")


def git_init(env, cwd, bare_path=None):
    cmd = ["git", "init", "-q", "-b", "main"] + (["--bare", bare_path] if bare_path else [])
    run_out(cmd, cwd=cwd, env=env)


def git_rev(env, cwd, ref="HEAD"):
    return run_out(["git", "rev-parse", ref], cwd=cwd, env=env).strip()


# ---------------------------------------------------------------------------------- measurements

def measure_git_raw(size_dir, tree, timeout):
    """Commit the raw tree with git.  Returns (record, aborted)."""
    gitdir = os.path.join(size_dir, "raw.git")
    bare = os.path.join(size_dir, "raw-bare.git")
    clone = os.path.join(size_dir, "raw-clone")
    env = git_env(gitdir, tree)
    os.makedirs(gitdir)
    git_init(env, tree)
    git_init(git_env(), size_dir, bare_path=bare)
    run_out(["git", "remote", "add", "origin", bare], cwd=tree, env=env)
    rec = {"git_dir": gitdir, "bare_repo": bare}

    def step(name, cmd, cwd=tree, e=env):
        rec[name] = timed(cmd, cwd=cwd, env=e, timeout=timeout)
        return rec[name].get("aborted") is None and rec[name]["exit"] == 0

    ok = step("add", ["git", "add", "-A"])
    ok = ok and step("commit", ["git", "commit", "-q", "-m", "raw trajectories, all rounds"])
    ok = ok and step("status", ["git", "status"])
    ok = ok and step("push", ["git", "push", "-q", "-u", "origin", "main"])
    ok = ok and step("clone", ["git", "clone", "-q", bare, clone], cwd=size_dir, e=git_env())
    if ok:
        rec["commit1_sha"] = git_rev(env, tree)
    return rec, not ok


def git_raw_second_commit(rec, tree, timeout):
    env = git_env(rec["git_dir"], tree)

    def step(name, cmd):
        rec[name] = timed(cmd, cwd=tree, env=env, timeout=timeout)
        return rec[name].get("aborted") is None and rec[name]["exit"] == 0

    ok = step("add2", ["git", "add", "-A"])
    ok = ok and step("commit2", ["git", "commit", "-q", "-m", "one more round"])
    if ok:
        rec["commit2_sha"] = git_rev(env, tree)
        ok = step("checkout_back", ["git", "checkout", "-q", rec["commit1_sha"]])
        ok = ok and step("checkout_forward", ["git", "checkout", "-q", "main"])
    rec["count_objects"] = run_out(["git", "count-objects", "-vH"], cwd=tree, env=env, check=False)
    rec["git_dir_bytes"] = du_bytes(rec["git_dir"])           # working repo: loose objects (gc.auto=0)
    if ok:  # untimed: bring the bare repo up to both commits so its packed size covers the same content
        run_out(["git", "push", "-q", "origin", "main"], cwd=tree, env=env, check=False)
    rec["bare_repo_bytes"] = du_bytes(rec["bare_repo"])       # packed objects for both commits
    return not ok


def measure_trajfs(size_dir, tree, traj, label, timeout):
    """Pack the tree into a store inside its own git repo and commit it with `traj commit`."""
    repo = os.path.join(size_dir, "store-repo")
    bare = os.path.join(size_dir, "store-bare.git")
    clone = os.path.join(size_dir, "store-clone")
    store = os.path.join(repo, "stores", "synthetic.trajstore")
    env = git_env()
    os.makedirs(repo)
    git_init(env, repo)
    git_init(env, size_dir, bare_path=bare)
    run_out(["git", "remote", "add", "origin", bare], cwd=repo, env=env)
    rec = {"repo": repo, "store": store, "bare_repo": bare,
           "commit_method": "traj commit <store> (no traj init; plain git repo with a stores/ dir)"}

    def step(name, cmd, cwd=repo, capture=False):
        rec[name] = timed(cmd, cwd=cwd, env=env, timeout=timeout, capture_stdout=capture)
        return rec[name].get("aborted") is None and rec[name]["exit"] == 0

    ok = step("pack", [traj, "pack", tree, "--out", store, "--adapter", "copilot-cli", "--label", label],
              capture=True)
    rec["pack_summary"] = parse_pack_summary(rec["pack"].get("stdout_tail"))
    ok = ok and step("commit", [traj, "commit", store])
    ok = ok and step("status", ["git", "status"])
    ok = ok and step("push", ["git", "push", "-q", "-u", "origin", "main"])
    ok = ok and step("clone", ["git", "clone", "-q", bare, clone], cwd=size_dir)
    if ok:
        rec["commit1_sha"] = git_rev(env, repo)
        rec["store_bytes_batch1"] = du_bytes(store)
    return rec, not ok


def trajfs_second_batch(rec, tree, traj, label, timeout):
    repo, store = rec["repo"], rec["store"]
    env = git_env()

    def step(name, cmd, capture=False):
        rec[name] = timed(cmd, cwd=repo, env=env, timeout=timeout, capture_stdout=capture)
        return rec[name].get("aborted") is None and rec[name]["exit"] == 0

    ok = step("pack2", [traj, "pack", tree, "--out", store, "--adapter", "copilot-cli", "--label", label],
              capture=True)
    rec["pack2_summary"] = parse_pack_summary(rec["pack2"].get("stdout_tail"))
    ok = ok and step("commit2", [traj, "commit", store])
    if ok:
        rec["commit2_sha"] = git_rev(env, repo)
        ok = step("checkout_back", ["git", "checkout", "-q", rec["commit1_sha"]])
        ok = ok and step("checkout_forward", ["git", "checkout", "-q", "main"])
    rec["store_bytes"] = du_bytes(store)
    derived = os.path.join(store, "derived")
    rec["store_derived_bytes"] = du_bytes(derived) if os.path.isdir(derived) else 0
    rec["git_dir_bytes"] = du_bytes(os.path.join(repo, ".git"))
    if ok:  # untimed second push, as on the git-raw side
        run_out(["git", "push", "-q", "origin", "main"], cwd=repo, env=env, check=False)
    rec["bare_repo_bytes"] = du_bytes(rec["bare_repo"])
    rec["count_objects"] = run_out(["git", "count-objects", "-vH"], cwd=repo, env=env, check=False)
    # Store facts (not timed): paths, distinct contents, event rows.
    facts = {}
    for key, q in (("paths", "select count(*) from files"),
                   ("distinct_contents", "select count(distinct sha) from files"),
                   ("event_rows", "select count(*) from events")):
        out = run_out([traj, "-S", store, "sql", "--csv", q], check=False).strip().splitlines()
        try:
            facts[key] = int(out[-1])
        except (IndexError, ValueError):
            facts[key] = None
    rec["store_facts"] = facts
    return not ok


PACK_RE = re.compile(r"batch (\d+) in .*?: (\d+) paths \(([\d.]+ \w+)\), (\d+) new blobs \(([\d.]+ \w+) raw, "
                     r"([\d.]+ \w+) packed\), (\d+) excluded, (\d+) unchanged skipped, (\d+) packs?, ([\d.]+) s")


def parse_pack_summary(text):
    """Structured form of the summary line `traj pack` prints, e.g.
    `batch 1 in <store>: 999960 paths (671.7 MB), 667374 new blobs (332.8 MB raw, 172.6 MB packed),
    120 excluded, 0 unchanged skipped, 3 packs, 69.7 s`.  Sizes are kept as traj printed them."""
    for line in (text or "").splitlines():
        m = PACK_RE.search(line)
        if m:
            return {"line": line.strip(), "batch": int(m[1]), "paths": int(m[2]), "paths_size": m[3],
                    "new_blobs": int(m[4]), "new_blobs_raw": m[5], "new_blobs_packed": m[6],
                    "excluded": int(m[7]), "unchanged_skipped": int(m[8]), "packs": int(m[9]),
                    "reported_seconds": float(m[10])}
    return None


def median_runs(cmd, runs, cwd=None, env=None, timeout=None):
    """One warm-up, then `runs` timed runs; returns per-run seconds and the median (ms)."""
    warm = timed(cmd, cwd=cwd, env=env, timeout=timeout)
    if warm.get("aborted") or warm["exit"] != 0:
        return {"cmd": warm["cmd"], "runs_seconds": [], "median_ms": None, "exit": warm.get("exit"),
                "aborted": warm.get("aborted"), "stderr_tail": warm.get("stderr_tail"), "single_shot": False}
    secs, exits = [], []
    for _ in range(runs):
        r = timed(cmd, cwd=cwd, env=env, timeout=timeout)
        if r.get("aborted"):
            return {"cmd": warm["cmd"], "runs_seconds": secs, "median_ms": None, "aborted": r["aborted"],
                    "single_shot": False}
        secs.append(r["seconds"])
        exits.append(r["exit"])
    return {"cmd": warm["cmd"], "runs_seconds": secs, "exit": max(exits),
            "median_ms": round(statistics.median(secs) * 1000, 1), "single_shot": False}


def single_run(cmd, cwd=None, env=None, timeout=None):
    """One timed run, no warm-up: for recursive walks of the whole tree through the FUSE mount,
    which take minutes at a million files.  `median_ms` is that one run so readers can treat it
    like the other rows; `single_shot` marks it."""
    r = timed(cmd, cwd=cwd, env=env, timeout=timeout)
    if r.get("aborted") or r["exit"] != 0:
        return {"cmd": r["cmd"], "runs_seconds": [], "median_ms": None, "exit": r.get("exit"),
                "aborted": r.get("aborted"), "stderr_tail": r.get("stderr_tail"), "single_shot": True}
    return {"cmd": r["cmd"], "runs_seconds": [r["seconds"]], "exit": r["exit"],
            "median_ms": round(r["seconds"] * 1000, 1), "single_shot": True}


def measure_read_path(size_dir, tree, traj, store, rounds, runs, timeout):
    env = git_env()
    round_dir = f"rounds/round-{rounds:04d}"          # the last full round: the largest checkpoint
    rp = {"runs": runs, "ls_dir": LS_DIR, "cat_file": CAT_FILE, "round_dir": round_dir,
          "single_shot": ["mount.find", "mount.grep"],
          "note": "find/grep are whole-tree walks; find_round/grep_round are bounded to round_dir. "
                  "Whole-tree walks through the mount are measured once (single_shot), everything else "
                  "is the median of `runs` runs after one warm-up."}
    M = lambda cmd, cwd=None: median_runs(cmd, runs, cwd=cwd, env=env, timeout=timeout)  # noqa: E731
    S = lambda cmd: single_run(cmd, env=env, timeout=timeout)  # noqa: E731

    log("read path: raw filesystem")
    rp["raw"] = {
        "ls": M(["ls", os.path.join(tree, LS_DIR)]),
        "cat": M(["cat", os.path.join(tree, CAT_FILE)]),
        "find": M(["find", tree, "-name", "review.json"]),
        "grep": M(["grep", "-rl", "Traceback", "--include=*.stderr", tree]),
        "find_round": M(["find", os.path.join(tree, round_dir), "-name", "review.json"]),
        "grep_round": M(["grep", "-rl", "Traceback", "--include=*.stderr", os.path.join(tree, round_dir)]),
    }
    log("read path: traj commands")
    T = [traj, "-S", store]
    rp["traj"] = {
        "ls": M(T + ["ls", LS_DIR]),
        "cat": M(T + ["cat", CAT_FILE]),
        "find": M(T + ["find", "--name", "review.json"]),
        "grep": M(T + ["grep", "-l", "-e", "Traceback", "--name", "*.stderr"]),
        "find_round": M(T + ["find", round_dir, "--name", "review.json"]),
        "grep_round": M(T + ["grep", "-l", "-e", "Traceback", "--name", "*.stderr", "--path", round_dir]),
        "sql_count": M(T + ["sql", SQL_COUNT]),
        "sql_tool_calls": M(T + ["sql", SQL_TOOLS]),
    }
    log("read path: FUSE mount")
    mp = os.path.join(size_dir, "mnt")
    os.makedirs(mp, exist_ok=True)
    m = subprocess.run(T + ["mount", mp, "--daemon", "--no-vscode"], capture_output=True, text=True)
    if m.returncode != 0 or not os.path.ismount(mp):
        reason = (m.stderr or m.stdout).strip()[-300:] or f"exit {m.returncode}"
        log(f"  mount failed, skipping mount rows: {reason}")
        rp["mount"] = {"skipped": reason}
        return rp
    try:
        rp["mount"] = {
            "ls": M(["ls", os.path.join(mp, LS_DIR)]),
            "cat": M(["cat", os.path.join(mp, CAT_FILE)]),
            "find_round": M(["find", os.path.join(mp, round_dir), "-name", "review.json"]),
            "grep_round": M(["grep", "-rl", "Traceback", "--include=*.stderr", os.path.join(mp, round_dir)]),
            "find": S(["find", mp, "-name", "review.json"]),
            "grep": S(["grep", "-rl", "Traceback", "--include=*.stderr", mp]),
        }
    finally:
        u = subprocess.run([traj, "umount", mp], capture_output=True, text=True)
        rp["mount"]["umount_exit"] = u.returncode
        log(f"  umount exit {u.returncode}")
    return rp


# ------------------------------------------------------------------------------------------ main

def run_size(files, rounds, traj, res, args):
    size_dir = os.path.join(WORK, f"{files}")
    rm_rf(size_dir)
    os.makedirs(size_dir)
    tree = os.path.join(size_dir, "tree")
    timeout = args.git_timeout_min * 60
    entry = {"files_requested": files, "rounds": rounds, "work_dir": size_dir}
    res.data["runs"].append(entry)
    res.save()

    log(f"=== {files:,} files / {rounds} rounds: generating tree")
    entry["generated"] = synthetic_tree.generate(tree, files, rounds, seed=args.seed)
    entry["raw"] = {"bytes": du_bytes(tree), "file_count": count_files(tree)}
    log(f"  tree: {entry['raw']['file_count']:,} files, {entry['raw']['bytes'] / 1e9:.2f} GB, "
        f"{entry['generated']['seconds']} s")
    entry["cache_state"] = "warm: sync + dirty-page drain + full read pass before every measured side"
    entry["warmups"] = []
    res.save()

    # TrajFS first (the tree is not yet a git worktree; the raw repo keeps GIT_DIR outside it anyway).
    entry["warmups"].append(settle_and_warm(tree, "before trajfs batch 1"))
    log("trajfs: pack, commit, status, push, clone")
    entry["trajfs"], tf_fail = measure_trajfs(size_dir, tree, traj, f"round-{rounds:04d}", timeout)
    res.save()

    entry["warmups"].append(settle_and_warm(tree, "before git commit 1"))
    log("git-raw: add, commit, status, push, clone")
    entry["git"], git_abort = measure_git_raw(size_dir, tree, timeout)
    res.save()

    log(f"appending round {rounds + 1}")
    extra = synthetic_tree.generate(tree, files, rounds, seed=args.seed, only_rounds=(rounds + 1, rounds + 1))
    entry["extra_round"] = extra
    entry["raw"]["bytes_after_extra_round"] = du_bytes(tree)

    if not git_abort:
        entry["warmups"].append(settle_and_warm(tree, "before git commit 2"))
        log("git-raw: second commit, checkout back and forward")
        git_abort = git_raw_second_commit(entry["git"], tree, timeout)
    else:
        entry["git"]["git_dir_bytes"] = du_bytes(entry["git"]["git_dir"])
    entry["git"]["aborted"] = git_abort
    res.save()

    if not tf_fail:
        entry["warmups"].append(settle_and_warm(tree, "before trajfs batch 2"))
        log("trajfs: second batch, commit, checkout back and forward")
        tf_fail = trajfs_second_batch(entry["trajfs"], tree, traj, f"round-{rounds + 1:04d}", timeout)
    entry["trajfs"]["failed"] = tf_fail
    res.save()

    if args.read_path_all or files == max(args.sizes):
        entry["read_path"] = measure_read_path(size_dir, tree, traj, entry["trajfs"]["store"], rounds,
                                               args.read_runs, timeout)
        res.save()

    if not args.keep:
        log("removing trees and repositories")
        rm_rf(size_dir)
        entry["work_dir"] = None
    return git_abort


def main():
    global WORK
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--sizes", default=DEFAULT_SIZES, help=f"comma list of file counts [default: {DEFAULT_SIZES}]")
    ap.add_argument("--rounds", default=None,
                    help="comma list of round counts, one per size [default: 20/30/40 for the default sizes]")
    ap.add_argument("--work", default="/workspace/trajfs-bench", help="scratch directory (outside any git worktree)")
    ap.add_argument("--traj", default=os.path.join(REPO, "target", "release", "traj"), help="traj binary")
    ap.add_argument("--out", default=None, help="results JSON path [default: bench/results/<UTC>.json]")
    ap.add_argument("--read-runs", type=int, default=5, help="timed runs per read-path command")
    ap.add_argument("--read-path-all", action="store_true", help="measure the read path at every size")
    ap.add_argument("--git-timeout-min", type=float, default=25.0, help="abort any single command after this")
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--keep", action="store_true", help="keep generated trees and repositories")
    args = ap.parse_args()

    args.sizes = [int(s) for s in args.sizes.split(",") if s]
    if args.rounds:
        rounds = [int(r) for r in args.rounds.split(",")]
        if len(rounds) != len(args.sizes):
            sys.exit("--rounds must have one entry per size")
    else:
        rounds = [DEFAULT_ROUNDS.get(s, 20 if s < 200_000 else 30 if s < 600_000 else 40) for s in args.sizes]
    traj = os.path.abspath(args.traj)
    if not os.access(traj, os.X_OK):
        sys.exit(f"{traj} is not executable; run `cargo build --release -p traj` first")
    WORK = os.path.abspath(args.work)
    os.makedirs(WORK, exist_ok=True)
    write_global_gitconfig()

    stamp = dt.datetime.now(dt.timezone.utc).strftime("%Y-%m-%dT%H%M%SZ")
    out = args.out or os.path.join(HERE, "results", f"{stamp}.json")
    os.makedirs(os.path.dirname(out), exist_ok=True)
    res = Results(out)
    res.data = {
        "schema": 1,
        "started_utc": stamp,
        "host": host_info(traj),
        "params": {"sizes": args.sizes, "rounds": rounds, "read_runs": args.read_runs, "seed": args.seed,
                   "git_timeout_min": args.git_timeout_min, "git_config": dict(GIT_CONFIG),
                   "generator": "bench/synthetic_tree.py", "adapter": "copilot-cli", "rules": "default",
                   "cache": "warm: before each measured side the runner syncs, waits for Dirty: in /proc/meminfo "
                            "to drop below 50 MB (max 60 s) and reads every file once (find | xargs cat); "
                            "that pass is recorded per size under warmups but never counted"},
        "sizes_requested": args.sizes, "sizes_run": [], "sizes_skipped": [],
        "runs": [],
    }
    res.save()
    log(f"results -> {out}")
    log(f"host: {res.data['host']['cpu']} x{res.data['host']['cores']}, {res.data['host']['traj_version']}")

    t_all = time.perf_counter()
    aborted = False
    for files, r in zip(args.sizes, rounds):
        if aborted:
            res.data["sizes_skipped"].append({"files": files, "reason": "a git command exceeded the timeout at a smaller size"})
            log(f"skipping {files:,}: a git command exceeded the timeout at a smaller size")
            continue
        aborted = run_size(files, r, traj, res, args)
        res.data["sizes_run"].append(files)
        res.data["total_seconds"] = round(time.perf_counter() - t_all, 1)
        res.save()
    res.data["finished_utc"] = dt.datetime.now(dt.timezone.utc).strftime("%Y-%m-%dT%H%M%SZ")
    res.data["total_seconds"] = round(time.perf_counter() - t_all, 1)
    res.save()
    log(f"done in {res.data['total_seconds']:.0f} s -> {out}")


if __name__ == "__main__":
    main()
