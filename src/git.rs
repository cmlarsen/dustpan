use crate::util::run;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

const GIT_TIMEOUT: Duration = Duration::from_secs(20);
const SKIP_DIRS: &[&str] = &["node_modules", "Pods", "build", "DerivedData", "target", "dist", "vendor"];

#[derive(Debug, Clone, PartialEq)]
pub struct Worktree {
    pub path: PathBuf,
    pub head: String,
    pub branch: Option<String>,
    pub prunable: bool,
    pub locked: bool,
}

#[derive(Debug, Clone)]
pub struct Repo {
    pub root: PathBuf,
    pub default_ref: Option<String>,
    pub worktrees: Vec<Worktree>,
}

pub fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = run("git", args, Some(dir), GIT_TIMEOUT)?;
    out.ok.then(|| out.stdout.trim_end().to_string())
}

pub fn discover_repos(roots: &[PathBuf], max_depth: usize) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for root in roots {
        walk_for_repos(root, max_depth, &mut found);
    }
    found.sort();
    found.dedup();
    found
}

fn walk_for_repos(dir: &Path, depth: usize, found: &mut Vec<PathBuf>) {
    if dir.join(".git").is_dir() {
        found.push(dir.to_path_buf());
    }
    if depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || SKIP_DIRS.contains(&name.as_ref()) {
            continue;
        }
        if e.file_type().is_ok_and(|t| t.is_dir()) {
            walk_for_repos(&e.path(), depth - 1, found);
        }
    }
}

pub fn parse_worktree_porcelain(text: &str) -> Vec<Worktree> {
    let mut out = Vec::new();
    let mut cur: Option<Worktree> = None;
    for line in text.lines().chain(std::iter::once("")) {
        if line.is_empty() {
            if let Some(w) = cur.take() {
                out.push(w);
            }
            continue;
        }
        let (key, val) = line.split_once(' ').unwrap_or((line, ""));
        match key {
            "worktree" => {
                cur = Some(Worktree {
                    path: PathBuf::from(val),
                    head: String::new(),
                    branch: None,
                    prunable: false,
                    locked: false,
                })
            }
            "HEAD" => {
                if let Some(w) = cur.as_mut() {
                    w.head = val.to_string();
                }
            }
            "branch" => {
                if let Some(w) = cur.as_mut() {
                    w.branch = Some(val.trim_start_matches("refs/heads/").to_string());
                }
            }
            "prunable" => {
                if let Some(w) = cur.as_mut() {
                    w.prunable = true;
                }
            }
            "locked" => {
                if let Some(w) = cur.as_mut() {
                    w.locked = true;
                }
            }
            _ => {}
        }
    }
    out
}

pub fn load_repo(root: &Path) -> Repo {
    let worktrees = git(root, &["worktree", "list", "--porcelain"])
        .map(|s| parse_worktree_porcelain(&s))
        .unwrap_or_default()
        .into_iter()
        .skip(1)
        .collect();
    Repo {
        root: root.to_path_buf(),
        default_ref: default_ref(root),
        worktrees,
    }
}

pub fn default_ref(root: &Path) -> Option<String> {
    if let Some(r) = git(root, &["symbolic-ref", "--quiet", "--short", "refs/remotes/origin/HEAD"]) {
        return Some(r);
    }
    ["origin/main", "origin/master", "origin/beta"]
        .into_iter()
        .find(|r| git(root, &["rev-parse", "--verify", "--quiet", r]).is_some())
        .map(String::from)
}

#[derive(Debug, Clone, Default)]
pub struct WtGitState {
    pub dirty: Option<usize>,
    pub unpushed: Option<usize>,
    pub merged_ancestor: bool,
}

pub fn worktree_state(wt: &Path, default_ref: Option<&str>) -> WtGitState {
    let dirty = git(wt, &["status", "--porcelain", "--untracked-files=normal"])
        .map(|s| s.lines().filter(|l| !l.is_empty()).count());
    let unpushed = git(wt, &["rev-list", "--count", "HEAD", "--not", "--remotes"])
        .and_then(|s| s.trim().parse().ok());
    let merged_ancestor = default_ref.is_some_and(|r| {
        run("git", &["merge-base", "--is-ancestor", "HEAD", r], Some(wt), GIT_TIMEOUT)
            .is_some_and(|o| o.ok)
    });
    WtGitState {
        dirty,
        unpushed,
        merged_ancestor,
    }
}

pub fn head_contained_in(wt: &Path, oid: &str) -> bool {
    run("git", &["merge-base", "--is-ancestor", "HEAD", oid], Some(wt), GIT_TIMEOUT).is_some_and(|o| o.ok)
}

pub fn gitdir_of(wt: &Path) -> Option<PathBuf> {
    let dotgit = wt.join(".git");
    if dotgit.is_dir() {
        return Some(dotgit);
    }
    let text = std::fs::read_to_string(dotgit).ok()?;
    let p = text.trim().strip_prefix("gitdir:")?.trim();
    Some(PathBuf::from(p))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrState {
    Open,
    Merged,
    Closed,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PrInfo {
    pub number: u64,
    pub state: PrState,
    pub head_oid: String,
}

#[derive(Debug, Clone, Default)]
pub struct Prs {
    pub by_head: HashMap<String, Vec<PrInfo>>,
    pub bases: HashSet<String>,
}

impl Prs {
    pub fn for_branch(&self, branch: &str, default_branch: Option<&str>) -> &[PrInfo] {
        if default_branch == Some(branch) || self.bases.contains(branch) {
            return &[];
        }
        self.by_head.get(branch).map(Vec::as_slice).unwrap_or(&[])
    }
}

pub fn branch_of_ref(r: &str) -> &str {
    r.split_once('/').map_or(r, |(_, b)| b)
}

pub fn pick_pr(prs: &[PrInfo], head: &str, contains: impl Fn(&str) -> bool) -> Option<PrInfo> {
    if let Some(open) = prs.iter().filter(|p| p.state == PrState::Open).max_by_key(|p| p.number) {
        return Some(open.clone());
    }
    prs.iter()
        .filter(|p| !p.head_oid.is_empty() && (p.head_oid == head || contains(&p.head_oid)))
        .max_by_key(|p| (p.state == PrState::Merged, p.number))
        .cloned()
}

pub fn pr_states(repo: &Path) -> Prs {
    let Some(out) = run(
        "gh",
        &[
            "pr", "list", "--state", "all", "--limit", "400", "--json",
            "number,headRefName,headRefOid,state,isCrossRepository,baseRefName",
        ],
        Some(repo),
        Duration::from_secs(15),
    ) else {
        return Prs::default();
    };
    if !out.ok {
        return Prs::default();
    }
    parse_pr_json(&out.stdout)
}

pub fn parse_pr_json(text: &str) -> Prs {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Pr {
        number: u64,
        head_ref_name: String,
        #[serde(default)]
        head_ref_oid: String,
        state: String,
        #[serde(default)]
        is_cross_repository: bool,
        #[serde(default)]
        base_ref_name: String,
    }
    let prs: Vec<Pr> = serde_json::from_str(text).unwrap_or_default();
    let mut out = Prs::default();
    for pr in prs {
        if !pr.base_ref_name.is_empty() {
            out.bases.insert(pr.base_ref_name);
        }
        if pr.is_cross_repository {
            continue;
        }
        let state = match pr.state.as_str() {
            "OPEN" => PrState::Open,
            "MERGED" => PrState::Merged,
            _ => PrState::Closed,
        };
        out.by_head.entry(pr.head_ref_name).or_default().push(PrInfo {
            number: pr.number,
            state,
            head_oid: pr.head_ref_oid,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_porcelain() {
        let text = "worktree /r/main\nHEAD abc\nbranch refs/heads/beta\n\nworktree /r/wt1\nHEAD def\nbranch refs/heads/feature/x\nlocked\n\nworktree /gone\nHEAD 123\ndetached\nprunable gitdir file points to non-existent location\n";
        let w = parse_worktree_porcelain(text);
        assert_eq!(w.len(), 3);
        assert_eq!(w[0].branch.as_deref(), Some("beta"));
        assert_eq!(w[1].branch.as_deref(), Some("feature/x"));
        assert!(w[1].locked);
        assert!(w[2].branch.is_none());
        assert!(w[2].prunable);
    }

    fn pr(number: u64, state: PrState, oid: &str) -> PrInfo {
        PrInfo { number, state, head_oid: oid.into() }
    }

    #[test]
    fn pr_json_parses_state_enum_and_skips_forks() {
        let text = r#"[{"number":1,"headRefName":"a","headRefOid":"o1","state":"CLOSED","isCrossRepository":false,"baseRefName":"main"},{"number":2,"headRefName":"a","headRefOid":"o2","state":"MERGED","isCrossRepository":false,"baseRefName":"main"},{"number":3,"headRefName":"b","headRefOid":"o3","state":"OPEN","isCrossRepository":true,"baseRefName":"main"}]"#;
        let p = parse_pr_json(text);
        assert_eq!(p.by_head["a"], vec![pr(1, PrState::Closed, "o1"), pr(2, PrState::Merged, "o2")]);
        assert!(!p.by_head.contains_key("b"));
        assert!(p.bases.contains("main"));
    }

    #[test]
    fn base_and_default_branches_never_get_pr_state() {
        let text = r#"[{"number":4027,"headRefName":"beta","headRefOid":"x","state":"MERGED","isCrossRepository":false,"baseRefName":"master"},{"number":9,"headRefName":"feat","headRefOid":"y","state":"MERGED","isCrossRepository":false,"baseRefName":"beta"},{"number":10,"headRefName":"main","headRefOid":"z","state":"MERGED","isCrossRepository":false,"baseRefName":"release"}]"#;
        let p = parse_pr_json(text);
        assert!(p.for_branch("beta", Some("master")).is_empty());
        assert!(p.for_branch("main", Some("main")).is_empty());
        assert_eq!(p.for_branch("feat", Some("master")).len(), 1);
        assert_eq!(branch_of_ref("origin/beta"), "beta");
    }

    #[test]
    fn merged_pr_counts_only_when_head_is_its_commit_or_behind_it() {
        let prs = vec![pr(5, PrState::Merged, "old"), pr(6, PrState::Closed, "cur")];
        assert_eq!(pick_pr(&prs, "new", |_| false), None);
        assert_eq!(pick_pr(&prs, "old", |_| false).unwrap().number, 5);
        assert_eq!(pick_pr(&prs, "anc", |o| o == "old").unwrap().number, 5);
        assert_eq!(pick_pr(&prs, "cur", |_| false).unwrap().state, PrState::Closed);
        let both = vec![pr(5, PrState::Merged, "h"), pr(6, PrState::Closed, "h")];
        assert_eq!(pick_pr(&both, "h", |_| false).unwrap().state, PrState::Merged);
        assert_eq!(pick_pr(&[pr(5, PrState::Merged, "")], "", |_| true), None);
    }

    #[test]
    fn open_pr_outranks_merged_for_the_same_branch() {
        let prs = vec![pr(5, PrState::Merged, "h"), pr(8, PrState::Open, "other")];
        assert_eq!(pick_pr(&prs, "h", |_| true).unwrap(), pr(8, PrState::Open, "other"));
    }

    #[test]
    fn worktree_state_on_real_repo() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        let g = |args: &[&str]| {
            let out = run("git", args, Some(r), GIT_TIMEOUT).unwrap();
            assert!(out.ok, "git {args:?}: {}", out.stderr);
        };
        g(&["init", "-q", "-b", "main"]);
        g(&["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "--allow-empty", "-m", "one"]);
        std::fs::write(r.join("new.txt"), "x").unwrap();
        let s = worktree_state(r, Some("main"));
        assert_eq!(s.dirty, Some(1));
        assert_eq!(s.unpushed, Some(1));
        assert!(s.merged_ancestor);
        assert_eq!(gitdir_of(r), Some(r.join(".git")));
        let head = git(r, &["rev-parse", "HEAD"]).unwrap();
        assert!(head_contained_in(r, &head));
        assert!(!head_contained_in(r, "0123456789abcdef0123456789abcdef01234567"));
    }

    #[test]
    fn worktree_state_is_unknown_when_git_fails() {
        let dir = tempfile::tempdir().unwrap();
        let s = worktree_state(&dir.path().join("missing"), Some("main"));
        assert_eq!(s.dirty, None);
        assert_eq!(s.unpushed, None);
        assert!(!s.merged_ancestor);
    }
}
