//! Adapter interface: everything runner-specific goes through this trait (docs/PLAN.md §3.6).
//! trajfs ships only generic format parsers; a runner's adapter is a TOML file kept in the runner's own repo.

use crate::events::Event;
use std::path::Path;

pub trait Adapter: Send + Sync {
    fn name(&self) -> &str;
    fn version(&self) -> u16 {
        1
    }
    /// Path attributes recorded in `files.attrs` (e.g. `round=37`, `role=builder`).
    fn attrs(&self, _path: &str) -> Vec<(String, String)> {
        Vec::new()
    }
    /// Is this store path a trajectory that `parse_events` understands?
    fn is_trajectory(&self, _path: &str) -> bool {
        false
    }
    /// Split a trajectory blob into envelope events (docs/PLAN.md §3.5).
    fn parse_events(&self, _path: &str, _bytes: &[u8]) -> Vec<Event> {
        Vec::new()
    }
    /// Rule profile for trees produced by this runner: a built-in name or an absolute path to a TOML file.
    fn rule_profile(&self) -> Option<String> {
        None
    }
    /// For `traj watch`: glob (relative to data_root) selecting run directories. Default: direct children.
    fn run_glob(&self) -> Option<String> {
        None
    }
    /// For `traj watch`: a label when a new batch of the source tree is ready to be packed.
    fn batch_ready(&self, _src: &Path, _already: &[String]) -> Option<String> {
        None
    }
    /// Fallible readiness discovery. Watchers must not treat unreadable source
    /// trees as a successful "nothing ready" result.
    fn try_batch_ready(&self, src: &Path, already: &[String]) -> anyhow::Result<Option<String>> {
        Ok(self.batch_ready(src, already))
    }
    /// Regexes on repo-relative paths that the pre-commit hook must refuse (raw run output).
    fn raw_patterns(&self) -> Vec<String> {
        Vec::new()
    }
}

#[derive(Default)]
pub struct NoAdapter;

impl Adapter for NoAdapter {
    fn name(&self) -> &str {
        "none"
    }
}
