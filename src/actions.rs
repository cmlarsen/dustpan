use crate::model::{Action, Item, Verdict};
use crate::state::{append_history, HistoryEntry};
use anyhow::{bail, Context, Result};
use chrono::Utc;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

const FORBIDDEN: &[&str] = &[
    "Documents",
    "Desktop",
    "Pictures",
    "Movies",
    "Music",
    ".ssh",
    ".gnupg",
    ".config",
    "Library/Mobile Documents",
    "Library/Messages",
    "Library/Mail",
    "Library/Keychains",
    "Library/Photos",
];

pub fn guard_delete(p: &Path, home: &Path, roots: &[PathBuf], allow_git_clones: bool) -> Result<()> {
    if !p.is_absolute() {
        bail!("refusing relative path {}", p.display());
    }
    if p.components().any(|c| matches!(c, Component::ParentDir)) {
        bail!("refusing path with .. in it: {}", p.display());
    }
    if let Ok(rel) = p.strip_prefix(home) {
        if rel.components().count() < 2 {
            bail!("refusing a top-level folder of your home: {}", p.display());
        }
        if let Some(f) = FORBIDDEN.iter().find(|f| rel.starts_with(f)) {
            bail!("refusing anything under ~/{f}");
        }
    } else if !(p.starts_with("/private/var/folders") || p.starts_with("/var/folders")) {
        bail!("refusing path outside your home folder: {}", p.display());
    }
    if let Some(r) = roots.iter().find(|r| r.starts_with(p)) {
        bail!("refusing {}: it contains project root {}", p.display(), r.display());
    }
    if !allow_git_clones && p.join(".git").is_dir() {
        bail!("refusing {}: it is a git repository", p.display());
    }
    Ok(())
}

pub fn preflight(action: &Action, home: &Path, roots: &[PathBuf]) -> Result<()> {
    match action {
        Action::None => Ok(()),
        Action::Delete { paths, allow_git_clones } => {
            for p in paths {
                guard_delete(p, home, roots, *allow_git_clones)?;
            }
            Ok(())
        }
        Action::Run { program, .. } => {
            if crate::util::which(program).is_none() {
                bail!("{program} is not on PATH");
            }
            Ok(())
        }
    }
}

pub fn apply_preflight(item: &mut Item, home: &Path, roots: &[PathBuf]) {
    if let Err(e) = preflight(&item.action, home, roots) {
        if item.verdict == Verdict::Safe {
            item.verdict = Verdict::Review;
        }
        item.reasons.push(format!("Dustpan won't clean this automatically: {e:#}"));
        item.action = Action::None;
    }
}

fn remove(p: &Path) -> Result<()> {
    match std::fs::symlink_metadata(p) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(p).with_context(|| format!("deleting {}", p.display())),
        Ok(_) => std::fs::remove_file(p).with_context(|| format!("deleting {}", p.display())),
    }
}

pub fn execute(action: &Action, home: &Path, roots: &[PathBuf]) -> Result<String> {
    match action {
        Action::None => bail!("this item has no automatic action"),
        Action::Delete { paths, allow_git_clones } => {
            for p in paths {
                guard_delete(p, home, roots, *allow_git_clones)?;
            }
            for p in paths {
                remove(p)?;
            }
            Ok(format!("deleted {} path(s)", paths.len()))
        }
        Action::Run { program, args, cwd } => {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let out = crate::util::run(program, &args, cwd.as_deref(), Duration::from_secs(1800))
                .with_context(|| format!("could not run {program} (missing or timed out)"))?;
            if !out.ok {
                let msg = if out.stderr.trim().is_empty() { out.stdout } else { out.stderr };
                bail!("{program} failed: {}", msg.trim());
            }
            let last = out.stdout.lines().last().unwrap_or("").trim().to_string();
            Ok(if last.is_empty() { format!("{program} finished") } else { last })
        }
    }
}

pub fn clean(item: &Item, home: &Path, roots: &[PathBuf]) -> Result<String> {
    if item.protected {
        bail!("{} is protected; unpin it first", item.name);
    }
    if !item.cleanable() {
        bail!("{} is marked KEEP or has no action", item.name);
    }
    let result = execute(&item.action, home, roots);
    let _ = append_history(&HistoryEntry {
        at: Utc::now(),
        id: item.id.clone(),
        name: item.name.clone(),
        category: item.category.key().into(),
        bytes: item.reclaimable,
        action: item.action.describe(),
        ok: result.is_ok(),
        message: match &result {
            Ok(m) => m.clone(),
            Err(e) => format!("{e:#}"),
        },
    });
    result
}

pub fn kill(pid: u32, name: &str) -> Result<()> {
    let rc = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    let result = if rc == 0 {
        Ok(())
    } else {
        Err(anyhow::anyhow!("kill {pid}: {}", std::io::Error::last_os_error()))
    };
    let _ = append_history(&HistoryEntry {
        at: Utc::now(),
        id: format!("proc:{pid}:{name}"),
        name: format!("{name} (pid {pid})"),
        category: "process".into(),
        bytes: 0,
        action: format!("kill -TERM {pid}"),
        ok: result.is_ok(),
        message: result.as_ref().err().map(|e| e.to_string()).unwrap_or_else(|| "sent SIGTERM".into()),
    });
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_rules() {
        let home = Path::new("/Users/me");
        let roots = vec![PathBuf::from("/Users/me/Work")];
        let ok = |p: &str| guard_delete(Path::new(p), home, &roots, false).is_ok();
        assert!(ok("/Users/me/Library/Developer/Xcode/DerivedData/MyApp-abc"));
        assert!(ok("/Users/me/.cache/uv"));
        assert!(!ok("/Users/me/Library"));
        assert!(!ok("/Users/me"));
        assert!(!ok("/etc/hosts"));
        assert!(!ok("/Users/me/Documents/x"));
        assert!(!ok("/Users/me/Library/Mobile Documents/x"));
        assert!(!ok("/Users/me/Work/../Library"));
        assert!(!ok("relative/path"));
        assert!(ok("/private/var/folders/xy/T/CFNetworkDownload_1.tmp"));
    }

    #[test]
    fn guard_refuses_ancestors_of_roots_and_repos() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path();
        let roots = vec![home.join("Work/Projects")];
        std::fs::create_dir_all(home.join("Other/repo/.git")).unwrap();
        assert!(guard_delete(&home.join("Work/Projects"), home, &roots, false).is_err());
        assert!(guard_delete(&home.join("Work"), home, &roots, false).is_err());
        assert!(guard_delete(&home.join("Other/repo"), home, &roots, false).is_err());
        assert!(guard_delete(&home.join("Other/repo/node_modules"), home, &roots, false).is_ok());
        assert!(guard_delete(&home.join("Other/repo"), home, &roots, true).is_ok());
        assert!(guard_delete(&home.join("Work"), home, &roots, true).is_err(), "clones flag never unlocks project roots");
    }

    #[test]
    fn preflight_downgrades_items_the_guard_would_refuse() {
        use crate::model::Category;
        let d = tempfile::tempdir().unwrap();
        let home = d.path();
        let repo = home.join("Stuff/clone");
        std::fs::create_dir_all(repo.join(".git")).unwrap();

        let mut refused = Item::new(Category::Leftover, "x", &repo);
        refused.verdict = Verdict::Safe;
        refused.action = Action::delete_all(vec![repo.clone()]);
        apply_preflight(&mut refused, home, &[]);
        assert_eq!(refused.verdict, Verdict::Review);
        assert!(refused.action.is_none());
        assert!(refused.reasons.last().unwrap().contains("git repository"));

        let mut clones = Item::new(Category::Leftover, "x", &repo);
        clones.verdict = Verdict::Safe;
        clones.action = Action::delete_clones(vec![repo.clone()]);
        apply_preflight(&mut clones, home, &[]);
        assert_eq!(clones.verdict, Verdict::Safe);
        assert!(!clones.action.is_none());

        let mut missing_tool = Item::new(Category::PackageCache, "x", &repo);
        missing_tool.verdict = Verdict::Safe;
        missing_tool.action = Action::run("definitely-not-a-real-tool-xyz", &[], None);
        apply_preflight(&mut missing_tool, home, &[]);
        assert_eq!(missing_tool.verdict, Verdict::Review);
    }

    #[test]
    fn execute_deletes_dirs_and_files_and_tolerates_missing() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path();
        let dir = home.join("Library/Caches/thing");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/f"), b"x").unwrap();
        let file = home.join("Library/Caches/file.tmp");
        std::fs::write(&file, b"x").unwrap();
        let action = Action::delete_all(vec![dir.clone(), file.clone(), home.join("Library/Caches/missing")]);
        execute(&action, home, &[]).unwrap();
        assert!(!dir.exists());
        assert!(!file.exists());
    }

    #[test]
    fn execute_refuses_before_deleting_anything() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path();
        let a = home.join("Library/Caches/a");
        std::fs::create_dir_all(&a).unwrap();
        let action = Action::delete_all(vec![a.clone(), home.join("Documents/x")]);
        assert!(execute(&action, home, &[]).is_err());
        assert!(a.exists(), "first path must survive when a later one is refused");
    }
}
