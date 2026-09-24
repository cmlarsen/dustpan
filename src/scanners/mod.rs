pub mod appdata;
pub mod caches;
pub mod derived_data;
pub mod device_support;
pub mod docker;
pub mod downloads;
pub mod leftovers;
pub mod node_modules;
pub mod runtimes;
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
    pub proc_cwds: Option<Vec<(u32, String, PathBuf)>>,
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
            .filter(|w| !matches!(w.path.try_exists(), Ok(false)))
            .flat_map(|w| {
                let dir = w.path.file_name().map(|n| n.to_string_lossy().into_owned());
                let branch = w.branch.as_ref().map(|b| b.replace('/', "-"));
                dir.into_iter().chain(branch)
            })
            .collect()
    }

    pub fn processes_in(&self, dir: &Path) -> Option<Vec<String>> {
        let me = std::process::id();
        let mut names: Vec<String> = self
            .proc_cwds
            .as_ref()?
            .iter()
            .filter(|(pid, name, cwd)| {
                *pid != me && !name.is_empty() && name != "lsof" && cwd.starts_with(dir)
            })
            .map(|(_, name, _)| name.clone())
            .collect();
        names.sort();
        names.dedup();
        Some(names)
    }
}

pub fn tilde(ctx: &Ctx, p: &Path) -> String {
    crate::util::tilde(p, &ctx.home)
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::git::Worktree;

    pub fn ctx(repos: Vec<Repo>, proc_cwds: Option<Vec<(u32, String, PathBuf)>>) -> Ctx {
        Ctx {
            home: PathBuf::from("/nonexistent-home"),
            cfg: Config::default(),
            now: Utc::now(),
            roots: vec![],
            repos,
            proc_cwds,
        }
    }

    fn wt(path: PathBuf, branch: &str) -> Worktree {
        Worktree { path, head: String::new(), branch: Some(branch.into()), prunable: false, locked: false }
    }

    #[test]
    fn processes_in_is_unknown_when_lsof_failed() {
        let c = ctx(vec![], None);
        assert_eq!(c.processes_in(Path::new("/w")), None);
        let c = ctx(vec![], Some(vec![(1, "node".into(), PathBuf::from("/w/app"))]));
        assert_eq!(c.processes_in(Path::new("/w")), Some(vec!["node".to_string()]));
        assert_eq!(c.processes_in(Path::new("/x")), Some(vec![]));
    }

    #[test]
    fn processes_in_drops_pids_without_process_names() {
        let c = ctx(
            vec![],
            Some(vec![
                (1, "".into(), PathBuf::from("/w/missing-from-ps")),
                (2, "caffeinate".into(), PathBuf::from("/w/app")),
            ]),
        );
        assert_eq!(c.processes_in(Path::new("/w")), Some(vec!["caffeinate".into()]));
    }

    #[test]
    fn live_worktree_names_keep_unstatable_paths_and_branch_slugs() {
        let d = tempfile::tempdir().unwrap();
        let locked = d.path().join("locked");
        std::fs::create_dir_all(locked.join("hidden")).unwrap();
        std::fs::create_dir(d.path().join("here")).unwrap();
        let repo = Repo {
            root: d.path().to_path_buf(),
            default_ref: None,
            worktrees: vec![
                wt(d.path().join("here"), "cmlarsen/feat"),
                wt(d.path().join("gone"), "gone-branch"),
                wt(locked.join("hidden"), "hidden-branch"),
            ],
        };
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let names = ctx(vec![repo], Some(vec![])).live_worktree_names();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(names.contains(&"here".to_string()));
        assert!(names.contains(&"cmlarsen-feat".to_string()));
        assert!(names.contains(&"hidden".to_string()));
        assert!(!names.contains(&"gone".to_string()));
    }
}
