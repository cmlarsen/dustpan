use crate::model::{Category, Item, Verdict};
use crate::procs::{fmt_uptime, ProcSnapshot};
use crate::util::{ago, home, human, tilde};
use chrono::Utc;
use std::collections::BTreeMap;

pub struct DiskInfo {
    pub total: u64,
    pub free: u64,
}

pub fn disk_info(path: &std::path::Path) -> Option<DiskInfo> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut s: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    let bs = s.f_bsize as u64;
    Some(DiskInfo {
        total: s.f_blocks * bs,
        free: s.f_bavail * bs,
    })
}

pub fn totals(items: &[Item]) -> BTreeMap<Verdict, (u64, usize)> {
    let mut m = BTreeMap::new();
    for i in items {
        let e = m.entry(i.effective_verdict()).or_insert((0, 0));
        e.0 += i.reclaimable;
        e.1 += 1;
    }
    m
}

pub fn print_disk(items: &[Item], secs: f64, verbose: bool) {
    let home = home();
    let now = Utc::now();
    if let Some(d) = disk_info(&home) {
        println!(
            "Disk: {} used of {} ({} free) · scanned in {:.0}s",
            human(d.total - d.free),
            human(d.total),
            human(d.free),
            secs
        );
    }
    let t = totals(items);
    let line: Vec<String> = [Verdict::Safe, Verdict::Review, Verdict::Active, Verdict::Keep]
        .iter()
        .filter_map(|v| t.get(v).map(|(b, n)| format!("{} {} in {n}", v.label(), human(*b))))
        .collect();
    println!("{}\n", line.join(" · "));

    for verdict in [Verdict::Safe, Verdict::Review] {
        let list: Vec<&Item> = items.iter().filter(|i| i.effective_verdict() == verdict).collect();
        if list.is_empty() {
            continue;
        }
        println!("{} ({})", verdict.label(), human(list.iter().map(|i| i.reclaimable).sum()));
        for i in list.iter().take(if verbose { usize::MAX } else { 25 }) {
            println!(
                "  {:>7}  {:>4}  {:<14} {}",
                human(i.reclaimable),
                ago(i.last_used, now),
                i.category.label(),
                i.name
            );
            println!("  {:>7}  {:>4}  {:<14} └ {}", "", "", "", i.reasons.first().cloned().unwrap_or_default());
        }
        if !verbose && list.len() > 25 {
            println!("  … {} more (dp report --all)", list.len() - 25);
        }
        println!();
    }

    let mut by_cat: BTreeMap<Category, (u64, usize)> = BTreeMap::new();
    for i in items.iter().filter(|i| matches!(i.effective_verdict(), Verdict::Active | Verdict::Keep)) {
        let e = by_cat.entry(i.category).or_default();
        e.0 += i.bytes;
        e.1 += 1;
    }
    if !by_cat.is_empty() {
        let parts: Vec<String> = by_cat
            .iter()
            .map(|(c, (b, n))| format!("{} {} ({n})", c.label(), human(*b)))
            .collect();
        println!("In use or kept: {}", parts.join(" · "));
        if verbose {
            for i in items.iter().filter(|i| matches!(i.effective_verdict(), Verdict::Active | Verdict::Keep)) {
                println!(
                    "  {:>7}  {:<6} {:<14} {} — {}",
                    human(i.bytes),
                    i.effective_verdict().label(),
                    i.category.label(),
                    i.name,
                    i.reasons.first().cloned().unwrap_or_default()
                );
            }
        }
    }
    let _ = tilde(&home, &home);
}

pub fn print_mem(snap: &ProcSnapshot) {
    let m = &snap.mem;
    println!(
        "Memory: {} of {} used (apps {} · wired {} · compressed {}) · cached files {} (reclaimable) · swap {}",
        human(m.used()),
        human(m.total),
        human(m.app),
        human(m.wired),
        human(m.compressed),
        human(m.cached),
        human(m.swap_used)
    );
    println!("\nTop apps by memory:");
    for (name, bytes, n) in snap.apps.iter().take(12) {
        println!("  {:>7}  {name}{}", human(*bytes), if *n > 1 { format!(" ({n} processes)") } else { String::new() });
    }
    let flagged: Vec<_> = snap
        .dev
        .iter()
        .filter(|p| matches!(p.verdict, Verdict::Safe | Verdict::Review))
        .collect();
    println!("\nDev processes: {} running, {} worth a look", snap.dev.len(), flagged.len());
    for p in snap.dev.iter().take(30) {
        println!(
            "  {:<6} {:>7}  {:>4}  pid {:<6} {:<18} {}",
            p.verdict.label(),
            human(p.rss),
            fmt_uptime(p.uptime_secs),
            p.pid,
            p.name,
            p.reasons.first().cloned().unwrap_or_default()
        );
    }
}
