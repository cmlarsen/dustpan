mod app;
mod draw;
mod modal;
mod picker;

use crate::catalog::Catalog;
use crate::config::Config;
use crate::model::Verdict;
use crate::procs::ProcSnapshot;
use crate::scan::ScanMsg;
use anyhow::Result;
use chrono::Utc;
use ratatui::widgets::TableState;
use std::collections::HashSet;
use std::hash::Hash;

pub(crate) const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub(crate) enum Msg {
    Scan(ScanMsg),
    Procs(Box<ProcSnapshot>, chrono::DateTime<Utc>),
    Ai {
        key: String,
        provider: String,
        result: Result<String, String>,
    },
    Cleaned {
        id: String,
        outcome: CleanOutcome,
        msg: String,
        freed: u64,
    },
    CleanDone,
    Killed {
        ok: bool,
        msg: String,
    },
    Catalog(Box<Catalog>),
}

pub(crate) enum CleanOutcome {
    Success,
    Failure,
    SkippedPinned,
}

pub(crate) struct ListSel<K> {
    pub(crate) selected: Option<K>,
    pub(crate) marked: HashSet<K>,
    pub(crate) anchor: Option<K>,
    pub(crate) table: TableState,
}

impl<K: Clone + Eq + Hash> ListSel<K> {
    pub(crate) fn new() -> Self {
        Self {
            selected: None,
            marked: HashSet::new(),
            anchor: None,
            table: TableState::default(),
        }
    }

    pub(crate) fn index(&self, keys: &[K]) -> Option<usize> {
        self.selected
            .as_ref()
            .and_then(|selected| keys.iter().position(|key| key == selected))
    }

    pub(crate) fn move_by(&mut self, keys: &[K], delta: isize) {
        if keys.is_empty() {
            return;
        }
        let current = self.index(keys).unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, keys.len() as isize - 1) as usize;
        self.selected = Some(keys[next].clone());
    }

    pub(crate) fn toggle_mark(&mut self, key: K) {
        if !self.marked.remove(&key) {
            self.marked.insert(key);
        }
    }
}

#[derive(PartialEq, Clone, Copy)]
pub(crate) enum Tab {
    Disk,
    Procs,
    History,
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Filter {
    All,
    Safe,
    Review,
    InUse,
    Keep,
}

impl Filter {
    pub(crate) fn next(self) -> Self {
        match self {
            Filter::All => Filter::Safe,
            Filter::Safe => Filter::Review,
            Filter::Review => Filter::InUse,
            Filter::InUse => Filter::Keep,
            Filter::Keep => Filter::All,
        }
    }
    pub(crate) fn label(self) -> &'static str {
        match self {
            Filter::All => "all",
            Filter::Safe => "safe",
            Filter::Review => "review",
            Filter::InUse => "in use",
            Filter::Keep => "keep",
        }
    }
    pub(crate) fn matches(self, v: Verdict) -> bool {
        match self {
            Filter::All => true,
            Filter::Safe => v == Verdict::Safe,
            Filter::Review => v == Verdict::Review,
            Filter::InUse => v == Verdict::Active,
            Filter::Keep => v == Verdict::Keep,
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Sort {
    Verdict,
    Size,
    Age,
    Name,
}

impl Sort {
    pub(crate) fn next(self) -> Self {
        match self {
            Sort::Verdict => Sort::Size,
            Sort::Size => Sort::Age,
            Sort::Age => Sort::Name,
            Sort::Name => Sort::Verdict,
        }
    }
    pub(crate) fn label(self) -> &'static str {
        match self {
            Sort::Verdict => "verdict",
            Sort::Size => "size",
            Sort::Age => "oldest",
            Sort::Name => "name",
        }
    }
}

pub(crate) enum Modal {
    None,
    Help,
    ConfirmClean {
        ids: Vec<String>,
        skipped: Vec<String>,
    },
    ConfirmKill(Vec<(u32, String)>),
    AiPicker(usize),
}

pub fn run(cfg: Config) -> Result<()> {
    let mut app = app::App::new(cfg);
    app.start_scan();
    app.refresh_procs();
    let mut terminal = ratatui::init();
    let result = app.run(&mut terminal);
    ratatui::restore();
    result
}
