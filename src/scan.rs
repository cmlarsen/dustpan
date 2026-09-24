use crate::config::{resolve_roots, Config, Protector};
use crate::git;
use crate::model::{Category, Item, Verdict};
use crate::procs;
use crate::scanners::{self, Ctx};
use crate::state::State;
use crate::util::home;
use chrono::Utc;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Sender};
use std::sync::Mutex;
use std::time::Instant;

pub enum ScanMsg {
    Item(Box<Item>),
    Progress(String),
    Done { secs: f64 },
}

type Scanner = fn(&Ctx, scanners::Emit);

const SCANNERS: &[(&str, Scanner)] = &[
    ("worktrees", scanners::worktrees::scan),
    ("derived data", scanners::derived_data::scan),
    ("simulators", scanners::simulators::scan),
    ("sim runtimes", scanners::runtimes::scan),
    ("docker", scanners::docker::scan),
    ("downloads", scanners::downloads::scan),
    ("device support", scanners::device_support::scan),
    ("node_modules", scanners::node_modules::scan),
    ("caches", scanners::caches::scan),
    ("leftovers", scanners::leftovers::scan),
    ("app data", scanners::appdata::scan_known),
];

pub fn keep_item(item: &Item, min_bytes: u64) -> bool {
    item.category == Category::Worktree
        || item.bytes >= min_bytes
        || (item.verdict == Verdict::Safe && item.bytes > 0)
}

pub fn run_scan(cfg: Config, tx: Sender<ScanMsg>) {
    let start = Instant::now();
    let home = home();
    let state = State::load();
    let protector = Protector::new(&cfg.protect, state.pins.clone(), &home).unwrap_or_else(|_| Protector::everything());
    let send = |m: ScanMsg| {
        let _ = tx.send(m);
    };

    send(ScanMsg::Progress("finding projects".into()));
    let roots = resolve_roots(&cfg, &home);
    let repo_paths = git::discover_repos(&roots, 3);
    let repos: Vec<git::Repo> = std::thread::scope(|s| {
        let handles: Vec<_> = repo_paths
            .iter()
            .map(|p| s.spawn(move || git::load_repo(p)))
            .collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    });

    send(ScanMsg::Progress("reading process working directories".into()));
    let cwds = procs::cwds();
    let names: std::collections::HashMap<u32, String> = procs::list()
        .into_iter()
        .map(|p| (p.pid, procs::app_name(&p.comm)))
        .collect();
    let proc_cwds = cwds.map(|cwds| {
        cwds.into_iter()
            .map(|(pid, cwd)| (pid, names.get(&pid).cloned().unwrap_or_default(), cwd))
            .collect()
    });

    let ctx = Ctx {
        home: home.clone(),
        now: Utc::now(),
        roots,
        repos,
        proc_cwds,
        cfg: cfg.clone(),
    };
    let min_bytes = cfg.min_size_mb << 20;
    let covered: Mutex<Vec<(PathBuf, u64)>> = Mutex::new(Vec::new());
    let running: Mutex<BTreeSet<&str>> = Mutex::new(BTreeSet::new());
    let report = |running: &BTreeSet<&str>| {
        let list: Vec<&str> = running.iter().copied().collect();
        send(ScanMsg::Progress(format!("scanning {}", list.join(", "))));
    };
    let emit = |mut item: Item| {
        covered.lock().unwrap().push((item.path.clone(), item.bytes));
        if !keep_item(&item, min_bytes) {
            return;
        }
        let protected = protector.is_protected(&item);
        crate::actions::apply_preflight(&mut item, &ctx.home, &ctx.roots, &protector);
        item.set_protected(protected);
        send(ScanMsg::Item(Box::new(item)));
    };

    std::thread::scope(|s| {
        for (name, scanner) in SCANNERS {
            let ctx = &ctx;
            let emit = &emit;
            let running = &running;
            let report = &report;
            s.spawn(move || {
                {
                    let mut r = running.lock().unwrap();
                    r.insert(name);
                    report(&r);
                }
                scanner(ctx, emit);
                let mut r = running.lock().unwrap();
                r.remove(name);
                if !r.is_empty() {
                    report(&r);
                }
            });
        }
    });

    send(ScanMsg::Progress("sizing unrecognized app folders".into()));
    let covered = covered.lock().unwrap().clone();
    scanners::appdata::scan_catch_all(&ctx, &covered, &emit);
    send(ScanMsg::Done {
        secs: start.elapsed().as_secs_f64(),
    });
}

pub fn collect(cfg: Config, mut on_progress: impl FnMut(&str)) -> (Vec<Item>, f64) {
    let (tx, rx) = channel();
    let handle = std::thread::spawn(move || run_scan(cfg, tx));
    let mut items = Vec::new();
    let mut secs = 0.0;
    for msg in rx {
        match msg {
            ScanMsg::Item(i) => items.push(*i),
            ScanMsg::Progress(p) => on_progress(&p),
            ScanMsg::Done { secs: s } => secs = s,
        }
    }
    let _ = handle.join();
    sort_items(&mut items);
    (items, secs)
}

pub fn sort_items(items: &mut [Item]) {
    items.sort_by(Item::listing_order);
}
