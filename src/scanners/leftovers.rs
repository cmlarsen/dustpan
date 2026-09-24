use super::{Ctx, Emit};
use crate::model::{Action, Category, Item, Verdict};
use crate::size::dir_stats;
use crate::util::{age_days, mtime, newer};
use chrono::{DateTime, Utc};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub fn older_than(t: Option<DateTime<Utc>>, now: DateTime<Utc>, days: i64) -> bool {
    age_days(t, now).is_some_and(|d| d >= days)
}

pub fn scan(ctx: &Ctx, emit: Emit) {
    codex_staging(ctx, emit);
    unfinished_downloads(ctx, emit);
}

fn codex_staging(ctx: &Ctx, emit: Emit) {
    let dir = ctx.home.join(".codex/.tmp/marketplaces/.staging");
    let Ok(entries) = std::fs::read_dir(&dir) else { return };
    let stale: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| older_than(mtime(p), ctx.now, 1))
        .collect();
    if stale.is_empty() {
        return;
    }
    let stats = dir_stats(&dir);
    let oldest = stale.iter().filter_map(|p| mtime(p)).min();
    let mut item = Item::sized(Category::Leftover, "Codex marketplace upgrade staging", &dir, &stats);
    item.verdict = Verdict::Safe;
    item.reasons = vec![
        format!(
            "{} abandoned `marketplace-upgrade-*` folders that Codex never removed",
            stale.len()
        ),
        format!(
            "oldest from {}",
            oldest.map(|t| t.format("%Y-%m-%d").to_string()).unwrap_or_default()
        ),
        "folders from the last day are left alone in case an upgrade is running".into(),
        "most are throwaway git clones of plugin marketplaces, so the git-repo guard is relaxed for them".into(),
    ];
    item.action = Action::delete_clones(stale);
    emit(item);
}

fn unfinished_downloads(ctx: &Ctx, emit: Emit) {
    let containers = ctx.home.join("Library/Containers");
    let mut groups: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    if let Ok(entries) = std::fs::read_dir(&containers) {
        for e in entries.flatten() {
            let bundle = e.file_name().to_string_lossy().into_owned();
            collect_cfnetwork(&e.path().join("Data/tmp"), ctx.now, &mut |p| {
                groups.entry(bundle.clone()).or_default().push(p)
            });
        }
    }
    if let Some(tmp) = std::env::var_os("TMPDIR") {
        collect_cfnetwork(Path::new(&tmp), ctx.now, &mut |p| {
            groups.entry("TMPDIR".into()).or_default().push(p)
        });
    }
    for (bundle, files) in groups {
        let mut bytes = 0;
        let mut excl = 0;
        let mut last = None;
        for f in &files {
            let s = dir_stats(f);
            bytes += s.bytes;
            excl += s.exclusive;
            last = newer(last, s.newest);
        }
        let parent = files[0].parent().unwrap_or(&files[0]).to_path_buf();
        let mut item = Item::new(
            Category::Leftover,
            format!("{bundle} unfinished downloads"),
            parent.join("CFNetworkDownload_*.tmp"),
        );
        item.bytes = bytes;
        item.reclaimable = excl;
        item.last_used = last;
        item.verdict = Verdict::Safe;
        item.reasons = vec![
            format!("{} interrupted CFNetwork downloads older than a day", files.len()),
            "the app downloads again if it still needs the file".into(),
        ];
        item.action = Action::delete_all(files);
        emit(item);
    }
}

fn collect_cfnetwork(dir: &Path, now: DateTime<Utc>, push: &mut dyn FnMut(PathBuf)) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("CFNetworkDownload_") && name.ends_with(".tmp") {
            let p = e.path();
            if older_than(mtime(&p), now, 1) {
                push(p);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collects_only_old_cfnetwork_files() {
        let d = tempfile::tempdir().unwrap();
        let old = d.path().join("CFNetworkDownload_a.tmp");
        let fresh = d.path().join("CFNetworkDownload_b.tmp");
        let other = d.path().join("other.tmp");
        for p in [&old, &fresh, &other] {
            std::fs::write(p, b"x").unwrap();
        }
        let two_days = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 86_400);
        std::fs::File::options().write(true).open(&old).unwrap().set_modified(two_days).unwrap();
        std::fs::File::options().write(true).open(&other).unwrap().set_modified(two_days).unwrap();
        let mut got = Vec::new();
        collect_cfnetwork(d.path(), Utc::now(), &mut |p| got.push(p));
        assert_eq!(got, vec![old]);
    }
}
