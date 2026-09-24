use super::{Ctx, Emit};
use crate::model::{Action, Category, Item, Verdict};
use crate::size::{dir_stats_opts, Opts};
use crate::util::{age_days, human, run};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

#[derive(Deserialize)]
struct SimList {
    devices: HashMap<String, Vec<SimDev>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SimDev {
    udid: String,
    name: String,
    state: String,
    is_available: bool,
}

#[derive(Debug, Clone, Default)]
pub struct SimFacts {
    pub available: bool,
    pub booted: bool,
    pub worktree: Option<String>,
    pub worktree_live: bool,
    pub idle_days: Option<i64>,
    pub bytes: u64,
}

pub const HEAVY_BYTES: u64 = 10 << 30;

pub fn worktree_suffix(name: &str) -> Option<&str> {
    let start = name.rfind("(wt-")? + 4;
    let end = name[start..].find(')')? + start;
    Some(&name[start..end])
}

pub fn classify(f: &SimFacts, stale_days: i64) -> (Verdict, Vec<String>, SimAction) {
    if !f.available {
        return (
            Verdict::Safe,
            vec!["its iOS runtime is no longer installed".into()],
            SimAction::Delete,
        );
    }
    if f.booted {
        return (Verdict::Active, vec!["booted right now".into()], SimAction::Erase);
    }
    if let Some(wt) = &f.worktree {
        return if f.worktree_live {
            (
                Verdict::Keep,
                vec![format!("dedicated simulator for worktree {wt}")],
                SimAction::Erase,
            )
        } else {
            (
                Verdict::Safe,
                vec![format!("clone for worktree {wt}, which no longer exists")],
                SimAction::Delete,
            )
        };
    }
    match f.idle_days {
        Some(d) if d > stale_days => (
            Verdict::Review,
            vec![
                format!("not used in {d} days"),
                "erase wipes its apps and data but keeps the device".into(),
            ],
            SimAction::Erase,
        ),
        d if f.bytes >= HEAVY_BYTES => (
            Verdict::Review,
            vec![
                format!("holds {} even though it was used {}d ago", human(f.bytes), d.unwrap_or(0)),
                "erase wipes its apps and data but keeps the device".into(),
            ],
            SimAction::Erase,
        ),
        d => (
            Verdict::Active,
            vec![format!("used {}d ago", d.unwrap_or(0))],
            SimAction::Erase,
        ),
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SimAction {
    Erase,
    Delete,
}

fn last_used(device_dir: &Path) -> Option<DateTime<Utc>> {
    let v = plist::Value::from_file(device_dir.join("device.plist")).ok()?;
    let date = v.as_dictionary()?.get("lastUsedAt")?.as_date()?;
    Some(DateTime::<Utc>::from(SystemTime::from(date)))
}

fn app_bundle_id(container: &Path) -> Option<String> {
    let v = plist::Value::from_file(container.join(".com.apple.mobile_container_manager.metadata.plist")).ok()?;
    v.as_dictionary()?
        .get("MCMMetadataIdentifier")?
        .as_string()
        .map(String::from)
}

pub fn scan(ctx: &Ctx, emit: Emit) {
    let Some(out) = run(
        "xcrun",
        &["simctl", "list", "devices", "-j"],
        None,
        Duration::from_secs(30),
    ) else {
        return;
    };
    let Ok(list) = serde_json::from_str::<SimList>(&out.stdout) else { return };
    let live = ctx.live_worktree_names();
    let base = ctx.home.join("Library/Developer/CoreSimulator/Devices");
    let devices: Vec<(String, SimDev)> = list
        .devices
        .into_iter()
        .flat_map(|(rt, devs)| devs.into_iter().map(move |d| (rt.clone(), d)))
        .collect();
    std::thread::scope(|s| {
        for (runtime, dev) in &devices {
            let base = &base;
            let live = &live;
            s.spawn(move || {
                if let Some(item) = build(ctx, base, runtime, dev, live) {
                    emit(item);
                }
            });
        }
    });
}

fn build(ctx: &Ctx, base: &Path, runtime: &str, dev: &SimDev, live: &[String]) -> Option<Item> {
    let dir: PathBuf = base.join(&dev.udid);
    let apps_dir = dir.join("data/Containers/Data/Application");
    let stats = dir_stats_opts(
        &dir,
        &Opts {
            breakdown_under: Some(&apps_dir),
            mtime_ignore: &[],
        },
    );
    let runtime_label = runtime
        .rsplit('.')
        .next()
        .unwrap_or(runtime)
        .replace("iOS-", "iOS ")
        .replace("watchOS-", "watchOS ")
        .replace('-', ".");
    let mut item = Item::new(
        Category::Simulator,
        format!("{} · {}", dev.name, runtime_label),
        &dir,
    );
    item.bytes = stats.bytes;
    item.reclaimable = stats.exclusive;
    item.last_used = last_used(&dir);
    let worktree = worktree_suffix(&dev.name).map(String::from);
    let facts = SimFacts {
        available: dev.is_available,
        booted: dev.state == "Booted",
        worktree_live: worktree.as_ref().is_some_and(|w| live.contains(w)),
        worktree,
        idle_days: age_days(item.last_used, ctx.now),
        bytes: item.bytes,
    };
    let (verdict, mut reasons, act) = classify(&facts, ctx.cfg.stale_days);
    item.verdict = verdict;
    let mut apps: Vec<(&String, &u64)> = stats.breakdown.iter().collect();
    apps.sort_by(|a, b| b.1.cmp(a.1));
    for (container, bytes) in apps.into_iter().take(3).filter(|(_, b)| **b > 200 << 20) {
        let id = app_bundle_id(&apps_dir.join(container)).unwrap_or_else(|| container.clone());
        reasons.push(format!("app data: {id} ({})", human(*bytes)));
    }
    item.reasons = reasons;
    item.action = match act {
        SimAction::Erase => Action::run("xcrun", &["simctl", "erase", &dev.udid], None),
        SimAction::Delete => Action::run("xcrun", &["simctl", "delete", &dev.udid], None),
    };
    Some(item)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suffix() {
        assert_eq!(worktree_suffix("iPhone 17 Pro (wt-feature-x)"), Some("feature-x"));
        assert_eq!(worktree_suffix("iPhone 17 Pro"), None);
    }

    #[test]
    fn rules() {
        let base = SimFacts { available: true, idle_days: Some(2), ..Default::default() };
        assert_eq!(classify(&base, 30).0, Verdict::Active);
        let old = SimFacts { idle_days: Some(60), ..base.clone() };
        let (v, _, a) = classify(&old, 30);
        assert_eq!((v, a), (Verdict::Review, SimAction::Erase));
        let gone = SimFacts { worktree: Some("x".into()), worktree_live: false, ..base.clone() };
        let (v, _, a) = classify(&gone, 30);
        assert_eq!((v, a), (Verdict::Safe, SimAction::Delete));
        let live = SimFacts { worktree: Some("x".into()), worktree_live: true, ..base.clone() };
        assert_eq!(classify(&live, 30).0, Verdict::Keep);
        let booted = SimFacts { booted: true, ..old.clone() };
        assert_eq!(classify(&booted, 30).0, Verdict::Active);
        let heavy = SimFacts { bytes: 20 << 30, ..base.clone() };
        assert_eq!(classify(&heavy, 30).0, Verdict::Review);
        let heavy_live = SimFacts { bytes: 20 << 30, worktree: Some("x".into()), worktree_live: true, ..base.clone() };
        assert_eq!(classify(&heavy_live, 30).0, Verdict::Keep);
        let unavailable = SimFacts { available: false, ..base };
        assert_eq!(classify(&unavailable, 30).2, SimAction::Delete);
    }
}
