use crate::util::run;
use std::collections::HashMap;
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
    pub dirty: usize,
    pub unpushed: usize,
    pub merged_ancestor: bool,
}

pub fn worktree_state(wt: &Path, default_ref: Option<&str>) -> WtGitState {
    let dirty = git(wt, &["status", "--porcelain", "--untracked-files=normal"])
        .map(|s| s.lines().filter(|l| !l.is_empty()).count())
        .unwrap_or(0);
    let unpushed = git(wt, &["rev-list", "--count", "HEAD", "--not", "--remotes"])
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
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

pub fn gitdir_of(wt: &Path) -> Option<PathBuf> {
    let dotgit = wt.join(".git");
    if dotgit.is_dir() {
        return Some(dotgit);
    }
    let text = std::fs::read_to_string(dotgit).ok()?;
    let p = text.trim().strip_prefix("gitdir:")?.trim();
    Some(PathBuf::from(p))
}

#[derive(Debug, Clone, PartialEq)]
pub struct PrInfo {
    pub number: u64,
    pub state: String,
}

pub fn pr_states(repo: &Path) -> HashMap<String, PrInfo> {
    let Some(out) = run(
        "gh",
        &[
            "pr", "list", "--state", "all", "--limit", "400", "--json", "number,headRefName,state",
        ],
        Some(repo),
        Duration::from_secs(15),
    ) else {
        return HashMap::new();
    };
    if !out.ok {
        return HashMap::new();
    }
    parse_pr_json(&out.stdout)
}

pub fn parse_pr_json(text: &str) -> HashMap<String, PrInfo> {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Pr {
        number: u64,
        head_ref_name: String,
        state: String,
    }
    let prs: Vec<Pr> = serde_json::from_str(text).unwrap_or_default();
    let rank = |s: &str| match s {
        "MERGED" => 3,
        "OPEN" => 2,
        _ => 1,
    };
    let mut map: HashMap<String, PrInfo> = HashMap::new();
    for pr in prs {
        let better = map
            .get(&pr.head_ref_name)
            .is_none_or(|cur| rank(&pr.state) > rank(&cur.state));
        if better {
            map.insert(
                pr.head_ref_name,
                PrInfo {
                    number: pr.number,
                    state: pr.state,
                },
            );
        }
    }
    map
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

    #[test]
    fn pr_json_prefers_merged_over_closed() {
        let text = r#"[{"number":1,"headRefName":"a","state":"CLOSED"},{"number":2,"headRefName":"a","state":"MERGED"},{"number":3,"headRefName":"b","state":"OPEN"}]"#;
        let m = parse_pr_json(text);
        assert_eq!(m["a"], PrInfo { number: 2, state: "MERGED".into() });
        assert_eq!(m["b"].state, "OPEN");
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
        assert_eq!(s.dirty, 1);
        assert_eq!(s.unpushed, 1);
        assert!(s.merged_ancestor);
        assert_eq!(gitdir_of(r), Some(r.join(".git")));
    }
}
