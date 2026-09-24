use crate::model::Item;
use crate::util::expand;
use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

#[derive(Deserialize, Serialize, Clone, Debug)]
#[serde(default)]
pub struct Config {
    pub roots: Vec<String>,
    pub stale_days: i64,
    pub min_size_mb: u64,
    pub catch_all_min_gb: u64,
    pub protect: Vec<String>,
    pub ai: AiConfig,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
#[serde(default)]
pub struct AiConfig {
    pub provider: String,
    pub claude_model: String,
    pub codex_model: String,
    pub effort: String,
    pub timeout_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            roots: Vec::new(),
            stale_days: 30,
            min_size_mb: 100,
            catch_all_min_gb: 2,
            protect: Vec::new(),
            ai: AiConfig::default(),
        }
    }
}

impl Default for AiConfig {
    fn default() -> Self {
        AiConfig {
            provider: "claude".into(),
            claude_model: "claude-opus-5".into(),
            codex_model: "gpt-5.6-sol".into(),
            effort: "medium".into(),
            timeout_secs: 240,
        }
    }
}

const TEMPLATE_HEADER: &str = "\
# Dustpan config.
# roots:            folders that hold your projects. Empty = every folder in ~ that contains a git repo.
# stale_days:       days without activity before something is flagged for review.
# min_size_mb:      hide items smaller than this.
# catch_all_min_gb: report unrecognized app/tool folders at least this big.
# protect:          globs that are never cleaned (a folder glob also protects everything inside it).
#                   e.g. protect = [\"~/Work/MyApp\", \"~/Library/Developer/Xcode/DerivedData/MyApp-*\"]
# ai.provider:      \"claude\" or \"codex\"; used by `x` in the TUI and `dp ask`.
# ai.*_model:       exact model IDs, so verdicts don't shift when an alias moves. \"\" = the CLI's default.
# ai.effort:        low | medium | high | xhigh, passed to both CLIs. \"\" = the CLI's default.

";

pub fn config_dir() -> PathBuf {
    crate::util::home().join(".config/dustpan")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn load() -> Result<Config> {
    let path = config_path();
    if !path.exists() {
        let cfg = Config::default();
        std::fs::create_dir_all(config_dir())?;
        let body = toml::to_string_pretty(&cfg)?;
        std::fs::write(&path, format!("{TEMPLATE_HEADER}{body}"))?;
        return Ok(cfg);
    }
    let text = std::fs::read_to_string(&path)?;
    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

const NON_PROJECT_DIRS: &[&str] = &[
    "Library",
    "Applications",
    "Movies",
    "Music",
    "Pictures",
    "Desktop",
    "Downloads",
    "Public",
    "Documents",
];

pub fn resolve_roots(cfg: &Config, home: &Path) -> Vec<PathBuf> {
    if !cfg.roots.is_empty() {
        return cfg.roots.iter().map(|r| expand(r, home)).collect();
    }
    let Ok(entries) = std::fs::read_dir(home) else {
        return Vec::new();
    };
    let mut roots: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            !name.starts_with('.') && !NON_PROJECT_DIRS.contains(&name.as_ref())
        })
        .map(|e| e.path())
        .filter(|p| has_repo_within(p, 3))
        .collect();
    roots.sort();
    roots
}

fn has_repo_within(dir: &Path, depth: usize) -> bool {
    if dir.join(".git").exists() {
        return true;
    }
    if depth == 0 {
        return false;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|e| {
        let name = e.file_name();
        let name = name.to_string_lossy();
        e.file_type().is_ok_and(|t| t.is_dir())
            && !name.starts_with('.')
            && name != "node_modules"
            && has_repo_within(&e.path(), depth - 1)
    })
}

pub struct Protector {
    globs: GlobSet,
    pins: BTreeSet<String>,
}

impl Protector {
    pub fn new(patterns: &[String], pins: BTreeSet<String>, home: &Path) -> Self {
        let mut builder = GlobSetBuilder::new();
        for p in patterns {
            let base = expand(p, home).display().to_string();
            let base = base.trim_end_matches('/').to_string();
            for pat in [base.clone(), format!("{base}/**")] {
                if let Ok(g) = Glob::new(&pat) {
                    builder.add(g);
                }
            }
        }
        Protector {
            globs: builder.build().unwrap_or_else(|_| GlobSet::empty()),
            pins,
        }
    }

    pub fn is_protected(&self, item: &Item) -> bool {
        self.pins.contains(&item.id)
            || self.globs.is_match(&item.path)
            || item.owner.as_ref().is_some_and(|o| self.globs.is_match(o))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Category;

    #[test]
    fn protect_globs_cover_folder_and_children_and_owner() {
        let home = Path::new("/Users/me");
        let p = Protector::new(
            &["~/Work/MyApp".into(), "~/Library/Developer/Xcode/DerivedData/Keep-*".into()],
            BTreeSet::new(),
            home,
        );
        let item = |path: &str| Item::new(Category::DerivedData, "x", path);
        assert!(p.is_protected(&item("/Users/me/Work/MyApp")));
        assert!(p.is_protected(&item("/Users/me/Work/MyApp/node_modules")));
        assert!(p.is_protected(&item("/Users/me/Library/Developer/Xcode/DerivedData/Keep-abc")));
        assert!(!p.is_protected(&item("/Users/me/Work/MyApp-other")));

        let mut dd = item("/Users/me/Library/Developer/Xcode/DerivedData/MyApp-zzz");
        dd.owner = Some("/Users/me/Work/MyApp/ios/MyApp.xcworkspace".into());
        assert!(p.is_protected(&dd));
    }

    #[test]
    fn pins_protect_by_id() {
        let item = Item::new(Category::Simulator, "x", "/sim/1");
        let pins = BTreeSet::from([item.id.clone()]);
        let p = Protector::new(&[], pins, Path::new("/Users/me"));
        assert!(p.is_protected(&item));
    }

    #[test]
    fn auto_roots_find_folders_with_repos() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        std::fs::create_dir_all(h.join("Work/proj/.git")).unwrap();
        std::fs::create_dir_all(h.join("Apps/group/deep/.git")).unwrap();
        std::fs::create_dir_all(h.join("Empty/nothing")).unwrap();
        std::fs::create_dir_all(h.join("Library/x/.git")).unwrap();
        let roots = resolve_roots(&Config::default(), h);
        assert_eq!(roots, vec![h.join("Apps"), h.join("Work")]);
    }

    #[test]
    fn partial_config_uses_defaults() {
        let cfg: Config = toml::from_str("stale_days = 7\n[ai]\nprovider = \"codex\"\n").unwrap();
        assert_eq!(cfg.stale_days, 7);
        assert_eq!(cfg.min_size_mb, 100);
        assert_eq!(cfg.ai.provider, "codex");
        assert_eq!(cfg.ai.claude_model, "claude-opus-5");
        assert_eq!(cfg.ai.codex_model, "gpt-5.6-sol");
        assert_eq!(cfg.ai.effort, "medium");
    }
}
