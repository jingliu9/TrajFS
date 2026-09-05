//! `traj mount` / `traj umount` (docs/PLAN-fuse.md §3, §8). The filesystem itself is in `crate::mount`.

use anyhow::{bail, Result};
use clap::Args;
use std::path::{Path, PathBuf};

#[derive(Args, Debug)]
pub struct MountArgs {
    /// Mountpoint (default: mount_root from trajfs.toml); an empty directory outside every git work tree
    pub mountpoint: Option<PathBuf>,
    /// Detach: re-execute in the background, return once the mount is live
    #[arg(long)]
    pub daemon: bool,
    #[arg(long, hide = true)]
    pub foreground_child: bool,
    /// Log file for --daemon (default: <mountpoint>.log)
    #[arg(long)]
    pub log: Option<PathBuf>,
    /// Let other users read the mount (needs user_allow_other in /etc/fuse.conf)
    #[arg(long)]
    pub allow_other: bool,
    /// Kernel entry/attribute cache lifetime in seconds
    #[arg(long, default_value_t = 5.0)]
    pub ttl: f64,
    /// Bytes of decompressed blobs kept in memory (e.g. 256M, 1G)
    #[arg(long, default_value = "256M")]
    pub blob_cache: String,
    /// Directory entries kept in memory, per store
    #[arg(long, default_value_t = 1_000_000)]
    pub listing_cache: usize,
    /// Best-effort memory target for blobs + listings + inodes: a percentage of RAM (20%) or a size (2G); 0 = unbounded
    #[arg(long, default_value = "20%")]
    pub memory: String,
    /// FUSE worker threads
    #[arg(long, default_value_t = 1)]
    pub threads: usize,
    /// Skip the sha check on every read
    #[arg(long)]
    pub no_verify: bool,
    /// Do not touch VS Code's machine settings (the watcher exclude and read-only marking for the mountpoint)
    #[arg(long)]
    pub no_vscode: bool,
    /// Mount with noexec
    #[arg(long)]
    pub noexec: bool,
    /// Allow a mountpoint inside a git work tree (git and editors will crawl it)
    #[arg(long)]
    pub allow_in_repo: bool,
    /// Record the mountpoint as mount_root in trajfs.toml and exclude it from VS Code's file watcher
    /// (.vscode/settings.json in the repo)
    #[arg(long)]
    pub save: bool,
}

/// Merge a `files.watcherExclude` entry for `mount` into `<repo>/.vscode/settings.json` (created if absent;
/// other settings kept). VS Code crawls every workspace folder to set inotify watches; a 2 M-path mount must not
/// be one of them (docs/PLAN-fuse.md §9).
pub fn write_vscode_exclude(repo: &Path, mount: &Path) -> Result<PathBuf> {
    let file = repo.join(".vscode").join("settings.json");
    merge_vscode_settings(&file, mount)?;
    Ok(file)
}

/// The VS Code settings files on this machine that apply to every folder opened here: the Remote-SSH server's
/// machine settings and, for a local VS Code, the user settings. Only files whose directory already exists.
pub fn vscode_machine_settings() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    [
        home.join(".vscode-server/data/Machine/settings.json"),
        home.join(".config/Code/User/settings.json"),
    ]
    .into_iter()
    .filter(|f| f.parent().map(|d| d.is_dir()).unwrap_or(false))
    .collect()
}

/// Merge into one settings.json: `files.watcherExclude` and `files.readonlyInclude` for `mount` (absolute
/// path), `search.followSymlinks: false`. Existing settings are kept; a file that is not JSON is left
/// alone and reported.
pub fn merge_vscode_settings(file: &Path, mount: &Path) -> Result<()> {
    let mut root: serde_json::Value = match std::fs::read_to_string(file) {
        Ok(text) if !text.trim().is_empty() => serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("{}: not JSON ({e}); not touching it", file.display()))?,
        _ => serde_json::json!({}),
    };
    let obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{}: top level is not an object", file.display()))?;
    // absolute only: a basename pattern such as `**/mnt/**` would hit unrelated directories in every workspace
    let abs = crate::config::canon(mount);
    let patterns = vec![format!("{}/**", abs.display())];
    for key in ["files.watcherExclude", "files.readonlyInclude"] {
        let map = obj.entry(key).or_insert_with(|| serde_json::json!({}));
        if !map.is_object() {
            *map = serde_json::json!({});
        }
        let map = map.as_object_mut().unwrap();
        for p in &patterns {
            map.insert(p.clone(), serde_json::Value::Bool(true));
        }
    }
    obj.entry("search.followSymlinks")
        .or_insert(serde_json::Value::Bool(false));
    if let Some(d) = file.parent() {
        std::fs::create_dir_all(d)?;
    }
    // write-then-rename: a concurrent reader never sees a truncated file
    let tmp = file.with_extension(format!("json.traj-{}", std::process::id()));
    std::fs::write(&tmp, format!("{}\n", serde_json::to_string_pretty(&root)?))?;
    std::fs::rename(&tmp, file)?;
    Ok(())
}

#[derive(Args, Debug)]
pub struct UmountArgs {
    /// Mountpoint (default: every `traj mount` of this user)
    pub mountpoint: Option<PathBuf>,
    /// Lazy unmount (detach now, finish when the last file is closed)
    #[arg(long)]
    pub lazy: bool,
}

/// (mountpoint, fstype) of every mount visible to this process.
pub fn proc_mounts() -> Vec<(PathBuf, String)> {
    let text = std::fs::read_to_string("/proc/self/mounts").unwrap_or_default();
    text.lines()
        .filter_map(|l| {
            let mut it = l.split(' ');
            let _dev = it.next()?;
            let mp = unescape_mount(it.next()?);
            let ty = it.next()?.to_string();
            Some((PathBuf::from(mp), ty))
        })
        .collect()
}

/// `/proc/self/mounts` escapes space, tab, newline and backslash as `\040`-style octal.
fn unescape_mount(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() {
            if let Ok(v) =
                u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 4]).unwrap_or("x"), 8)
            {
                out.push(v);
                i += 4;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// Every `fuse.traj` mount of this user.
pub fn traj_mounts() -> Vec<PathBuf> {
    proc_mounts()
        .into_iter()
        .filter(|(_, ty)| ty == "fuse.traj")
        .map(|(mp, _)| mp)
        .collect()
}

pub fn is_mounted(mp: &Path) -> bool {
    let want = crate::config::canon(mp);
    proc_mounts().iter().any(|(m, _)| *m == want || *m == mp)
}

/// A mountpoint whose FUSE process died: `stat` fails with ENOTCONN.
pub fn is_stale(mp: &Path) -> bool {
    match std::fs::metadata(mp) {
        Err(e) => e.raw_os_error() == Some(107), // ENOTCONN
        Ok(_) => false,
    }
}

pub fn fusermount_unmount(mp: &Path, lazy: bool) -> Result<()> {
    let mut cmd = std::process::Command::new("fusermount3");
    cmd.arg("-u");
    if lazy {
        cmd.arg("-z");
    }
    let out = cmd.arg(mp).output();
    let out = match out {
        Ok(o) => o,
        Err(_) => {
            // older hosts ship only `fusermount`
            let mut cmd = std::process::Command::new("fusermount");
            cmd.arg("-u");
            if lazy {
                cmd.arg("-z");
            }
            cmd.arg(mp).output().map_err(|e| {
                anyhow::anyhow!("fusermount3/fusermount not found ({e}); install the fuse3 package")
            })?
        }
    };
    if !out.status.success() {
        bail!(
            "unmount {}: {}",
            mp.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

pub fn umount(a: UmountArgs) -> Result<i32> {
    let targets = match a.mountpoint {
        Some(mp) => vec![mp],
        None => traj_mounts(),
    };
    if targets.is_empty() {
        println!("no traj mounts");
        return Ok(0);
    }
    let mut code = 0;
    for mp in targets {
        match fusermount_unmount(&mp, a.lazy) {
            Ok(()) => println!("unmounted {}", mp.display()),
            Err(e) => {
                eprintln!("traj: {e:#}");
                code = 1;
            }
        }
    }
    Ok(code)
}

#[cfg(not(all(feature = "mount", target_os = "linux")))]
pub fn run(_stores: &[String], _a: MountArgs) -> Result<i32> {
    bail!("traj mount needs FUSE on Linux (this build has no `mount` feature); use `traj extract <dir> <dst>`")
}

#[cfg(all(feature = "mount", target_os = "linux"))]
pub fn run(stores: &[String], a: MountArgs) -> Result<i32> {
    linux::run(stores, a)
}

#[cfg(all(feature = "mount", target_os = "linux"))]
mod linux {
    use super::*;
    use crate::config::{canon, git_toplevel, is_inside, resolve_store, Config};
    use crate::mount::{parse_size, store_id_of, Options, StoreHandle, TrajFs};
    use anyhow::Context;
    use std::os::unix::fs::MetadataExt;
    use std::sync::Arc;
    use std::time::Duration;

    pub fn run(stores: &[String], a: MountArgs) -> Result<i32> {
        let cfg = Config::find();
        let mp = match (&a.mountpoint, &cfg) {
            (Some(p), _) => p.clone(),
            (None, Some(c)) => match &c.file.mount_root {
                Some(m) if m.is_absolute() => m.clone(),
                Some(m) => c.dir.join(m),
                None => bail!("no mountpoint given and no mount_root in trajfs.toml"),
            },
            (None, None) => bail!("no mountpoint given (and no trajfs.toml with mount_root)"),
        };
        // which stores
        let mut handles: Vec<Arc<StoreHandle>> = Vec::new();
        let mut store_root: Option<PathBuf> = None;
        let multi;
        if stores.is_empty() {
            let c = cfg
                .as_ref()
                .context("no store given: pass -S <store> or run inside a repo with trajfs.toml")?;
            let root = c.store_root();
            let mut dirs: Vec<PathBuf> = std::fs::read_dir(&root)
                .with_context(|| format!("read store_root {}", root.display()))?
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.join("MANIFEST.json").is_file())
                .collect();
            dirs.sort();
            for d in &dirs {
                handles.push(StoreHandle::open(&store_id_of(d), d, a.listing_cache)?);
            }
            store_root = Some(root);
            multi = true;
        } else {
            for s in stores {
                let p = resolve_store(s, cfg.as_ref())?;
                let id = store_id_of(&p);
                handles.push(StoreHandle::open(&id, &p, a.listing_cache)?);
            }
            multi = stores.len() > 1;
        }
        if multi {
            let mut ids: Vec<&str> = handles.iter().map(|h| h.id.as_str()).collect();
            ids.sort();
            ids.dedup();
            if ids.len() != handles.len() {
                bail!("two stores share an id; mount them separately");
            }
        }
        // the mountpoint
        if is_stale(&mp) {
            eprintln!("traj mount: {} is a stale mount; clearing it", mp.display());
            fusermount_unmount(&mp, true)?;
        }
        std::fs::create_dir_all(&mp).with_context(|| format!("create {}", mp.display()))?;
        if is_mounted(&mp) {
            bail!(
                "{} is already mounted; `traj umount {}` first",
                mp.display(),
                mp.display()
            );
        }
        let mp = canon(&mp);
        if !mp.is_dir() {
            bail!("{} is not a directory", mp.display());
        }
        if std::fs::read_dir(&mp)?.next().is_some() {
            bail!("{} is not empty", mp.display());
        }
        if let Some(c) = &cfg {
            if is_inside(&mp, &c.data_root()) {
                bail!("{} is inside data_root", mp.display());
            }
            if is_inside(&mp, &c.store_root()) {
                bail!("{} is inside store_root", mp.display());
            }
        }
        for h in &handles {
            if is_inside(&mp, &h.path) {
                bail!("{} is inside the store {}", mp.display(), h.path.display());
            }
        }
        if let Some(top) = git_toplevel(&mp) {
            if !a.allow_in_repo {
                bail!(
                    "{} is inside the git work tree {} (git and editors would crawl the mount); choose a mountpoint outside, or pass --allow-in-repo",
                    mp.display(),
                    top.display()
                );
            }
        }
        if a.save && !a.foreground_child {
            let Some(c) = &cfg else {
                bail!("--save needs a trajfs.toml (none found above the current directory)");
            };
            let mut c = c.clone();
            c.file.mount_root = Some(mp.clone());
            c.save()?;
            let f = write_vscode_exclude(&c.dir, &mp)?;
            eprintln!(
                "traj mount: mount_root = {} saved in {}; watcher exclude in {}",
                mp.display(),
                c.dir.join(crate::config::CONFIG_NAME).display(),
                f.display()
            );
        }
        // VS Code on this machine: never crawl the mount, show its files as read-only (docs/PLAN-fuse.md §9)
        if !a.no_vscode && !a.foreground_child {
            for f in vscode_machine_settings() {
                match merge_vscode_settings(&f, &mp) {
                    Ok(()) => eprintln!(
                        "traj mount: VS Code settings for {} in {}",
                        mp.display(),
                        f.display()
                    ),
                    Err(e) => eprintln!("traj mount: warning: {e:#}"),
                }
            }
        }
        if a.daemon && !a.foreground_child {
            return daemonise(&mp, a.log.as_deref());
        }

        let md = std::fs::metadata(&mp)?;
        let opts = Options {
            ttl: Duration::from_secs_f64(a.ttl.max(0.0)),
            blob_cache_bytes: parse_size(&a.blob_cache)?,
            listing_cache: a.listing_cache,
            memory_bytes: parse_memory(&a.memory)?,
            verify: !a.no_verify,
            debug: std::env::var_os("TRAJ_MOUNT_DEBUG").is_some(),
        };
        let label = if multi {
            store_root
                .as_ref()
                .map(|r| r.display().to_string())
                .unwrap_or_else(|| "stores".into())
        } else {
            handles[0].path.display().to_string()
        };
        let fs_memory = opts.memory_bytes;
        let fs = if multi {
            TrajFs::multi(handles, store_root, opts, md.uid(), md.gid())
        } else {
            TrajFs::single(handles.remove(0), opts, md.uid(), md.gid())
        };
        let (tx, rx) = std::sync::mpsc::channel::<(u64, String, u64, bool)>();
        fs.set_invalidator(tx);

        let mut options = vec![
            fuser::MountOption::RO,
            fuser::MountOption::NoSuid,
            fuser::MountOption::NoDev,
            fuser::MountOption::NoAtime,
            fuser::MountOption::DefaultPermissions,
            fuser::MountOption::FSName(format!("traj:{label}")),
            fuser::MountOption::Subtype("traj".into()),
        ];
        if a.noexec {
            options.push(fuser::MountOption::NoExec);
        }
        let mut config = fuser::Config::default();
        config.mount_options = options;
        // allow_other is expressed through the ACL in fuser 0.18 (it adds the mount option itself)
        config.acl = if a.allow_other {
            fuser::SessionACL::All
        } else {
            fuser::SessionACL::Owner
        };
        config.n_threads = Some(a.threads.max(1));
        let mut session = fuser::Session::new(fs, &mp, &config)
            .with_context(|| format!("mount {}", mp.display()))?;

        // kernel cache invalidation after a store reopen, off the request thread
        let notifier = session.notifier();
        std::thread::Builder::new()
            .name("traj-inval".into())
            .spawn(move || {
                let debug = std::env::var_os("TRAJ_MOUNT_DEBUG").is_some();
                for (parent, name, ino, data) in rx {
                    let r =
                        notifier.inval_entry(fuser::INodeNo(parent), std::ffi::OsStr::new(&name));
                    if debug {
                        eprintln!("traj mount: inval_entry({parent}, {name:?}) -> {r:?}");
                    }
                    if data {
                        let _ = notifier.inval_inode(fuser::INodeNo(ino), 0, 0);
                    }
                }
            })?;
        // SIGINT/SIGTERM/SIGHUP: unmount, which ends the session loop
        let mut unmounter = session.unmount_callable();
        let mut signals = signal_hook::iterator::Signals::new([
            signal_hook::consts::SIGINT,
            signal_hook::consts::SIGTERM,
            signal_hook::consts::SIGHUP,
        ])?;
        std::thread::Builder::new()
            .name("traj-signals".into())
            .spawn(move || {
                if signals.forever().next().is_some() {
                    let _ = unmounter.unmount();
                }
            })?;
        eprintln!(
            "traj mount: {} mounted at {} (read-only; memory target {}; Ctrl-C or `traj umount` to unmount)",
            label,
            mp.display(),
            if fs_memory == 0 {
                "unbounded".to_string()
            } else {
                format!("{} MB", fs_memory >> 20)
            }
        );
        match session.run() {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotConnected => {}
            Err(e) if e.raw_os_error() == Some(libc::ENODEV) => {}
            Err(e) => eprintln!("traj mount: session ended: {e}"),
        }
        eprintln!("traj mount: {} unmounted", mp.display());
        Ok(0)
    }

    /// `20%` of MemTotal, a size such as `2G`, or `0` for unbounded.
    pub fn parse_memory(s: &str) -> Result<usize> {
        let s = s.trim();
        if s == "0" {
            return Ok(0);
        }
        if let Some(pct) = s.strip_suffix('%') {
            let pct: f64 = pct
                .trim()
                .parse()
                .with_context(|| format!("{s}: not a percentage"))?;
            let total = std::fs::read_to_string("/proc/meminfo")
                .ok()
                .and_then(|t| {
                    t.lines()
                        .find_map(|l| l.strip_prefix("MemTotal:"))
                        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
                })
                .map(|kb| kb * 1024)
                .context("MemTotal not found in /proc/meminfo; give --memory as a size")?;
            return Ok(((total as f64) * pct / 100.0) as usize);
        }
        parse_size(s)
    }

    /// Re-execute detached; wait until the mount is live or the child dies.
    fn daemonise(mp: &Path, log: Option<&Path>) -> Result<i32> {
        use std::os::unix::process::CommandExt;
        let exe = std::env::current_exe()?;
        let log = log
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from(format!("{}.log", mp.display())));
        let f =
            std::fs::File::create(&log).with_context(|| format!("create log {}", log.display()))?;
        let mut args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
        args.push("--foreground-child".into());
        let mut cmd = std::process::Command::new(exe);
        cmd.args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(f.try_clone()?)
            .stderr(f);
        // SAFETY: setsid is async-signal-safe and touches no memory of the parent
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let mut child = cmd.spawn()?;
        let start = std::time::Instant::now();
        loop {
            if let Some(status) = child.try_wait()? {
                let tail = std::fs::read_to_string(&log).unwrap_or_default();
                bail!(
                    "mount process exited with {status} before mounting; log {}:\n{}",
                    log.display(),
                    tail.trim_end()
                );
            }
            if is_mounted(mp) && !is_stale(mp) {
                println!(
                    "mounted {} (pid {}, log {})",
                    mp.display(),
                    child.id(),
                    log.display()
                );
                return Ok(0);
            }
            if start.elapsed() > Duration::from_secs(10) {
                let _ = child.kill();
                bail!("mount did not come up within 10 s; log {}", log.display());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}
