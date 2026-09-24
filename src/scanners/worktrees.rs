use super::{Ctx, Emit, tilde};
use crate::git::{self, PrInfo, PrState, Prs, Repo, Worktree};
use crate::model::{Action, Category, Item, Verdict};
use crate::size::{Opts, dir_stats_opts};
use crate::util::{age_days, human, mtime, newer, which};
use std::path::Path;

pub const MTIME_IGNORE: &[&str] = &[
    ".git",
    "node_modules",
    "Pods",
    "build",
    "DerivedData",
    ".next",
    "target",
    "dist",
    ".turbo",
    ".expo",
    ".cache",
];

#[derive(Debug, Clone, Default, PartialEq)]
pub enum Presence {
    #[default]
    Present,
    Missing,
    Unknown(String),
}

impl Presence {
    pub fn of(path: &Path) -> Self {
        match path.try_exists() {
            Ok(true) => Presence::Present,
            Ok(false) => Presence::Missing,
            Err(e) => Presence::Unknown(e.to_string()),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct WtFacts {
    pub presence: Presence,
    pub on_volume: bool,
    pub prunable: bool,
    pub locked: bool,
    pub dirty: Option<usize>,
    pub unpushed: Option<usize>,
    pub merged_ancestor: bool,
    pub pr: Option<PrInfo>,
    pub idle_days: Option<i64>,
    pub default_ref: Option<String>,
    pub running: Option<Vec<String>>,
}

pub fn classify(f: &WtFacts, stale_days: i64) -> (Verdict, Vec<String>) {
    let mut r: Vec<String> = Vec::new();
    if f.locked {
        r.push("locked with `git worktree lock`".into());
        return (Verdict::Keep, r);
    }
    if let Presence::Unknown(e) = &f.presence {
        r.push(format!(
            "couldn't check whether the checkout folder exists: {e}"
        ));
        return (Verdict::Review, r);
    }
    if f.presence == Presence::Missing || f.prunable {
        r.push("checkout folder is gone but git still lists it".into());
        if f.on_volume {
            r.push("it is on /Volumes: is the drive not mounted?".into());
            return (Verdict::Review, r);
        }
        return (Verdict::Safe, r);
    }
    let (verdict, mut r) = classify_present(f, stale_days);
    if verdict == Verdict::Safe && f.running.is_none() {
        r.push("couldn't check for running processes (lsof failed)".into());
        return (Verdict::Review, r);
    }
    (verdict, r)
}

fn classify_present(f: &WtFacts, stale_days: i64) -> (Verdict, Vec<String>) {
    let mut r: Vec<String> = Vec::new();
    if let Some(n) = f.dirty.filter(|n| *n > 0) {
        r.push(format!("{n} uncommitted change(s)"));
        return (Verdict::Keep, r);
    }
    if let Some(running) = f.running.as_ref().filter(|p| !p.is_empty()) {
        r.push(format!("in use by {}", running.join(", ")));
        return (Verdict::Active, r);
    }
    if f.dirty.is_none() {
        r.push("couldn't read uncommitted changes (`git status` failed or timed out)".into());
    }
    if f.unpushed.is_none() {
        r.push("couldn't count unpushed commits (`git rev-list` failed or timed out)".into());
    }
    if !r.is_empty() {
        return (Verdict::Review, r);
    }
    let unpushed = f.unpushed.unwrap_or(0);
    let idle = f.idle_days.unwrap_or(0);
    let default = f.default_ref.as_deref().unwrap_or("the default branch");
    if let Some(pr) = f.pr.as_ref().filter(|p| p.state == PrState::Merged) {
        r.push(format!("PR #{} merged", pr.number));
        if idle >= 1 {
            return (Verdict::Safe, r);
        }
        r.push("but touched in the last day".into());
        return (Verdict::Review, r);
    }
    if f.merged_ancestor {
        if idle >= 3 {
            r.push(format!("no commits beyond {default}; idle {idle} days"));
            return (Verdict::Safe, r);
        }
        r.push(format!("no commits beyond {default} yet (fresh worktree)"));
        return (Verdict::Active, r);
    }
    if let Some(pr) = f.pr.as_ref().filter(|p| p.state == PrState::Closed)
        && unpushed == 0
    {
        r.push(format!("PR #{} closed without merging", pr.number));
        return (Verdict::Review, r);
    }
    if unpushed > 0 {
        r.push(format!("{unpushed} commit(s) not pushed to any remote"));
        return (Verdict::Keep, r);
    }
    if let Some(pr) = f.pr.as_ref().filter(|p| p.state == PrState::Open) {
        r.push(format!("PR #{} open", pr.number));
    }
    if idle > stale_days {
        r.push(format!("idle {idle} days; branch is pushed"));
        return (Verdict::Review, r);
    }
    r.push(format!("last activity {idle}d ago"));
    (Verdict::Active, r)
}

pub fn scan(ctx: &Ctx, emit: Emit) {
    let have_gh = which("gh").is_some();
    std::thread::scope(|s| {
        for repo in ctx.repos.iter().filter(|r| !r.worktrees.is_empty()) {
            s.spawn(move || {
                let prs = if have_gh {
                    git::pr_states(&repo.root)
                } else {
                    Prs::default()
                };
                std::thread::scope(|s2| {
                    for wt in &repo.worktrees {
                        let prs = &prs;
                        s2.spawn(move || emit(build(ctx, repo, wt, prs)));
                    }
                });
            });
        }
    });
}

pub fn action_for(wt: &Path, repo_root: &Path) -> Action {
    Action::run(
        "git",
        &["worktree", "remove", &wt.display().to_string()],
        Some(repo_root.to_path_buf()),
    )
}

fn build(ctx: &Ctx, repo: &Repo, wt: &Worktree, prs: &Prs) -> Item {
    let repo_name = repo
        .root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let branch = wt.branch.clone().unwrap_or_else(|| "(detached)".into());
    let mut item = Item::new(
        Category::Worktree,
        format!("{repo_name} · {branch}"),
        &wt.path,
    );
    item.owner = Some(repo.root.clone());
    let presence = Presence::of(&wt.path);
    let default_branch = repo.default_ref.as_deref().map(git::branch_of_ref);
    let candidates = wt
        .branch
        .as_deref()
        .map(|b| prs.for_branch(b, default_branch))
        .unwrap_or_default();
    let mut facts = WtFacts {
        presence: presence.clone(),
        on_volume: wt.path.starts_with("/Volumes"),
        prunable: wt.prunable,
        locked: wt.locked,
        default_ref: repo.default_ref.clone(),
        ..Default::default()
    };
    let present = presence == Presence::Present && !wt.prunable;
    if present {
        let reflog = git::gitdir_of(&wt.path).and_then(|g| mtime(&g.join("logs/HEAD")));
        let stats = dir_stats_opts(
            &wt.path,
            &Opts {
                breakdown_under: None,
                mtime_ignore: MTIME_IGNORE,
            },
        );
        item.set_stats(&stats);
        item.last_used = newer(reflog, stats.newest);
        let gs = git::worktree_state(&wt.path, repo.default_ref.as_deref());
        facts.dirty = gs.dirty;
        facts.unpushed = gs.unpushed;
        facts.merged_ancestor = gs.merged_ancestor;
        facts.idle_days = age_days(item.last_used, ctx.now);
        facts.running = ctx.processes_in(&wt.path);
        facts.pr = git::pick_pr(&candidates, &wt.head, |oid| {
            git::head_contained_in(&wt.path, oid)
        });
    }
    let (verdict, mut reasons) = classify(&facts, ctx.cfg.stale_days);
    item.verdict = verdict;
    reasons.push(format!("branch {branch} of {}", tilde(ctx, &repo.root)));
    if item.bytes > 0 && item.reclaimable + (1 << 30) < item.bytes {
        reasons.push(format!(
            "frees about {}; the rest is hardlinked from a shared store (pnpm)",
            human(item.reclaimable)
        ));
    }
    if wt.path.to_string_lossy().contains("/orca/workspaces/") {
        reasons.push("Orca-managed: deleting it in Orca also runs its archive hook".into());
    }
    if verdict == Verdict::Safe && present {
        reasons.push("its DerivedData and simulators show up as leftovers after a rescan".into());
    }
    item.reasons = reasons;
    item.action = action_for(&wt.path, &repo.root);
    item
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> WtFacts {
        WtFacts {
            presence: Presence::Present,
            dirty: Some(0),
            unpushed: Some(0),
            idle_days: Some(10),
            default_ref: Some("origin/beta".into()),
            running: Some(vec![]),
            ..Default::default()
        }
    }

    fn pr(state: PrState) -> Option<PrInfo> {
        Some(PrInfo {
            number: 7,
            state,
            head_oid: "abc".into(),
        })
    }

    #[test]
    fn missing_or_prunable_is_safe() {
        let f = WtFacts {
            presence: Presence::Missing,
            ..facts()
        };
        assert_eq!(classify(&f, 30).0, Verdict::Safe);
        let f = WtFacts {
            prunable: true,
            ..facts()
        };
        assert_eq!(classify(&f, 30).0, Verdict::Safe);
    }

    #[test]
    fn unstatable_folder_is_review() {
        let f = WtFacts {
            presence: Presence::Unknown("permission denied".into()),
            ..facts()
        };
        let (v, r) = classify(&f, 30);
        assert_eq!(v, Verdict::Review);
        assert!(r[0].contains("permission denied"));
    }

    #[test]
    fn presence_uses_try_exists() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(Presence::of(d.path()), Presence::Present);
        assert_eq!(Presence::of(&d.path().join("nope")), Presence::Missing);
        let locked = d.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::create_dir(locked.join("wt")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let p = Presence::of(&locked.join("wt"));
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(p, Presence::Unknown(_)), "{p:?}");
    }

    #[test]
    fn missing_on_unmounted_volume_is_review() {
        let f = WtFacts {
            presence: Presence::Missing,
            on_volume: true,
            ..facts()
        };
        let (v, r) = classify(&f, 30);
        assert_eq!(v, Verdict::Review);
        assert!(r.iter().any(|x| x.contains("not mounted")));
    }

    #[test]
    fn locked_beats_missing() {
        let f = WtFacts {
            presence: Presence::Missing,
            locked: true,
            ..facts()
        };
        assert_eq!(classify(&f, 30).0, Verdict::Keep);
        let f = WtFacts {
            prunable: true,
            locked: true,
            ..facts()
        };
        assert_eq!(classify(&f, 30).0, Verdict::Keep);
    }

    #[test]
    fn unknown_git_state_is_review_not_safe() {
        let f = WtFacts {
            dirty: None,
            pr: pr(PrState::Merged),
            ..facts()
        };
        let (v, r) = classify(&f, 30);
        assert_eq!(v, Verdict::Review);
        assert!(r[0].contains("git status"));
        let f = WtFacts {
            unpushed: None,
            merged_ancestor: true,
            ..facts()
        };
        let (v, r) = classify(&f, 30);
        assert_eq!(v, Verdict::Review);
        assert!(r[0].contains("git rev-list"));
        let f = WtFacts {
            dirty: Some(1),
            unpushed: None,
            ..facts()
        };
        assert_eq!(classify(&f, 30).0, Verdict::Keep);
    }

    #[test]
    fn unknown_processes_block_safe() {
        let f = WtFacts {
            running: None,
            pr: pr(PrState::Merged),
            ..facts()
        };
        let (v, r) = classify(&f, 30);
        assert_eq!(v, Verdict::Review);
        assert!(r.iter().any(|x| x.contains("lsof")));
        let f = WtFacts {
            running: None,
            idle_days: Some(2),
            ..facts()
        };
        assert_eq!(classify(&f, 30).0, Verdict::Active);
    }

    #[test]
    fn remove_targets_only_its_own_missing_worktree() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path().join("r");
        let g = |args: &[&str]| {
            let out = crate::util::run("git", args, Some(&r), std::time::Duration::from_secs(20))
                .unwrap();
            assert!(out.ok, "git {args:?}: {}", out.stderr);
            out.stdout
        };
        std::fs::create_dir(&r).unwrap();
        g(&["init", "-q", "-b", "main"]);
        g(&[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "one",
        ]);
        let (w1, w2) = (d.path().join("w1"), d.path().join("w2"));
        g(&[
            "worktree",
            "add",
            "-q",
            &w1.display().to_string(),
            "-b",
            "b1",
        ]);
        g(&[
            "worktree",
            "add",
            "-q",
            &w2.display().to_string(),
            "-b",
            "b2",
        ]);
        std::fs::remove_dir_all(&w1).unwrap();
        std::fs::remove_dir_all(&w2).unwrap();
        let Action::Run { program, args, cwd } = action_for(&w1, &r) else {
            panic!()
        };
        assert_eq!(cwd.as_deref(), Some(r.as_path()));
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        assert_eq!(program, "git");
        g(&args);
        let list = g(&["worktree", "list", "--porcelain"]);
        assert!(!list.contains("/w1\n"));
        assert!(list.contains("/w2\n"));
    }

    #[test]
    fn dirty_beats_merged() {
        let f = WtFacts {
            dirty: Some(2),
            pr: pr(PrState::Merged),
            ..facts()
        };
        let (v, r) = classify(&f, 30);
        assert_eq!(v, Verdict::Keep);
        assert!(r[0].contains("2 uncommitted"));
    }

    #[test]
    fn merged_pr_is_safe_even_with_squashed_local_commits() {
        let f = WtFacts {
            unpushed: Some(5),
            pr: pr(PrState::Merged),
            ..facts()
        };
        assert_eq!(classify(&f, 30).0, Verdict::Safe);
    }

    #[test]
    fn merged_pr_touched_today_needs_review() {
        let f = WtFacts {
            pr: pr(PrState::Merged),
            idle_days: Some(0),
            ..facts()
        };
        assert_eq!(classify(&f, 30).0, Verdict::Review);
    }

    #[test]
    fn fresh_worktree_with_no_commits_is_active_not_safe() {
        let f = WtFacts {
            merged_ancestor: true,
            idle_days: Some(0),
            ..facts()
        };
        assert_eq!(classify(&f, 30).0, Verdict::Active);
        let f = WtFacts {
            merged_ancestor: true,
            idle_days: Some(5),
            ..facts()
        };
        assert_eq!(classify(&f, 30).0, Verdict::Safe);
    }

    #[test]
    fn running_process_keeps_it_active() {
        let f = WtFacts {
            pr: pr(PrState::Merged),
            running: Some(vec!["node".into()]),
            ..facts()
        };
        assert_eq!(classify(&f, 30).0, Verdict::Active);
    }

    #[test]
    fn unpushed_commits_are_kept() {
        let f = WtFacts {
            unpushed: Some(3),
            idle_days: Some(200),
            ..facts()
        };
        assert_eq!(classify(&f, 30).0, Verdict::Keep);
    }

    #[test]
    fn closed_pr_without_unpushed_is_review() {
        let f = WtFacts {
            pr: pr(PrState::Closed),
            ..facts()
        };
        assert_eq!(classify(&f, 30).0, Verdict::Review);
    }

    #[test]
    fn pushed_and_idle_is_review_recent_is_active() {
        let f = WtFacts {
            idle_days: Some(45),
            ..facts()
        };
        assert_eq!(classify(&f, 30).0, Verdict::Review);
        let f = WtFacts {
            idle_days: Some(4),
            pr: pr(PrState::Open),
            ..facts()
        };
        let (v, r) = classify(&f, 30);
        assert_eq!(v, Verdict::Active);
        assert!(r.iter().any(|x| x.contains("PR #7 open")));
    }

    #[test]
    fn locked_is_kept() {
        let f = WtFacts {
            locked: true,
            pr: pr(PrState::Merged),
            ..facts()
        };
        assert_eq!(classify(&f, 30).0, Verdict::Keep);
    }
}
