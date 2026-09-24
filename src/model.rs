use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::path::PathBuf;

pub const PROTECTED_REASON: &str = "protected by you (pin or config)";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Safe,
    Review,
    Active,
    Keep,
}

impl Verdict {
    pub fn label(self) -> &'static str {
        match self {
            Verdict::Safe => "SAFE",
            Verdict::Review => "REVIEW",
            Verdict::Active => "IN USE",
            Verdict::Keep => "KEEP",
        }
    }

    pub fn rank(self) -> u8 {
        match self {
            Verdict::Safe => 0,
            Verdict::Review => 1,
            Verdict::Active => 2,
            Verdict::Keep => 3,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Worktree,
    DerivedData,
    Simulator,
    SimRuntime,
    DeviceSupport,
    NodeModules,
    PackageCache,
    Docker,
    Download,
    Leftover,
    AppData,
}

impl Category {
    pub const ALL: [Category; 11] = [
        Category::Worktree,
        Category::DerivedData,
        Category::Simulator,
        Category::SimRuntime,
        Category::DeviceSupport,
        Category::NodeModules,
        Category::PackageCache,
        Category::Docker,
        Category::Download,
        Category::Leftover,
        Category::AppData,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Category::Worktree => "worktree",
            Category::DerivedData => "derived data",
            Category::Simulator => "simulator",
            Category::SimRuntime => "sim runtime",
            Category::DeviceSupport => "device support",
            Category::NodeModules => "node_modules",
            Category::PackageCache => "pkg cache",
            Category::Docker => "docker",
            Category::Download => "download",
            Category::Leftover => "leftover",
            Category::AppData => "app data",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Category::Worktree => "worktree",
            Category::DerivedData => "derived_data",
            Category::Simulator => "simulator",
            Category::SimRuntime => "sim_runtime",
            Category::DeviceSupport => "device_support",
            Category::NodeModules => "node_modules",
            Category::PackageCache => "package_cache",
            Category::Docker => "docker",
            Category::Download => "download",
            Category::Leftover => "leftover",
            Category::AppData => "app_data",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    None,
    Delete {
        paths: Vec<PathBuf>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        allow_git_clones: bool,
    },
    Run {
        program: String,
        args: Vec<String>,
        cwd: Option<PathBuf>,
    },
}

impl Action {
    pub fn delete(path: impl Into<PathBuf>) -> Self {
        Action::delete_all(vec![path.into()])
    }

    pub fn delete_all(paths: Vec<PathBuf>) -> Self {
        Action::Delete {
            paths,
            allow_git_clones: false,
        }
    }

    pub fn delete_clones(paths: Vec<PathBuf>) -> Self {
        Action::Delete {
            paths,
            allow_git_clones: true,
        }
    }

    pub fn run(program: &str, args: &[&str], cwd: Option<PathBuf>) -> Self {
        Action::Run {
            program: program.to_string(),
            args: args.iter().map(|a| a.to_string()).collect(),
            cwd,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Action::None => "no automatic action; inspect it yourself".into(),
            Action::Delete { paths, .. } => match paths.as_slice() {
                [] => "nothing to delete".into(),
                [one] => format!("rm -rf {}", shell_quote(&one.display().to_string())),
                [first, rest @ ..] => format!(
                    "rm -rf {} (+{} more)",
                    shell_quote(&first.display().to_string()),
                    rest.len()
                ),
            },
            Action::Run { program, args, cwd } => {
                let cmd = std::iter::once(program.clone())
                    .chain(args.iter().map(|a| shell_quote(a)))
                    .collect::<Vec<_>>()
                    .join(" ");
                let cmd = match cwd {
                    Some(dir) => format!("cd {} && {}", shell_quote(&dir.display().to_string()), cmd),
                    None => cmd,
                };
                if program == "git" && args.iter().map(String::as_str).eq(["worktree", "prune"]) {
                    format!("{cmd} (repo-wide: drops every missing worktree of this repo)")
                } else {
                    cmd
                }
            }
        }
    }

    pub fn is_none(&self) -> bool {
        matches!(self, Action::None)
    }
}

fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-~:=@+,".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Item {
    pub id: String,
    pub category: Category,
    pub name: String,
    pub path: PathBuf,
    pub bytes: u64,
    pub reclaimable: u64,
    pub last_used: Option<DateTime<Utc>>,
    pub verdict: Verdict,
    pub reasons: Vec<String>,
    pub action: Action,
    pub owner: Option<PathBuf>,
    pub protected: bool,
}

impl Item {
    pub fn new(category: Category, name: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        Item {
            id: format!("{}:{}", category.key(), path.display()),
            category,
            name: name.into(),
            path,
            bytes: 0,
            reclaimable: 0,
            last_used: None,
            verdict: Verdict::Review,
            reasons: Vec::new(),
            action: Action::None,
            owner: None,
            protected: false,
        }
    }

    pub fn effective_verdict(&self) -> Verdict {
        if self.protected {
            Verdict::Keep
        } else {
            self.verdict
        }
    }

    pub fn cleanable(&self) -> bool {
        !self.action.is_none() && self.effective_verdict() != Verdict::Keep
    }

    pub fn set_protected(&mut self, protected: bool) {
        self.protected = protected;
        self.reasons.retain(|r| r != PROTECTED_REASON);
        if protected {
            self.reasons.insert(0, PROTECTED_REASON.into());
        }
    }

    pub fn listing_order(&self, other: &Item) -> Ordering {
        self.effective_verdict()
            .rank()
            .cmp(&other.effective_verdict().rank())
            .then(other.reclaimable.max(other.bytes / 8).cmp(&self.reclaimable.max(self.bytes / 8)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_quotes_paths_with_spaces() {
        let a = Action::delete("/Users/me/Library/Application Support/Claude/vm_bundles");
        assert_eq!(
            a.describe(),
            "rm -rf '/Users/me/Library/Application Support/Claude/vm_bundles'"
        );
    }

    #[test]
    fn describe_counts_extra_paths() {
        let a = Action::delete_all(vec!["/a/b".into(), "/a/c".into(), "/a/d".into()]);
        assert_eq!(a.describe(), "rm -rf /a/b (+2 more)");
    }

    #[test]
    fn describe_run_with_cwd() {
        let a = Action::run("git", &["worktree", "remove", "/r/x-wt"], Some("/r/x".into()));
        assert_eq!(a.describe(), "cd /r/x && git worktree remove /r/x-wt");
    }

    #[test]
    fn describe_says_worktree_prune_is_repo_wide() {
        let a = Action::run("git", &["worktree", "prune"], Some("/r/x".into()));
        assert_eq!(
            a.describe(),
            "cd /r/x && git worktree prune (repo-wide: drops every missing worktree of this repo)"
        );
    }

    #[test]
    fn set_protected_keeps_one_reason_at_the_top() {
        let mut item = Item::new(Category::DerivedData, "x", "/tmp/x");
        item.reasons = vec!["project gone".into()];
        item.set_protected(true);
        item.set_protected(true);
        assert_eq!(item.reasons, vec![PROTECTED_REASON.to_string(), "project gone".into()]);
        item.set_protected(false);
        assert!(!item.protected);
        assert_eq!(item.reasons, vec!["project gone".to_string()]);
    }

    #[test]
    fn listing_order_puts_safe_first_then_bigger() {
        let mk = |v: Verdict, bytes: u64| {
            let mut i = Item::new(Category::DerivedData, "x", format!("/tmp/{bytes}"));
            i.verdict = v;
            i.bytes = bytes;
            i.reclaimable = bytes;
            i
        };
        let mut pinned = mk(Verdict::Safe, 900);
        pinned.protected = true;
        let mut items = [mk(Verdict::Keep, 500), pinned, mk(Verdict::Review, 10), mk(Verdict::Safe, 1), mk(Verdict::Safe, 50)];
        items.sort_by(Item::listing_order);
        let got: Vec<(Verdict, u64)> = items.iter().map(|i| (i.effective_verdict(), i.bytes)).collect();
        assert_eq!(
            got,
            vec![(Verdict::Safe, 50), (Verdict::Safe, 1), (Verdict::Review, 10), (Verdict::Keep, 900), (Verdict::Keep, 500)]
        );
    }

    #[test]
    fn protected_items_are_keep_and_not_cleanable() {
        let mut item = Item::new(Category::DerivedData, "x", "/tmp/x");
        item.verdict = Verdict::Safe;
        item.action = Action::delete("/tmp/x");
        assert!(item.cleanable());
        item.protected = true;
        assert_eq!(item.effective_verdict(), Verdict::Keep);
        assert!(!item.cleanable());
    }
}
