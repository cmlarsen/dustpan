use crate::model::Verdict;
use crate::util::{run, CmdOut};
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    pub rss_kb: u64,
    pub uptime_secs: u64,
    pub tty: String,
    pub comm: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProcItem {
    pub pid: u32,
    pub name: String,
    pub comm: String,
    pub rss: u64,
    pub uptime_secs: u64,
    pub cwd: Option<PathBuf>,
    pub ports: Vec<String>,
    pub verdict: Verdict,
    pub reasons: Vec<String>,
}

impl ProcItem {
    pub fn id(&self) -> String {
        format!("proc:{}:{}", self.pid, self.name)
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct MemSummary {
    pub total: u64,
    pub app: u64,
    pub wired: u64,
    pub compressed: u64,
    pub cached: u64,
    pub swap_used: u64,
}

impl MemSummary {
    pub fn used(&self) -> u64 {
        self.app + self.wired + self.compressed
    }
}

#[derive(Debug, Clone, Default)]
pub struct ProcSnapshot {
    pub mem: MemSummary,
    pub apps: Vec<(String, u64, usize)>,
    pub dev: Vec<ProcItem>,
}

pub fn parse_etime(s: &str) -> u64 {
    let (days, rest) = match s.split_once('-') {
        Some((d, r)) => (d.parse::<u64>().unwrap_or(0), r),
        None => (0, s),
    };
    let parts: Vec<u64> = rest.split(':').map(|p| p.parse().unwrap_or(0)).collect();
    let hms = match parts.as_slice() {
        [h, m, s] => h * 3600 + m * 60 + s,
        [m, s] => m * 60 + s,
        [s] => *s,
        _ => 0,
    };
    days * 86_400 + hms
}

pub fn parse_ps(text: &str) -> Vec<Proc> {
    text.lines()
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            let pid = it.next()?.parse().ok()?;
            let ppid = it.next()?.parse().ok()?;
            let rss_kb = it.next()?.parse().ok()?;
            let etime = it.next()?;
            let tty = it.next()?.to_string();
            let comm = it.collect::<Vec<_>>().join(" ");
            if comm.is_empty() {
                return None;
            }
            Some(Proc {
                pid,
                ppid,
                rss_kb,
                uptime_secs: parse_etime(etime),
                tty,
                comm,
            })
        })
        .collect()
}

pub fn list() -> Vec<Proc> {
    run(
        "ps",
        &["-axo", "pid=,ppid=,rss=,etime=,tty=,comm="],
        None,
        Duration::from_secs(10),
    )
    .map(|o| parse_ps(&o.stdout))
    .unwrap_or_default()
}

pub fn decode_lsof_name(v: &str) -> String {
    let b = v.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = (b[i] == b'\\' && b.get(i + 1) == Some(&b'x'))
            .then(|| b.get(i + 2..i + 4))
            .flatten()
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match hex {
            Some(byte) => {
                out.push(byte);
                i += 4;
            }
            None => {
                out.push(b[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn parse_lsof_fields(text: &str) -> HashMap<u32, Vec<String>> {
    let mut map: HashMap<u32, Vec<String>> = HashMap::new();
    let mut pid = None;
    for line in text.lines() {
        match line.split_at_checked(1) {
            Some(("p", v)) => pid = v.parse().ok(),
            Some(("n", v)) => {
                if let Some(p) = pid {
                    let v = decode_lsof_name(v);
                    let list = map.entry(p).or_default();
                    if !list.contains(&v) {
                        list.push(v);
                    }
                }
            }
            _ => {}
        }
    }
    map
}

pub fn lsof_output(out: Option<&CmdOut>) -> Option<HashMap<u32, Vec<String>>> {
    let out = out?;
    let no_matches = out.stdout.trim().is_empty() && out.stderr.trim().is_empty();
    (out.ok || no_matches).then(|| parse_lsof_fields(&out.stdout))
}

pub fn cwds() -> Option<HashMap<u32, PathBuf>> {
    let out = run("lsof", &["-nP", "-a", "-d", "cwd", "-Fpn"], None, Duration::from_secs(20));
    let fields = lsof_output(out.as_ref())?;
    if fields.is_empty() {
        return None;
    }
    Some(
        fields
            .into_iter()
            .filter_map(|(pid, v)| v.into_iter().next().map(|p| (pid, PathBuf::from(p))))
            .collect(),
    )
}

pub fn listeners() -> Option<HashMap<u32, Vec<String>>> {
    let out = run(
        "lsof",
        &["-nP", "-iTCP", "-sTCP:LISTEN", "-Fpn"],
        None,
        Duration::from_secs(20),
    );
    Some(
        lsof_output(out.as_ref())?
            .into_iter()
            .map(|(pid, addrs)| {
                let ports = addrs
                    .into_iter()
                    .filter_map(|a| a.rsplit_once(':').map(|(_, p)| format!(":{p}")))
                    .collect::<Vec<_>>();
                (pid, dedup(ports))
            })
            .collect(),
    )
}

pub fn cwd_deleted(cwd: &Path) -> bool {
    matches!(cwd.try_exists(), Ok(false))
}

fn dedup(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v.dedup();
    v
}

pub fn app_name(comm: &str) -> String {
    if let Some(idx) = comm.find(".app/") {
        let before = &comm[..idx];
        let start = before.rfind('/').map(|i| i + 1).unwrap_or(0);
        return format!("{}.app", &before[start..]);
    }
    Path::new(comm)
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| comm.to_string())
}

pub fn group_by_app(procs: &[Proc]) -> Vec<(String, u64, usize)> {
    let mut map: HashMap<String, (u64, usize)> = HashMap::new();
    for p in procs {
        let key = app_name(&p.comm);
        let e = map.entry(key).or_default();
        e.0 += p.rss_kb * 1024;
        e.1 += 1;
    }
    let mut v: Vec<_> = map.into_iter().map(|(k, (b, n))| (k, b, n)).collect();
    v.sort_by_key(|x| std::cmp::Reverse(x.1));
    v
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DevKind {
    Server,
    Agent,
    BuildDaemon,
    Simulator,
}

pub fn dev_kind(comm: &str) -> Option<(DevKind, String)> {
    let base = app_name(comm);
    if comm.contains("/Simulator.app/") {
        return Some((DevKind::Simulator, "Simulator".into()));
    }
    if base == "launchd_sim" {
        return Some((DevKind::Simulator, "booted simulator".into()));
    }
    if comm.contains(".app/") {
        return None;
    }
    let lower = base.to_lowercase();
    let kind = match lower.as_str() {
        "node" | "bun" | "deno" | "ruby" | "java" | "esbuild" | "watchman" | "postgres"
        | "redis-server" | "uvicorn" | "gunicorn" | "vite" => DevKind::Server,
        l if l.starts_with("python") => DevKind::Server,
        "claude" | "codex" | "opencode" | "cursor-agent" | "gemini" | "aider" => DevKind::Agent,
        "swbbuildservice" | "xcbbuildservice" | "sourcekit-lsp" | "xcodebuild" | "swift-frontend"
        | "clang" | "rustc" | "cargo" | "rust-analyzer" | "gradle" => DevKind::BuildDaemon,
        _ => return None,
    };
    Some((kind, base))
}

const OTHER_TOOL: &str = "running under another app or tool";

pub struct ProcFacts<'a> {
    pub kind: DevKind,
    pub cwd: Option<&'a Path>,
    pub cwd_deleted: bool,
    pub ports_known: bool,
    pub orphaned: bool,
    pub tty: &'a str,
    pub uptime_secs: u64,
    pub ports: &'a [String],
}

pub fn classify_proc(f: &ProcFacts) -> (Verdict, Vec<String>) {
    let mut reasons = Vec::new();
    let has_tty = !f.tty.is_empty() && f.tty != "??";
    if f.cwd.is_some() && f.cwd_deleted {
        reasons.push("its working directory was deleted".into());
        if !has_tty && f.ports.is_empty() && f.ports_known {
            return (Verdict::Safe, reasons);
        }
    }
    if !f.ports.is_empty() {
        reasons.push(format!("listening on {}", f.ports.join(", ")));
    }
    if !f.ports_known {
        reasons.push("couldn't check its listening ports (lsof failed)".into());
    }
    if has_tty {
        reasons.push(format!("attached to terminal {}", f.tty));
        return (Verdict::Active, reasons);
    }
    let long_running = f.uptime_secs > 86_400;
    match f.kind {
        DevKind::Agent if f.orphaned => {
            reasons.push("agent session with no terminal; its parent exited".into());
            (Verdict::Review, reasons)
        }
        DevKind::Server | DevKind::BuildDaemon if f.orphaned && long_running => {
            reasons.push(format!(
                "detached from any terminal for {}",
                fmt_uptime(f.uptime_secs)
            ));
            (Verdict::Review, reasons)
        }
        _ => {
            if reasons.is_empty() {
                reasons.push(OTHER_TOOL.into());
            }
            (Verdict::Active, reasons)
        }
    }
}

pub fn fmt_uptime(secs: u64) -> String {
    match secs {
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

pub fn parse_vm_stat(text: &str) -> (u64, HashMap<String, u64>) {
    let page = text
        .lines()
        .next()
        .and_then(|l| l.split("page size of ").nth(1))
        .and_then(|r| r.split_whitespace().next())
        .and_then(|n| n.parse().ok())
        .unwrap_or(16_384);
    let map = text
        .lines()
        .skip(1)
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            let v = v.trim().trim_end_matches('.').parse().ok()?;
            Some((k.trim().trim_matches('"').to_string(), v))
        })
        .collect();
    (page, map)
}

pub fn mem_summary() -> MemSummary {
    let total = run("sysctl", &["-n", "hw.memsize"], None, Duration::from_secs(5))
        .and_then(|o| o.stdout.trim().parse().ok())
        .unwrap_or(0);
    let swap_used = run("sysctl", &["-n", "vm.swapusage"], None, Duration::from_secs(5))
        .and_then(|o| parse_swap_used(&o.stdout))
        .unwrap_or(0);
    let Some(vm) = run("vm_stat", &[], None, Duration::from_secs(5)) else {
        return MemSummary {
            total,
            swap_used,
            ..Default::default()
        };
    };
    let (page, m) = parse_vm_stat(&vm.stdout);
    let get = |k: &str| m.get(k).copied().unwrap_or(0) * page;
    MemSummary {
        total,
        app: get("Anonymous pages").saturating_sub(get("Pages purgeable")),
        wired: get("Pages wired down"),
        compressed: get("Pages occupied by compressor"),
        cached: get("File-backed pages") + get("Pages purgeable"),
        swap_used,
    }
}

fn parse_swap_used(s: &str) -> Option<u64> {
    let used = s.split("used = ").nth(1)?.split_whitespace().next()?;
    let (num, unit) = used.split_at(used.len() - 1);
    let n: f64 = num.parse().ok()?;
    let mult = match unit {
        "K" => 1024.0,
        "M" => 1024.0 * 1024.0,
        "G" => 1024.0 * 1024.0 * 1024.0,
        _ => 1.0,
    };
    Some((n * mult) as u64)
}

pub fn snapshot() -> ProcSnapshot {
    let procs = list();
    let cwds = cwds().unwrap_or_default();
    let ports = listeners();
    let mem = mem_summary();
    let apps = group_by_app(&procs);
    let names: HashMap<u32, String> = procs.iter().map(|p| (p.pid, app_name(&p.comm))).collect();
    let mut dev = Vec::new();
    for p in &procs {
        let Some((kind, name)) = dev_kind(&p.comm) else { continue };
        let cwd = cwds.get(&p.pid).cloned();
        let p_ports = ports.as_ref().and_then(|m| m.get(&p.pid)).cloned().unwrap_or_default();
        let (verdict, mut reasons) = classify_proc(&ProcFacts {
            kind,
            cwd: cwd.as_deref(),
            cwd_deleted: cwd.as_deref().is_some_and(cwd_deleted),
            ports_known: ports.is_some(),
            orphaned: p.ppid == 1,
            tty: &p.tty,
            uptime_secs: p.uptime_secs,
            ports: &p_ports,
        });
        if let Some(parent) = names.get(&p.ppid).filter(|_| p.ppid > 1) {
            let parent = format!("child of {parent} (pid {})", p.ppid);
            if reasons.first().is_some_and(|r| r == OTHER_TOOL) {
                reasons[0] = parent;
            } else {
                reasons.push(parent);
            }
        }
        if let Some(c) = &cwd {
            reasons.push(format!("cwd {}", crate::util::tilde(c, &crate::util::home())));
        }
        dev.push(ProcItem {
            pid: p.pid,
            name,
            comm: p.comm.clone(),
            rss: p.rss_kb * 1024,
            uptime_secs: p.uptime_secs,
            cwd,
            ports: p_ports,
            verdict,
            reasons,
        });
    }
    dev.sort_by(|a, b| a.verdict.cmp(&b.verdict).then(b.rss.cmp(&a.rss)));
    ProcSnapshot { mem, apps, dev }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn etime_formats() {
        assert_eq!(parse_etime("05:07"), 307);
        assert_eq!(parse_etime("01:00:00"), 3600);
        assert_eq!(parse_etime("2-00:00:01"), 172_801);
    }

    #[test]
    fn ps_lines_keep_spaces_in_comm() {
        let text = "  123     1  2048   01:02 ??       /Applications/Orca.app/Contents/Frameworks/Orca Helper.app/Contents/MacOS/Orca Helper\n  456   123   100 1-00:00:00 ttys003  /Users/me/.local/bin/claude\n";
        let p = parse_ps(text);
        assert_eq!(p.len(), 2);
        assert!(p[0].comm.ends_with("Orca Helper"));
        assert_eq!(p[1].tty, "ttys003");
        assert_eq!(p[1].uptime_secs, 86_400);
    }

    #[test]
    fn groups_helpers_under_outer_app() {
        assert_eq!(
            app_name("/Applications/Orca.app/Contents/Frameworks/Orca Helper.app/Contents/MacOS/Orca Helper"),
            "Orca.app"
        );
        assert_eq!(app_name("/Users/me/.local/bin/claude"), "claude");
    }

    #[test]
    fn lsof_fields() {
        let text = "p100\nfcwd\nn/Users/me/x\np200\nf12\nn*:8081\nf13\nn127.0.0.1:8081\nf14\nn[::1]:3000\n";
        let m = parse_lsof_fields(text);
        assert_eq!(m[&100], vec!["/Users/me/x"]);
        assert_eq!(m[&200].len(), 3);
    }

    #[test]
    fn lsof_names_decode_hex_escapes() {
        let m = parse_lsof_fields("p7\nfcwd\nn/Users/me/caf\\xc3\\xa9 dir\n");
        assert_eq!(m[&7], vec!["/Users/me/café dir"]);
        assert_eq!(decode_lsof_name("/a\\xzz/b\\x4"), "/a\\xzz/b\\x4");
    }

    #[test]
    fn lsof_failure_is_unknown_but_no_matches_is_empty() {
        let out = |ok: bool, stdout: &str, stderr: &str| CmdOut { ok, stdout: stdout.into(), stderr: stderr.into() };
        assert_eq!(lsof_output(None), None);
        assert_eq!(lsof_output(Some(&out(false, "", "lsof: fatal"))), None);
        assert_eq!(lsof_output(Some(&out(false, "p1\nn/x\n", "lsof: WARNING"))), None);
        assert_eq!(lsof_output(Some(&out(false, "", ""))), Some(HashMap::new()));
        assert_eq!(lsof_output(Some(&out(true, "p1\nn/x\n", ""))).unwrap()[&1], vec!["/x"]);
    }

    #[test]
    fn cwd_deleted_only_when_stat_says_missing() {
        let d = tempfile::tempdir().unwrap();
        assert!(!cwd_deleted(d.path()));
        assert!(cwd_deleted(&d.path().join("gone")));
        let locked = d.path().join("locked");
        std::fs::create_dir_all(locked.join("cwd")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let deleted = cwd_deleted(&locked.join("cwd"));
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!deleted);
    }

    #[test]
    fn dev_kinds_skip_app_bundles() {
        assert!(dev_kind("/Applications/ChatGPT.app/Contents/Resources/codex").is_none());
        assert_eq!(dev_kind("/opt/homebrew/bin/node").unwrap().0, DevKind::Server);
        assert_eq!(dev_kind("/Users/me/.local/bin/claude").unwrap().0, DevKind::Agent);
        assert_eq!(
            dev_kind("/Applications/Xcode.app/Contents/Developer/Applications/Simulator.app/Contents/MacOS/Simulator").unwrap().0,
            DevKind::Simulator
        );
    }

    #[test]
    fn classify_rules() {
        let base = ProcFacts {
            kind: DevKind::Server,
            cwd: Some(Path::new("/gone")),
            cwd_deleted: true,
            ports_known: true,
            orphaned: true,
            tty: "??",
            uptime_secs: 10,
            ports: &[],
        };
        assert_eq!(classify_proc(&base).0, Verdict::Safe);
        let alive = ProcFacts { cwd_deleted: false, ..base };
        assert_eq!(classify_proc(&alive).0, Verdict::Active);
        let old = ProcFacts { uptime_secs: 3 * 86_400, cwd_deleted: false, ..alive };
        assert_eq!(classify_proc(&old).0, Verdict::Review);
        let tty = ProcFacts { tty: "ttys001", uptime_secs: 3 * 86_400, cwd_deleted: false, ..alive };
        assert_eq!(classify_proc(&tty).0, Verdict::Active);
        let agent = ProcFacts { kind: DevKind::Agent, cwd_deleted: false, ..alive };
        assert_eq!(classify_proc(&agent).0, Verdict::Review);
    }

    #[test]
    fn deleted_cwd_is_not_safe_with_tty_ports_or_unknown_ports() {
        let base = ProcFacts {
            kind: DevKind::Server,
            cwd: Some(Path::new("/gone")),
            cwd_deleted: true,
            ports_known: true,
            orphaned: true,
            tty: "??",
            uptime_secs: 10,
            ports: &[],
        };
        let tty = ProcFacts { tty: "ttys002", ..base };
        let (v, r) = classify_proc(&tty);
        assert_eq!(v, Verdict::Active);
        assert!(r[0].contains("deleted"));
        let ports = [":3000".to_string()];
        let listening = ProcFacts { ports: &ports, ..base };
        assert_ne!(classify_proc(&listening).0, Verdict::Safe);
        let unknown = ProcFacts { ports_known: false, ..base };
        let (v, r) = classify_proc(&unknown);
        assert_ne!(v, Verdict::Safe);
        assert!(r.iter().any(|x| x.contains("lsof failed")));
    }

    #[test]
    fn vm_stat_parsing() {
        let text = "Mach Virtual Memory Statistics: (page size of 16384 bytes)\nPages free:                                    86086.\nPages wired down:                             288396.\n\"Translation faults\":                       35801720.\n";
        let (page, m) = parse_vm_stat(text);
        assert_eq!(page, 16384);
        assert_eq!(m["Pages wired down"], 288_396);
        assert_eq!(m["Translation faults"], 35_801_720);
    }

    #[test]
    fn swap_parsing() {
        let s = "total = 1024.00M  used = 341.00M  free = 683.00M  (encrypted)";
        assert_eq!(parse_swap_used(s), Some(341 * 1024 * 1024));
    }
}
