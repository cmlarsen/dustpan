use chrono::{DateTime, Utc};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
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
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let mut child = cmd.spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let mut stderr = child.stderr.take()?;
    let out_reader = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let err_reader = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        buf
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if start.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => thread::sleep(Duration::from_millis(15)),
            Err(_) => break None,
        }
    };
    let out = out_reader.join().unwrap_or_default();
    let err = err_reader.join().unwrap_or_default();
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
}
