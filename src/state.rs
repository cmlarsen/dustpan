use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::PathBuf;

#[derive(Default, Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct State {
    pub pins: BTreeSet<String>,
    pub ai_notes: BTreeMap<String, AiNote>,
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

impl State {
    pub fn load() -> State {
        std::fs::read_to_string(state_dir().join("state.json"))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(state_dir())?;
        let tmp = state_dir().join("state.json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(tmp, state_dir().join("state.json"))?;
        Ok(())
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
    std::fs::create_dir_all(state_dir())?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(state_dir().join("history.jsonl"))?;
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
