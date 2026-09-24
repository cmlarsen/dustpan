use super::{Ctx, Emit};
use crate::model::{Action, Category, Item, Verdict};
use crate::size::dir_stats;
use crate::util::{age_days, which};

enum Clean {
    Delete,
    Command(&'static str, &'static [&'static str]),
}

struct Known {
    name: &'static str,
    path: &'static str,
    verdict: Verdict,
    clean: Clean,
    note: &'static str,
}

const KNOWN: &[Known] = &[
    Known { name: "pnpm store", path: "Library/pnpm/store", verdict: Verdict::Safe, clean: Clean::Command("pnpm", &["store", "prune"]), note: "prune removes packages no project references; frees part of this" },
    Known { name: "uv cache", path: ".cache/uv", verdict: Verdict::Safe, clean: Clean::Command("uv", &["cache", "prune"]), note: "prune removes unused wheels and sources" },
    Known { name: "Homebrew downloads", path: "Library/Caches/Homebrew", verdict: Verdict::Review, clean: Clean::Command("brew", &["cleanup", "--prune=all"]), note: "old bottles and downloads; `brew cleanup --prune=all` also removes old formula versions, which breaks virtualenvs pinned to a Cellar path" },
    Known { name: "CocoaPods cache", path: "Library/Caches/CocoaPods", verdict: Verdict::Safe, clean: Clean::Delete, note: "pods re-download on the next pod install" },
    Known { name: "npm cache", path: ".npm/_cacache", verdict: Verdict::Safe, clean: Clean::Delete, note: "npm re-downloads on demand" },
    Known { name: "bun cache", path: ".bun/install/cache", verdict: Verdict::Safe, clean: Clean::Delete, note: "bun re-downloads on demand" },
    Known { name: "Yarn cache", path: "Library/Caches/Yarn", verdict: Verdict::Safe, clean: Clean::Delete, note: "yarn re-downloads on demand" },
    Known { name: "cargo registry cache", path: ".cargo/registry/cache", verdict: Verdict::Safe, clean: Clean::Delete, note: "crate archives; cargo re-downloads on demand" },
    Known { name: "Go build cache", path: "Library/Caches/go-build", verdict: Verdict::Safe, clean: Clean::Delete, note: "rebuilt on the next go build" },
    Known { name: "pip cache", path: "Library/Caches/pip", verdict: Verdict::Safe, clean: Clean::Delete, note: "pip re-downloads on demand" },
    Known { name: "node-gyp headers", path: "Library/Caches/node-gyp", verdict: Verdict::Safe, clean: Clean::Delete, note: "re-downloaded on the next native build" },
    Known { name: "Electron downloads", path: "Library/Caches/electron", verdict: Verdict::Safe, clean: Clean::Delete, note: "Electron zips cached by electron-builder and friends" },
    Known { name: "Simulator caches", path: "Library/Developer/CoreSimulator/Caches", verdict: Verdict::Safe, clean: Clean::Delete, note: "dyld and runtime caches; rebuilt on next boot" },
    Known { name: "Xcode previews", path: "Library/Developer/Xcode/UserData/Previews", verdict: Verdict::Safe, clean: Clean::Delete, note: "SwiftUI preview simulators and builds" },
    Known { name: "XCTest device clones", path: "Library/Developer/XCTestDevices", verdict: Verdict::Safe, clean: Clean::Delete, note: "clones left by parallel test runs" },
    Known { name: "Xcode archives", path: "Library/Developer/Xcode/Archives", verdict: Verdict::Review, clean: Clean::Delete, note: "needed to re-export or symbolicate old builds" },
    Known { name: "Gradle caches", path: ".gradle/caches", verdict: Verdict::Review, clean: Clean::Delete, note: "Android/Gradle dependencies; slow to re-download" },
    Known { name: "Puppeteer browsers", path: ".cache/puppeteer", verdict: Verdict::Review, clean: Clean::Delete, note: "headless Chrome builds; re-downloaded on the next install" },
    Known { name: "Playwright browsers", path: "Library/Caches/ms-playwright", verdict: Verdict::Review, clean: Clean::Delete, note: "browser builds; `npx playwright install` restores them" },
    Known { name: "Android emulators", path: ".android/avd", verdict: Verdict::Review, clean: Clean::Delete, note: "emulator disks and snapshots; recreate them in Android Studio" },
    Known { name: "Android system images", path: "Library/Android/sdk/system-images", verdict: Verdict::Review, clean: Clean::Delete, note: "emulator OS images; the SDK Manager downloads them again" },
    Known { name: "Hugging Face models", path: ".cache/huggingface", verdict: Verdict::Review, clean: Clean::Delete, note: "downloaded models; large to fetch again" },
];

pub fn recency(verdict: Verdict, action: &Action, idle_days: Option<i64>, stale_days: i64) -> (Verdict, Option<String>) {
    let deletes = matches!(action, Action::Delete { .. });
    match idle_days {
        Some(d) if deletes && verdict == Verdict::Safe && d <= stale_days => (
            Verdict::Review,
            Some(format!("in active use ({d}d ago): deleting it only costs re-download time")),
        ),
        _ => (verdict, None),
    }
}

pub fn scan(ctx: &Ctx, emit: Emit) {
    std::thread::scope(|s| {
        for k in KNOWN {
            s.spawn(move || {
                let path = ctx.home.join(k.path);
                if !path.exists() {
                    return;
                }
                let stats = dir_stats(&path);
                let mut item = Item::sized(Category::PackageCache, k.name, &path, &stats);
                item.verdict = k.verdict;
                item.reasons = vec![k.note.to_string()];
                item.action = match &k.clean {
                    Clean::Command(prog, args) if which(prog).is_some() => {
                        item.reclaimable = 0;
                        Action::run(prog, args, None)
                    }
                    Clean::Command(prog, _) => {
                        item.reasons.push(format!("{prog} is not on PATH, so the folder is deleted instead"));
                        Action::delete(&path)
                    }
                    Clean::Delete => Action::delete(&path),
                };
                let (verdict, note) = recency(item.verdict, &item.action, age_days(item.last_used, ctx.now), ctx.cfg.stale_days);
                item.verdict = verdict;
                item.reasons.extend(note);
                emit(item);
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recently_used_delete_caches_need_review_but_prunes_stay_safe() {
        let del = Action::delete("/h/.npm/_cacache");
        let prune = Action::run("pnpm", &["store", "prune"], None);
        assert_eq!(recency(Verdict::Safe, &del, Some(2), 30).0, Verdict::Review);
        assert_eq!(recency(Verdict::Safe, &del, Some(60), 30).0, Verdict::Safe);
        assert_eq!(recency(Verdict::Safe, &prune, Some(0), 30).0, Verdict::Safe);
        assert_eq!(recency(Verdict::Review, &del, Some(1), 30), (Verdict::Review, None));
    }

    #[test]
    fn brew_cleanup_is_review_because_it_drops_old_formula_versions() {
        let brew = KNOWN.iter().find(|k| k.path == "Library/Caches/Homebrew").unwrap();
        assert_eq!(brew.verdict, Verdict::Review);
        assert!(brew.note.contains("old formula versions"));
    }
}
