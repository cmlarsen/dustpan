use super::{Ctx, Emit};
use crate::model::{Action, Category, Item, Verdict};
use crate::size::{Opts, dir_stats, dir_stats_opts};
use crate::util::{age_days, mtime, newer};
use std::path::{Path, PathBuf};

pub fn scan_known(ctx: &Ctx, emit: Emit) {
    opencode_db(ctx, emit);
    claude_vm(ctx, emit);
    container_models(ctx, emit);
}

fn opencode_db(ctx: &Ctx, emit: Emit) {
    let db = ctx.home.join(".local/share/opencode/opencode.db");
    let Ok(meta) = std::fs::metadata(&db) else {
        return;
    };
    let files: Vec<PathBuf> = ["", "-wal", "-shm"]
        .iter()
        .map(|s| PathBuf::from(format!("{}{s}", db.display())))
        .filter(|p| p.exists())
        .collect();
    let written = files.iter().filter_map(|f| mtime(f)).max();
    let mut item = Item::new(Category::AppData, "opencode session database", &db);
    item.bytes = files
        .iter()
        .map(|f| dir_stats(f).bytes)
        .sum::<u64>()
        .max(meta.len());
    item.reclaimable = item.bytes;
    item.last_used = written;
    item.verdict = Verdict::Review;
    let idle = age_days(written, ctx.now).unwrap_or(0);
    item.reasons = vec![
        "opencode's local session history (every conversation it has stored)".into(),
        format!("last written {idle}d ago"),
        "delete it if you don't need old opencode sessions; opencode recreates an empty one".into(),
    ];
    item.action = Action::delete_all(files);
    emit(item);
}

fn claude_vm(ctx: &Ctx, emit: Emit) {
    let dir = ctx
        .home
        .join("Library/Application Support/Claude/vm_bundles");
    if !dir.exists() {
        return;
    }
    let s = dir_stats(&dir);
    let mut item = Item::sized(
        Category::AppData,
        "Claude desktop sandbox VM image",
        &dir,
        &s,
    );
    item.verdict = Verdict::Review;
    item.reasons = vec![
        "VM image the Claude desktop app uses to run code in a sandbox".into(),
        "it downloads again the next time that feature runs".into(),
    ];
    item.action = Action::delete(&dir);
    emit(item);
}

fn container_models(ctx: &Ctx, emit: Emit) {
    let Ok(entries) = std::fs::read_dir(ctx.home.join("Library/Containers")) else {
        return;
    };
    for e in entries.flatten() {
        let hf = e.path().join("Data/Library/Caches/huggingface");
        if !hf.exists() {
            continue;
        }
        let bundle = e.file_name().to_string_lossy().into_owned();
        let s = dir_stats(&hf);
        let mut item = Item::sized(
            Category::AppData,
            format!("{bundle} Hugging Face models"),
            &hf,
            &s,
        );
        item.verdict = Verdict::Review;
        item.reasons = vec![
            format!("ML models downloaded by {bundle}"),
            "the app downloads them again when it needs them".into(),
        ];
        item.action = Action::delete(&hf);
        emit(item);
    }
}

pub fn catch_all_parents(home: &Path) -> Vec<PathBuf> {
    [
        "Library/Containers",
        "Library/Application Support",
        "Library/Caches",
        "Library/Group Containers",
        ".cache",
        ".local/share",
    ]
    .iter()
    .map(|p| home.join(p))
    .collect()
}

const SKIP_DOTDIRS: &[&str] = &[
    ".Trash", ".cache", ".local", ".git", ".ssh", ".gnupg", ".config",
];

fn dotdirs(home: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(home) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter(|e| {
            let n = e.file_name();
            let n = n.to_string_lossy();
            n.starts_with('.') && !SKIP_DOTDIRS.contains(&n.as_ref())
        })
        .map(|e| e.path())
        .collect()
}

pub fn uncovered(bytes: u64, path: &Path, covered: &[(PathBuf, u64)]) -> u64 {
    let inside: u64 = covered
        .iter()
        .filter(|(p, _)| p.starts_with(path) || p.parent().is_some_and(|pp| pp.starts_with(path)))
        .map(|(_, b)| *b)
        .sum();
    bytes.saturating_sub(inside)
}

pub fn scan_catch_all(ctx: &Ctx, covered: &[(PathBuf, u64)], emit: Emit) {
    let min = ctx.cfg.catch_all_min_gb << 30;
    let caches = ctx.home.join("Library/Caches");
    let mut candidates: Vec<(PathBuf, u64, Option<chrono::DateTime<chrono::Utc>>)> = Vec::new();
    std::thread::scope(|s| {
        let handles: Vec<_> = catch_all_parents(&ctx.home)
            .into_iter()
            .map(|parent| {
                s.spawn(move || {
                    let stats = dir_stats_opts(
                        &parent,
                        &Opts {
                            breakdown_under: Some(&parent),
                            mtime_ignore: &[],
                        },
                    );
                    stats
                        .breakdown
                        .into_iter()
                        .map(|(child, b)| (parent.join(child), b, None))
                        .collect::<Vec<_>>()
                })
            })
            .chain(dotdirs(&ctx.home).into_iter().map(|d| {
                s.spawn(move || {
                    let st = dir_stats(&d);
                    vec![(d, st.bytes, st.newest)]
                })
            }))
            .collect();
        for h in handles {
            if let Ok(v) = h.join() {
                candidates.extend(v);
            }
        }
    });
    for (path, bytes, newest) in candidates {
        let rest = uncovered(bytes, &path, covered);
        if rest < min || !path.is_dir() {
            continue;
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let is_other = rest + (1 << 30) < bytes;
        let mut item = Item::new(
            Category::AppData,
            if is_other {
                format!("{name} (other data)")
            } else {
                name.clone()
            },
            &path,
        );
        item.bytes = rest;
        item.reclaimable = rest;
        item.last_used = newer(newest, mtime(&path));
        item.verdict = Verdict::Review;
        item.reasons = vec![format!(
            "no Dustpan rule recognizes this folder; press x to ask {} what it is",
            ctx.cfg.ai.provider
        )];
        if is_other {
            item.reasons
                .push("size excludes items listed separately".into());
        }
        if path.starts_with(&caches) && !is_other {
            item.reasons
                .push("it lives in ~/Library/Caches, which apps must be able to rebuild".into());
            item.action = Action::delete(&path);
        }
        emit(item);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uncovered_subtracts_items_inside() {
        let covered = vec![
            (
                PathBuf::from("/h/Library/Containers/app/Data/Library/Caches/huggingface"),
                24,
            ),
            (
                PathBuf::from("/h/Library/Containers/app/Data/tmp/CFNetworkDownload_*.tmp"),
                15,
            ),
            (PathBuf::from("/h/Library/Containers/other/x"), 100),
        ];
        assert_eq!(
            uncovered(44, Path::new("/h/Library/Containers/app"), &covered),
            5
        );
        assert_eq!(
            uncovered(10, Path::new("/h/Library/Containers/app"), &covered[..0]),
            10
        );
    }
}
