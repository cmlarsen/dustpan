use super::{CleanOutcome, Filter, ListSel, Modal, Msg, Sort, Tab};
use crate::actions;
use crate::ai;
use crate::catalog::Catalog;
use crate::config::{Config, Protector, resolve_roots};
use crate::model::{Category, Item, Verdict};
use crate::procs::{self, ProcItem, ProcSnapshot};
use crate::scan::{self, ScanMsg};
use crate::state::{self, AiNote, HistoryEntry, State};
use crate::util::{home, human, tilde};
use anyhow::Result;
use chrono::Utc;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::collections::{BTreeSet, HashSet};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub(crate) struct App {
    pub(crate) cfg: Config,
    pub(crate) home: PathBuf,
    pub(crate) roots: Vec<PathBuf>,
    pub(crate) state: State,
    pub(crate) items: Vec<Item>,
    pub(crate) procs: ProcSnapshot,
    pub(crate) procs_at: chrono::DateTime<Utc>,
    pub(crate) history: Vec<HistoryEntry>,
    pub(crate) disk: Option<crate::report::DiskInfo>,
    pub(crate) tab: Tab,
    pub(crate) disk_sel: ListSel<String>,
    pub(crate) proc_sel: ListSel<u32>,
    pub(crate) filter: Filter,
    pub(crate) cat: Option<Category>,
    pub(crate) sort: Sort,
    pub(crate) search: String,
    pub(crate) searching: bool,
    pub(crate) modal: Modal,
    pub(crate) status: String,
    pub(crate) scanning: bool,
    pub(crate) scan_progress: String,
    pub(crate) scan_secs: Option<f64>,
    pub(crate) scan_started: Instant,
    pub(crate) procs_loading: bool,
    pub(crate) pending_ai: HashSet<String>,
    pub(crate) cleaning: HashSet<String>,
    pub(crate) pins: Arc<Mutex<BTreeSet<String>>>,
    pub(crate) clean_successes: usize,
    pub(crate) clean_failures: usize,
    pub(crate) clean_skipped: usize,
    pub(crate) freed_session: u64,
    pub(crate) free_at_start: Option<u64>,
    pub(crate) catalog: Option<Catalog>,
    pub(crate) catalog_loading: bool,
    pub(crate) picker_origin: (String, String),
    pub(crate) persist: bool,
    pub(crate) tick: usize,
    pub(crate) tx: Sender<Msg>,
    pub(crate) rx: Receiver<Msg>,
    pub(crate) quit: bool,
}

impl App {
    pub(crate) fn new(cfg: Config) -> Self {
        let (tx, rx) = channel();
        let home = home();
        let roots = resolve_roots(&cfg, &home);
        let state = State::load();
        let status = state.warning.clone().unwrap_or_default();
        let mut cfg = cfg;
        state.ai.apply(&mut cfg.ai);
        let disk = crate::report::disk_info(&home);
        let pins = Arc::new(Mutex::new(state.pins.clone()));
        App {
            free_at_start: disk.as_ref().map(|d| d.free),
            catalog: None,
            catalog_loading: false,
            picker_origin: (String::new(), String::new()),
            persist: true,
            disk,
            home,
            roots,
            cfg,
            state,
            items: Vec::new(),
            procs: ProcSnapshot::default(),
            procs_at: Utc::now(),
            history: state::read_history(200),
            tab: Tab::Disk,
            disk_sel: ListSel::new(),
            proc_sel: ListSel::new(),
            filter: Filter::All,
            cat: None,
            sort: Sort::Verdict,
            search: String::new(),
            searching: false,
            modal: Modal::None,
            status,
            scanning: false,
            scan_progress: String::new(),
            scan_secs: None,
            scan_started: Instant::now(),
            procs_loading: false,
            pending_ai: HashSet::new(),
            cleaning: HashSet::new(),
            pins,
            clean_successes: 0,
            clean_failures: 0,
            clean_skipped: 0,
            freed_session: 0,
            tick: 0,
            tx,
            rx,
            quit: false,
        }
    }

    pub(crate) fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.quit {
            while let Ok(m) = self.rx.try_recv() {
                self.on_msg(m);
            }
            self.tick = self.tick.wrapping_add(1);
            terminal.draw(|f| self.draw(f))?;
            if event::poll(Duration::from_millis(120))?
                && let Event::Key(k) = event::read()?
                && k.kind == KeyEventKind::Press
            {
                self.on_key(k);
            }
        }
        Ok(())
    }

    pub(crate) fn start_scan(&mut self) {
        if self.scanning {
            return;
        }
        self.items.clear();
        self.disk_sel.marked.clear();
        self.scanning = true;
        self.scan_secs = None;
        self.scan_started = Instant::now();
        self.scan_progress = "starting".into();
        let (stx, srx) = channel();
        let tx = self.tx.clone();
        let cfg = self.cfg.clone();
        std::thread::spawn(move || scan::run_scan(cfg, stx));
        std::thread::spawn(move || {
            for m in srx {
                if tx.send(Msg::Scan(m)).is_err() {
                    break;
                }
            }
        });
    }

    pub(crate) fn refresh_procs(&mut self) {
        if self.procs_loading {
            return;
        }
        self.procs_loading = true;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let at = Utc::now();
            let _ = tx.send(Msg::Procs(Box::new(procs::snapshot()), at));
        });
    }

    fn on_msg(&mut self, m: Msg) {
        match m {
            Msg::Scan(ScanMsg::Item(item)) => {
                self.items.push(*item);
                if self.disk_sel.selected.is_none() {
                    self.disk_sel.selected =
                        self.visible().first().map(|&i| self.items[i].id.clone());
                }
            }
            Msg::Scan(ScanMsg::Progress(p)) => self.scan_progress = p,
            Msg::Scan(ScanMsg::Done { secs }) => {
                self.scanning = false;
                self.scan_secs = Some(secs);
                self.disk = crate::report::disk_info(&self.home);
            }
            Msg::Procs(snap, at) => {
                self.procs = *snap;
                self.procs_at = at;
                self.procs_loading = false;
                let live: HashSet<u32> = self.procs.dev.iter().map(|p| p.pid).collect();
                self.proc_sel.marked.retain(|p| live.contains(p));
            }
            Msg::Ai {
                key,
                provider,
                result,
            } => {
                self.pending_ai.remove(&key);
                match result {
                    Ok(text) => {
                        self.state.ai_notes.insert(
                            key,
                            AiNote {
                                provider,
                                at: Utc::now(),
                                text,
                            },
                        );
                        let _ = self.state.save();
                        self.status = "AI answer saved; it shows in the details pane".into();
                    }
                    Err(e) => self.status = format!("AI failed: {e}"),
                }
            }
            Msg::Cleaned {
                id,
                outcome,
                msg,
                freed,
            } => {
                self.cleaning.remove(&id);
                self.disk_sel.marked.remove(&id);
                match outcome {
                    CleanOutcome::Success => {
                        self.clean_successes += 1;
                        self.freed_session += freed;
                        self.items.retain(|i| i.id != id);
                        self.status = format!("freed {} · {msg}", human(freed));
                    }
                    CleanOutcome::Failure => {
                        self.clean_failures += 1;
                        self.status = format!("failed: {msg}");
                    }
                    CleanOutcome::SkippedPinned => {
                        self.clean_skipped += 1;
                        self.status = format!("skipped: pinned · {msg}");
                    }
                }
            }
            Msg::CleanDone => {
                self.history = state::read_history(200);
                self.disk = crate::report::disk_info(&self.home);
                if self.cleaning.is_empty() {
                    self.status = format!(
                        "done · {} cleaned · {} failed · {} skipped: pinned · freed {} this session",
                        self.clean_successes,
                        self.clean_failures,
                        self.clean_skipped,
                        human(self.freed_session),
                    );
                }
            }
            Msg::Catalog(cat) => {
                self.catalog = Some(*cat);
                self.catalog_loading = false;
            }
            Msg::Killed { ok, msg } => {
                self.status = if ok {
                    msg
                } else {
                    format!("kill failed: {msg}")
                };
                self.history = state::read_history(200);
                self.refresh_procs();
            }
        }
    }

    pub(crate) fn visible(&self) -> Vec<usize> {
        let q = self.search.to_lowercase();
        let mut v: Vec<usize> = (0..self.items.len())
            .filter(|&i| {
                let it = &self.items[i];
                self.filter.matches(it.effective_verdict())
                    && self.cat.is_none_or(|c| c == it.category)
                    && (q.is_empty()
                        || it.name.to_lowercase().contains(&q)
                        || it.path.to_string_lossy().to_lowercase().contains(&q))
            })
            .collect();
        let items = &self.items;
        match self.sort {
            Sort::Verdict => v.sort_by(|&a, &b| items[a].listing_order(&items[b])),
            Sort::Size => v.sort_by(|&a, &b| items[b].bytes.cmp(&items[a].bytes)),
            Sort::Age => v.sort_by(|&a, &b| items[a].last_used.cmp(&items[b].last_used)),
            Sort::Name => v.sort_by(|&a, &b| {
                items[a]
                    .name
                    .to_lowercase()
                    .cmp(&items[b].name.to_lowercase())
            }),
        }
        v
    }

    pub(crate) fn selected_index(&self, vis: &[usize]) -> Option<usize> {
        let keys: Vec<&str> = vis.iter().map(|&i| self.items[i].id.as_str()).collect();
        self.disk_sel
            .selected
            .as_deref()
            .and_then(|selected| keys.iter().position(|key| *key == selected))
    }

    pub(crate) fn selected_item(&self) -> Option<&Item> {
        let vis = self.visible();
        let pos = self
            .selected_index(&vis)
            .or(if vis.is_empty() { None } else { Some(0) })?;
        vis.get(pos).map(|&i| &self.items[i])
    }

    fn move_sel(&mut self, delta: isize) {
        match self.tab {
            Tab::Disk => {
                let vis = self.visible();
                let keys: Vec<String> = vis.iter().map(|&i| self.items[i].id.clone()).collect();
                self.disk_sel.move_by(&keys, delta);
            }
            Tab::Procs => {
                let keys: Vec<u32> = self.procs.dev.iter().map(|p| p.pid).collect();
                self.proc_sel.move_by(&keys, delta);
            }
            Tab::History => {}
        }
    }

    pub(crate) fn selected_proc(&self) -> Option<&ProcItem> {
        let list = &self.procs.dev;
        self.proc_sel
            .selected
            .and_then(|p| list.iter().find(|x| x.pid == p))
            .or_else(|| list.first())
    }

    fn on_key(&mut self, k: KeyEvent) {
        if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
            self.quit = true;
            return;
        }
        if self.searching {
            match k.code {
                KeyCode::Esc => {
                    self.search.clear();
                    self.searching = false;
                }
                KeyCode::Enter => self.searching = false,
                KeyCode::Backspace => {
                    self.search.pop();
                }
                KeyCode::Char(c) => self.search.push(c),
                _ => {}
            }
            return;
        }
        match std::mem::replace(&mut self.modal, Modal::None) {
            Modal::None => {}
            Modal::Help => return,
            Modal::AiPicker(row) => {
                self.picker_key(k, row);
                return;
            }
            Modal::ConfirmClean { ids, .. } => {
                if matches!(k.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                    self.do_clean(ids);
                } else {
                    self.status = "nothing cleaned".into();
                }
                return;
            }
            Modal::ConfirmKill(pids) => {
                if matches!(k.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                    self.do_kill(pids);
                } else {
                    self.status = "nothing killed".into();
                }
                return;
            }
        }
        self.status.clear();
        let shift = k.modifiers.contains(KeyModifiers::SHIFT);
        match k.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Esc => {
                if self.disk_sel.anchor.take().is_some() {
                    self.status = "range cancelled".into();
                }
            }
            KeyCode::Char('?') => self.modal = Modal::Help,
            KeyCode::Char('A') => self.open_picker(),
            KeyCode::Down | KeyCode::Up if shift => {
                self.mark_and_move(if k.code == KeyCode::Down { 1 } else { -1 })
            }
            KeyCode::Char('J') => self.mark_and_move(1),
            KeyCode::Char('K') => self.mark_and_move(-1),
            KeyCode::Tab => {
                self.tab = match self.tab {
                    Tab::Disk => Tab::Procs,
                    Tab::Procs => Tab::History,
                    Tab::History => Tab::Disk,
                }
            }
            KeyCode::BackTab => {
                self.tab = match self.tab {
                    Tab::Disk => Tab::History,
                    Tab::Procs => Tab::Disk,
                    Tab::History => Tab::Procs,
                }
            }
            KeyCode::Char('1') => self.tab = Tab::Disk,
            KeyCode::Char('2') => self.tab = Tab::Procs,
            KeyCode::Char('3') => self.tab = Tab::History,
            KeyCode::Down | KeyCode::Char('j') => self.move_sel(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_sel(-1),
            KeyCode::PageDown => self.move_sel(15),
            KeyCode::PageUp => self.move_sel(-15),
            KeyCode::Home | KeyCode::Char('g') => self.move_sel(-100_000),
            KeyCode::End | KeyCode::Char('G') => self.move_sel(100_000),
            KeyCode::Char('r') => match self.tab {
                Tab::Disk => self.start_scan(),
                Tab::Procs => self.refresh_procs(),
                Tab::History => self.history = state::read_history(200),
            },
            _ => match self.tab {
                Tab::Disk => self.disk_key(k),
                Tab::Procs => self.proc_key(k),
                Tab::History => {}
            },
        }
    }

    fn disk_key(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Char(' ') if self.disk_sel.anchor.is_some() => self.toggle_range(),
            KeyCode::Char(' ') => {
                if let Some(id) = self.selected_item().map(|i| i.id.clone()) {
                    self.disk_sel.toggle_mark(id);
                    self.move_sel(1);
                }
            }
            KeyCode::Char('a') => {
                let vis = self.visible();
                let ids: Vec<String> = vis
                    .iter()
                    .map(|&i| &self.items[i])
                    .filter(|i| i.effective_verdict() == Verdict::Safe && i.cleanable())
                    .map(|i| i.id.clone())
                    .collect();
                self.status = format!("marked {} SAFE items", ids.len());
                self.disk_sel.marked.extend(ids);
            }
            KeyCode::Char('*') => {
                let vis = self.visible();
                let ids: Vec<String> = vis
                    .iter()
                    .map(|&i| &self.items[i])
                    .filter(|i| i.cleanable())
                    .map(|i| i.id.clone())
                    .collect();
                self.status = format!("marked {} cleanable items in view", ids.len());
                self.disk_sel.marked.extend(ids);
            }
            KeyCode::Char('v') => self.toggle_range(),
            KeyCode::Char('u') => {
                self.disk_sel.marked.clear();
                self.disk_sel.anchor = None;
            }
            KeyCode::Char('d') => {
                let ids: Vec<String> = if self.disk_sel.marked.is_empty() {
                    self.selected_item()
                        .map(|i| vec![i.id.clone()])
                        .unwrap_or_default()
                } else {
                    self.disk_sel.marked.iter().cloned().collect()
                };
                let blocked: Vec<String> = ids
                    .iter()
                    .filter_map(|id| self.items.iter().find(|i| &i.id == id))
                    .filter(|i| !i.cleanable())
                    .map(|i| i.name.clone())
                    .collect();
                let ok: Vec<String> = ids
                    .into_iter()
                    .filter(|id| {
                        self.items
                            .iter()
                            .any(|i| &i.id == id && i.cleanable() && !self.cleaning.contains(id))
                    })
                    .collect();
                if ok.is_empty() {
                    self.status = if blocked.is_empty() {
                        "nothing to clean".into()
                    } else {
                        format!(
                            "can't clean {} (KEEP, pinned, or no action)",
                            blocked.join(", ")
                        )
                    };
                } else {
                    self.modal = Modal::ConfirmClean {
                        ids: ok,
                        skipped: blocked,
                    };
                }
            }
            KeyCode::Char('x') => {
                if let Some(item) = self.selected_item().cloned() {
                    self.ask_item(&item);
                }
            }
            KeyCode::Char('X') => {
                let items: Vec<Item> = self
                    .items
                    .iter()
                    .filter(|i| self.disk_sel.marked.contains(&i.id))
                    .cloned()
                    .collect();
                for i in &items {
                    self.ask_item(i);
                }
            }
            KeyCode::Char('p') => self.toggle_pin(),
            KeyCode::Char('o') => {
                if let Some(i) = self.selected_item() {
                    let target = ai::workdir_for(&i.path, &self.home);
                    let _ = Command::new("open").arg("-R").arg(&target).spawn();
                    self.status = format!("revealed {}", tilde(&target, &self.home));
                }
            }
            KeyCode::Char('y') => {
                if let Some(i) = self.selected_item() {
                    let path = i.path.display().to_string();
                    self.status = if copy(&path) {
                        format!("copied {path}")
                    } else {
                        "pbcopy failed".into()
                    };
                }
            }
            KeyCode::Char('f') => self.filter = self.filter.next(),
            KeyCode::Char('c') => {
                self.cat = match self.cat {
                    None => Some(Category::ALL[0]),
                    Some(c) => {
                        let idx = Category::ALL.iter().position(|x| *x == c).unwrap_or(0);
                        Category::ALL.get(idx + 1).copied()
                    }
                }
            }
            KeyCode::Char('s') => self.sort = self.sort.next(),
            KeyCode::Char('/') => {
                self.searching = true;
                self.search.clear();
            }
            _ => {}
        }
    }

    fn mark_and_move(&mut self, delta: isize) {
        match self.tab {
            Tab::Disk => {
                if let Some(id) = self.selected_item().map(|i| i.id.clone()) {
                    self.disk_sel.marked.insert(id);
                }
            }
            Tab::Procs => {
                if let Some(pid) = self.selected_proc().map(|p| p.pid) {
                    self.proc_sel.marked.insert(pid);
                }
            }
            Tab::History => return,
        }
        self.move_sel(delta);
    }

    pub(crate) fn range_ids(&self) -> Vec<String> {
        let Some(anchor) = &self.disk_sel.anchor else {
            return Vec::new();
        };
        let vis = self.visible();
        let Some(a) = vis.iter().position(|&i| &self.items[i].id == anchor) else {
            return Vec::new();
        };
        let b = self.selected_index(&vis).unwrap_or(0);
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        vis[lo..=hi]
            .iter()
            .map(|&i| self.items[i].id.clone())
            .collect()
    }

    fn toggle_range(&mut self) {
        if self.disk_sel.anchor.is_none() {
            self.disk_sel.anchor = self.selected_item().map(|i| i.id.clone());
            self.status =
                "range started: move, then v or space marks every row in between (esc cancels)"
                    .into();
            return;
        }
        let ids = self.range_ids();
        self.status = format!("marked {} rows", ids.len());
        self.disk_sel.marked.extend(ids);
        self.disk_sel.anchor = None;
    }

    fn proc_key(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Char('*') => {
                let pids: Vec<u32> = self.procs.dev.iter().map(|p| p.pid).collect();
                self.status = format!("marked {} processes", pids.len());
                self.proc_sel.marked.extend(pids);
            }
            KeyCode::Char(' ') => {
                if let Some(pid) = self.selected_proc().map(|p| p.pid) {
                    self.proc_sel.toggle_mark(pid);
                    self.move_sel(1);
                }
            }
            KeyCode::Char('u') => {
                self.proc_sel.marked.clear();
            }
            KeyCode::Char('d') => {
                let pids: Vec<(u32, String)> = if self.proc_sel.marked.is_empty() {
                    self.selected_proc()
                        .map(|p| vec![(p.pid, p.name.clone())])
                        .unwrap_or_default()
                } else {
                    self.procs
                        .dev
                        .iter()
                        .filter(|p| self.proc_sel.marked.contains(&p.pid))
                        .map(|p| (p.pid, p.name.clone()))
                        .collect()
                };
                if !pids.is_empty() {
                    self.modal = Modal::ConfirmKill(pids);
                }
            }
            KeyCode::Char('x') => {
                if let Some(p) = self.selected_proc().cloned() {
                    let key = p.id();
                    if self.pending_ai.insert(key.clone()) {
                        let prompt = ai::proc_prompt(&p);
                        let dir = p
                            .cwd
                            .clone()
                            .filter(|c| c.is_dir())
                            .unwrap_or_else(|| self.home.clone());
                        self.spawn_ai(key, prompt, dir, &p.name);
                    }
                }
            }
            _ => {}
        }
    }

    fn ask_item(&mut self, item: &Item) {
        if !self.pending_ai.insert(item.id.clone()) {
            return;
        }
        let prompt = ai::item_prompt(item, &self.home);
        let dir = ai::workdir_for(&item.path, &self.home);
        self.spawn_ai(item.id.clone(), prompt, dir, &item.name);
    }

    fn spawn_ai(&mut self, key: String, prompt: String, dir: PathBuf, name: &str) {
        let cfg = self.cfg.ai.clone();
        let tx = self.tx.clone();
        self.status = format!("asking {} about {name}… (keep browsing)", cfg.provider);
        std::thread::spawn(move || {
            let provider = cfg.provider.to_string();
            let result = ai::ask(&cfg, &prompt, &dir).map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Ai {
                key,
                provider,
                result,
            });
        });
    }

    fn toggle_pin(&mut self) {
        let Some(id) = self.selected_item().map(|i| i.id.clone()) else {
            return;
        };
        let pinned = self.state.toggle_pin(&id);
        if let Ok(mut pins) = self.pins.lock() {
            *pins = self.state.pins.clone();
        }
        let save_error = if self.persist {
            self.state.save().err()
        } else {
            None
        };
        let protector = self.protector();
        if let Some(item) = self.items.iter_mut().find(|i| i.id == id) {
            let protected = protector.is_protected(item);
            item.set_protected(protected);
            self.status = match save_error {
                Some(e) => format!("pin changed for this run, but could not save it: {e:#}"),
                None => match (pinned, item.protected) {
                    (true, _) => format!("pinned {}: Dustpan will never clean it", item.name),
                    (false, true) => format!(
                        "unpinned {}, but a protect glob in the config still covers it",
                        item.name
                    ),
                    (false, false) => format!("unpinned {}", item.name),
                },
            };
        }
        self.disk_sel.marked.remove(&id);
    }

    fn protector(&self) -> Protector {
        Protector::new(&self.cfg.protect, self.state.pins.clone(), &self.home)
            .unwrap_or_else(|_| Protector::everything())
    }

    fn currently_pinned(pins: &Arc<Mutex<BTreeSet<String>>>, id: &str) -> bool {
        pins.lock().map_or(true, |pins| pins.contains(id))
    }

    fn clean_item(
        item: Item,
        home: &std::path::Path,
        roots: &[PathBuf],
        protector: &Protector,
        pins: &Arc<Mutex<BTreeSet<String>>>,
    ) -> Msg {
        if Self::currently_pinned(pins, &item.id) {
            return Msg::Cleaned {
                id: item.id,
                outcome: CleanOutcome::SkippedPinned,
                msg: item.name,
                freed: 0,
            };
        }
        let result = actions::clean(&item, home, roots, protector);
        Msg::Cleaned {
            id: item.id,
            outcome: if result.is_ok() {
                CleanOutcome::Success
            } else {
                CleanOutcome::Failure
            },
            msg: match result {
                Ok(message) => format!("{}: {message}", item.name),
                Err(error) => format!("{}: {error:#}", item.name),
            },
            freed: item.reclaimable,
        }
    }

    fn do_clean(&mut self, ids: Vec<String>) {
        let items: Vec<Item> = ids
            .iter()
            .filter_map(|id| self.items.iter().find(|i| &i.id == id).cloned())
            .collect();
        for i in &items {
            self.cleaning.insert(i.id.clone());
        }
        self.clean_successes = 0;
        self.clean_failures = 0;
        self.clean_skipped = 0;
        self.status = format!("cleaning {} item(s)…", items.len());
        let tx = self.tx.clone();
        let home = self.home.clone();
        let roots = self.roots.clone();
        let protector = Protector::new(&self.cfg.protect, BTreeSet::new(), &home)
            .unwrap_or_else(|_| Protector::everything());
        let pins = Arc::clone(&self.pins);
        std::thread::spawn(move || {
            for item in items {
                let _ = tx.send(App::clean_item(item, &home, &roots, &protector, &pins));
            }
            let _ = tx.send(Msg::CleanDone);
        });
    }

    fn do_kill(&mut self, pids: Vec<(u32, String)>) {
        let tx = self.tx.clone();
        self.proc_sel.marked.clear();
        let mut failures = Vec::new();
        let mut targets = Vec::new();
        for (pid, name) in &pids {
            match self.procs.dev.iter().find(|p| p.pid == *pid) {
                Some(p) => targets.push(actions::KillTarget::from_proc(p, self.procs_at)),
                None => failures.push(format!(
                    "{name} (pid {pid}) is no longer listed; refresh and try again"
                )),
            }
        }
        std::thread::spawn(move || {
            for t in &targets {
                if let Err(e) = actions::kill(t) {
                    failures.push(e.to_string());
                }
            }
            std::thread::sleep(Duration::from_millis(800));
            let _ = tx.send(Msg::Killed {
                ok: failures.is_empty(),
                msg: if failures.is_empty() {
                    format!("sent SIGTERM to {} process(es)", pids.len())
                } else {
                    failures.join("; ")
                },
            });
        });
    }
}

fn copy(text: &str) -> bool {
    let Ok(mut child) = Command::new("pbcopy").stdin(Stdio::piped()).spawn() else {
        return false;
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes());
    }
    child.wait().is_ok_and(|s| s.success())
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::model::Action;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    pub(crate) fn app_with_items() -> App {
        let mut app = App::new(Config::default());
        app.state = State::default();
        app.persist = false;
        app.cfg.ai = crate::config::AiConfig::default();
        let mut a = Item::new(
            Category::DerivedData,
            "MyApp · ~/gone/ios/MyApp.xcworkspace",
            "/Users/me/Library/Developer/Xcode/DerivedData/MyApp-a",
        );
        a.verdict = Verdict::Safe;
        a.bytes = 20 << 30;
        a.reclaimable = 20 << 30;
        a.reasons = vec!["project ~/gone no longer exists".into()];
        a.action = Action::delete("/Users/me/Library/Developer/Xcode/DerivedData/MyApp-a");
        let mut b = Item::new(
            Category::Worktree,
            "MyApp · feature/x",
            "/Users/me/Work/MyApp-wt/x",
        );
        b.verdict = Verdict::Keep;
        b.bytes = 3 << 30;
        b.reasons = vec!["2 uncommitted change(s)".into()];
        b.action = Action::run(
            "git",
            &["worktree", "remove", "/Users/me/Work/MyApp-wt/x"],
            None,
        );
        app.items = vec![a, b];
        app
    }

    pub(crate) fn render(app: &mut App, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let buf = term.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    pub(crate) fn key(app: &mut App, c: KeyCode) {
        app.on_key(KeyEvent::new(c, KeyModifiers::NONE));
    }

    pub(crate) fn safe_item(n: usize) -> Item {
        let long =
            format!("/Users/me/.codex/.tmp/marketplaces/.staging/marketplace-upgrade-{n:0>40}");
        let mut i = Item::new(
            Category::Leftover,
            format!("leftover {n} with a fairly long descriptive name"),
            &long,
        );
        i.verdict = Verdict::Safe;
        i.bytes = 1 << 30;
        i.reclaimable = 1 << 30;
        i.action = Action::delete_all(
            (0..600)
                .map(|k| PathBuf::from(format!("{long}/{k}")))
                .collect(),
        );
        i
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use ratatui::crossterm::event::KeyCode;

    #[test]
    fn pin_save_failure_is_reported_without_success_claim() {
        let mut app = app_with_items();
        app.persist = true;
        app.state.read_only = true;
        app.state.warning = Some("test state is read only".into());
        key(&mut app, KeyCode::Char('p'));
        assert!(app.status.contains("could not save"));
        assert!(!app.status.contains("will never clean"));
    }

    #[test]
    fn running_clean_observes_new_pins() {
        let app = app_with_items();
        let pins = Arc::new(Mutex::new(BTreeSet::new()));
        assert!(!App::currently_pinned(&pins, "item"));
        pins.lock().unwrap().insert("item".into());
        assert!(App::currently_pinned(&pins, "item"));
        let mut item = safe_item(999);
        item.id = "item".into();
        let protector = Protector::new(&[], BTreeSet::new(), &app.home).unwrap();
        assert!(matches!(
            App::clean_item(item, &app.home, &app.roots, &protector, &pins),
            Msg::Cleaned { outcome: CleanOutcome::SkippedPinned, msg, .. } if msg.contains("leftover")
        ));
    }

    #[test]
    fn clean_done_keeps_explicit_failure_count() {
        let mut app = app_with_items();
        app.cleaning.extend(["failed".into(), "ok".into()]);
        app.on_msg(Msg::Cleaned {
            id: "failed".into(),
            outcome: CleanOutcome::Failure,
            msg: "failed first".into(),
            freed: 0,
        });
        app.on_msg(Msg::Cleaned {
            id: "ok".into(),
            outcome: CleanOutcome::Success,
            msg: "ok last".into(),
            freed: 10,
        });
        app.on_msg(Msg::CleanDone);
        assert!(app.status.contains("1 cleaned · 1 failed"));
    }

    #[test]
    fn keep_items_cannot_be_queued_for_cleaning() {
        let mut app = app_with_items();
        app.disk_sel.selected = Some(app.items[1].id.clone());
        key(&mut app, KeyCode::Char('d'));
        assert!(matches!(app.modal, Modal::None));
        assert!(app.status.contains("can't clean"));
    }

    #[test]
    fn mark_safe_then_confirm_modal_then_cancel() {
        let mut app = app_with_items();
        key(&mut app, KeyCode::Char('a'));
        assert_eq!(app.disk_sel.marked.len(), 1);
        key(&mut app, KeyCode::Char('d'));
        assert!(matches!(&app.modal, Modal::ConfirmClean { ids, .. } if ids.len() == 1));
        let screen = render(&mut app, 180, 45);
        assert!(screen.contains("Confirm cleanup"));
        assert!(screen.contains("free about 20.0G"));
        key(&mut app, KeyCode::Char('n'));
        assert!(matches!(app.modal, Modal::None));
        assert_eq!(app.items.len(), 2);
        assert!(app.cleaning.is_empty());
    }

    #[test]
    fn range_mark_with_v() {
        let mut app = app_with_items();
        app.items.extend((0..5).map(safe_item));
        key(&mut app, KeyCode::Char('g'));
        key(&mut app, KeyCode::Char('v'));
        for _ in 0..3 {
            key(&mut app, KeyCode::Char('j'));
        }
        key(&mut app, KeyCode::Char('v'));
        assert_eq!(app.disk_sel.marked.len(), 4);
        assert!(app.disk_sel.anchor.is_none());
    }

    #[test]
    fn shift_j_marks_while_moving_and_star_marks_all_cleanable() {
        let mut app = app_with_items();
        app.items.extend((0..3).map(safe_item));
        key(&mut app, KeyCode::Char('g'));
        key(&mut app, KeyCode::Char('J'));
        key(&mut app, KeyCode::Char('J'));
        assert_eq!(app.disk_sel.marked.len(), 2);
        key(&mut app, KeyCode::Char('u'));
        key(&mut app, KeyCode::Char('*'));
        assert_eq!(
            app.disk_sel.marked.len(),
            4,
            "KEEP worktree must not be marked"
        );
    }

    #[test]
    fn esc_does_not_quit() {
        let mut app = app_with_items();
        key(&mut app, KeyCode::Esc);
        assert!(!app.quit);
        key(&mut app, KeyCode::Char('q'));
        assert!(app.quit);
    }

    #[test]
    fn filter_and_search_narrow_the_list() {
        let mut app = app_with_items();
        key(&mut app, KeyCode::Char('f'));
        assert_eq!(app.visible().len(), 1);
        key(&mut app, KeyCode::Char('f'));
        assert_eq!(app.visible().len(), 0);
        app.filter = Filter::All;
        key(&mut app, KeyCode::Char('/'));
        for c in "feature".chars() {
            key(&mut app, KeyCode::Char(c));
        }
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.visible().len(), 1);
        assert_eq!(app.selected_item().unwrap().category, Category::Worktree);
    }
}
