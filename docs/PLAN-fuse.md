# trajfs — `traj mount`: a read-only FUSE projection of a store

Status: draft 1, 2026-09-04, implemented the same day (§16 has the measurements and the deviations). Implements the decision recorded in `vscode-viewer.md`: a mount serves VS Code and
every other viewer (terminal tools, diff tools, other editors) at once, so it is built first and the VS Code
extension idea of `vscode-viewer.md` is not pursued. Section numbers in `PLAN.md` that this plan touches are listed in
§14. Numbers quoted are the `PLAN.md` §14/§15 measurements on the rank 1 store (2.14 M paths, 12.0 GB kept).

## 1. Goal and non-goals

Goal: a directory that shows a store, or every store under `store_root`, as an ordinary read-only tree, with the
latency of the catalog verbs (`ls` 10–20 ms, `cat` 1–2 ms) and no copy on disk. Opening a round of the reference run
in the VS Code Explorer over Remote-SSH must feel like a local directory; `diff -r`, `grep -r` and Python must give
the same bytes `traj extract` gives.

Non-goals, unchanged from `PLAN.md` §1: FUSE is not the storage format; nothing is ever written through the mount;
no cross-store dedupe; no attempt to make a whole-run `grep -r` fast (`traj grep` reads each blob once, the mount
cannot).

## 2. Decisions

| decision | choice | why |
|---|---|---|
| crate | `fuser = "0.18"`, default features (pure Rust; mounts by executing `fusermount3`) | no libfuse headers on the host and none needed; verified in the crate source: `default = []`, `fuse_pure.rs` execs `fusermount3`/`fusermount` |
| feature gate | Cargo feature `mount`, default on, `#[cfg(all(feature = "mount", target_os = "linux"))]`; elsewhere the verb prints a one-line refusal naming `traj extract` | like `sql`; one binary |
| threading | `fuser::Config::n_threads = 1` at first; handlers take `&self`, so state is behind `RwLock`/`Mutex` from day one and `--threads N` is a flag later | one `pread` + one frame decode per read; an editor never saturates that |
| namespace source | directory listings from `Store::children`; lookups served from the parent's listing; no per-path `stat` on the hot path | `children` is one row-group-pruned scan; the listing already carries kind, mode, size, mtime, sha |
| inode policy | allocated on first lookup, never reused in a session; entry = parent, name, kind, mode, size, mtime, sha (≈ 80 B + name); path rebuilt by walking parents | only visited paths cost memory; a full `ls -R` of the run is ≈ 200 MB, the `PLAN.v1.md` §9 bound |
| read granularity | whole blob per `open`, held in the file handle; sha verified once per open; blob LRU keyed by sha | p99 file 80 KB; 91 paths share a blob; verify stays in the loop as for `cat` |
| freshness | reopen the `Store` when `MANIFEST.json` mtime changes (checked ≤ 1/s); listing cache dropped; kernel caches invalidated through `fuser::Notifier::inval_entry`/`inval_inode` for cached entries | batches are additive and a path may be re-recorded by a later batch, so path → sha can change; content per sha never does |
| kernel TTLs | entry and attr TTL = `--ttl`, default 5 s; negative lookups not cached | bounded staleness with no notifier dependence; ENOENT for a path that a batch then adds resolves at once |
| mount options | `RO`, `NoSuid`, `NoDev`, `NoAtime`, `DefaultPermissions`, `FSName("traj:<id or store_root>")`, `Subtype("traj")`; `AllowOther` only with `--allow-other` | read-only enforced by the kernel; `fuse.traj` in `/proc/self/mounts` identifies our mounts |
| `auto_unmount` | not used | `fuser` refuses it without `allow_other`/`allow_root`, which needs `user_allow_other` in `/etc/fuse.conf` (off on this host); §8 handles stale mounts instead |
| daemonising | `--daemon` re-executes the binary detached (`setsid`), parent waits for a "mounted" byte on a pipe | no shell, no fork of a threaded process |
| where | `crates/traj/src/mount/{mod.rs, fs.rs, inodes.rs, cache.rs}` and `cmd/mount.rs`; three small additions to `trajfs-core::Store` (§10) | `fuser` stays out of the core crate |

## 3. Command line

```
traj mount [-S <store>]... [<mountpoint>] [--daemon] [--save] [--no-vscode] [--allow-other] [--ttl 5]
           [--memory 20%] [--blob-cache 256M] [--listing-cache 1000000] [--threads 1] [--allow-in-repo]
traj umount [<mountpoint>] [--lazy]
```

- One `-S`: the mountpoint is that store's tree (`<mnt>/rounds/round-0037/...`).
- Several `-S`, or none inside a repo with `trajfs.toml`: the root holds one directory per store id
  (`<mnt>/run-42/rounds/...`), from `store_root`, rescanned on root `readdir`/`lookup` with a 5 s TTL. A store that
  arrives with `git pull` appears without remounting.
- `<mountpoint>` defaults to `mount_root` in `trajfs.toml` (new, optional key; `traj init --mount-root <dir>` writes
  it; documented default `~/traj-mnt`). Created if absent, must be an empty directory.
- Refused unless `--allow-in-repo`: a mountpoint inside a git work tree (`git status` and the VS Code git extension
  would crawl it). Always refused: inside `data_root` or `store_root`, or a path that is already a live mount.
- `--daemon`: §8. Default is foreground, which is what tmux and a systemd user unit want.
- `--save`: records the mountpoint as `mount_root` in `trajfs.toml` and writes the VS Code settings (§9) into the
  repo's `.vscode/settings.json`, so an existing repo is set up without re-running `traj init`.
- Every mount also merges the same settings into VS Code's machine-level files on this host when they exist
  (`~/.vscode-server/data/Machine/settings.json` for Remote-SSH, `~/.config/Code/User/settings.json` for a local
  VS Code), so opening the mountpoint itself, or any folder, is safe; `--no-vscode` skips that.
- `traj umount`: `fusermount3 -u` (`-z` with `--lazy`); with no argument, every `fuse.traj` mount of this user.
- `traj doctor` prints a `mounts:` line: each `fuse.traj` entry from `/proc/self/mounts`, PROBLEM when one is stale
  (`stat` fails with ENOTCONN) or inside a repo or a root.

## 4. Namespace

- **Root.** ino 1. Single-store mode: root = store root (`children("")`). Multi-store mode: root children are the
  store ids; each store has its own inode subtable and `Store` handle.
- **Directories** come from `dirs-*.parquet` through `children`, so empty directories exist (as in T4). Attributes:
  mode `0555`, `nlink = 2 + n_dirs`, size 4096, mtime = newest mtime among direct children, or the batch `created`
  time from the manifest when the directory has none. uid/gid = the mounting user.
- **Files**: mode `= recorded mode & 0555` (no write bit ever), size, mtime from the row, `blocks = ceil(size/512)`,
  `nlink 1`. `Kind::Empty` rows are zero-length regular files.
- **Symlinks**: `S_IFLNK | 0777`, `readlink` returns the stored target bytes (the blob).
- **lookup(parent, name)**: from the parent's listing (§5); miss → `children(parent)` once → ENOENT if still absent.
- **readdirplus** is implemented (fuser 0.18 has it), so one kernel call per Explorer expansion returns names and
  attributes together; `readdir` is kept for kernels that do not ask for plus.
- **xattrs**: `user.traj.sha256` (hex), `user.traj.batch`, `user.traj.attr.<key>` for each adapter attr. Attrs are
  not in the listing cache; `getxattr` does one `stat` (1–2 ms). `listxattr` names them. Directories have none.
- **statfs**: `blocks = kept bytes / 4096` from the manifest batches, `bfree = bavail = 0`, `files = paths`,
  `namelen 255`.
- **open for write, mkdir, unlink, rename, setattr, …**: not implemented; with `RO` the kernel answers `EROFS`
  before we are asked.

## 5. Caching and memory

| cache | key → value | size bound | invalidation |
|---|---|---|---|
| listing | dir → `Vec<Entry>` (name, kind, mode, size, mtime_ns, sha, for subdirs n_dirs) | LRU, `--listing-cache` entries, default 4096 (a round has ≈ 10–20 K directories; VS Code opens a few hundred) | store reopen (§7); TTL `--ttl` |
| inode table | ino ↔ (parent ino, name) + last attributes | grows with visited paths; never shrinks in a session | attributes refreshed from the parent listing when older than `--ttl` |
| blob | sha → `Arc<Vec<u8>>` | LRU by bytes, `--blob-cache`, default 256 MiB; blobs over 64 MiB (multi-part) bypass it | never (content per sha is immutable) |
| frames | `PackReader`'s existing 8-frame LRU | 16 MiB | never |
| kernel | entry, attr | TTL `--ttl` | `Notifier` on reopen for entries we know the kernel holds |

Memory after opening one round in VS Code: index (23 K shas) + a few hundred listings + blobs of the opened files,
well under 150 MB (§11).

### 5.1 Memory budget (specified and implemented 2026-09-04)

The three caches compete for the same memory and an inode count is the wrong unit (names vary, machines differ), so
the bound is one **best-effort budget**, `--memory <percent|size>`: a fraction of `MemTotal` (default 20 %) or an
absolute size; `0` means unbounded. It is a soft target, stated as such, because the kernel holds references to every
inode it has looked up and the filesystem may drop one only after the kernel's `forget`.

- **Accounting by estimate:** a blob counts its length, a listing entry ≈ 150 B, an inode ≈ 300 B (the measured
  averages of §16); estimates, not measured allocations.
- **Enforcement where possible:** listings and blobs evict oldest-first while the estimate is over budget. Inodes
  are released only through `forget` (implemented for this), and when over budget the mount nudges the kernel with
  `inval_entry` for inodes not served within the TTL window; the kernel then forgets what it no longer needs. Open
  handles are never touched.
- **Why relative:** the same store is opened on a 2 GB laptop and a 512 GB host; a fraction adapts, an absolute
  count does not.

This replaces the `--max-inodes` idea; `--listing-cache` and `--blob-cache` stay as per-cache overrides.

As built: the check runs at most once a second from `lookup`, `readdir` and `open`; each trim logs the estimate and
what it did; `TRAJ_MOUNT_DEBUG=1` also logs every invalidation and `forget`. An inode the kernel never looked up (a
plain `readdir` entry) is dropped outright; a looked-up inode is nudged only when it was not served within the last
`ttl + 1` s, so a walk in progress is not thrashed. Measured on the rank 1 store with `--memory 512M`: a walk of
the whole run (3.14 M entries) ends at 805 MB RSS instead of 1.2 GB unbounded, with the estimate held at about
530 MB; the gap is the ~300 B-per-inode estimate against a measured ~430 B plus allocator overhead, and inodes
served within the window that the walk keeps alive. On the fixture, 25 inodes shrink to 2 once the kernel forgets.
The default of 20 % of `MemTotal` is 26 GB on this 128 GB host, so it never trims here; laptops are the reason it
is relative.

## 6. Reads

`open(ino)` resolves the inode's sha and kind, takes the blob from the cache or reads it through
`Store::read_blob(reader, sha, verify = true)` (§10), and stores `Arc<Vec<u8>>` in a file-handle table.
`read(fh, offset, size)` copies a slice. `release` drops the handle. A sha mismatch or a missing pack is `EIO` on
`open`, logged once with the path; other files are unaffected (the T3 property of `cat`). `O_DIRECT`, `flock`,
`fallocate` are refused with `EINVAL`/`EROFS`.

Multi-part blobs (> 64 MiB, split across packs, T1) are assembled by `PackReader::blob` as today and not cached.

## 7. Freshness

- Each mounted store keeps the mtime of its `MANIFEST.json`; on any `lookup`/`readdir`, at most once per second,
  the mtime is re-read. On change: `Store::open` again (new segment lists, index reloaded), swap it in under the
  `RwLock`, clear that store's listing cache, and send `inval_entry(parent, name)` for every inode whose attributes
  were served within the last TTL, plus `inval_inode` for those whose sha changed. Open file handles keep their
  bytes.
- The mount opens stores without the shared reader lock that other verbs take (`Store::open_unlocked`), so a
  daemon that runs for hours never blocks `traj pack`. Packs and Parquet segments already written are immutable
  (`PLAN.md` §2), and a batch is either fully present in the manifest or not, so a reopen during a `traj pack` sees either the old or the new batch, never a partial one
  (the T8b SIGKILL property).
- Root of a multi-store mount: rescan `store_root` on `readdir`/`lookup` with a 5 s TTL; a removed store directory
  gives ENOENT and its inodes are dropped from the table.

## 8. Lifecycle

- **Foreground** (default): `Session::new(fs, mountpoint, &Config)` then `run()`. SIGINT/SIGTERM (via `signal-hook`,
  the one new small dependency besides `fuser`) trigger `SessionUnmounter::unmount()`, so the process exits with
  the mountpoint clean.
- **`--daemon`**: re-exec `current_exe()` with the same arguments plus `--foreground-child`, a `pre_exec` `setsid`,
  stdin/stdout/stderr to `<mountpoint>.log` (beside the mountpoint, or `--log`), and a pipe; the child writes one
  byte after `Session::new` succeeds; the parent prints the mountpoint and exits 0, or forwards the child's error
  and exits 2 if the byte does not arrive within 10 s.
- **Stale mounts** (after SIGKILL or a crash): `stat(mountpoint)` fails with ENOTCONN. `traj mount` runs
  `fusermount3 -u` on that state before mounting; `traj umount` does the same; `doctor` reports it.
- **Unmount with open files**: `fusermount3 -u` reports busy; `--lazy` detaches and the process ends when the last
  handle closes.
- **Systemd user unit** (documented, not installed by `traj`): `ExecStart=traj mount <mnt>` in the repo directory,
  `ExecStop=traj umount <mnt>`.

## 9. Viewers

**VS Code (Remote-SSH, this host).** The server runs as the same user, so the mount is visible with no
`allow_other`. Two rules, both in the README and the skill:

1. VS Code's file watcher crawls every workspace folder to set inotify watches, which on a 2 M-path tree is a
   `readdir` per directory and exhausts `max_user_watches`. So the mount is excluded from the watcher, and its
   files are marked read-only for editors (`files.readonlyInclude`, a lock icon instead of a failed save).
2. Where: `traj mount` merges the settings below into VS Code's machine-level settings on this host on every mount
   (they apply to whatever folder is opened, the mountpoint itself included); `traj init --mount-root <dir>` and
   `traj mount <dir> --save` also put them into the repo's `.vscode/settings.json`. Existing settings are kept, the
   write is atomic, a file that is not JSON is left alone and reported, and only the absolute path is used (a
   basename pattern such as `**/mnt/**` would hit unrelated directories):

   ```json
   { "files.watcherExclude": { "/home/me/traj-mnt/**": true },
     "files.readonlyInclude": { "/home/me/traj-mnt/**": true },
     "search.followSymlinks": false }
   ```

   Nothing under a mount changes without a batch, so the watcher loses nothing; the Explorer's manual refresh shows
   a new batch. Opening the whole mountpoint is fine for browsing; Ctrl+Shift+F over it still reads every file.

Editors show files read-only (the kernel says `EROFS`); "Save As" is the `traj edit` behaviour without a temp copy.
Ctrl+Shift+F is ripgrep over the mount: fine inside a round (450 MB logical, 17 MB of packs), slow over a run; run-wide
questions stay with `traj grep` / `traj sql`.

**Other viewers.** `diff -r --no-dereference` between two rounds, `meld`, `git diff --no-index`, Neovim, `ranger`,
`less`, Python `open()`: all work unchanged. A JupyterLab file browser rooted at the mount works too. Exec is
allowed (stored `+x` files run), which is what a reviewer re-running a script expects; `--noexec` is a flag.

## 10. Implementation map

| piece | where | content | size |
|---|---|---|---|
| CLI | `crates/traj/src/cmd/mount.rs`, `main.rs` `Cmd::Mount`, `Cmd::Umount` | args, refusals (§3), daemon (§8), umount, stale-mount check | ~150 lines |
| filesystem | `crates/traj/src/mount/fs.rs` | `impl fuser::Filesystem`: `init`, `lookup`, `getattr`, `readlink`, `open`, `read`, `release`, `opendir`, `readdir`, `readdirplus`, `releasedir`, `getxattr`, `listxattr`, `statfs`, `access` | ~350 lines |
| inodes | `crates/traj/src/mount/inodes.rs` | ino allocator, `(parent, name) → ino`, entry table, path reconstruction, per-store subtables | ~120 lines |
| caches | `crates/traj/src/mount/cache.rs` | listing LRU, blob LRU by bytes, file-handle table | ~120 lines |
| core additions | `trajfs-core::Store` | `read_blob(&self, reader, sha, verify) -> Result<Vec<u8>>` (today only `read_row` verifies, and it needs a `FileRow`); `manifest(&self) -> &Manifest` and `root(&self) -> &Path` if not already public; `manifest_mtime()` | ~30 lines |
| config, doctor, init | `config.rs`, `cmd/init.rs` | `mount_root`; `mounts:` line; `--mount-root` (also writes the watcher exclude) | ~60 lines |
| Cargo | `crates/traj/Cargo.toml` | `fuser = { version = "0.18", optional = true }`, `signal-hook`, feature `mount` in `default` | |
| docs | §14 | | |

Nothing in the store format, the catalog, the packs or `pack`/`verify` changes.

## 11. Performance targets (warm, rank 1 store, foreground, `--threads 1`)

| operation | target | basis |
|---|---|---|
| `ls` of a round directory, first time | ≤ 30 ms | `children` 10–20 ms |
| same, cached | ≤ 1 ms | listing cache |
| `cat` of a 40 B file (open + read + release) | ≤ 5 ms | `cat` 1–2 ms in-process |
| `cat` of a 5 MB log | ≤ 30 ms | 5 frames, one `pread` each |
| `ls -R` of round-0037 (106 K paths) | ≤ 3× the extracted tree | `PLAN.v1.md` T10 |
| `diff -r` round-0037 mount vs extract | ≤ 60 s | 106 K opens |
| RSS after opening a round in VS Code | ≤ 150 MB | index + listings + blobs |
| RSS after `ls -R` of the whole run | ≤ 400 MB | 2.14 M inode entries |
| mount time | ≤ 200 ms | manifest + index load |

## 12. Tests (T10, in `crates/traj/tests/cli.rs`)

Preconditions checked at the start of each test: `/dev/fuse` exists and `fusermount3` is on PATH; otherwise print
`skipped: no FUSE` and return (CI containers usually lack `/dev/fuse`). A `Mounted` guard spawns
`traj mount -S <store> <tmp>/mnt` in the foreground, waits until `/proc/self/mounts` lists the mountpoint (5 s
timeout), and on `Drop` sends SIGTERM, waits, and runs `fusermount3 -u` if the mountpoint is still listed, so a
panicking test never leaves a mount behind.

| test | what |
|---|---|
| `t10_mount_is_byte_identical` | `packed("none")` fixture; `snapshot(mnt)` equals `snapshot(extract)`: bytes, modes (`& 0555`), symlink targets, empty file, exec bit, unicode name, the 2 MiB file |
| `t10b_mount_matches_catalog_verbs` | `ls -A`, `find`, `du -sb` on the mount equal `traj ls`/`find`/`du` (the T4 oracle reversed); `stat` reports `nlink`, sizes, mtimes |
| `t10c_read_only` | open for write, `mkdir`, `touch`, `rm`, `chmod` each fail with `EROFS`; `getfattr -n user.traj.sha256` equals the catalog sha when `getfattr` is installed |
| `t10d_grep_equals_traj_grep` | `grep -rlE` on the mount for the T5b patterns equals `traj grep -l` |
| `t10e_concurrent_readers` | 8 threads read random paths for 2 s; every read verifies; no `EIO` |
| `t10f_new_batch_appears` | `traj pack` a second batch (T8 fixture change) into the mounted store; the new path lists within `--ttl 1`; a path re-recorded with new content shows the new bytes on a fresh open; a handle opened before the batch still returns the old bytes |
| `t10g_corruption_is_eio` | flip a byte in a pack (T3): `cat` of an affected path returns `EIO`, an unaffected path reads fine |
| `t10h_lifecycle` | SIGTERM unmounts cleanly; SIGKILL leaves a stale mount that `traj mount` clears before remounting; `umount` busy with an open file, `--lazy` succeeds; `--daemon` returns within 10 s with the mount live and a log file |
| `t10i_refusals` | mountpoint inside a git work tree, inside `data_root`, inside `store_root`, non-empty directory, already mounted |
| `t10j_multi_store_root` | two stores under `store_root`; root lists both ids; a third store added on disk appears within 5 s |
| `slow::t10k_reference_run` | mount the rank 1 store; every row of §11 measured and asserted; `diff -r` of round-0037 clean |

`t11` gains: the skill's mount example runs.

## 13. Milestones

| id | deliverable | exit criterion | estimate |
|---|---|---|---|
| M6a | single-store foreground mount: `lookup/getattr/readdir(+plus)/readlink/open/read/release/statfs`, listing and blob caches, `umount` | T10, T10b, T10c (minus xattrs), T10d, T10e, T10g green; round-0037 opens in VS Code over Remote-SSH | 1 day |
| M6b | freshness (§7), multi-store root, xattrs, `--daemon`, stale-mount handling, `doctor`, refusals, `mount_root` | T10f, T10h, T10i, T10j green | 1 day |
| M6c | docs (§14), the watcher exclude written by `traj init --mount-root`, T10k on rank 1, `traj bench` gains a mount row | §11 met and recorded in `PLAN.md` §14 | ½ day |

## 14. Documentation changes when M6a lands

- `PLAN.md`: §1 non-goal reworded to "FUSE as the storage format"; §5 verb table gains `mount`/`umount`; §9 points
  here; §10 milestone M6; §11 T10 as §12 above; §14 gets the measured rows.
- `README.md`: a "Browsing in VS Code and other tools" section after Quick start with the three-line recipe:

  ```
  traj mount ~/traj-mnt --daemon                       # every store under stores/, one directory per run
  code -r ~/traj-mnt/run-42/rounds/round-0037          # open a round, not a run (file watcher)
  traj umount ~/traj-mnt
  ```

- `skills/traj/SKILL.md`, "Materialise for a person": `traj mount` first, `extract` second; the rule "never write
  back into a store" gains "mounts are read-only; the kernel enforces it".
- `vscode-viewer.md`: decision line at the top pointing here (done with this draft).

## 15. Risks, with the default chosen

| risk | default |
|---|---|
| A directory with 100 K direct entries (some log directories) makes `lookup` of any child pay one big listing | accept; the listing is cached; measure on rank 1 and add a per-name `stat` fallback above 50 K entries only if needed |
| Inode table growth on a full-run `ls -R` | the best-effort `--memory` budget of §5.1: blobs and listings evicted, inodes handed back through `forget` |
| Kernel notifier calls fail on old kernels | ignore errors; the TTL bounds staleness anyway |
| `fusermount3` missing on a future host | clear message naming the package (`fuse3`) and `traj extract` |
| Users mount inside the repo | refused; `doctor` reports |
| Two `traj mount` processes on one mountpoint | second refused ("already mounted") |

## 16. Implementation status — 2026-09-04 (first build)

Built the same day as this plan: `traj mount`, `traj umount`, the `mounts:` line of `traj doctor`, `mount_root` in
`trajfs.toml` (`traj init --mount-root`), the skill text, and T10 as ten CLI tests plus the slow `t10k`.
`cargo test --release`: 25 CLI tests green in about 8 s (the T10 ones skip themselves, with a message, where
`/dev/fuse` or `fusermount3` is missing). Code: `crates/traj/src/mount/{mod.rs, fs.rs}` (≈ 1,000 lines),
`crates/traj/src/cmd/mount.rs` (≈ 400), `Store::read_blob` in the core (10). Dependencies added: `fuser` 0.18,
`signal-hook`, `libc`; `predicates` and `libc` for the tests. The `mount` Cargo feature is on by default.

Measured on this host against the rank 1 store (2.14 M paths, 17,039 directories and 105,940 files in
`round-0037`), foreground, one FUSE thread, defaults:

| operation | target (§11) | measured |
|---|---|---|
| mount (manifest + index load) | ≤ 200 ms | 70 ms, RSS 20 MB |
| `ls rounds` (38 entries; one `dirs` scan over the whole run) | – | 0.63–0.72 s cold, same as `traj ls rounds` |
| `ls` of a round directory, first time (prefetches the round: 17 K listings, 106 K entries) | ≤ 30 ms | 0.18 s (0.74 s when `rounds` was not listed before) |
| same, cached | ≤ 1 ms | 2–3 ms (process start included) |
| `cat` of a 6 KB file | ≤ 5 ms | 3 ms cold, 2 ms warm; p50 of 200 reads in-process 71 µs |
| `ls -R` of round-0037 (156,690 lines) | ≤ 3× the extracted tree | 1.8 s cold, 1.45 s warm, vs 0.31 s native: **5×** |
| `diff -r --no-dereference` round-0037 mount vs `extract` | ≤ 60 s | 9.3–9.7 s, clean |
| RSS after listing a round | ≤ 150 MB | 66–98 MB |
| RSS after `ls -R` + `diff -r` of a round | – | 280–315 MB (the 256 MiB blob cache is full) |
| `ls -R` of the whole run (3.14 M lines) | ≤ 400 MB | 41–47 s, **1.2 GB** (1.85 GB before the inode slimming) |

Two targets are missed. The recursive walk is 5× native instead of 3×: the cost is one FUSE round trip per
directory (`opendir`, `readdirplus`, `releasedir`) plus one `intern` per entry; `--threads` does not help a
single-threaded `ls -R`. The whole-run walk holds 2.1 M inodes at roughly 300 B each plus up to a million cached
listing entries; `--listing-cache` and `--blob-cache` bound the caches, the inode table is not bounded (the
`--memory` budget of §5.1 bounds it, best-effort, see there). Neither affects the intended use, which is one round in an editor.

Deviations from the design text above, all deliberate:

- **Listings are prefetched per subtree** (§4, §5). One catalog scan per directory costs 50–90 ms (row-group pruning
  cannot help a leaf directory), so a recursive walk of a round would take minutes. A cache miss on a directory whose
  subtree holds ≤ 250,000 files now runs one `dirs_under` + one `files_under` scan (53 ms for round-0037) and fills
  the listings of every directory below it. Bigger subtrees (the store root, `rounds/`) are listed one level at a
  time as before.
- **Listings have no TTL.** They change only with a batch, and the manifest-mtime check (§7) clears them then.
  `--ttl` is the kernel entry/attribute lifetime only. `--listing-cache` counts entries (default 1,000,000), not
  listings.
- **A changed sha means a new inode.** The inode key is (parent, name); an entry whose sha differs from the
  inode's gets a fresh inode number, and the old inode keeps a snapshot of its attributes (kind, mode, size, mtime).
  Without this the kernel's page cache, which is per inode, served the old bytes to a fresh open and the new size
  to an old handle. The kernel is also asked for `FUSE_AUTO_INVAL_DATA`, and the reopen sends `inval_inode` as well
  as `inval_entry`.
- **Directory mtime is the manifest's mtime** (the newest batch), not the newest child mtime: the latter would need
  the directory's own listing on every `getattr` of a directory entry.
- **Inodes hold no path string.** The path inside the store is rebuilt from the parent chain when a listing or an
  xattr needs it; names are `Box<str>`, shared with the lookup key.
- **`allow_other`** is expressed through `fuser`'s session ACL (0.18 has no such mount option).
- `traj extract .` now means the whole store, as the README always said (`.` normalised to the root).
- T10 lives in `crates/traj/tests/cli.rs` as `t10::t10_…` through `t10::t10j_…` plus `t10::t10k_reference_store`
  (`--features slow`, `TRAJ_SLOW_STORE=<store dir>`); the `Mounted` guard sends SIGTERM on drop and falls back to
  `fusermount3 -u -z`, so a failing test leaves no mount behind.

Done later the same day: `traj init --mount-root` and `traj mount --save` write the watcher exclude (§9 item 2; test
`t9d`); the farm repo was set up with `traj mount ~/traj-mnt --save --daemon` (22 stores); the `--memory` budget
with `forget` (§5.1; tests `t10l` and the `Inodes::forget` unit test); `traj bench` measures the mount (`mount_s`,
`mount_ls_cold_s`, `mount_ls_warm_s`, `mount_cat_s`; test `t12`), which also fixed `traj bench`'s `--store` flag
clashing with the global `-S` (the verb panicked in clap before; it now takes `-S`).

Later the same day: every mount writes VS Code's machine-level settings (§9; `--no-vscode`; test `t10m`), after
the owner opened the mountpoint itself in VS Code. Lesson recorded: the first version of that write let the test
suite's mounts touch the developer's real settings file and a write race dropped two unrelated keys (restored by
hand); the tests now run with a throwaway `HOME`, the write is write-then-rename, and basename patterns are gone.

Everything in this plan is implemented. Open: the two missed performance targets above.
