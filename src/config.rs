use crate::model::{Action, Item};
use crate::util::expand;
use anyhow::{Context, Result, bail};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

#[derive(Deserialize, Serialize, Clone, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub roots: Vec<String>,
    pub stale_days: i64,
    pub min_size_mb: u64,
    pub catch_all_min_gb: u64,
    pub protect: Vec<String>,
    pub ai: AiConfig,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    #[default]
    Claude,
    Codex,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Provider::Claude => "claude",
            Provider::Codex => "codex",
        }
    }
}

impl std::fmt::Display for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Deserialize, Serialize, Clone, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct AiConfig {
    pub provider: Provider,
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
            provider: Provider::Claude,
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
    load_from(&config_path(), &crate::util::home())
}

pub fn load_from(path: &Path, home: &Path) -> Result<Config> {
    if !path.exists() {
        let cfg = Config::default();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let body = toml::to_string_pretty(&cfg)?;
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
        {
            Ok(mut f) => {
                f.write_all(format!("{TEMPLATE_HEADER}{body}").as_bytes())?;
                return Ok(cfg);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e).with_context(|| format!("creating {}", path.display())),
        }
    }
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let cfg: Config =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Protector::new(&cfg.protect, BTreeSet::new(), home)
        .with_context(|| format!("in {}", path.display()))?;
    Ok(cfg)
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
    prefixes: Vec<PathBuf>,
    prefixes_of_globs: Vec<PathBuf>,
    pins: BTreeSet<String>,
    everything: bool,
}

fn is_glob(p: &str) -> bool {
    p.contains(['*', '?', '[', ']', '{', '}', '\\'])
}

fn lower(p: &Path) -> PathBuf {
    PathBuf::from(p.to_string_lossy().to_lowercase())
}

impl Protector {
    pub fn new(patterns: &[String], pins: BTreeSet<String>, home: &Path) -> Result<Self> {
        let mut builder = GlobSetBuilder::new();
        let mut prefixes = Vec::new();
        let mut prefixes_of_globs = Vec::new();
        for p in patterns {
            let base = expand(p.trim(), home).display().to_string();
            let base = base.trim_end_matches('/').to_string();
            if !is_glob(&base) {
                let path = PathBuf::from(&base);
                if !path.is_absolute() {
                    bail!(
                        "protect pattern {p:?} must be an absolute path, start with ~/, or be a glob"
                    );
                }
                if let Ok(real) = std::fs::canonicalize(&path) {
                    prefixes.push(lower(&real));
                }
                prefixes.push(lower(&path));
                continue;
            }
            let literal: PathBuf = Path::new(&base)
                .components()
                .take_while(|c| !is_glob(&c.as_os_str().to_string_lossy()))
                .collect();
            if literal.components().count() > 1 {
                prefixes_of_globs.push(lower(&literal));
            }
            for pat in [base.clone(), format!("{base}/**")] {
                let g = GlobBuilder::new(&pat)
                    .case_insensitive(true)
                    .build()
                    .with_context(|| format!("invalid protect pattern {p:?}"))?;
                builder.add(g);
            }
        }
        Ok(Protector {
            globs: builder.build().context("building protect patterns")?,
            prefixes,
            prefixes_of_globs,
            pins,
            everything: false,
        })
    }

    pub fn everything() -> Self {
        Protector {
            globs: GlobSet::empty(),
            prefixes: Vec::new(),
            prefixes_of_globs: Vec::new(),
            pins: BTreeSet::new(),
            everything: true,
        }
    }

    fn matches(&self, p: &Path, or_contains: bool) -> bool {
        if self.everything {
            return true;
        }
        [p.to_path_buf(), crate::actions::resolve_parent(p)]
            .iter()
            .any(|c| {
                let low = lower(c);
                self.globs.is_match(c)
                    || self
                        .prefixes
                        .iter()
                        .any(|pre| low.starts_with(pre) || (or_contains && pre.starts_with(&low)))
                    || (or_contains
                        && self
                            .prefixes_of_globs
                            .iter()
                            .any(|pre| pre.starts_with(&low)))
            })
    }

    pub fn protects_path(&self, p: &Path) -> bool {
        self.matches(p, true)
    }

    pub fn check_action(&self, action: &Action) -> Result<()> {
        let Action::Delete { paths, .. } = action else {
            return Ok(());
        };
        match paths.iter().find(|p| self.protects_path(p)) {
            Some(p) => bail!("{} is protected by your config", p.display()),
            None => Ok(()),
        }
    }

    pub fn is_protected(&self, item: &Item) -> bool {
        self.pins.contains(&item.id)
            || self.protects_path(&item.path)
            || item.owner.as_ref().is_some_and(|o| self.matches(o, false))
            || self.check_action(&item.action).is_err()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Action, Category};

    #[test]
    fn protect_globs_cover_folder_and_children_and_owner() {
        let home = Path::new("/Users/me");
        let p = Protector::new(
            &[
                "~/Work/MyApp".into(),
                "~/Library/Developer/Xcode/DerivedData/Keep-*".into(),
            ],
            BTreeSet::new(),
            home,
        )
        .unwrap();
        let item = |path: &str| Item::new(Category::DerivedData, "x", path);
        assert!(p.is_protected(&item("/Users/me/Work/MyApp")));
        assert!(p.is_protected(&item("/Users/me/Work/MyApp/node_modules")));
        assert!(p.is_protected(&item(
            "/Users/me/Library/Developer/Xcode/DerivedData/Keep-abc"
        )));
        assert!(!p.is_protected(&item("/Users/me/Work/MyApp-other")));

        let mut dd = item("/Users/me/Library/Developer/Xcode/DerivedData/MyApp-zzz");
        dd.owner = Some("/Users/me/Work/MyApp/ios/MyApp.xcworkspace".into());
        assert!(p.is_protected(&dd));
    }

    #[test]
    fn plain_protect_paths_are_prefixes_not_globs() {
        let home = Path::new("/Users/me");
        let p = Protector::new(
            &[
                "~/Work/My App/".into(),
                "/Users/me/Work/mono/apps/legacy".into(),
            ],
            BTreeSet::new(),
            home,
        )
        .unwrap();
        assert!(p.protects_path(Path::new("/Users/me/Work/My App")));
        assert!(p.protects_path(Path::new("/Users/me/Work/My App/ios/build")));
        assert!(p.protects_path(Path::new("/Users/me/work/my app/ios")));
        assert!(!p.protects_path(Path::new("/Users/me/Work/My Apps")));
        assert!(
            p.protects_path(Path::new("/Users/me/Work/mono")),
            "deleting an ancestor would delete it too"
        );
        assert!(!p.protects_path(Path::new("/Users/me/Work/mono/apps/web/node_modules")));

        let mut nm = Item::new(
            Category::NodeModules,
            "x",
            "/Users/me/Work/mono/node_modules",
        );
        nm.owner = Some("/Users/me/Work/mono".into());
        nm.action = Action::delete_all(vec!["/Users/me/Work/mono/apps/web/node_modules".into()]);
        assert!(
            !p.is_protected(&nm),
            "an owner that merely contains a protected folder isn't protected"
        );
        nm.action = Action::delete_all(vec![
            "/Users/me/Work/mono/apps/web/node_modules".into(),
            "/Users/me/Work/mono/apps/legacy/node_modules".into(),
        ]);
        assert!(p.is_protected(&nm));
        assert!(
            p.check_action(&nm.action)
                .unwrap_err()
                .to_string()
                .contains("apps/legacy/node_modules")
        );
    }

    #[test]
    fn protect_follows_symlinked_paths() {
        let d = tempfile::tempdir().unwrap();
        let real = d.path().join("real/proj");
        std::fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(d.path().join("real"), d.path().join("link")).unwrap();
        let p = Protector::new(
            &[d.path().join("link/proj").display().to_string()],
            BTreeSet::new(),
            d.path(),
        )
        .unwrap();
        assert!(p.protects_path(&real.join("node_modules")));
        let q = Protector::new(&[real.display().to_string()], BTreeSet::new(), d.path()).unwrap();
        assert!(q.protects_path(&d.path().join("link/proj/node_modules")));
    }

    #[test]
    fn invalid_protect_glob_is_an_error_naming_it() {
        let e = Protector::new(
            &["~/Work/[oops".into()],
            BTreeSet::new(),
            Path::new("/Users/me"),
        )
        .err()
        .unwrap();
        assert!(format!("{e:#}").contains("~/Work/[oops"), "{e:#}");
        let e = Protector::new(
            &["Work/MyApp".into()],
            BTreeSet::new(),
            Path::new("/Users/me"),
        )
        .err()
        .unwrap();
        assert!(format!("{e:#}").contains("Work/MyApp"), "{e:#}");
        assert!(
            Protector::new(
                &["**/keep-me".into()],
                BTreeSet::new(),
                Path::new("/Users/me")
            )
            .is_ok()
        );
    }

    #[test]
    fn load_rejects_invalid_protect_glob() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("config.toml");
        std::fs::write(&path, "protect = [\"~/Work/{a,b\"]\n").unwrap();
        let e = load_from(&path, d.path()).unwrap_err();
        assert!(format!("{e:#}").contains("~/Work/{a,b"), "{e:#}");
    }

    #[test]
    fn unknown_config_keys_are_errors() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("config.toml");
        std::fs::write(&path, "protec = [\"~/Work/MyApp\"]\n").unwrap();
        let e = load_from(&path, d.path()).unwrap_err();
        assert!(format!("{e:#}").contains("protec"), "{e:#}");
        assert!(toml::from_str::<Config>("[ai]\nprovidr = \"codex\"\n").is_err());
    }

    #[test]
    fn first_run_writes_a_private_template_that_loads() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("dustpan/config.toml");
        let cfg = load_from(&path, d.path()).unwrap();
        assert_eq!(cfg.stale_days, 30);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(load_from(&path, d.path()).unwrap().min_size_mb, 100);
    }

    #[test]
    fn pins_protect_by_id() {
        let item = Item::new(Category::Simulator, "x", "/sim/1");
        let pins = BTreeSet::from([item.id.clone()]);
        let p = Protector::new(&[], pins, Path::new("/Users/me")).unwrap();
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
    fn provider_parses_legacy_lowercase_strings() {
        let claude: Config = toml::from_str("[ai]\nprovider = \"claude\"\n").unwrap();
        assert_eq!(claude.ai.provider, Provider::Claude);
        let codex: Config = toml::from_str("[ai]\nprovider = \"codex\"\n").unwrap();
        assert_eq!(codex.ai.provider, Provider::Codex);
        assert!(toml::from_str::<Config>("[ai]\nprovider = \"Claude\"\n").is_err());
        assert!(toml::from_str::<Config>("[ai]\nprovider = \"other\"\n").is_err());
    }

    #[test]
    fn partial_config_uses_defaults() {
        let cfg: Config = toml::from_str("stale_days = 7\n[ai]\nprovider = \"codex\"\n").unwrap();
        assert_eq!(cfg.stale_days, 7);
        assert_eq!(cfg.min_size_mb, 100);
        assert_eq!(cfg.ai.provider, Provider::Codex);
        assert_eq!(cfg.ai.claude_model, "claude-opus-5");
        assert_eq!(cfg.ai.codex_model, "gpt-5.6-sol");
        assert_eq!(cfg.ai.effort, "medium");
    }
}
