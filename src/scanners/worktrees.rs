use super::{tilde, Ctx, Emit};
use crate::git::{self, PrInfo, Repo, Worktree};
use crate::model::{Action, Category, Item, Verdict};
use crate::size::{dir_stats_opts, Opts};
use crate::util::{age_days, human, mtime, newer, which};
use std::collections::HashMap;

pub const MTIME_IGNORE: &[&str] = &[
    ".git", "node_modules", "Pods", "build", "DerivedData", ".next", "target", "dist", ".turbo",
    ".expo", ".cache",
];

#[derive(Debug, Clone, Default)]
pub struct WtFacts {
    pub exists: bool,
    pub prunable: bool,
    pub locked: bool,
    pub dirty: usize,
    pub unpushed: usize,
    pub merged_ancestor: bool,
    pub pr: Option<PrInfo>,
    pub idle_days: Option<i64>,
    pub default_ref: Option<String>,
    pub running: Vec<String>,
}

pub fn classify(f: &WtFacts, stale_days: i64) -> (Verdict, Vec<String>) {
    let mut r: Vec<String> = Vec::new();
    if !f.exists || f.prunable {
        r.push("checkout folder is gone but git still lists it".into());
        return (Verdict::Safe, r);
    }
    if f.locked {
        r.push("locked with `git worktree lock`".into());
        return (Verdict::Keep, r);
    }
    if f.dirty > 0 {
        r.push(format!("{} uncommitted change(s)", f.dirty));
        return (Verdict::Keep, r);
    }
    if !f.running.is_empty() {
        r.push(format!("in use by {}", f.running.join(", ")));
        return (Verdict::Active, r);
    }
    let idle = f.idle_days.unwrap_or(0);
    let default = f.default_ref.as_deref().unwrap_or("the default branch");
    if let Some(pr) = f.pr.as_ref().filter(|p| p.state == "MERGED") {
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
    if let Some(pr) = f.pr.as_ref().filter(|p| p.state == "CLOSED")
        && f.unpushed == 0 {
            r.push(format!("PR #{} closed without merging", pr.number));
            return (Verdict::Review, r);
        }
    if f.unpushed > 0 {
        r.push(format!("{} commit(s) not pushed to any remote", f.unpushed));
        return (Verdict::Keep, r);
    }
    if let Some(pr) = f.pr.as_ref().filter(|p| p.state == "OPEN") {
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
                    HashMap::new()
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

fn build(ctx: &Ctx, repo: &Repo, wt: &Worktree, prs: &HashMap<String, PrInfo>) -> Item {
    let repo_name = repo
        .root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let branch = wt.branch.clone().unwrap_or_else(|| "(detached)".into());
    let mut item = Item::new(Category::Worktree, format!("{repo_name} · {branch}"), &wt.path);
    item.owner = Some(repo.root.clone());
    let exists = wt.path.exists();
    let mut facts = WtFacts {
        exists,
        prunable: wt.prunable,
        locked: wt.locked,
        default_ref: repo.default_ref.clone(),
        pr: wt.branch.as_ref().and_then(|b| prs.get(b).cloned()),
        ..Default::default()
    };
    if exists && !wt.prunable {
        let reflog = git::gitdir_of(&wt.path).and_then(|g| mtime(&g.join("logs/HEAD")));
        let stats = dir_stats_opts(
            &wt.path,
            &Opts {
                breakdown_under: None,
                mtime_ignore: MTIME_IGNORE,
            },
        );
        item.bytes = stats.bytes;
        item.reclaimable = stats.exclusive;
        item.last_used = newer(reflog, stats.newest);
        let gs = git::worktree_state(&wt.path, repo.default_ref.as_deref());
        facts.dirty = gs.dirty;
        facts.unpushed = gs.unpushed;
        facts.merged_ancestor = gs.merged_ancestor;
        facts.idle_days = age_days(item.last_used, ctx.now);
        facts.running = ctx.processes_in(&wt.path);
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
    if verdict == Verdict::Safe && exists {
        reasons.push("its DerivedData and simulators show up as leftovers after a rescan".into());
    }
    item.reasons = reasons;
    item.action = if !exists || wt.prunable {
        Action::run("git", &["worktree", "prune"], Some(repo.root.clone()))
    } else {
        Action::run(
            "git",
            &["worktree", "remove", &wt.path.display().to_string()],
            Some(repo.root.clone()),
        )
    };
    item
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> WtFacts {
        WtFacts {
            exists: true,
            idle_days: Some(10),
            default_ref: Some("origin/beta".into()),
            ..Default::default()
        }
    }

    fn pr(state: &str) -> Option<PrInfo> {
        Some(PrInfo {
            number: 7,
            state: state.into(),
        })
    }

    #[test]
    fn missing_or_prunable_is_safe() {
        let f = WtFacts { exists: false, ..facts() };
        assert_eq!(classify(&f, 30).0, Verdict::Safe);
        let f = WtFacts { prunable: true, ..facts() };
        assert_eq!(classify(&f, 30).0, Verdict::Safe);
    }

    #[test]
    fn dirty_beats_merged() {
        let f = WtFacts { dirty: 2, pr: pr("MERGED"), ..facts() };
        let (v, r) = classify(&f, 30);
        assert_eq!(v, Verdict::Keep);
        assert!(r[0].contains("2 uncommitted"));
    }

    #[test]
    fn merged_pr_is_safe_even_with_squashed_local_commits() {
        let f = WtFacts { unpushed: 5, pr: pr("MERGED"), ..facts() };
        assert_eq!(classify(&f, 30).0, Verdict::Safe);
    }

    #[test]
    fn merged_pr_touched_today_needs_review() {
        let f = WtFacts { pr: pr("MERGED"), idle_days: Some(0), ..facts() };
        assert_eq!(classify(&f, 30).0, Verdict::Review);
    }

    #[test]
    fn fresh_worktree_with_no_commits_is_active_not_safe() {
        let f = WtFacts { merged_ancestor: true, idle_days: Some(0), ..facts() };
        assert_eq!(classify(&f, 30).0, Verdict::Active);
        let f = WtFacts { merged_ancestor: true, idle_days: Some(5), ..facts() };
        assert_eq!(classify(&f, 30).0, Verdict::Safe);
    }

    #[test]
    fn running_process_keeps_it_active() {
        let f = WtFacts { pr: pr("MERGED"), running: vec!["node".into()], ..facts() };
        assert_eq!(classify(&f, 30).0, Verdict::Active);
    }

    #[test]
    fn unpushed_commits_are_kept() {
        let f = WtFacts { unpushed: 3, idle_days: Some(200), ..facts() };
        assert_eq!(classify(&f, 30).0, Verdict::Keep);
    }

    #[test]
    fn closed_pr_without_unpushed_is_review() {
        let f = WtFacts { pr: pr("CLOSED"), ..facts() };
        assert_eq!(classify(&f, 30).0, Verdict::Review);
    }

    #[test]
    fn pushed_and_idle_is_review_recent_is_active() {
        let f = WtFacts { idle_days: Some(45), ..facts() };
        assert_eq!(classify(&f, 30).0, Verdict::Review);
        let f = WtFacts { idle_days: Some(4), pr: pr("OPEN"), ..facts() };
        let (v, r) = classify(&f, 30);
        assert_eq!(v, Verdict::Active);
        assert!(r.iter().any(|x| x.contains("PR #7 open")));
    }

    #[test]
    fn locked_is_kept() {
        let f = WtFacts { locked: true, pr: pr("MERGED"), ..facts() };
        assert_eq!(classify(&f, 30).0, Verdict::Keep);
    }
}
