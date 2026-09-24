use super::{Ctx, Emit, tilde};
use crate::model::{Action, Category, Item, Verdict};
use crate::size::dir_stats;
use crate::util::age_days;
use chrono::{DateTime, Utc};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const SHARED_CACHES: &[&str] = &[
    "ModuleCache.noindex",
    "SDKExplicitPrecompiledModules",
    "CompilationCache.noindex",
];

pub fn classify(
    workspace: Option<&Path>,
    workspace_exists: bool,
    idle_days: Option<i64>,
    stale_days: i64,
    display_ws: &str,
) -> (Verdict, Vec<String>) {
    match workspace {
        None => (
            Verdict::Review,
            vec!["no info.plist, so the project that built this is unknown".into()],
        ),
        Some(_) if !workspace_exists => (
            Verdict::Safe,
            vec![format!("project {display_ws} no longer exists")],
        ),
        Some(_) => match idle_days {
            Some(d) if d > stale_days => (
                Verdict::Review,
                vec![
                    format!("last built {d} days ago"),
                    "deleting forces a full rebuild next time".into(),
                ],
            ),
            d => (
                Verdict::Active,
                vec![format!("built {}d ago for {display_ws}", d.unwrap_or(0))],
            ),
        },
    }
}

fn read_info(dir: &Path) -> (Option<PathBuf>, Option<DateTime<Utc>>) {
    let Ok(v) = plist::Value::from_file(dir.join("info.plist")) else {
        return (None, None);
    };
    let Some(d) = v.as_dictionary() else {
        return (None, None);
    };
    let ws = d
        .get("WorkspacePath")
        .and_then(|w| w.as_string())
        .map(PathBuf::from);
    let accessed = d
        .get("LastAccessedDate")
        .and_then(|w| w.as_date())
        .map(|date| DateTime::<Utc>::from(SystemTime::from(date)));
    (ws, accessed)
}

pub fn scan(ctx: &Ctx, emit: Emit) {
    let root = ctx.home.join("Library/Developer/Xcode/DerivedData");
    let Ok(entries) = std::fs::read_dir(&root) else {
        return;
    };
    let dirs: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect();
    std::thread::scope(|s| {
        for dir in &dirs {
            s.spawn(move || emit(build(ctx, dir)));
        }
    });
}

fn build(ctx: &Ctx, dir: &Path) -> Item {
    let dirname = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let stats = dir_stats(dir);
    if SHARED_CACHES.contains(&dirname.as_str()) {
        let mut item = Item::sized(
            Category::DerivedData,
            format!("Xcode {dirname}"),
            dir,
            &stats,
        );
        item.verdict = Verdict::Review;
        item.reasons = vec![
            "shared Clang/Swift module cache used by every Xcode project".into(),
            "Xcode rebuilds it on the next build, which makes that build slower".into(),
        ];
        item.action = Action::delete(dir);
        return item;
    }
    let (ws, accessed) = read_info(dir);
    let project = dirname.rsplit_once('-').map(|(p, _)| p).unwrap_or(&dirname);
    let display_ws = ws
        .as_ref()
        .map(|w| tilde(ctx, w))
        .unwrap_or_else(|| "?".into());
    let mut item = Item::sized(
        Category::DerivedData,
        format!("{project} · {display_ws}"),
        dir,
        &stats,
    );
    item.last_used = accessed.or(stats.newest);
    item.owner = ws.clone();
    let exists = ws.as_ref().is_some_and(|w| w.exists());
    let (verdict, reasons) = classify(
        ws.as_deref(),
        exists,
        age_days(item.last_used, ctx.now),
        ctx.cfg.stale_days,
        &display_ws,
    );
    item.verdict = verdict;
    item.reasons = reasons;
    item.action = Action::delete(dir);
    item
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules() {
        let ws = Some(Path::new("/w/ios/App.xcworkspace"));
        assert_eq!(classify(ws, false, Some(1), 30, "x").0, Verdict::Safe);
        assert_eq!(classify(ws, true, Some(45), 30, "x").0, Verdict::Review);
        assert_eq!(classify(ws, true, Some(2), 30, "x").0, Verdict::Active);
        assert_eq!(classify(None, false, None, 30, "x").0, Verdict::Review);
    }

    #[test]
    fn reads_workspace_from_info_plist() {
        let dir = tempfile::tempdir().unwrap();
        let mut dict = plist::Dictionary::new();
        dict.insert(
            "WorkspacePath".into(),
            plist::Value::String("/gone/App.xcworkspace".into()),
        );
        plist::Value::Dictionary(dict)
            .to_file_xml(dir.path().join("info.plist"))
            .unwrap();
        let (ws, _) = read_info(dir.path());
        assert_eq!(ws, Some(PathBuf::from("/gone/App.xcworkspace")));
    }
}
