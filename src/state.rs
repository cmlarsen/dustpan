use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Default, Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct State {
    pub pins: BTreeSet<String>,
    pub ai_notes: BTreeMap<String, AiNote>,
    pub ai: AiChoice,
    #[serde(skip)]
    pub warning: Option<String>,
    #[serde(skip)]
    pub read_only: bool,
}

#[derive(Default, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct AiChoice {
    pub provider: Option<String>,
    pub claude_model: Option<String>,
    pub codex_model: Option<String>,
    pub effort: Option<String>,
}

impl AiChoice {
    pub fn apply(&self, cfg: &mut crate::config::AiConfig) {
        if let Some(v) = &self.provider {
            cfg.provider = v.clone();
        }
        if let Some(v) = &self.claude_model {
            cfg.claude_model = v.clone();
        }
        if let Some(v) = &self.codex_model {
            cfg.codex_model = v.clone();
        }
        if let Some(v) = &self.effort {
            cfg.effort = v.clone();
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AiNote {
    pub provider: String,
    pub at: DateTime<Utc>,
    pub text: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct HistoryEntry {
    pub at: DateTime<Utc>,
    pub id: String,
    pub name: String,
    pub category: String,
    pub bytes: u64,
    pub action: String,
    pub ok: bool,
    pub message: String,
}

pub fn state_dir() -> PathBuf {
    crate::util::home().join(".local/state/dustpan")
}

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

fn write_private(dir: &Path, name: &str, body: &[u8]) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = Utc::now().timestamp_nanos_opt().unwrap_or_default();
    let tmp = dir.join(format!(".{name}.{}.{nanos}.{seq}.tmp", std::process::id()));
    let result = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut f| {
            f.write_all(body)?;
            f.sync_all()
        })
        .and_then(|_| std::fs::rename(&tmp, dir.join(name)));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result.with_context(|| format!("writing {}", dir.join(name).display()))
}

impl State {
    pub fn load() -> State {
        State::load_from(&state_dir())
    }

    pub fn load_from(dir: &Path) -> State {
        let path = dir.join("state.json");
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return State::default(),
            Err(e) => {
                return State {
                    warning: Some(format!("could not read {}: {e}; pins won't be saved this run", path.display())),
                    read_only: true,
                    ..State::default()
                };
            }
        };
        match serde_json::from_str(&text) {
            Ok(s) => s,
            Err(e) => {
                let kept = dir.join(format!("state.json.corrupt-{}", Utc::now().timestamp()));
                match std::fs::rename(&path, &kept) {
                    Ok(()) => State {
                        warning: Some(format!("{} was unreadable ({e}); moved it to {} and started fresh", path.display(), kept.display())),
                        ..State::default()
                    },
                    Err(re) => State {
                        warning: Some(format!("{} is unreadable ({e}) and could not be moved aside ({re}); pins won't be saved this run", path.display())),
                        read_only: true,
                        ..State::default()
                    },
                }
            }
        }
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&state_dir())
    }

    pub fn save_to(&self, dir: &Path) -> Result<()> {
        if self.read_only {
            bail!("not saving state: {}", self.warning.as_deref().unwrap_or("it failed to load"));
        }
        write_private(dir, "state.json", serde_json::to_string_pretty(self)?.as_bytes())
    }

    pub fn toggle_pin(&mut self, id: &str) -> bool {
        if self.pins.remove(id) {
            false
        } else {
            self.pins.insert(id.to_string());
            true
        }
    }
}

pub fn append_history(entry: &HistoryEntry) -> Result<()> {
    append_history_in(&state_dir(), entry)
}

fn append_history_in(dir: &Path, entry: &HistoryEntry) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(dir.join("history.jsonl"))?;
    writeln!(f, "{}", serde_json::to_string(entry)?)?;
    Ok(())
}

pub fn read_history(limit: usize) -> Vec<HistoryEntry> {
    let Ok(text) = std::fs::read_to_string(state_dir().join("history.jsonl")) else {
        return Vec::new();
    };
    let mut entries: Vec<HistoryEntry> = text
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    entries.reverse();
    entries.truncate(limit);
    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn corrupt_state_is_moved_aside_and_not_overwritten() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("state.json");
        std::fs::write(&path, b"{\"pins\": [\"keep-me\"").unwrap();
        let mut state = State::load_from(d.path());
        assert!(state.pins.is_empty());
        assert!(state.warning.as_deref().is_some_and(|w| w.contains("corrupt")));
        let kept: Vec<PathBuf> = std::fs::read_dir(d.path())
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with("state.json.corrupt-"))
            .collect();
        assert_eq!(kept.len(), 1);
        state.toggle_pin("new");
        state.save_to(d.path()).unwrap();
        assert_eq!(std::fs::read_to_string(&kept[0]).unwrap(), "{\"pins\": [\"keep-me\"");
        let reloaded = State::load_from(d.path());
        assert!(reloaded.warning.is_none());
        assert!(reloaded.pins.contains("new"));
    }

    #[test]
    fn unreadable_state_refuses_to_save() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("state.json")).unwrap();
        let state = State::load_from(d.path());
        assert!(state.read_only);
        assert!(state.warning.is_some());
        assert!(state.save_to(d.path()).is_err());
        assert!(d.path().join("state.json").is_dir());
    }

    #[test]
    fn history_is_created_private() {
        let d = tempfile::tempdir().unwrap();
        let entry = HistoryEntry {
            at: Utc::now(),
            id: "x".into(),
            name: "x".into(),
            category: "leftover".into(),
            bytes: 0,
            action: "rm -rf /x".into(),
            ok: true,
            message: String::new(),
        };
        append_history_in(d.path(), &entry).unwrap();
        append_history_in(d.path(), &entry).unwrap();
        let path = d.path().join("history.jsonl");
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 2);
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn concurrent_saves_do_not_collide_and_are_private() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().to_path_buf();
        std::thread::scope(|s| {
            for i in 0..16 {
                let dir = &dir;
                s.spawn(move || {
                    let mut state = State::default();
                    state.toggle_pin(&format!("pin-{i}"));
                    for _ in 0..20 {
                        state.save_to(dir).unwrap();
                    }
                });
            }
        });
        let state = State::load_from(&dir);
        assert!(state.warning.is_none());
        assert_eq!(state.pins.len(), 1);
        let leftovers: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().filter(|e| e.file_name() != "state.json").collect();
        assert!(leftovers.is_empty(), "temp files left behind: {leftovers:?}");
        let mode = std::fs::metadata(dir.join("state.json")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
