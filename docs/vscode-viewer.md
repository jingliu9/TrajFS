# Viewing a store as ordinary files in VS Code — plan

Status: draft 1, 2026-09-04. Companion to `PLAN.md` (§1 goal 3, §5, §9) and `idea-review.md` §4 (the "V2: FUSE as
a projection" clause). Numbers quoted are the §14/§15 measurements on the rank 1 store (2.14 M paths, 12.0 GB).

**Decision (2026-09-04):** the owner chose the FUSE mount, because it serves VS Code and every other viewer at
once. The implementation plan is `PLAN-fuse.md`; this file stays as the record of the comparison. The extension is not pursued (removed
2026-09-04).

> Q: Is that possible that we make the results of packed directory to be visible like normal directorys and files
> in vscode editor?

**Yes.** Two ways are practical, and both are thin because the store reader already answers the three questions a
filesystem asks (list a directory, stat a path, read a file) in milliseconds. The recommended order is:

1. **`traj mount`** — a read-only FUSE projection of one store, or of every store under `store_root`, at a path of
   the user's choice. VS Code (Remote-SSH, as used on this host), `grep -r`, `diff -r`, Python and every other tool
   see a normal directory tree. Rust only, in the existing binary, ~500 lines. Works on this host today
   (`/dev/fuse` and `fusermount3` are present; no root needed).
2. **A VS Code extension** (`FileSystemProvider` over a long-lived `traj serve` process) — the same tree without a
   kernel mount, so it would also work from a laptop clone on macOS/Windows and in Codespaces. Compared below and
   not pursued: it serves one editor, needs a TypeScript toolchain this host lacks, and VS Code's search providers
   are still proposed API.

Until either exists, `traj extract <dir> /tmp/x` followed by `code -r /tmp/x` is the way (10 s and 450 MB for one
round of 106 K paths; the copy goes stale when a batch lands).

## 1. Why this is cheap: the reader is already a filesystem without the mount

| filesystem call | `trajfs_core::Store` today | cost on the 2.14 M-path store |
|---|---|---|
| `readdir(dir)` | `children(dir)` → subdirectories from `dirs-*.parquet`, direct files from `files-*.parquet` (row groups whose path range cannot hold direct children are skipped) | 10–20 ms, whole process 70 ms |
| `getattr(path)` | `stat(path)` → one `FileRow`: kind, mode, size, sha, mtime, attrs; `dir_info(dir)` for directories | 1–2 ms |
| `read(path)` | `read_row` → `parts(sha)` from the in-memory index, `PackReader::blob` = one `pread` + one zstd frame (≤ 1 MiB) | µs to ms; index load ≤ 100 ms once (23–82 K shas) |
| `readlink(path)` | `read_row` of a `Kind::Symlink` row (the blob is the target) | as `read` |

Everything a mount or an extension does is a cache in front of these four calls. The store format does not change,
nothing is written, and the verification path (`read_row(.., verify=true)`) stays in the loop, so a bit flip in a pack
surfaces as a read error, not as silently wrong bytes in an editor.

## 2. Options compared

| | (0) `extract` + open (today) | (A) `traj mount` (FUSE) | (B) VS Code extension + `traj serve` |
|---|---|---|---|
| Explorer tree, open file, editor features | yes, on the copy | yes | yes (`isReadonly` provider) |
| Ctrl+Shift+F, Quick Open | yes | yes (ripgrep on the mount; scope it to a round) | only through extension commands: search providers are proposed API |
| `grep -r`, `diff -r`, Python, git diff --no-index | yes | yes | no (only VS Code sees the tree) |
| freshness after a new batch | stale | live (§4.4) | live |
| disk | one copy per extract | none | none |
| where it works | anywhere | Linux with `/dev/fuse` (this host: yes; many containers: no) | anywhere VS Code runs, including a laptop clone and Codespaces |
| toolchain | – | Rust, `fuser` 0.18, no libfuse needed | TypeScript + node (absent here), plus Rust for `serve` |
| read-only guarantee | copy is writable (a known drift risk) | kernel-enforced `ro` | provider-enforced |
| size | 0 | ~500 lines + tests | ~250 lines Rust + ~600 lines TS |
| risk | none | VS Code's watcher crawling 2 M paths (§4.6, mitigated by one setting) | search gap; second toolchain in the repo |

(A) first because it answers the question completely for the environment actually in use (VS Code Remote-SSH on a
Linux host with FUSE), it is in the project's one language, and it is exactly the "projection over the same backend"
that `idea-review.md` §4 reserved for the moment extract-to-view became a daily friction. `PLAN.md` §1 lists "a
mounted filesystem" as a non-goal; that referred to FUSE as the storage design, which stays dead. A mount that is a
disposable cache over the store keeps every property of §1 (git sees packs and Parquet, nothing else).

## 3. Stages

| stage | deliverable | exit criterion |
|---|---|---|
| S0 | (nothing to build) document `extract` + `code -r` in the skill; add `--open` is *not* worth it | – |
| S1 | `traj mount`, `traj umount`, `doctor` awareness, T10 tests, docs | a round of the reference run opens in the VS Code Explorer over Remote-SSH; `diff -r` mount vs source is clean; §4.8 latencies met |

S1 is one to two days (done the same day; `PLAN-fuse.md` §16).

## 4. Design: `traj mount`

### 4.1 Command line

```
traj mount [-S <store>]... <mountpoint> [--daemon] [--allow-other] [--attr-ttl 60] [--blob-cache 256M]
traj umount <mountpoint>
```

- With one `-S`, the mountpoint *is* the store's tree (`<mnt>/rounds/round-0037/...`).
- With several `-S`, or with none inside a repo that has `trajfs.toml`, the root lists one directory per store id
  (`<mnt>/run-42/rounds/...`), taken from `store_root`. The root is re-scanned on each `readdir` (5 s TTL), so a store
  that arrives with `git pull` appears without remounting.
- `--daemon` re-executes the binary detached (`setsid`, stdio to `<mountpoint>.log`) and returns once the mount is
  live; default is foreground, which is what a tmux pane or a systemd user unit wants. No shell anywhere.
- `traj umount` runs `fusermount3 -u`. On SIGINT/SIGTERM `traj mount` unmounts itself before exiting. After a
  SIGKILL or a crash the mountpoint is stale ("Transport endpoint is not connected"); both `traj mount` (before
  mounting) and `traj umount` detect that state and run `fusermount3 -u` first. FUSE's own `auto_unmount` is not
  usable here: `fuser` refuses it unless `allow_other`/`allow_root` is set, which needs `user_allow_other` in
  `/etc/fuse.conf` (§4.5).
- Refused: a mountpoint inside a git work tree (git would walk the mount on `status`) unless `--allow-in-repo`; a
  mountpoint inside `data_root` or `store_root`. `traj doctor` reports live `traj` mounts (from `/proc/self/mounts`,
  fsname `traj`) and warns about any inside a repo.

### 4.2 Namespace and caching

- **Inodes** are allocated on `lookup` (`ino → path`, `path → ino`, two maps, u64 counter, never reused during a
  session). Only visited paths occupy memory: opening one round in the Explorer touches a few hundred; a full
  `ls -R` of the run touches all 2.14 M at roughly 100 B each, ≈ 200 MB, which is the bound `PLAN.v1.md` §9 gave.
- **Directory listings** are cached per directory (`children` result: names, kinds, modes, sizes, mtimes) in an LRU of
  a few thousand entries with a TTL (`--attr-ttl`, default 60 s). `getattr` of a listed child is served from the
  listing, so an Explorer expansion is one catalog scan, not one per entry. Paths not in any cached listing go to
  `stat` (1–2 ms).
- **Directories** get mode `0555`, `nlink 2`, size 4096, mtime = the time of the newest batch that contains them
  (`MANIFEST.json` batch timestamp). **Files** get `mode & 0555` (no write bits, ever), the recorded size and mtime,
  uid/gid of the mounting user. **Symlinks** are real symlinks (`readlink` returns the stored target). `Kind::Empty`
  rows are zero-length files.
- **Kernel caching**: entry and attr TTL = `--attr-ttl`; negative lookups are not cached, so a path that appears with
  a new batch resolves at once.
- **xattrs** (optional, cheap with `fuser`): `user.traj.sha256`, `user.traj.batch`, `user.traj.attr.<k>` for each
  attr. `getfattr -d` then shows round and role; VS Code does not use them, humans in a terminal do.

### 4.3 Reads

`open` resolves the row, reads the whole blob through `read_row(.., verify=true)` once, and keeps it as
`Arc<Vec<u8>>` in the file handle; `read` calls slice it. Blobs are shared in an LRU keyed by sha (`--blob-cache`,
default 256 MiB), so the 91 paths that map to one blob decompress it once. p99 file size in the corpus is 80 KB and
the largest kept files are a few MB, so whole-blob reads are the right granularity; a multi-part blob (> 64 MiB,
split across packs) is assembled the same way and simply not cached. `PackReader` is behind a `Mutex`; `fuser`'s
session is single-threaded by default, and a read is dominated by one `pread` plus one frame decode, so one reader
is enough for an editor. `Store` is `Sync` already (the index is a `OnceLock`).

A sha mismatch returns `EIO` for that file and logs the path; nothing else is affected, which is what §11 T3
requires of `cat` too.

### 4.4 New batches and new stores

Stores are append-only. `Store::open` lists catalog and index segments once; a batch adds segments. The mount keeps
the `MANIFEST.json` mtime per store and reopens the `Store` (new segment lists, index reloaded, listing cache
dropped) when it changes, checked at most once per second on `lookup`/`readdir`. Packs and Parquet segments already
written are immutable, so a reopen while a file is open is safe: the open handle holds its bytes.

### 4.5 Build

- `fuser = "0.18"` (checked: its default feature set is empty, and the pure-Rust path mounts by executing
  `fusermount3`, present here as `/usr/bin/fusermount3`) behind a Cargo feature `mount`, default on for Linux builds
  like `sql`, so the binary stays a single static file. `libfuse3-dev` and `pkg-config` are not installed on this
  host and are not needed.
- `allow_other` is opt-in and needs `user_allow_other` in `/etc/fuse.conf` (currently commented out); the default
  mount is visible to the mounting user only, which is what VS Code Remote-SSH under the same user needs.
- Linux only; on other targets the verb prints "mount needs FUSE on Linux; use `traj extract`".

### 4.6 VS Code specifics (the part that is not obvious)

1. **The file watcher.** VS Code sets up recursive inotify watches on every workspace folder, and to do so it crawls
   the folder. On a mount of a whole run that is a `readdir` per directory over the whole tree: minutes of catalog
   scans, and it would exhaust `max_user_watches`. Two mitigations, both needed in the docs:
   - open a *round* (`<mnt>/run-42/rounds/round-0037`) as the workspace folder, not the run; and
   - `files.watcherExclude` for the mount root. `traj init --mount-root <dir>` adds to the repo's `.vscode/settings.json` when a
     `.vscode/` directory exists (not yet implemented; the snippet is documented):

     ```json
     {
       "files.watcherExclude": { "**/traj-mnt/**": true },
       "search.followSymlinks": false
     }
     ```

   Nothing under a mount ever changes without a batch, so a watcher there has nothing to report anyway.
2. **Search.** Ctrl+Shift+F runs ripgrep on the mount and reads 91× redundant bytes. Fine inside a round (450 MB
   logical, 17 MB of packs); slow over the run. `traj grep` stays the tool for run-wide questions and the docs say so.
3. **Git.** Mount outside every git work tree (§4.1 refuses otherwise), or the git extension and `git status` crawl
   it. `~/traj-mnt/` is the documented default, with `mount_root` optional in `trajfs.toml`.
4. **Remote-SSH.** The VS Code server runs on this host as the same user (`~/.vscode-server` is present, and its
   `remote-cli/code` opens paths from a terminal), so the mount is visible to it with no `allow_other`.
5. **Saving.** The kernel says read-only; VS Code shows the editor as read-only and offers "Save As", which is the
   `traj edit` behaviour ("never written back") without a temp copy.

### 4.7 What `traj extract` and `traj edit` become

Both stay: `extract` is still the way to hand a tree to a tool that must write next to the files (a build, a
re-run), and it is the byte-identical oracle the tests use. `edit` becomes a two-liner over the mount when one is
present and keeps its extract path otherwise.

### 4.8 Latency targets (warm, 2.14 M-path store)

| operation | target | basis |
|---|---|---|
| Explorer expands a directory (readdir + getattr per child) | ≤ 30 ms cold, ≤ 1 ms cached | `children` 10–20 ms |
| open a 40 B file | ≤ 5 ms | `cat` 1–2 ms in-process |
| open a 5 MB log | ≤ 30 ms | 5 frames, one `pread` each |
| `ls -R` of one round (106 K paths) | ≤ 3× the extracted tree | `PLAN.v1.md` T10 |
| memory after opening a round in VS Code | ≤ 150 MB | index 23 K entries + listing cache |

## 5. Tests

**T10 mount** (revives `PLAN.v1.md` §11 T10; skipped with a message when `/dev/fuse` is absent):

- Mount the T2 fixture store; `diff -r --no-dereference` between the mount and `traj extract` is empty; modes and
  symlink targets equal; every file fails to open for writing with `EROFS`.
- `ls -A`, `find`, `du -sb` on the mount equal the store verbs (the T4 oracle, reversed).
- `grep -rl` on the mount for the 20 T5b patterns equals `traj grep -l`.
- Eight threads `cat` random paths concurrently for two seconds; every read verifies.
- A second `traj pack` batch into the mounted store: the new paths are listed within `--attr-ttl`; previously open
  handles keep reading.
- Unmount with a file open: `umount` reports busy, `--lazy` succeeds, no zombie process.
- Refusals: mountpoint inside a git work tree, inside `data_root`, inside `store_root`.
- Slow (`--features slow`): mount the rank 1 store; `ls` of a round directory ≤ 50 ms; `cat` p50 ≤ 5 ms; `ls -R` of
  round-0037 ≤ 3× the extracted tree; RSS bound from §4.8.

**T12 serve** (stage 2): every `serve` operation against the fixture store equals the CLI verb; a malformed line
gets an error reply and the process stays up; `read` of a 5 MB file round-trips.

## 6. Changes to the other documents when S1 lands

- `PLAN.md` §1: non-goal reworded to "FUSE as the *storage* format"; §5 verb table gains `mount`/`umount`; §9 gets this
  design (the section number was reserved for it); §10 milestone M6 "mount"; §11 T10 as above.
- `README.md`: a "Browsing in VS Code" paragraph after Quick start with the two-line mount recipe and the watcher
  setting.
- `skills/traj/SKILL.md`, "Materialise for a person": `traj mount` first, `extract` second; the rule "never write
  back into a store" gains "mounts are read-only".
- `traj init`: optional `mount_root` in `trajfs.toml`; `.vscode/settings.json` snippet (§4.6).

## 7. Checked on this host today

| fact | value | consequence |
|---|---|---|
| VS Code | `~/.vscode-server` with two server builds and `remote-cli/code` | Remote-SSH; the mount is on the machine VS Code's server runs on |
| FUSE | `/dev/fuse` `crw-rw-rw-`, `fusermount3` and `fusermount` in `/usr/bin`, kernel `fuse` registered | user-mode mounts work without root |
| `/etc/fuse.conf` | `user_allow_other` commented out | `--allow-other` unavailable without an admin; not needed for the owner's own VS Code |
| libfuse headers, `pkg-config` | absent | fine: `fuser` 0.18 needs neither (verified in its source) |
| node, npm | absent | not needed: nothing here is built with node |
| toolchain | cargo 1.98; crates.io reachable | no change |
