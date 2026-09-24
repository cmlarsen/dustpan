use super::{Ctx, Emit};
use crate::model::{Action, Category, Item, Verdict};
use crate::util::{age_days, mtime};
use std::path::Path;

const INSTALLERS: &[&str] = &["dmg", "pkg", "mpkg", "xip", "iso"];
const ARCHIVES: &[&str] = &["zip", "tgz", "7z", "rar"];
pub const MIN_AGE_DAYS: i64 = 14;

pub fn kind_of(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_string_lossy().to_ascii_lowercase();
    if INSTALLERS.contains(&ext.as_str()) {
        Some("installer")
    } else if ARCHIVES.contains(&ext.as_str()) {
        Some("archive")
    } else {
        None
    }
}

pub fn scan(ctx: &Ctx, emit: Emit) {
    let dir = ctx.home.join("Downloads");
    let Ok(entries) = std::fs::read_dir(&dir) else { return };
    for e in entries.flatten() {
        let path = e.path();
        let Some(kind) = kind_of(&path) else { continue };
        let Ok(meta) = std::fs::symlink_metadata(&path) else { continue };
        if !meta.is_file() {
            continue;
        }
        let modified = mtime(&path);
        let Some(age) = age_days(modified, ctx.now).filter(|d| *d >= MIN_AGE_DAYS) else { continue };
        let name = e.file_name().to_string_lossy().into_owned();
        let mut item = Item::new(Category::Download, name, &path);
        item.bytes = meta.len();
        item.reclaimable = meta.len();
        item.last_used = modified;
        item.verdict = Verdict::Review;
        item.reasons = vec![match kind {
            "installer" => format!("installer downloaded {age} days ago; the installed app doesn't need it"),
            _ => format!("archive downloaded {age} days ago; check you extracted what you need"),
        }];
        item.action = Action::delete(&path);
        emit(item);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds() {
        assert_eq!(kind_of(Path::new("/d/Xcode_26.xip")), Some("installer"));
        assert_eq!(kind_of(Path::new("/d/Thing.DMG")), Some("installer"));
        assert_eq!(kind_of(Path::new("/d/photos.zip")), Some("archive"));
        assert_eq!(kind_of(Path::new("/d/notes.pdf")), None);
    }
}
