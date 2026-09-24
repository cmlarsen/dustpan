use chrono::{DateTime, Utc};
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

pub fn home() -> PathBuf {
    dirs::home_dir().expect("no home directory")
}

pub fn expand(p: &str, home: &Path) -> PathBuf {
    if p == "~" {
        home.to_path_buf()
    } else if let Some(rest) = p.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(p)
    }
}

pub fn tilde(p: &Path, home: &Path) -> String {
    match p.strip_prefix(home) {
        Ok(r) if r.as_os_str().is_empty() => "~".into(),
        Ok(r) => format!("~/{}", r.display()),
        Err(_) => p.display().to_string(),
    }
}

pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes}B")
    } else if v >= 100.0 {
        format!("{:.0}{}", v, UNITS[i])
    } else {
        format!("{:.1}{}", v, UNITS[i])
    }
}

pub fn mtime(p: &Path) -> Option<DateTime<Utc>> {
    std::fs::symlink_metadata(p)
        .ok()?
        .modified()
        .ok()
        .map(Into::into)
}

pub fn age_days(t: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Option<i64> {
    t.map(|t| (now - t).num_days().max(0))
}

pub fn ago(t: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
    let Some(t) = t else { return "?".into() };
    let secs = (now - t).num_seconds().max(0);
    match secs {
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s if s < 86_400 * 60 => format!("{}d", s / 86_400),
        s => format!("{}mo", s / (86_400 * 30)),
    }
}

pub fn newer(a: Option<DateTime<Utc>>, b: Option<DateTime<Utc>>) -> Option<DateTime<Utc>> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, None) => a,
        (None, b) => b,
    }
}

#[derive(Debug, Clone)]
pub struct CmdOut {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

pub fn run(program: &str, args: &[&str], cwd: Option<&Path>, timeout: Duration) -> Option<CmdOut> {
    run_with(program, args, cwd, &[], None, timeout)
}

fn read_all(mut r: impl Read + Send + 'static) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = r.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    rx
}

static CHILD_GROUPS: [AtomicI32; 64] = [const { AtomicI32::new(0) }; 64];

struct GroupGuard(Option<usize>);

impl GroupGuard {
    fn register(pgid: libc::pid_t) -> Self {
        if pgid <= 0 {
            return GroupGuard(None);
        }
        GroupGuard(CHILD_GROUPS.iter().position(|slot| {
            slot.compare_exchange(0, pgid, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        }))
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if let Some(i) = self.0 {
            CHILD_GROUPS[i].store(0, Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
fn registered_groups() -> Vec<libc::pid_t> {
    CHILD_GROUPS
        .iter()
        .map(|slot| slot.load(Ordering::SeqCst))
        .filter(|&g| g > 0)
        .collect()
}

extern "C" fn kill_groups_and_exit(_signal: libc::c_int) {
    for slot in &CHILD_GROUPS {
        let pgid = slot.load(Ordering::SeqCst);
        if pgid > 0 {
            unsafe {
                libc::killpg(pgid, libc::SIGKILL);
            }
        }
    }
    unsafe { libc::_exit(130) }
}

pub fn kill_children_on_interrupt() {
    let handler = kill_groups_and_exit as extern "C" fn(libc::c_int) as libc::sighandler_t;
    unsafe {
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
    }
}

fn kill_group(child: &Child) {
    if let Ok(pgid) = libc::pid_t::try_from(child.id()) {
        unsafe {
            libc::killpg(pgid, libc::SIGKILL);
        }
    }
}

pub fn run_with(
    program: &str,
    args: &[&str],
    cwd: Option<&Path>,
    env: &[(&str, &str)],
    stdin: Option<&[u8]>,
    timeout: Duration,
) -> Option<CmdOut> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .envs(env.iter().copied())
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let mut child = cmd.spawn().ok()?;
    let _group = GroupGuard::register(libc::pid_t::try_from(child.id()).unwrap_or(0));
    if let (Some(mut pipe), Some(input)) = (child.stdin.take(), stdin) {
        let input = input.to_vec();
        thread::spawn(move || {
            let _ = pipe.write_all(&input);
        });
    }
    let Some((stdout, stderr)) = child.stdout.take().zip(child.stderr.take()) else {
        kill_group(&child);
        let _ = child.wait();
        return None;
    };
    let out_rx = read_all(stdout);
    let err_rx = read_all(stderr);
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() > deadline => {
                kill_group(&child);
                let _ = child.wait();
                break None;
            }
            Ok(None) => thread::sleep(Duration::from_millis(15)),
            Err(_) => {
                kill_group(&child);
                let _ = child.wait();
                break None;
            }
        }
    };
    let collect = |rx: &mpsc::Receiver<Vec<u8>>| {
        let wait = deadline.saturating_duration_since(Instant::now());
        rx.recv_timeout(wait).ok().or_else(|| {
            if status.is_some() {
                kill_group(&child);
            }
            rx.recv_timeout(Duration::from_secs(1)).ok()
        })
    };
    let out = collect(&out_rx).unwrap_or_default();
    let err = collect(&err_rx).unwrap_or_default();
    status.map(|s| CmdOut {
        ok: s.success(),
        stdout: String::from_utf8_lossy(&out).into_owned(),
        stderr: String::from_utf8_lossy(&err).into_owned(),
    })
}

pub fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn human_sizes() {
        assert_eq!(human(512), "512B");
        assert_eq!(human(1536), "1.5K");
        assert_eq!(human(20 * 1024 * 1024 * 1024), "20.0G");
        assert_eq!(human(300 * 1024 * 1024 * 1024), "300G");
    }

    #[test]
    fn expand_and_tilde_round_trip() {
        let home = Path::new("/Users/me");
        assert_eq!(expand("~/Work", home), PathBuf::from("/Users/me/Work"));
        assert_eq!(expand("/abs", home), PathBuf::from("/abs"));
        assert_eq!(tilde(Path::new("/Users/me/Work/x"), home), "~/Work/x");
        assert_eq!(tilde(Path::new("/Users/me"), home), "~");
        assert_eq!(tilde(Path::new("/opt/x"), home), "/opt/x");
    }

    #[test]
    fn ago_formats() {
        let now = Utc.with_ymd_and_hms(2026, 9, 24, 12, 0, 0).unwrap();
        assert_eq!(ago(Some(now - chrono::Duration::minutes(5)), now), "5m");
        assert_eq!(ago(Some(now - chrono::Duration::hours(5)), now), "5h");
        assert_eq!(ago(Some(now - chrono::Duration::days(12)), now), "12d");
        assert_eq!(ago(Some(now - chrono::Duration::days(95)), now), "3mo");
        assert_eq!(ago(None, now), "?");
    }

    #[test]
    fn run_times_out() {
        let out = run("sleep", &["5"], None, Duration::from_millis(100));
        assert!(out.is_none());
        let out = run("echo", &["hi"], None, Duration::from_secs(5)).unwrap();
        assert!(out.ok);
        assert_eq!(out.stdout.trim(), "hi");
    }

    #[test]
    fn run_timeout_kills_grandchildren_holding_stdout() {
        let start = Instant::now();
        let out = run(
            "sh",
            &["-c", "sleep 30 & sleep 30"],
            None,
            Duration::from_millis(500),
        );
        assert!(out.is_none());
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "took {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn run_returns_when_exited_child_leaves_stdout_held_open() {
        let start = Instant::now();
        let out = run(
            "sh",
            &["-c", "sleep 30 & echo hi"],
            None,
            Duration::from_millis(500),
        )
        .unwrap();
        assert!(out.ok);
        assert_eq!(out.stdout.trim(), "hi");
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "took {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn group_guard_registers_until_dropped() {
        let pgid = 999_999_001;
        {
            let _g = GroupGuard::register(pgid);
            assert!(registered_groups().contains(&pgid));
        }
        assert!(!registered_groups().contains(&pgid));
        let _none = GroupGuard::register(0);
        assert!(!registered_groups().contains(&0));
    }

    #[test]
    fn run_registers_the_child_group_only_while_it_runs() {
        let d = tempfile::tempdir().unwrap();
        let pid_file = d.path().join("pid");
        let script = format!("echo $$ > '{}'; sleep 1", pid_file.display());
        let handle =
            thread::spawn(move || run("sh", &["-c", &script], None, Duration::from_secs(5)));
        let start = Instant::now();
        let pgid = loop {
            if let Some(p) = std::fs::read_to_string(&pid_file)
                .ok()
                .and_then(|s| s.trim().parse::<i32>().ok())
            {
                break p;
            }
            assert!(start.elapsed() < Duration::from_secs(3));
            thread::sleep(Duration::from_millis(10));
        };
        assert!(registered_groups().contains(&pgid));
        assert!(handle.join().unwrap().unwrap().ok);
        assert!(!registered_groups().contains(&pgid));
    }

    #[test]
    fn run_with_feeds_stdin_and_env() {
        let out = run_with(
            "cat",
            &[],
            None,
            &[],
            Some(b"secret-header"),
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(out.stdout, "secret-header");
        let out = run_with(
            "sh",
            &["-c", "echo $DP_TEST_VAR"],
            None,
            &[("DP_TEST_VAR", "set")],
            None,
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(out.stdout.trim(), "set");
    }
}
