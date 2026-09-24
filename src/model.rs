use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Worktree,
    DerivedData,
    Simulator,
    DeviceSupport,
    NodeModules,
    PackageCache,
    Leftover,
    AppData,
}

impl Category {
    pub const ALL: [Category; 8] = [
        Category::Worktree,
        Category::DerivedData,
        Category::Simulator,
        Category::DeviceSupport,
        Category::NodeModules,
        Category::PackageCache,
        Category::Leftover,
        Category::AppData,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Category::Worktree => "worktree",
            Category::DerivedData => "derived data",
            Category::Simulator => "simulator",
            Category::DeviceSupport => "device support",
            Category::NodeModules => "node_modules",
            Category::PackageCache => "pkg cache",
            Category::Leftover => "leftover",
            Category::AppData => "app data",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Category::Worktree => "worktree",
            Category::DerivedData => "derived_data",
            Category::Simulator => "simulator",
            Category::DeviceSupport => "device_support",
            Category::NodeModules => "node_modules",
            Category::PackageCache => "package_cache",
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
    },
    Run {
        program: String,
        args: Vec<String>,
        cwd: Option<PathBuf>,
    },
}

impl Action {
    pub fn delete(path: impl Into<PathBuf>) -> Self {
        Action::Delete {
            paths: vec![path.into()],
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
            Action::Delete { paths } => match paths.as_slice() {
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
                match cwd {
                    Some(dir) => format!("cd {} && {}", shell_quote(&dir.display().to_string()), cmd),
                    None => cmd,
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
        let a = Action::Delete {
            paths: vec!["/a/b".into(), "/a/c".into(), "/a/d".into()],
        };
        assert_eq!(a.describe(), "rm -rf /a/b (+2 more)");
    }

    #[test]
    fn describe_run_with_cwd() {
        let a = Action::run("git", &["worktree", "prune"], Some("/r/x".into()));
        assert_eq!(a.describe(), "cd /r/x && git worktree prune");
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
