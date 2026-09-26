#!/usr/bin/env python3
"""Generate a synthetic agent-trajectory tree shaped like a real multi-round run.

Layout (the "rounds" layout used throughout the TrajFS docs):

    <root>/rounds/round-0001/COMPLETE
    <root>/rounds/round-0001/status.json
    <root>/rounds/round-0001/builder/events.jsonl          Copilot-CLI style event stream
    <root>/rounds/round-0001/builder/workspace/...          the agent's project, snapshotted again every round
    <root>/rounds/round-0001/builder/logs/round-0001/...    this round's tool logs
    <root>/rounds/round-0001/reviewer/review.json
    <root>/rounds/round-0001/reviewer/logs/...
    <root>/rounds/round-0001/tester/logs/...

How the duplication arises (this is the property that motivates TrajFS, so it is modelled explicitly):
every round is a *checkpoint of the whole task directory*. Round r therefore contains the workspace
(W files, ~2% of them edited that round) plus the logs of every round up to r (round k added L files).
Most paths are byte-for-byte copies of files that exist in earlier rounds. With R rounds the tree has
R*W + L*R(R+1)/2 paths but only about W + R*(0.02*W + L) distinct contents, roughly 5% for 40 rounds.
Recorded real runs are more extreme (2.1 M paths, 24 k distinct contents, about 1%), so this is a
conservative model.

Content is text that compresses like real output: log files are timestamped lines drawn from a small
vocabulary, workspace files are code-like lines, events are JSON. Sizes are log-normal with a median
near 1 KB and a long tail up to a few hundred KB, giving about 3 KB per file on average.

Everything is deterministic for (--files, --rounds, --seed). Generation is parallel per round.

Usage:
    python3 bench/synthetic_tree.py /data/synthetic-run --files 1000000 --rounds 40
"""

import argparse
import json
import os
import random
import sys
import time
from multiprocessing import Pool

ROLES = ("builder", "reviewer", "tester")
WORKSPACE_EXTS = ("py", "rs", "toml", "md", "json", "txt", "yaml", "sh")
LOG_EXTS = ("stdout", "stderr", "log")
TOOLS = ("bash", "read_file", "edit_file", "grep", "cargo_test", "pytest", "git")
LEVELS = ("INFO", "INFO", "INFO", "DEBUG", "DEBUG", "WARN", "ERROR")
WORDS = (
    "compiling", "linking", "resolved", "dependency", "warning:", "unused", "variable", "module", "test",
    "passed", "failed", "assertion", "expected", "found", "line", "column", "src/lib.rs", "src/main.rs",
    "tests/integration.rs", "target/debug", "cargo", "rustc", "running", "finished", "profile", "ok",
    "FAILED", "panicked", "thread", "main", "note:", "error[E0308]:", "mismatched", "types", "help:",
    "consider", "borrowing", "here", "value", "moved", "into", "closure", "returns", "Result", "Option",
    "unwrap", "on", "None", "retrying", "request", "timeout", "tokens", "prompt", "completion",
)
CODE_LINES = (
    "use std::collections::HashMap;", "fn {name}({arg}: &str) -> Result<(), Error> {{", "    let mut out = Vec::new();",
    "    for item in {arg}.split('/') {{", "        out.push(item.to_string());", "    }}", "    Ok(())", "}}",
    "def {name}({arg}):", "    return [x for x in {arg} if x]", "class {Name}:", "    pass",
    "# {name}: {arg}", "{name} = \"{arg}\"", "[{name}]", "key = \"{arg}\"", "- {name}: {arg}",
    "if __name__ == '__main__':", "    main()", "", "",
)


def _rng(*parts) -> random.Random:
    return random.Random("|".join(str(p) for p in parts))


def _size(rng: random.Random, median: float, sigma: float, cap: int) -> int:
    import math
    return max(0, min(cap, int(rng.lognormvariate(math.log(median), sigma))))


def _log_text(rng: random.Random, size: int, round_no: int, role: str) -> bytes:
    out = []
    n = 0
    t = 1_700_000_000 + round_no * 3600 + rng.randint(0, 3000)
    tool = rng.choice(TOOLS)
    while n < size:
        t += rng.randint(0, 3)
        level = rng.choice(LEVELS)
        words = " ".join(rng.choice(WORDS) for _ in range(rng.randint(3, 12)))
        line = f"{t} {level} [{role}/{tool}] step={rng.randint(0, 200)} {words}\n"
        out.append(line)
        n += len(line)
    return "".join(out).encode()[:size]


def _code_text(rng: random.Random, size: int, i: int) -> bytes:
    out = []
    n = 0
    while n < size:
        line = rng.choice(CODE_LINES).format(name=f"item_{i}_{rng.randint(0, 40)}", Name=f"Item{i}",
                                             arg=rng.choice(("path", "value", "config", "input")))
        out.append(line + "\n")
        n += len(line) + 1
    return "".join(out).encode()[:size]


def _events(round_no: int, role: str, rng: random.Random) -> bytes:
    ts0 = 1_700_000_000 + round_no * 3600
    sid = f"s{round_no}-{role}"
    lines = [{"type": "session.start", "timestamp": ts0, "id": f"{sid}-0", "data": {"sessionId": sid}}]
    for k in range(rng.randint(20, 80)):
        t = ts0 + k * 7
        tool = rng.choice(TOOLS)
        call = f"{sid}-c{k}"
        lines.append({"type": "assistant.message", "timestamp": t, "id": f"{sid}-a{k}", "parentId": f"{sid}-0",
                      "data": {"content": f"step {k}: running {tool}",
                               "toolRequests": [{"toolCallId": call, "name": tool}]}})
        lines.append({"type": "tool.execution_start", "timestamp": t + 1, "id": f"{sid}-t{k}",
                      "data": {"toolCallId": call, "toolName": tool,
                               "arguments": {"command": f"{tool} --step {k}"}}})
        lines.append({"type": "tool.execution_complete", "timestamp": t + 5, "id": f"{sid}-d{k}",
                      "data": {"toolCallId": call, "exitCode": 0 if rng.random() > 0.08 else 1}})
    lines.append({"type": "model.usage", "timestamp": ts0 + 600, "id": f"{sid}-u",
                  "data": {"inputTokens": rng.randint(20_000, 200_000), "outputTokens": rng.randint(2_000, 20_000)}})
    return ("\n".join(json.dumps(l, separators=(",", ":")) for l in lines) + "\n").encode()


def _write(path: str, data: bytes):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "wb") as f:
        f.write(data)


def shape(files: int, rounds: int):
    """(workspace files W, logs per round L) so that R*W + L*R(R+1)/2 ~= files, with W = 10 L."""
    per_l = rounds * 10 + rounds * (rounds + 1) / 2
    logs = max(30, int(files / per_l))
    return logs * 10, logs


def _log_paths(seed: int, round_no: int, logs: int):
    """The logs written in `round_no`: role, relative path, size. Deterministic, so every later
    round's checkpoint reproduces exactly the same bytes."""
    rng = _rng(seed, "logs", round_no)
    out = []
    for j in range(logs):
        role = ROLES[j % len(ROLES)]
        ext = LOG_EXTS[j % len(LOG_EXTS)]
        size = _size(rng, 1200, 1.4, 400_000)
        out.append((role, f"{role}/logs/round-{round_no:04d}/call-{j:05d}.{ext}", size, rng.random()))
    return out


def _gen_round(args):
    root, round_no, seed, workspace, logs = args
    rd = os.path.join(root, "rounds", f"round-{round_no:04d}")
    written = 0
    # 1. the workspace: the same project files every round, ~2% edited in this round, and edits
    #    from earlier rounds carried forward (so a file's content depends on the last round that edited it)
    for i in range(workspace):
        frng = _rng(seed, "ws", i)
        ext = WORKSPACE_EXTS[i % len(WORKSPACE_EXTS)]
        depth = frng.randint(1, 4)
        dirs = "/".join(f"pkg{frng.randint(0, 9)}" if d else "src" for d in range(depth))
        base = _code_text(frng, _size(frng, 900, 1.0, 200_000), i)
        # rounds that edit this file: each round edits ~2% of files
        last_edit = 0
        for r in range(1, round_no + 1):
            if _rng(seed, "edit", i, r).random() < 0.02:
                last_edit = r
        data = base if last_edit == 0 else base + f"\n# edited in round {last_edit}\n".encode()
        _write(os.path.join(rd, "builder", "workspace", dirs, f"file{i:05d}.{ext}"), data)
        written += 1
    # 2. the logs of every round up to this one (a checkpoint carries the whole history)
    for k in range(1, round_no + 1):
        for role, rel, size, u in _log_paths(seed, k, logs):
            body = _log_text(_rng(seed, "logbody", k, rel), size, k, role)
            if u < 0.03:
                body = b"Traceback (most recent call last):\n  File \"agent.py\", line 42, in run\n" \
                       b"RuntimeError: step failed\n" + body
            _write(os.path.join(rd, rel), body)
            written += 1
    # 3. this round's own small files
    rng = _rng(seed, "round", round_no)
    for role in ROLES:
        _write(os.path.join(rd, role, "events.jsonl"), _events(round_no, role, rng))
        written += 1
    review = {"round": round_no, "verdict": rng.choice(["continue", "continue", "revise", "done"]),
              "tests_passed": rng.randint(0, 200), "tests_failed": rng.randint(0, 5),
              "summary": " ".join(rng.choice(WORDS) for _ in range(12))}
    _write(os.path.join(rd, "reviewer", "review.json"), (json.dumps(review, indent=2) + "\n").encode())
    _write(os.path.join(rd, "status.json"), json.dumps({"round": round_no, "state": "complete"}).encode())
    _write(os.path.join(rd, "tester", "results.xml"), _log_text(rng, 4000, round_no, "tester"))
    # 4. build products, so retention rules have something to exclude
    _write(os.path.join(rd, "builder", "node_modules", "pkg", "index.js"), b"module.exports = 1;\n")
    _write(os.path.join(rd, "builder", "workspace", "__pycache__", "mod.cpython-312.pyc"), b"\x00pyc" * 64)
    _write(os.path.join(rd, "builder", ".cache", "state"), b"cache\n")
    _write(os.path.join(rd, "COMPLETE"), b"complete\n")
    written += 7
    return written


def generate(root: str, files: int, rounds: int, seed: int = 1, workers: int | None = None,
             only_rounds: tuple[int, int] | None = None) -> dict:
    """Write the tree for (files, rounds). `only_rounds=(a, b)` writes just rounds a..b of that same
    tree, which is how a benchmark appends round R+1 to a tree generated with R rounds:
    generate(root, files, rounds, only_rounds=(rounds + 1, rounds + 1))."""
    workspace, logs = shape(files, rounds)
    first, last = only_rounds or (1, rounds)
    t0 = time.time()
    jobs = [(root, r, seed, workspace, logs) for r in range(first, last + 1)]
    # later rounds are bigger (they carry more history), so hand them out first
    jobs.sort(key=lambda j: -j[1])
    with Pool(workers or min(32, os.cpu_count() or 4)) as p:
        counts = p.map(_gen_round, jobs, chunksize=1)
    return {"root": root, "files": sum(counts), "rounds": last - first + 1, "first_round": first,
            "workspace_files": workspace, "logs_per_round": logs, "seconds": round(time.time() - t0, 1)}


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("root", help="directory to create (must not exist)")
    ap.add_argument("--files", type=int, default=100_000, help="approximate total file count")
    ap.add_argument("--rounds", type=int, default=20)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--workers", type=int, default=None)
    ap.add_argument("--only-rounds", type=int, nargs=2, metavar=("FIRST", "LAST"),
                    help="write only these rounds of the same tree (e.g. to append a round)")
    a = ap.parse_args()
    if os.path.exists(a.root) and not a.only_rounds:
        sys.exit(f"{a.root} exists; refusing to overwrite")
    info = generate(a.root, a.files, a.rounds, a.seed, a.workers, tuple(a.only_rounds) if a.only_rounds else None)
    print(json.dumps(info))


if __name__ == "__main__":
    main()
