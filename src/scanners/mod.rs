pub mod appdata;
pub mod caches;
pub mod derived_data;
pub mod device_support;
pub mod leftovers;
pub mod node_modules;
pub mod simulators;
pub mod worktrees;

use crate::config::Config;
use crate::git::Repo;
use crate::model::Item;
use chrono::{DateTime, Utc};
use std::path::{Path, PathBuf};

pub type Emit<'a> = &'a (dyn Fn(Item) + Sync);

pub struct Ctx {
    pub home: PathBuf,
    pub cfg: Config,
    pub now: DateTime<Utc>,
    pub roots: Vec<PathBuf>,
    pub repos: Vec<Repo>,
    pub proc_cwds: Vec<(u32, String, PathBuf)>,
}

impl Ctx {
    pub fn all_worktree_paths(&self) -> Vec<PathBuf> {
        self.repos
            .iter()
            .flat_map(|r| r.worktrees.iter().map(|w| w.path.clone()))
            .collect()
    }

    pub fn live_worktree_names(&self) -> Vec<String> {
        self.repos
            .iter()
            .flat_map(|r| r.worktrees.iter())
            .filter(|w| w.path.exists())
            .filter_map(|w| w.path.file_name().map(|n| n.to_string_lossy().into_owned()))
            .collect()
    }

    pub fn processes_in(&self, dir: &Path) -> Vec<String> {
        let me = std::process::id();
        let mut names: Vec<String> = self
            .proc_cwds
            .iter()
            .filter(|(pid, name, cwd)| *pid != me && name != "lsof" && cwd.starts_with(dir))
            .map(|(_, name, _)| name.clone())
            .collect();
        names.sort();
        names.dedup();
        names
    }

    pub fn stale(&self, idle_days: Option<i64>) -> bool {
        idle_days.is_some_and(|d| d > self.cfg.stale_days)
    }
}

pub fn tilde(ctx: &Ctx, p: &Path) -> String {
    crate::util::tilde(p, &ctx.home)
}
