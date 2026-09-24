use super::{Ctx, Emit};
use crate::model::{Action, Category, Item, Verdict};
use crate::size::dir_stats;
use std::collections::HashMap;

const PLATFORMS: &[&str] = &[
    "iOS DeviceSupport",
    "watchOS DeviceSupport",
    "tvOS DeviceSupport",
    "visionOS DeviceSupport",
    "xrOS DeviceSupport",
    "macOS DeviceSupport",
];

pub fn parse_name(name: &str) -> Option<(String, Vec<u32>)> {
    let head = name.split(" (").next()?.trim();
    let (model, version) = match head.rsplit_once(' ') {
        Some((m, v)) => (m.to_string(), v),
        None => (String::new(), head),
    };
    let parts: Option<Vec<u32>> = version.split('.').map(|p| p.parse().ok()).collect();
    Some((model, parts?))
}

pub fn scan(ctx: &Ctx, emit: Emit) {
    for platform in PLATFORMS {
        let root = ctx.home.join("Library/Developer/Xcode").join(platform);
        let Ok(entries) = std::fs::read_dir(&root) else { continue };
        let mut groups: HashMap<String, Vec<(Vec<u32>, std::path::PathBuf, String)>> = HashMap::new();
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if let Some((model, ver)) = parse_name(&name) {
                groups.entry(model).or_default().push((ver, e.path(), name));
            }
        }
        for (model, mut list) in groups {
            list.sort_by(|a, b| b.0.cmp(&a.0));
            let newest = list[0].2.clone();
            for (i, (_, path, name)) in list.into_iter().enumerate() {
                let stats = dir_stats(&path);
                let mut item = Item::new(
                    Category::DeviceSupport,
                    format!("{} {name}", platform.replace(" DeviceSupport", "")),
                    &path,
                );
                item.bytes = stats.bytes;
                item.reclaimable = stats.exclusive;
                item.last_used = stats.newest;
                item.action = Action::delete(&path);
                let device = if model.is_empty() { "this platform".to_string() } else { model.clone() };
                if i == 0 {
                    item.verdict = Verdict::Active;
                    item.reasons = vec![format!("newest debug symbols for {device}")];
                } else {
                    item.verdict = Verdict::Safe;
                    item.reasons = vec![
                        format!("{newest} is newer for {device}"),
                        "Xcode copies symbols again if a device on this version connects".into(),
                    ];
                }
                emit(item);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_names() {
        assert_eq!(
            parse_name("iPhone17,1 27.0 (24A435)"),
            Some(("iPhone17,1".into(), vec![27, 0]))
        );
        assert_eq!(parse_name("17.0.3 (21A360)"), Some((String::new(), vec![17, 0, 3])));
        assert_eq!(parse_name("garbage"), None);
    }
}
