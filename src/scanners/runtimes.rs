use super::{Ctx, Emit};
use crate::model::{Action, Category, Item, Verdict};
use crate::util::run;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Runtime {
    pub identifier: String,
    pub version: String,
    pub build: String,
    pub platform_identifier: String,
    #[serde(default)]
    pub runtime_identifier: Option<String>,
    #[serde(default)]
    pub size_bytes: u64,
    #[serde(default)]
    pub deletable: bool,
    #[serde(default)]
    pub last_used_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct RtFacts {
    pub platform: String,
    pub version: String,
    pub beta: bool,
    pub newest_for_platform: bool,
    pub release_of_same_version: bool,
    pub devices: usize,
    pub deletable: bool,
}

pub fn platform_label(id: &str) -> String {
    match id.rsplit('.').next().unwrap_or(id) {
        "iphonesimulator" => "iOS",
        "watchsimulator" => "watchOS",
        "appletvsimulator" => "tvOS",
        "xrsimulator" => "visionOS",
        other => other,
    }
    .to_string()
}

pub fn is_beta(build: &str) -> bool {
    build.chars().last().is_some_and(|c| c.is_ascii_lowercase())
}

fn version_key(v: &str) -> Vec<u32> {
    v.split('.').map(|p| p.parse().unwrap_or(0)).collect()
}

pub fn classify(f: &RtFacts) -> (Verdict, Vec<String>) {
    let users = match f.devices {
        0 => "no simulators use it".to_string(),
        1 => "1 simulator uses it".to_string(),
        n => format!("{n} simulators use it"),
    };
    if !f.deletable {
        return (Verdict::Keep, vec!["managed by Xcode; simctl won't delete it".into()]);
    }
    if f.beta && f.release_of_same_version {
        return (
            Verdict::Safe,
            vec![format!("beta build, superseded by the {} {} release", f.platform, f.version), users],
        );
    }
    if f.newest_for_platform {
        return (Verdict::Active, vec![format!("newest {} runtime", f.platform), users]);
    }
    if f.devices == 0 {
        return (Verdict::Safe, vec![format!("older {} runtime", f.platform), users]);
    }
    (
        Verdict::Review,
        vec![
            format!("older {} runtime", f.platform),
            format!("{users}; they become unavailable, and a rescan offers to delete them"),
        ],
    )
}

pub fn facts_for(all: &[Runtime], devices: &HashMap<String, usize>) -> Vec<(Runtime, RtFacts)> {
    let mut newest: HashMap<String, (Vec<u32>, bool)> = HashMap::new();
    for r in all {
        let key = (version_key(&r.version), !is_beta(&r.build));
        let e = newest.entry(r.platform_identifier.clone()).or_insert(key.clone());
        if key > *e {
            *e = key;
        }
    }
    all.iter()
        .map(|r| {
            let best = &newest[&r.platform_identifier];
            let facts = RtFacts {
                platform: platform_label(&r.platform_identifier),
                version: r.version.clone(),
                beta: is_beta(&r.build),
                newest_for_platform: (version_key(&r.version), !is_beta(&r.build)) == *best,
                release_of_same_version: all.iter().any(|o| {
                    o.platform_identifier == r.platform_identifier && o.version == r.version && !is_beta(&o.build)
                }),
                devices: r
                    .runtime_identifier
                    .as_ref()
                    .and_then(|id| devices.get(id))
                    .copied()
                    .unwrap_or(0),
                deletable: r.deletable,
            };
            (r.clone(), facts)
        })
        .collect()
}

fn device_counts() -> HashMap<String, usize> {
    #[derive(Deserialize)]
    struct List {
        devices: HashMap<String, Vec<serde_json::Value>>,
    }
    run("xcrun", &["simctl", "list", "devices", "-j"], None, Duration::from_secs(30))
        .and_then(|o| serde_json::from_str::<List>(&o.stdout).ok())
        .map(|l| l.devices.into_iter().map(|(k, v)| (k, v.len())).collect())
        .unwrap_or_default()
}

pub fn scan(ctx: &Ctx, emit: Emit) {
    let Some(out) = run("xcrun", &["simctl", "runtime", "list", "-j"], None, Duration::from_secs(30)) else {
        return;
    };
    let Ok(map) = serde_json::from_str::<HashMap<String, Runtime>>(&out.stdout) else { return };
    let runtimes: Vec<Runtime> = map.into_values().collect();
    for (rt, facts) in facts_for(&runtimes, &device_counts()) {
        let path = rt
            .path
            .clone()
            .map(PathBuf::from)
            .unwrap_or_else(|| ctx.home.join("Library/Developer/CoreSimulator"));
        let mut item = Item::new(
            Category::SimRuntime,
            format!("{} {} ({})", facts.platform, rt.version, rt.build),
            &path,
        );
        item.id = format!("sim_runtime:{}", rt.identifier);
        item.bytes = rt.size_bytes;
        item.reclaimable = rt.size_bytes;
        item.last_used = rt.last_used_at;
        let (verdict, mut reasons) = classify(&facts);
        reasons.push("re-download it in Xcode → Settings → Components".into());
        item.verdict = verdict;
        item.reasons = reasons;
        item.action = Action::run("xcrun", &["simctl", "runtime", "delete", &rt.identifier], None);
        emit(item);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt(id: &str, platform: &str, version: &str, build: &str, runtime: &str) -> Runtime {
        Runtime {
            identifier: id.into(),
            version: version.into(),
            build: build.into(),
            platform_identifier: format!("com.apple.platform.{platform}"),
            runtime_identifier: Some(runtime.into()),
            size_bytes: 8 << 30,
            deletable: true,
            last_used_at: None,
            path: None,
        }
    }

    #[test]
    fn beta_detection_and_labels() {
        assert!(is_beta("23A5276f"));
        assert!(!is_beta("23F77"));
        assert_eq!(platform_label("com.apple.platform.iphonesimulator"), "iOS");
    }

    #[test]
    fn classifies_newest_beta_old_and_used() {
        let all = vec![
            rt("new", "iphonesimulator", "26.5", "23F77", "ios-26-5"),
            rt("beta", "iphonesimulator", "26.5", "23F5043k", "ios-26-5-beta"),
            rt("old-unused", "iphonesimulator", "18.4", "22E238", "ios-18-4"),
            rt("old-used", "iphonesimulator", "17.5", "21F79", "ios-17-5"),
            rt("watch", "watchsimulator", "26.5", "23T570", "watch-26-5"),
        ];
        let devices = HashMap::from([("ios-17-5".to_string(), 2), ("ios-26-5".to_string(), 9)]);
        let f: HashMap<String, RtFacts> = facts_for(&all, &devices)
            .into_iter()
            .map(|(r, f)| (r.identifier, f))
            .collect();
        assert_eq!(classify(&f["new"]).0, Verdict::Active);
        assert_eq!(classify(&f["beta"]).0, Verdict::Safe);
        assert_eq!(classify(&f["old-unused"]).0, Verdict::Safe);
        assert_eq!(classify(&f["old-used"]).0, Verdict::Review);
        assert_eq!(classify(&f["watch"]).0, Verdict::Active);
    }

    #[test]
    fn newest_beta_without_release_stays_active() {
        let all = vec![
            rt("beta27", "iphonesimulator", "27.0", "24A5260e", "ios-27"),
            rt("rel26", "iphonesimulator", "26.5", "23F77", "ios-26-5"),
        ];
        let f: HashMap<String, RtFacts> = facts_for(&all, &HashMap::new())
            .into_iter()
            .map(|(r, f)| (r.identifier, f))
            .collect();
        assert_eq!(classify(&f["beta27"]).0, Verdict::Active);
        assert_eq!(classify(&f["rel26"]).0, Verdict::Safe);
    }

    #[test]
    fn parses_simctl_json() {
        let text = r#"{"5986A48F":{"build":"23T570","deletable":true,"identifier":"5986A48F","kind":"Disk Image","lastUsedAt":"2026-09-17T19:08:45Z","platformIdentifier":"com.apple.platform.watchsimulator","runtimeIdentifier":"com.apple.CoreSimulator.SimRuntime.watchOS-26-5","sizeBytes":3935033546,"state":"Ready","version":"26.5"}}"#;
        let m: HashMap<String, Runtime> = serde_json::from_str(text).unwrap();
        let r = &m["5986A48F"];
        assert_eq!(r.size_bytes, 3_935_033_546);
        assert!(r.last_used_at.is_some());
    }
}
