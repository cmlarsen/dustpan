use crate::config::Protector;
use crate::model::{Action, Item, Verdict};
use crate::procs::ProcItem;
use crate::state::{append_history, HistoryEntry};
use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Utc};
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

const START_TOLERANCE_SECS: i64 = 5;

fn strip_prefix_ci(p: &Path, base: &Path) -> Option<PathBuf> {
    let mut rest = p.components();
    for b in base.components() {
        let c = rest.next()?;
        if c.as_os_str().to_string_lossy().to_lowercase() != b.as_os_str().to_string_lossy().to_lowercase() {
            return None;
        }
    }
    Some(rest.as_path().to_path_buf())
}

fn starts_with_ci(p: &Path, base: &Path) -> bool {
    strip_prefix_ci(p, base).is_some()
}

fn resolve_existing(p: &Path) -> PathBuf {
    let mut base = p;
    let mut rest = Vec::new();
    loop {
        if let Ok(real) = std::fs::canonicalize(base) {
            return rest.iter().rev().fold(real, |acc, name| acc.join(name));
        }
        match (base.parent(), base.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                base = parent;
            }
            _ => return p.to_path_buf(),
        }
    }
}

pub fn resolve_parent(p: &Path) -> PathBuf {
    match (p.parent(), p.file_name()) {
        (Some(parent), Some(name)) => resolve_existing(parent).join(name),
        _ => p.to_path_buf(),
    }
}

fn own_temp_dir() -> Option<PathBuf> {
    let tmp = std::fs::canonicalize(std::env::temp_dir()).ok()?;
    let folders = Path::new("/private/var/folders");
    (tmp.starts_with(folders) && tmp.components().count() > folders.components().count() + 1).then_some(tmp)
}

fn check_location(p: &Path, home: &Path, roots: &[PathBuf], temps: &[PathBuf]) -> Result<()> {
    if home.components().count() < 3 {
        bail!("refusing to delete anything while home is {}", home.display());
    }
    if let Some(rel) = strip_prefix_ci(p, home) {
        if rel.components().count() < 2 {
            bail!("refusing a top-level folder of your home: {}", p.display());
        }
        if let Some(f) = FORBIDDEN.iter().find(|f| starts_with_ci(&rel, Path::new(f))) {
            bail!("refusing anything under ~/{f}");
        }
    } else if !temps
        .iter()
        .any(|t| strip_prefix_ci(p, t).is_some_and(|rel| rel.components().count() > 0))
    {
        bail!("refusing path outside your home folder: {}", p.display());
    }
    if let Some(r) = roots.iter().find(|r| starts_with_ci(r, p)) {
        bail!("refusing {}: it contains project root {}", p.display(), r.display());
    }
    Ok(())
}

pub fn guard_delete(p: &Path, home: &Path, roots: &[PathBuf], allow_git_clones: bool) -> Result<()> {
    if !p.is_absolute() {
        bail!("refusing relative path {}", p.display());
    }
    if p.components().any(|c| matches!(c, Component::ParentDir | Component::CurDir)) {
        bail!("refusing path with .. in it: {}", p.display());
    }
    let temp: Vec<PathBuf> = own_temp_dir().into_iter().collect();
    let raw_temps: Vec<PathBuf> = temp.iter().flat_map(|t| [std::env::temp_dir(), t.clone()]).collect();
    check_location(p, home, roots, &raw_temps)?;
    let real = resolve_parent(p);
    let all_roots: Vec<PathBuf> = roots.iter().flat_map(|r| [r.clone(), resolve_existing(r)]).collect();
    check_location(&real, &resolve_existing(home), &all_roots, &temp)?;
    if !allow_git_clones && p.join(".git").is_dir() {
        bail!("refusing {}: it is a git repository", p.display());
    }
    Ok(())
}

fn check_not_root(euid: u32) -> Result<()> {
    if euid == 0 {
        bail!("refusing to clean or kill as root; run dp as your own user");
    }
    Ok(())
}

fn euid() -> u32 {
    unsafe { libc::geteuid() }
}

pub fn preflight(action: &Action, home: &Path, roots: &[PathBuf], protector: &Protector) -> Result<()> {
    protector.check_action(action)?;
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

pub fn apply_preflight(item: &mut Item, home: &Path, roots: &[PathBuf], protector: &Protector) {
    if let Err(e) = preflight(&item.action, home, roots, protector) {
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

fn check_missing_worktree(program: &str, args: &[String]) -> Result<()> {
    let [subcommand, command, path] = args else {
        return Ok(());
    };
    if program != "git" || subcommand != "worktree" || command != "remove" {
        return Ok(());
    }
    match Path::new(path).try_exists() {
        Ok(false) => Ok(()),
        Ok(true) => bail!(
            "refusing to remove worktree {} because its checkout folder now exists; rescan first",
            path
        ),
        Err(e) => bail!(
            "refusing to remove worktree {} because its checkout folder could not be checked: {}",
            path,
            e
        ),
    }
}

pub fn execute(action: &Action, home: &Path, roots: &[PathBuf], protector: &Protector) -> Result<String> {
    check_not_root(euid())?;
    protector.check_action(action)?;
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
            check_missing_worktree(program, args)?;
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

pub fn clean(item: &Item, home: &Path, roots: &[PathBuf], protector: &Protector) -> Result<String> {
    if item.protected || protector.is_protected(item) {
        bail!("{} is protected; unpin it or change `protect` in the config first", item.name);
    }
    if !item.cleanable() {
        bail!("{} is marked KEEP or has no action", item.name);
    }
    let result = execute(&item.action, home, roots, protector);
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

#[derive(Clone, Debug)]
pub struct KillTarget {
    pub pid: u32,
    pub name: String,
    pub comm: String,
    pub started: DateTime<Utc>,
}

impl KillTarget {
    pub fn from_proc(p: &ProcItem, seen_at: DateTime<Utc>) -> Self {
        KillTarget {
            pid: p.pid,
            name: p.name.clone(),
            comm: p.comm.clone(),
            started: seen_at - chrono::Duration::seconds(p.uptime_secs.min(i64::MAX as u64) as i64),
        }
    }
}

struct LiveProc {
    comm: String,
    started: i64,
}

fn live_proc(pid: libc::pid_t) -> Option<LiveProc> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let n = unsafe {
        libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size)
    };
    if n != size {
        return None;
    }
    let out = crate::util::run("ps", &["-p", &pid.to_string(), "-o", "comm="], None, Duration::from_secs(5))?;
    Some(LiveProc {
        comm: out.stdout.trim().to_string(),
        started: i64::try_from(info.pbi_start_tvsec).ok()?,
    })
}

fn check_pid(pid: u32, own: u32) -> Result<libc::pid_t> {
    if pid <= 1 {
        bail!("refusing to signal pid {pid}");
    }
    if pid == own {
        bail!("refusing to signal dustpan itself (pid {pid})");
    }
    libc::pid_t::try_from(pid).map_err(|_| anyhow!("refusing out-of-range pid {pid}"))
}

fn same_process(t: &KillTarget, live: &LiveProc) -> Result<()> {
    if live.comm.is_empty() || live.comm != t.comm.trim() {
        bail!("pid {} is now {:?}, not {:?}; refresh and try again", t.pid, live.comm, t.comm);
    }
    if (live.started - t.started.timestamp()).abs() > START_TOLERANCE_SECS {
        bail!("pid {} is a different {} than the one listed (it restarted); refresh and try again", t.pid, live.comm);
    }
    Ok(())
}

fn send_term(t: &KillTarget) -> Result<()> {
    check_not_root(euid())?;
    let pid = check_pid(t.pid, std::process::id())?;
    let live = live_proc(pid).ok_or_else(|| anyhow!("pid {} ({}) is no longer running", t.pid, t.name))?;
    same_process(t, &live)?;
    if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
        bail!("kill {}: {}", t.pid, std::io::Error::last_os_error());
    }
    Ok(())
}

pub fn kill(t: &KillTarget) -> Result<()> {
    let result = send_term(t);
    let (pid, name) = (t.pid, &t.name);
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
    use crate::model::Category;
    use std::collections::BTreeSet;

    fn unprotected() -> Protector {
        Protector::new(&[], BTreeSet::new(), Path::new("/Users/me")).unwrap()
    }

    fn protecting(pattern: &Path, home: &Path) -> Protector {
        Protector::new(&[pattern.display().to_string()], BTreeSet::new(), home).unwrap()
    }

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
        assert!(!ok("/private/var/folders/xy/zz/T/CFNetworkDownload_1.tmp"));
        assert!(!ok("/var/folders/xy/zz/T/CFNetworkDownload_1.tmp"));
    }

    #[test]
    fn guard_allows_only_our_own_temp_dir() {
        let home = Path::new("/Users/me");
        let Some(tmp) = own_temp_dir() else { return };
        assert!(guard_delete(&tmp.join("CFNetworkDownload_1.tmp"), home, &[], false).is_ok());
        assert!(guard_delete(&std::env::temp_dir().join("CFNetworkDownload_1.tmp"), home, &[], false).is_ok());
        assert!(guard_delete(&tmp, home, &[], false).is_err());
        assert!(guard_delete(tmp.parent().unwrap(), home, &[], false).is_err());
        assert!(guard_delete(&tmp.parent().unwrap().join("C/other"), home, &[], false).is_err());
    }

    #[test]
    fn guard_refuses_forbidden_roots_by_case() {
        let home = Path::new("/Users/me");
        let roots = vec![PathBuf::from("/Users/me/Work/Proj")];
        let refused = |p: &str| guard_delete(Path::new(p), home, &roots, false).is_err();
        assert!(refused("/Users/me/documents/x"));
        assert!(refused("/Users/me/DOCUMENTS/x"));
        assert!(refused("/Users/me/library/keychains/login.keychain-db"));
        assert!(refused("/users/ME/.SSH/id_ed25519"));
        assert!(refused("/Users/me/work"));
        assert!(refused("/Users/me/WORK/proj"));
        assert!(!refused("/Users/me/Library/Caches/x"));
    }

    #[test]
    fn guard_refuses_symlinked_parent_into_forbidden_root() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path();
        std::fs::create_dir_all(home.join("Documents/secret")).unwrap();
        std::fs::create_dir_all(home.join("Library/Caches")).unwrap();
        std::os::unix::fs::symlink(home.join("Documents"), home.join("Library/Caches/link")).unwrap();
        let through = home.join("Library/Caches/link/secret");
        let e = guard_delete(&through, home, &[], false).unwrap_err();
        assert!(format!("{e:#}").contains("Documents"), "{e:#}");
        assert!(guard_delete(&home.join("Library/Caches/link"), home, &[], false).is_ok(), "a symlink leaf is removed, not followed");

        std::fs::create_dir_all(home.join("Work/Proj")).unwrap();
        std::os::unix::fs::symlink(home.join("Work"), home.join("Library/Caches/work")).unwrap();
        let roots = vec![home.join("Work/Proj")];
        assert!(guard_delete(&home.join("Library/Caches/work/Proj"), home, &roots, false).is_err());

        std::os::unix::fs::symlink("/private/etc", home.join("Library/Caches/etc")).unwrap();
        assert!(guard_delete(&home.join("Library/Caches/etc/hosts"), home, &[], false).is_err());
    }

    #[test]
    fn guard_refuses_a_shallow_home() {
        assert!(guard_delete(Path::new("/usr/local/thing"), Path::new("/"), &[], false).is_err());
        assert!(guard_delete(Path::new("/Users/x/y"), Path::new("/Users"), &[], false).is_err());
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
        let d = tempfile::tempdir().unwrap();
        let home = d.path();
        let repo = home.join("Stuff/clone");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let none = unprotected();

        let mut refused = Item::new(Category::Leftover, "x", &repo);
        refused.verdict = Verdict::Safe;
        refused.action = Action::delete_all(vec![repo.clone()]);
        apply_preflight(&mut refused, home, &[], &none);
        assert_eq!(refused.verdict, Verdict::Review);
        assert!(refused.action.is_none());
        assert!(refused.reasons.last().unwrap().contains("git repository"));

        let mut clones = Item::new(Category::Leftover, "x", &repo);
        clones.verdict = Verdict::Safe;
        clones.action = Action::delete_clones(vec![repo.clone()]);
        apply_preflight(&mut clones, home, &[], &none);
        assert_eq!(clones.verdict, Verdict::Safe);
        assert!(!clones.action.is_none());

        let mut missing_tool = Item::new(Category::PackageCache, "x", &repo);
        missing_tool.verdict = Verdict::Safe;
        missing_tool.action = Action::run("definitely-not-a-real-tool-xyz", &[], None);
        apply_preflight(&mut missing_tool, home, &[], &none);
        assert_eq!(missing_tool.verdict, Verdict::Review);
    }

    fn mono(home: &Path) -> (PathBuf, PathBuf, Item) {
        let mono = home.join("Work/mono");
        let legacy_nm = mono.join("apps/legacy/node_modules");
        let web_nm = mono.join("apps/web/node_modules");
        for d in [&legacy_nm, &web_nm] {
            std::fs::create_dir_all(d.join("pkg")).unwrap();
        }
        let mut item = Item::new(Category::NodeModules, "node_modules · ~/Work/mono", mono.join("node_modules"));
        item.owner = Some(mono.clone());
        item.verdict = Verdict::Safe;
        item.action = Action::delete_all(vec![web_nm.clone(), legacy_nm.clone()]);
        (legacy_nm, web_nm, item)
    }

    #[test]
    fn protected_subpath_blocks_node_modules_at_preflight() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path();
        let (_, _, mut item) = mono(home);
        let protector = protecting(Path::new("~/Work/mono/apps/legacy"), home);
        assert!(protector.is_protected(&item));
        let e = preflight(&item.action, home, &[], &protector).unwrap_err();
        assert!(format!("{e:#}").contains("apps/legacy/node_modules"), "{e:#}");
        apply_preflight(&mut item, home, &[], &protector);
        assert!(item.action.is_none());
        assert!(preflight(&item.action, home, &[], &unprotected()).is_ok());
    }

    #[test]
    fn protected_subpath_blocks_node_modules_at_execute() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path();
        let (legacy_nm, web_nm, item) = mono(home);
        let protector = protecting(Path::new("~/work/MONO/apps/legacy"), home);
        assert!(execute(&item.action, home, &[], &protector).is_err());
        assert!(clean(&item, home, &[], &protector).is_err());
        assert!(legacy_nm.exists());
        assert!(web_nm.exists(), "nothing is deleted when any path is protected");
    }

    #[test]
    fn worktree_remove_requires_checkout_to_remain_missing() {
        let d = tempfile::tempdir().unwrap();
        let checkout = d.path().join("Work/reappeared");
        let action = Action::run(
            "git",
            &["worktree", "remove", checkout.to_str().unwrap()],
            Some(d.path().to_path_buf()),
        );
        let Action::Run { program, args, .. } = &action else { panic!() };
        assert!(check_missing_worktree(program, args).is_ok());
        std::fs::create_dir_all(&checkout).unwrap();
        let error = check_missing_worktree(program, args).unwrap_err().to_string();
        assert!(error.contains("checkout folder now exists"), "{error}");
        assert!(execute(&action, d.path(), &[], &unprotected()).is_err());
        assert!(checkout.exists());
    }

    #[test]
    fn protected_child_of_codex_staging_blocks_its_deletion() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path();
        let staging = home.join(".codex/.tmp/marketplaces/.staging");
        let keep = staging.join("marketplace-upgrade-keep");
        let old = staging.join("marketplace-upgrade-old");
        for p in [&keep, &old] {
            std::fs::create_dir_all(p.join(".git")).unwrap();
        }
        let mut item = Item::new(Category::Leftover, "Codex marketplace upgrade staging", &staging);
        item.verdict = Verdict::Safe;
        item.action = Action::delete_clones(vec![keep.clone(), old.clone()]);
        let protector = protecting(Path::new("~/.codex/.tmp/marketplaces/.staging/marketplace-upgrade-keep"), home);
        assert!(protector.is_protected(&item));
        assert!(preflight(&item.action, home, &[], &protector).is_err());
        assert!(execute(&item.action, home, &[], &protector).is_err());
        assert!(keep.exists() && old.exists());
        let glob = Protector::new(&["~/.codex/.tmp/marketplaces/.staging/*-keep".into()], BTreeSet::new(), home).unwrap();
        assert!(execute(&item.action, home, &[], &glob).is_err());
        assert!(keep.exists() && old.exists());
    }

    #[test]
    fn protecting_a_folder_blocks_deleting_its_ancestors() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path();
        let dd = home.join("Library/Developer/Xcode/DerivedData");
        std::fs::create_dir_all(dd.join("Keep-abc")).unwrap();
        let glob = Protector::new(&["~/Library/Developer/Xcode/DerivedData/Keep-*".into()], BTreeSet::new(), home).unwrap();
        assert!(execute(&Action::delete(dd.clone()), home, &[], &glob).is_err());
        assert!(dd.join("Keep-abc").exists());
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
        execute(&action, home, &[], &unprotected()).unwrap();
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
        assert!(execute(&action, home, &[], &unprotected()).is_err());
        assert!(a.exists(), "first path must survive when a later one is refused");
    }

    #[test]
    fn everything_protector_blocks_all_deletes() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path();
        let a = home.join("Library/Caches/a");
        std::fs::create_dir_all(&a).unwrap();
        assert!(execute(&Action::delete(a.clone()), home, &[], &Protector::everything()).is_err());
        assert!(a.exists());
    }

    #[test]
    fn refuses_to_act_as_root() {
        assert!(check_not_root(0).is_err());
        assert!(check_not_root(501).is_ok());
    }

    fn target(pid: u32) -> KillTarget {
        KillTarget {
            pid,
            name: "x".into(),
            comm: "/bin/sleep".into(),
            started: Utc::now(),
        }
    }

    #[test]
    fn kill_rejects_dangerous_pids() {
        let own = std::process::id();
        for pid in [u32::MAX, i32::MAX as u32 + 1, 0, 1, own] {
            assert!(check_pid(pid, own).is_err(), "pid {pid}");
            assert!(send_term(&target(pid)).is_err(), "pid {pid}");
        }
        assert_eq!(check_pid(4242, own).unwrap(), 4242);
    }

    #[test]
    fn kill_refuses_a_recycled_or_renamed_pid() {
        let now = Utc::now();
        let t = KillTarget {
            pid: 4242,
            name: "node".into(),
            comm: "/opt/homebrew/bin/node".into(),
            started: now,
        };
        let live = |comm: &str, started: i64| LiveProc { comm: comm.into(), started };
        assert!(same_process(&t, &live("/opt/homebrew/bin/node", now.timestamp() + 2)).is_ok());
        assert!(same_process(&t, &live("/opt/homebrew/bin/node", now.timestamp() - 2)).is_ok());
        assert!(same_process(&t, &live("/usr/bin/python3", now.timestamp())).is_err());
        assert!(same_process(&t, &live("", now.timestamp())).is_err());
        assert!(same_process(&t, &live("/opt/homebrew/bin/node", now.timestamp() + 3600)).is_err());
    }

    #[test]
    fn live_identity_matches_a_ps_snapshot() {
        let mut child = std::process::Command::new("/bin/sleep").arg("30").spawn().unwrap();
        std::thread::sleep(Duration::from_millis(1200));
        let seen_at = Utc::now();
        let out = std::process::Command::new("ps")
            .args(["-o", "etime=,comm=", "-p", &child.id().to_string()])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        let (etime, comm) = text.trim().split_once(char::is_whitespace).unwrap();
        let t = KillTarget {
            pid: child.id(),
            name: "sleep".into(),
            comm: comm.trim().into(),
            started: seen_at - chrono::Duration::seconds(crate::procs::parse_etime(etime) as i64),
        };
        let live = live_proc(child.id() as libc::pid_t).unwrap();
        let checked = same_process(&t, &live);
        let stale = KillTarget { started: t.started - chrono::Duration::hours(3), ..t.clone() };
        let recycled = same_process(&stale, &live);
        let _ = child.kill();
        let _ = child.wait();
        checked.unwrap();
        assert!(recycled.is_err());
        assert!(live_proc(child.id() as libc::pid_t).is_none());
    }
}
