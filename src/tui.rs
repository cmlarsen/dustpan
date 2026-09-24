use crate::actions;
use crate::ai;
use crate::config::{resolve_roots, Config, Protector};
use crate::model::{Category, Item, Verdict};
use crate::procs::{self, fmt_uptime, ProcItem, ProcSnapshot};
use crate::report::{disk_info, totals, DiskInfo};
use crate::scan::{self, ScanMsg};
use crate::state::{self, AiNote, HistoryEntry, State};
use crate::util::{ago, home, human, tilde};
use anyhow::Result;
use chrono::Utc;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Cell, Clear, Paragraph, Row, Table, TableState, Tabs, Wrap};
use ratatui::{DefaultTerminal, Frame};
use std::collections::HashSet;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

const PROTECTED_REASON: &str = "protected by you (pin or config)";
const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

enum Msg {
    Scan(ScanMsg),
    Procs(Box<ProcSnapshot>),
    Ai {
        key: String,
        provider: String,
        result: Result<String, String>,
    },
    Cleaned {
        id: String,
        ok: bool,
        msg: String,
        freed: u64,
    },
    CleanDone,
    Killed {
        ok: bool,
        msg: String,
    },
}

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Disk,
    Procs,
    History,
}

#[derive(Clone, Copy, PartialEq)]
enum Filter {
    All,
    Safe,
    Review,
    InUse,
    Keep,
}

impl Filter {
    fn next(self) -> Self {
        match self {
            Filter::All => Filter::Safe,
            Filter::Safe => Filter::Review,
            Filter::Review => Filter::InUse,
            Filter::InUse => Filter::Keep,
            Filter::Keep => Filter::All,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Filter::All => "all",
            Filter::Safe => "safe",
            Filter::Review => "review",
            Filter::InUse => "in use",
            Filter::Keep => "keep",
        }
    }
    fn matches(self, v: Verdict) -> bool {
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
enum Sort {
    Verdict,
    Size,
    Age,
    Name,
}

impl Sort {
    fn next(self) -> Self {
        match self {
            Sort::Verdict => Sort::Size,
            Sort::Size => Sort::Age,
            Sort::Age => Sort::Name,
            Sort::Name => Sort::Verdict,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Sort::Verdict => "verdict",
            Sort::Size => "size",
            Sort::Age => "oldest",
            Sort::Name => "name",
        }
    }
}

enum Modal {
    None,
    Help,
    ConfirmClean(Vec<String>),
    ConfirmKill(Vec<(u32, String)>),
}

struct App {
    cfg: Config,
    home: PathBuf,
    roots: Vec<PathBuf>,
    state: State,
    items: Vec<Item>,
    procs: ProcSnapshot,
    history: Vec<HistoryEntry>,
    disk: Option<DiskInfo>,
    tab: Tab,
    sel_id: Option<String>,
    sel_pid: Option<u32>,
    marked: HashSet<String>,
    marked_pids: HashSet<u32>,
    filter: Filter,
    cat: Option<Category>,
    sort: Sort,
    search: String,
    searching: bool,
    modal: Modal,
    status: String,
    scanning: bool,
    scan_progress: String,
    scan_secs: Option<f64>,
    scan_started: Instant,
    procs_loading: bool,
    pending_ai: HashSet<String>,
    cleaning: HashSet<String>,
    freed_session: u64,
    tick: usize,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    quit: bool,
}

pub fn run(cfg: Config) -> Result<()> {
    let mut app = App::new(cfg);
    app.start_scan();
    app.refresh_procs();
    let mut terminal = ratatui::init();
    let result = app.run(&mut terminal);
    ratatui::restore();
    result
}

impl App {
    fn new(cfg: Config) -> Self {
        let (tx, rx) = channel();
        let home = home();
        let roots = resolve_roots(&cfg, &home);
        App {
            disk: disk_info(&home),
            home,
            roots,
            cfg,
            state: State::load(),
            items: Vec::new(),
            procs: ProcSnapshot::default(),
            history: state::read_history(200),
            tab: Tab::Disk,
            sel_id: None,
            sel_pid: None,
            marked: HashSet::new(),
            marked_pids: HashSet::new(),
            filter: Filter::All,
            cat: None,
            sort: Sort::Verdict,
            search: String::new(),
            searching: false,
            modal: Modal::None,
            status: String::new(),
            scanning: false,
            scan_progress: String::new(),
            scan_secs: None,
            scan_started: Instant::now(),
            procs_loading: false,
            pending_ai: HashSet::new(),
            cleaning: HashSet::new(),
            freed_session: 0,
            tick: 0,
            tx,
            rx,
            quit: false,
        }
    }

    fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.quit {
            while let Ok(m) = self.rx.try_recv() {
                self.on_msg(m);
            }
            self.tick = self.tick.wrapping_add(1);
            terminal.draw(|f| self.draw(f))?;
            if event::poll(Duration::from_millis(120))?
                && let Event::Key(k) = event::read()?
                    && k.kind == KeyEventKind::Press {
                        self.on_key(k);
                    }
        }
        Ok(())
    }

    fn start_scan(&mut self) {
        if self.scanning {
            return;
        }
        self.items.clear();
        self.marked.clear();
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

    fn refresh_procs(&mut self) {
        if self.procs_loading {
            return;
        }
        self.procs_loading = true;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Procs(Box::new(procs::snapshot())));
        });
    }

    fn on_msg(&mut self, m: Msg) {
        match m {
            Msg::Scan(ScanMsg::Item(item)) => {
                self.items.push(*item);
                if self.sel_id.is_none() {
                    self.sel_id = self.visible().first().map(|&i| self.items[i].id.clone());
                }
            }
            Msg::Scan(ScanMsg::Progress(p)) => self.scan_progress = p,
            Msg::Scan(ScanMsg::Done { secs }) => {
                self.scanning = false;
                self.scan_secs = Some(secs);
                self.disk = disk_info(&self.home);
            }
            Msg::Procs(snap) => {
                self.procs = *snap;
                self.procs_loading = false;
                let live: HashSet<u32> = self.procs.dev.iter().map(|p| p.pid).collect();
                self.marked_pids.retain(|p| live.contains(p));
            }
            Msg::Ai { key, provider, result } => {
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
            Msg::Cleaned { id, ok, msg, freed } => {
                self.cleaning.remove(&id);
                self.marked.remove(&id);
                if ok {
                    self.freed_session += freed;
                    self.items.retain(|i| i.id != id);
                    self.status = format!("freed {} · {msg}", human(freed));
                } else {
                    self.status = format!("failed: {msg}");
                }
            }
            Msg::CleanDone => {
                self.history = state::read_history(200);
                self.disk = disk_info(&self.home);
                if self.cleaning.is_empty() && !self.status.starts_with("failed") {
                    self.status = format!(
                        "done · freed {} this session · press r to rescan for new leftovers",
                        human(self.freed_session)
                    );
                }
            }
            Msg::Killed { ok, msg } => {
                self.status = if ok { msg } else { format!("kill failed: {msg}") };
                self.history = state::read_history(200);
                self.refresh_procs();
            }
        }
    }

    fn visible(&self) -> Vec<usize> {
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
            Sort::Verdict => v.sort_by(|&a, &b| {
                let (a, b) = (&items[a], &items[b]);
                a.effective_verdict()
                    .cmp(&b.effective_verdict())
                    .then(b.reclaimable.max(b.bytes / 8).cmp(&a.reclaimable.max(a.bytes / 8)))
            }),
            Sort::Size => v.sort_by(|&a, &b| items[b].bytes.cmp(&items[a].bytes)),
            Sort::Age => v.sort_by(|&a, &b| items[a].last_used.cmp(&items[b].last_used)),
            Sort::Name => v.sort_by(|&a, &b| items[a].name.to_lowercase().cmp(&items[b].name.to_lowercase())),
        }
        v
    }

    fn selected_index(&self, vis: &[usize]) -> Option<usize> {
        let id = self.sel_id.as_ref()?;
        vis.iter().position(|&i| &self.items[i].id == id)
    }

    fn selected_item(&self) -> Option<&Item> {
        let vis = self.visible();
        let pos = self.selected_index(&vis).or(if vis.is_empty() { None } else { Some(0) })?;
        vis.get(pos).map(|&i| &self.items[i])
    }

    fn move_sel(&mut self, delta: isize) {
        match self.tab {
            Tab::Disk => {
                let vis = self.visible();
                if vis.is_empty() {
                    return;
                }
                let cur = self.selected_index(&vis).unwrap_or(0) as isize;
                let next = (cur + delta).clamp(0, vis.len() as isize - 1) as usize;
                self.sel_id = Some(self.items[vis[next]].id.clone());
            }
            Tab::Procs => {
                let list = &self.procs.dev;
                if list.is_empty() {
                    return;
                }
                let cur = self
                    .sel_pid
                    .and_then(|p| list.iter().position(|x| x.pid == p))
                    .unwrap_or(0) as isize;
                let next = (cur + delta).clamp(0, list.len() as isize - 1) as usize;
                self.sel_pid = Some(list[next].pid);
            }
            Tab::History => {}
        }
    }

    fn selected_proc(&self) -> Option<&ProcItem> {
        let list = &self.procs.dev;
        self.sel_pid
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
            Modal::ConfirmClean(ids) => {
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
        match k.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('?') => self.modal = Modal::Help,
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
            KeyCode::Char(' ') => {
                if let Some(id) = self.selected_item().map(|i| i.id.clone()) {
                    if !self.marked.remove(&id) {
                        self.marked.insert(id);
                    }
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
                self.marked.extend(ids);
            }
            KeyCode::Char('u') => self.marked.clear(),
            KeyCode::Char('d') => {
                let ids: Vec<String> = if self.marked.is_empty() {
                    self.selected_item().map(|i| vec![i.id.clone()]).unwrap_or_default()
                } else {
                    self.marked.iter().cloned().collect()
                };
                let blocked: Vec<String> = ids
                    .iter()
                    .filter_map(|id| self.items.iter().find(|i| &i.id == id))
                    .filter(|i| !i.cleanable())
                    .map(|i| i.name.clone())
                    .collect();
                let ok: Vec<String> = ids
                    .into_iter()
                    .filter(|id| self.items.iter().any(|i| &i.id == id && i.cleanable() && !self.cleaning.contains(id)))
                    .collect();
                if ok.is_empty() {
                    self.status = if blocked.is_empty() {
                        "nothing to clean".into()
                    } else {
                        format!("can't clean {} (KEEP, pinned, or no action)", blocked.join(", "))
                    };
                } else {
                    self.modal = Modal::ConfirmClean(ok);
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
                    .filter(|i| self.marked.contains(&i.id))
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
                    self.status = if copy(&path) { format!("copied {path}") } else { "pbcopy failed".into() };
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

    fn proc_key(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Char(' ') => {
                if let Some(pid) = self.selected_proc().map(|p| p.pid) {
                    if !self.marked_pids.remove(&pid) {
                        self.marked_pids.insert(pid);
                    }
                    self.move_sel(1);
                }
            }
            KeyCode::Char('u') => self.marked_pids.clear(),
            KeyCode::Char('d') | KeyCode::Char('K') => {
                let pids: Vec<(u32, String)> = if self.marked_pids.is_empty() {
                    self.selected_proc().map(|p| vec![(p.pid, p.name.clone())]).unwrap_or_default()
                } else {
                    self.procs
                        .dev
                        .iter()
                        .filter(|p| self.marked_pids.contains(&p.pid))
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
                        let dir = p.cwd.clone().filter(|c| c.is_dir()).unwrap_or_else(|| self.home.clone());
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
            let provider = cfg.provider.clone();
            let result = ai::ask(&cfg, &prompt, &dir).map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Ai { key, provider, result });
        });
    }

    fn toggle_pin(&mut self) {
        let Some(id) = self.selected_item().map(|i| i.id.clone()) else { return };
        let pinned = self.state.toggle_pin(&id);
        let _ = self.state.save();
        let protector = Protector::new(&self.cfg.protect, self.state.pins.clone(), &self.home);
        if let Some(item) = self.items.iter_mut().find(|i| i.id == id) {
            item.protected = protector.is_protected(item);
            item.reasons.retain(|r| r != PROTECTED_REASON);
            if item.protected {
                item.reasons.insert(0, PROTECTED_REASON.into());
            }
            self.status = match (pinned, item.protected) {
                (true, _) => format!("pinned {}: Dustpan will never clean it", item.name),
                (false, true) => format!("unpinned {}, but a protect glob in the config still covers it", item.name),
                (false, false) => format!("unpinned {}", item.name),
            };
        }
        self.marked.remove(&id);
    }

    fn do_clean(&mut self, ids: Vec<String>) {
        let items: Vec<Item> = ids
            .iter()
            .filter_map(|id| self.items.iter().find(|i| &i.id == id).cloned())
            .collect();
        for i in &items {
            self.cleaning.insert(i.id.clone());
        }
        self.status = format!("cleaning {} item(s)…", items.len());
        let tx = self.tx.clone();
        let home = self.home.clone();
        let roots = self.roots.clone();
        std::thread::spawn(move || {
            for item in items {
                let r = actions::clean(&item, &home, &roots);
                let _ = tx.send(Msg::Cleaned {
                    id: item.id.clone(),
                    ok: r.is_ok(),
                    msg: match r {
                        Ok(m) => format!("{}: {m}", item.name),
                        Err(e) => format!("{}: {e:#}", item.name),
                    },
                    freed: item.reclaimable,
                });
            }
            let _ = tx.send(Msg::CleanDone);
        });
    }

    fn do_kill(&mut self, pids: Vec<(u32, String)>) {
        let tx = self.tx.clone();
        self.marked_pids.clear();
        std::thread::spawn(move || {
            let mut failures = Vec::new();
            for (pid, name) in &pids {
                if let Err(e) = actions::kill(*pid, name) {
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

    fn draw(&self, f: &mut Frame) {
        let [header, tabs, body, footer] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Min(6),
            Constraint::Length(1),
        ])
        .areas(f.area());
        self.draw_header(f, header);
        let titles = ["1 Disk", "2 Processes", "3 History"];
        let idx = match self.tab {
            Tab::Disk => 0,
            Tab::Procs => 1,
            Tab::History => 2,
        };
        f.render_widget(
            Tabs::new(titles)
                .select(idx)
                .highlight_style(Style::new().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)),
            tabs,
        );
        match self.tab {
            Tab::Disk => self.draw_disk(f, body),
            Tab::Procs => self.draw_procs(f, body),
            Tab::History => self.draw_history(f, body),
        }
        self.draw_footer(f, footer);
        match &self.modal {
            Modal::None => {}
            Modal::Help => draw_help(f),
            Modal::ConfirmClean(ids) => self.draw_confirm_clean(f, ids),
            Modal::ConfirmKill(pids) => self.draw_confirm_kill(f, pids),
        }
    }

    fn draw_header(&self, f: &mut Frame, area: Rect) {
        let mut l1 = vec![Span::styled(" Dustpan ", Style::new().fg(Color::Black).bg(Color::Yellow).add_modifier(Modifier::BOLD))];
        if let Some(d) = &self.disk {
            let used = d.total - d.free;
            let pct = used as f64 / d.total.max(1) as f64 * 100.0;
            l1.push(Span::raw(format!(
                "  disk {} / {} ({:.0}%) · {} free",
                human(used),
                human(d.total),
                pct,
                human(d.free)
            )));
        }
        let m = &self.procs.mem;
        if m.total > 0 {
            l1.push(Span::raw(format!(
                "   mem {} / {} (apps {} · wired {} · compressed {}) · swap {}",
                human(m.used()),
                human(m.total),
                human(m.app),
                human(m.wired),
                human(m.compressed),
                human(m.swap_used)
            )));
        }
        let t = totals(&self.items);
        let mut l2 = vec![Span::raw(" ")];
        for v in [Verdict::Safe, Verdict::Review, Verdict::Active, Verdict::Keep] {
            let (b, n) = t.get(&v).copied().unwrap_or((0, 0));
            l2.push(Span::styled(format!("{} ", v.label()), verdict_style(v).add_modifier(Modifier::BOLD)));
            l2.push(Span::raw(format!("{} ({n})   ", human(b))));
        }
        if self.scanning {
            l2.push(Span::styled(
                format!(
                    "{} {} ({}s)",
                    SPINNER[self.tick % SPINNER.len()],
                    self.scan_progress,
                    self.scan_started.elapsed().as_secs()
                ),
                Style::new().fg(Color::Cyan),
            ));
        } else if let Some(s) = self.scan_secs {
            l2.push(Span::styled(format!("scanned in {s:.0}s"), Style::new().fg(Color::DarkGray)));
        }
        if self.freed_session > 0 {
            l2.push(Span::styled(
                format!("   freed {} this session", human(self.freed_session)),
                Style::new().fg(Color::Green),
            ));
        }
        f.render_widget(Paragraph::new(vec![Line::from(l1), Line::from(l2)]), area);
    }

    fn draw_disk(&self, f: &mut Frame, area: Rect) {
        let (list_area, detail_area) = if area.width >= 140 {
            let [a, b] = Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)]).areas(area);
            (a, b)
        } else {
            let [a, b] = Layout::vertical([Constraint::Min(6), Constraint::Length(14)]).areas(area);
            (a, b)
        };
        let vis = self.visible();
        let now = Utc::now();
        let rows: Vec<Row> = vis
            .iter()
            .map(|&i| {
                let it = &self.items[i];
                let v = it.effective_verdict();
                let mark = if self.cleaning.contains(&it.id) {
                    SPINNER[self.tick % SPINNER.len()]
                } else if self.marked.contains(&it.id) {
                    "●"
                } else {
                    " "
                };
                let mut name = it.name.clone();
                if self.pending_ai.contains(&it.id) {
                    name.push_str("  (asking AI…)");
                } else if self.state.ai_notes.contains_key(&it.id) {
                    name.push_str("  ✦");
                }
                Row::new(vec![
                    Cell::from(mark).style(Style::new().fg(Color::Yellow)),
                    Cell::from(if it.protected { "PINNED" } else { v.label() }).style(verdict_style(v)),
                    Cell::from(human(it.bytes)),
                    Cell::from(human(it.reclaimable)).style(Style::new().fg(Color::DarkGray)),
                    Cell::from(ago(it.last_used, now)),
                    Cell::from(it.category.label()).style(Style::new().fg(Color::Blue)),
                    Cell::from(name),
                ])
            })
            .collect();
        let mut title = format!(
            " {} items · filter {} · sort {}",
            vis.len(),
            self.filter.label(),
            self.sort.label()
        );
        if let Some(c) = self.cat {
            title += &format!(" · {}", c.label());
        }
        if !self.search.is_empty() {
            title += &format!(" · \"{}\"", self.search);
        }
        if !self.marked.is_empty() {
            let bytes: u64 = self
                .items
                .iter()
                .filter(|i| self.marked.contains(&i.id))
                .map(|i| i.reclaimable)
                .sum();
            title += &format!(" · {} marked ({})", self.marked.len(), human(bytes));
        }
        title.push(' ');
        let table = Table::new(
            rows,
            [
                Constraint::Length(1),
                Constraint::Length(6),
                Constraint::Length(6),
                Constraint::Length(6),
                Constraint::Length(4),
                Constraint::Length(14),
                Constraint::Fill(1),
            ],
        )
        .header(
            Row::new(["", "STATE", "SIZE", "FREES", "AGE", "KIND", "NAME"])
                .style(Style::new().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)),
        )
        .row_highlight_style(Style::new().bg(Color::Rgb(50, 50, 70)).add_modifier(Modifier::BOLD))
        .block(Block::bordered().title(title));
        let mut ts = TableState::default();
        ts.select(self.selected_index(&vis).or(if vis.is_empty() { None } else { Some(0) }));
        f.render_stateful_widget(table, list_area, &mut ts);

        let detail = match self.selected_item() {
            Some(item) => self.item_detail(item),
            None if self.scanning => Text::from("scanning…"),
            None => Text::from("nothing matches this filter"),
        };
        f.render_widget(
            Paragraph::new(detail)
                .wrap(Wrap { trim: false })
                .block(Block::bordered().title(" Details ")),
            detail_area,
        );
    }

    fn item_detail(&self, item: &Item) -> Text<'static> {
        let now = Utc::now();
        let dim = Style::new().fg(Color::DarkGray);
        let v = item.effective_verdict();
        let mut lines = vec![
            Line::from(Span::styled(item.name.clone(), Style::new().add_modifier(Modifier::BOLD))),
            Line::from(Span::styled(tilde(&item.path, &self.home), dim)),
            Line::from(""),
            Line::from(vec![
                Span::styled(if item.protected { "PINNED" } else { v.label() }, verdict_style(v).add_modifier(Modifier::BOLD)),
                Span::raw(format!(
                    "   {} on disk · frees {} · last activity {}",
                    human(item.bytes),
                    human(item.reclaimable),
                    item.last_used
                        .map(|t| format!("{} ago ({})", ago(Some(t), now), t.format("%b %-d")))
                        .unwrap_or_else(|| "unknown".into())
                )),
            ]),
        ];
        for r in &item.reasons {
            lines.push(Line::from(format!("  • {r}")));
        }
        if let Some(o) = &item.owner {
            lines.push(Line::from(Span::styled(format!("  belongs to {}", tilde(o, &self.home)), dim)));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("clean: ", Style::new().add_modifier(Modifier::BOLD)),
            Span::styled(item.action.describe(), Style::new().fg(Color::Cyan)),
        ]));
        lines.push(Line::from(""));
        self.push_ai(&mut lines, &item.id);
        Text::from(lines)
    }

    fn push_ai(&self, lines: &mut Vec<Line<'static>>, key: &str) {
        let now = Utc::now();
        if self.pending_ai.contains(key) {
            lines.push(Line::from(Span::styled(
                format!("{} asking {}…", SPINNER[self.tick % SPINNER.len()], self.cfg.ai.provider),
                Style::new().fg(Color::Magenta),
            )));
        } else if let Some(note) = self.state.ai_notes.get(key) {
            lines.push(Line::from(Span::styled(
                format!("✦ {} · {} ago", note.provider, ago(Some(note.at), now)),
                Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD),
            )));
            for l in note.text.lines() {
                let style = if l.starts_with("VERDICT") {
                    Style::new().add_modifier(Modifier::BOLD)
                } else {
                    Style::new()
                };
                lines.push(Line::from(Span::styled(l.to_string(), style)));
            }
        } else {
            lines.push(Line::from(Span::styled(
                format!("press x to ask {} about this", self.cfg.ai.provider),
                Style::new().fg(Color::DarkGray),
            )));
        }
    }

    fn draw_procs(&self, f: &mut Frame, area: Rect) {
        let [top, detail_area] = Layout::vertical([Constraint::Min(6), Constraint::Length(11)]).areas(area);
        let [list_area, apps_area] = Layout::horizontal([Constraint::Fill(1), Constraint::Length(42)]).areas(top);
        let list = &self.procs.dev;
        let rows: Vec<Row> = list
            .iter()
            .map(|p| {
                let mut name = p.name.clone();
                if self.pending_ai.contains(&p.id()) {
                    name.push_str("  (asking AI…)");
                } else if self.state.ai_notes.contains_key(&p.id()) {
                    name.push_str("  ✦");
                }
                Row::new(vec![
                    Cell::from(if self.marked_pids.contains(&p.pid) { "●" } else { " " }).style(Style::new().fg(Color::Yellow)),
                    Cell::from(p.verdict.label()).style(verdict_style(p.verdict)),
                    Cell::from(human(p.rss)),
                    Cell::from(fmt_uptime(p.uptime_secs)),
                    Cell::from(p.pid.to_string()),
                    Cell::from(name),
                    Cell::from(p.reasons.first().cloned().unwrap_or_default()).style(Style::new().fg(Color::DarkGray)),
                ])
            })
            .collect();
        let title = if self.procs_loading {
            format!(" dev processes {} ", SPINNER[self.tick % SPINNER.len()])
        } else {
            format!(" {} dev processes · r refresh ", list.len())
        };
        let table = Table::new(
            rows,
            [
                Constraint::Length(1),
                Constraint::Length(6),
                Constraint::Length(6),
                Constraint::Length(4),
                Constraint::Length(6),
                Constraint::Length(22),
                Constraint::Fill(1),
            ],
        )
        .header(
            Row::new(["", "STATE", "RSS", "UP", "PID", "NAME", "WHY"])
                .style(Style::new().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)),
        )
        .row_highlight_style(Style::new().bg(Color::Rgb(50, 50, 70)).add_modifier(Modifier::BOLD))
        .block(Block::bordered().title(title));
        let mut ts = TableState::default();
        let pos = self.sel_pid.and_then(|p| list.iter().position(|x| x.pid == p));
        ts.select(pos.or(if list.is_empty() { None } else { Some(0) }));
        f.render_stateful_widget(table, list_area, &mut ts);

        let m = &self.procs.mem;
        let mut app_lines = vec![
            Line::from(format!("used {} of {}", human(m.used()), human(m.total))),
            Line::from(Span::styled(
                format!("cached files {} (freed on demand)", human(m.cached)),
                Style::new().fg(Color::DarkGray),
            )),
            Line::from(""),
        ];
        for (name, bytes, n) in self.procs.apps.iter().take(apps_area.height.saturating_sub(5) as usize) {
            let label = if *n > 1 { format!("{name} ×{n}") } else { name.clone() };
            app_lines.push(Line::from(format!("{:>7}  {}", human(*bytes), label)));
        }
        f.render_widget(
            Paragraph::new(app_lines).block(Block::bordered().title(" Memory by app ")),
            apps_area,
        );

        let mut lines = Vec::new();
        if let Some(p) = self.selected_proc() {
            lines.push(Line::from(Span::styled(
                format!("{} · pid {} · {} · up {}", p.name, p.pid, human(p.rss), fmt_uptime(p.uptime_secs)),
                Style::new().add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(Span::styled(p.comm.clone(), Style::new().fg(Color::DarkGray))));
            for r in &p.reasons {
                lines.push(Line::from(format!("  • {r}")));
            }
            lines.push(Line::from(""));
            self.push_ai(&mut lines, &p.id());
        }
        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(Block::bordered().title(" Details · d kill · x ask AI ")),
            detail_area,
        );
    }

    fn draw_history(&self, f: &mut Frame, area: Rect) {
        let total: u64 = self.history.iter().filter(|h| h.ok).map(|h| h.bytes).sum();
        let rows: Vec<Row> = self
            .history
            .iter()
            .map(|h| {
                Row::new(vec![
                    Cell::from(h.at.with_timezone(&chrono::Local).format("%b %-d %H:%M").to_string()),
                    Cell::from(if h.ok { "ok" } else { "FAIL" }).style(if h.ok {
                        Style::new().fg(Color::Green)
                    } else {
                        Style::new().fg(Color::Red)
                    }),
                    Cell::from(if h.bytes > 0 { human(h.bytes) } else { String::new() }),
                    Cell::from(h.name.clone()),
                    Cell::from(h.message.clone()).style(Style::new().fg(Color::DarkGray)),
                ])
            })
            .collect();
        let table = Table::new(
            rows,
            [
                Constraint::Length(13),
                Constraint::Length(4),
                Constraint::Length(7),
                Constraint::Percentage(45),
                Constraint::Fill(1),
            ],
        )
        .header(
            Row::new(["WHEN", "", "FREED", "WHAT", "RESULT"])
                .style(Style::new().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)),
        )
        .block(Block::bordered().title(format!(
            " {} cleanups · {} freed in total · {} ",
            self.history.len(),
            human(total),
            tilde(&state::state_dir().join("history.jsonl"), &self.home)
        )));
        f.render_widget(table, area);
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        let line = if self.searching {
            Line::from(vec![
                Span::styled(" / ", Style::new().fg(Color::Black).bg(Color::Yellow)),
                Span::raw(format!(" {}▏  enter keep · esc clear", self.search)),
            ])
        } else if !self.status.is_empty() {
            Line::from(Span::styled(format!(" {}", self.status), Style::new().fg(Color::Yellow)))
        } else {
            let hints = match self.tab {
                Tab::Disk => "space mark · a mark SAFE · d clean · x ask AI · p pin · o reveal · f filter · c kind · s sort · / search · r rescan · ? help · q quit",
                Tab::Procs => "space mark · d kill · x ask AI · r refresh · tab switch · ? help · q quit",
                Tab::History => "r reload · tab switch · q quit",
            };
            Line::from(Span::styled(format!(" {hints}"), Style::new().fg(Color::DarkGray)))
        };
        f.render_widget(Paragraph::new(line), area);
    }

    fn draw_confirm_clean(&self, f: &mut Frame, ids: &[String]) {
        let items: Vec<&Item> = ids
            .iter()
            .filter_map(|id| self.items.iter().find(|i| &i.id == id))
            .collect();
        let total: u64 = items.iter().map(|i| i.reclaimable).sum();
        let not_safe = items.iter().filter(|i| i.effective_verdict() != Verdict::Safe).count();
        let mut lines = vec![
            Line::from(Span::styled(
                format!("Clean {} item(s) and free about {}?", items.len(), human(total)),
                Style::new().add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
        ];
        if not_safe > 0 {
            lines.push(Line::from(Span::styled(
                format!("⚠ {not_safe} of these are not marked SAFE. Read their details first."),
                Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(""));
        }
        for i in items.iter().take(14) {
            let v = i.effective_verdict();
            lines.push(Line::from(vec![
                Span::styled(format!("{:<6} ", v.label()), verdict_style(v)),
                Span::raw(format!("{:>7}  {}", human(i.reclaimable), i.name)),
            ]));
            lines.push(Line::from(Span::styled(
                format!("         $ {}", i.action.describe()),
                Style::new().fg(Color::DarkGray),
            )));
        }
        if items.len() > 14 {
            lines.push(Line::from(format!("… and {} more", items.len() - 14)));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled(" y ", Style::new().fg(Color::Black).bg(Color::Green)),
            Span::raw(" clean   "),
            Span::styled(" any other key ", Style::new().fg(Color::Black).bg(Color::DarkGray)),
            Span::raw(" cancel"),
        ]));
        let h = (lines.len() as u16 + 2).min(f.area().height.saturating_sub(4));
        modal(f, " Confirm cleanup ", Text::from(lines), 100, h);
    }

    fn draw_confirm_kill(&self, f: &mut Frame, pids: &[(u32, String)]) {
        let mut lines = vec![
            Line::from(Span::styled(
                format!("Send SIGTERM to {} process(es)?", pids.len()),
                Style::new().add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
        ];
        for (pid, name) in pids.iter().take(14) {
            lines.push(Line::from(format!("  pid {pid:<7} {name}")));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled(" y ", Style::new().fg(Color::Black).bg(Color::Red)),
            Span::raw(" kill   "),
            Span::styled(" any other key ", Style::new().fg(Color::Black).bg(Color::DarkGray)),
            Span::raw(" cancel"),
        ]));
        let h = lines.len() as u16 + 2;
        modal(f, " Confirm kill ", Text::from(lines), 70, h);
    }
}

fn verdict_style(v: Verdict) -> Style {
    Style::new().fg(match v {
        Verdict::Safe => Color::Green,
        Verdict::Review => Color::Yellow,
        Verdict::Active => Color::Cyan,
        Verdict::Keep => Color::Magenta,
    })
}

fn modal(f: &mut Frame, title: &str, text: Text, w: u16, h: u16) {
    let area = f.area();
    let w = w.min(area.width.saturating_sub(4));
    let h = h.min(area.height.saturating_sub(2));
    let rect = Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    };
    f.render_widget(Clear, rect);
    f.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .block(Block::bordered().title(title.to_string()).border_style(Style::new().fg(Color::Yellow))),
        rect,
    );
}

fn draw_help(f: &mut Frame) {
    let rows = [
        ("", "STATES"),
        ("SAFE", "leftover with nothing depending on it (gone project, merged branch, orphan cache)"),
        ("REVIEW", "probably removable, but it costs something (rebuild, re-download) or is unrecognized"),
        ("IN USE", "tied to recent work or a running process"),
        ("KEEP", "uncommitted or unpushed work, dedicated to a live worktree, or pinned by you"),
        ("", ""),
        ("", "KEYS"),
        ("j/k ↑↓", "move · g/G top/bottom · PgUp/PgDn"),
        ("space", "mark / unmark · a marks every SAFE item in view · u clears marks"),
        ("d", "clean marked items (or the selected one) after a confirm; kills on the Processes tab"),
        ("x / X", "ask Claude or Codex (ai.provider) about the selected / every marked item"),
        ("p", "pin: never clean this item (saved across runs)"),
        ("o / y", "reveal in Finder / copy path"),
        ("f c s /", "filter by state · filter by kind · change sort · search"),
        ("r", "rescan (disk) · refresh (processes)"),
        ("1 2 3 tab", "switch tabs"),
        ("", ""),
        ("", "Config: ~/.config/dustpan/config.toml · state + history: ~/.local/state/dustpan"),
    ];
    let lines: Vec<Line> = rows
        .iter()
        .map(|(k, v)| {
            if k.is_empty() {
                Line::from(Span::styled(v.to_string(), Style::new().add_modifier(Modifier::BOLD)))
            } else {
                let style = match *k {
                    "SAFE" => verdict_style(Verdict::Safe),
                    "REVIEW" => verdict_style(Verdict::Review),
                    "IN USE" => verdict_style(Verdict::Active),
                    "KEEP" => verdict_style(Verdict::Keep),
                    _ => Style::new().fg(Color::Cyan),
                };
                Line::from(vec![Span::styled(format!("  {k:<10}"), style), Span::raw(v.to_string())])
            }
        })
        .collect();
    let h = lines.len() as u16 + 2;
    modal(f, " Help · any key closes ", Text::from(lines), 104, h);
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
mod tests {
    use super::*;
    use crate::model::Action;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn app_with_items() -> App {
        let mut app = App::new(Config::default());
        app.state = State::default();
        let mut a = Item::new(Category::DerivedData, "MyApp · ~/gone/ios/MyApp.xcworkspace", "/Users/me/Library/Developer/Xcode/DerivedData/MyApp-a");
        a.verdict = Verdict::Safe;
        a.bytes = 20 << 30;
        a.reclaimable = 20 << 30;
        a.reasons = vec!["project ~/gone no longer exists".into()];
        a.action = Action::delete("/Users/me/Library/Developer/Xcode/DerivedData/MyApp-a");
        let mut b = Item::new(Category::Worktree, "MyApp · feature/x", "/Users/me/Work/MyApp-wt/x");
        b.verdict = Verdict::Keep;
        b.bytes = 3 << 30;
        b.reasons = vec!["2 uncommitted change(s)".into()];
        b.action = Action::run("git", &["worktree", "remove", "/Users/me/Work/MyApp-wt/x"], None);
        app.items = vec![a, b];
        app
    }

    fn render(app: &App, w: u16, h: u16) -> String {
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

    fn key(app: &mut App, c: KeyCode) {
        app.on_key(KeyEvent::new(c, KeyModifiers::NONE));
    }

    #[test]
    fn renders_every_tab_wide_and_narrow() {
        let mut app = app_with_items();
        for (w, h) in [(180, 45), (80, 24), (40, 12)] {
            for tab in [Tab::Disk, Tab::Procs, Tab::History] {
                app.tab = tab;
                let screen = render(&app, w, h);
                assert!(screen.contains("Dustpan"), "{w}x{h}\n{screen}");
            }
        }
        app.tab = Tab::Disk;
        let screen = render(&app, 180, 45);
        assert!(screen.contains("no longer exists"));
        assert!(screen.contains("SAFE"));
        assert!(screen.contains("rm -rf"));
    }

    #[test]
    fn keep_items_cannot_be_queued_for_cleaning() {
        let mut app = app_with_items();
        app.sel_id = Some(app.items[1].id.clone());
        key(&mut app, KeyCode::Char('d'));
        assert!(matches!(app.modal, Modal::None));
        assert!(app.status.contains("can't clean"));
    }

    #[test]
    fn mark_safe_then_confirm_modal_then_cancel() {
        let mut app = app_with_items();
        key(&mut app, KeyCode::Char('a'));
        assert_eq!(app.marked.len(), 1);
        key(&mut app, KeyCode::Char('d'));
        assert!(matches!(&app.modal, Modal::ConfirmClean(ids) if ids.len() == 1));
        let screen = render(&app, 180, 45);
        assert!(screen.contains("Confirm cleanup"));
        assert!(screen.contains("free about 20.0G"));
        key(&mut app, KeyCode::Char('n'));
        assert!(matches!(app.modal, Modal::None));
        assert_eq!(app.items.len(), 2);
        assert!(app.cleaning.is_empty());
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

    #[test]
    #[ignore = "scans the real machine; run with --ignored --nocapture"]
    fn render_real_scan() {
        let cfg = crate::config::load().unwrap();
        let mut app = App::new(cfg.clone());
        let (items, _) = scan::collect(cfg, |_| {});
        app.items = items;
        app.procs = procs::snapshot();
        app.scan_secs = Some(0.0);
        for tab in [Tab::Disk, Tab::Procs] {
            app.tab = tab;
            println!("{}", render(&app, 190, 48));
        }
    }
}
