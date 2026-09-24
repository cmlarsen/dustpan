use crate::config::AiConfig;
use crate::model::Item;
use crate::procs::{ProcItem, fmt_uptime};
use crate::util::{human, run_with, tilde};
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tempfile::NamedTempFile;

const PREAMBLE: &str = "You are helping a developer decide whether to clean something up on their Mac. \
They work on many projects at once, so anything tied to active work must be kept. \
Investigate read-only: list folders, check sizes, read files, list git worktrees. \
Never modify, move, or delete anything.";

const ANSWER_FORMAT: &str = "Answer in at most 8 short lines, plain text, no markdown headers.\n\
Line 1 is exactly one of: VERDICT: delete / VERDICT: keep / VERDICT: unsure\n\
Then say what this is and what created it, why it is or isn't safe to remove, \
and the exact command you would run to clean it (or what to check first).";

pub fn item_prompt(item: &Item, home: &Path) -> String {
    let mut s = format!("{PREAMBLE}\n\nDisk item flagged by the Dustpan cleanup tool:\n");
    s += &format!("- name: {}\n", item.name);
    s += &format!("- path: {}\n", item.path.display());
    s += &format!("- category: {}\n", item.category.label());
    s += &format!(
        "- size: {} (deleting frees about {})\n",
        human(item.bytes),
        human(item.reclaimable)
    );
    if let Some(t) = item.last_used {
        s += &format!("- last activity: {}\n", t.format("%Y-%m-%d %H:%M UTC"));
    }
    if let Some(o) = &item.owner {
        s += &format!("- belongs to: {}\n", tilde(o, home));
    }
    s += &format!(
        "- Dustpan's rule-based verdict: {}\n",
        item.effective_verdict().label()
    );
    for r in &item.reasons {
        s += &format!("  - {r}\n");
    }
    s += &format!(
        "- cleanup action Dustpan would run: {}\n\n",
        item.action.describe()
    );
    s += ANSWER_FORMAT;
    s
}

pub fn proc_prompt(p: &ProcItem) -> String {
    let mut s = format!(
        "{PREAMBLE}\n\nProcess flagged by the Dustpan cleanup tool (it may be using memory for nothing):\n"
    );
    s += &format!("- pid {} · {}\n- executable: {}\n", p.pid, p.name, p.comm);
    s += &format!(
        "- memory: {} · running {}\n",
        human(p.rss),
        fmt_uptime(p.uptime_secs)
    );
    if let Some(c) = &p.cwd {
        s += &format!("- working directory: {}\n", c.display());
    }
    if !p.ports.is_empty() {
        s += &format!("- listening on: {}\n", p.ports.join(", "));
    }
    for r in &p.reasons {
        s += &format!("  - {r}\n");
    }
    s += &format!(
        "\nYou can inspect it with `ps -o pid,ppid,etime,command -p {}` and `lsof -p {}`.\n",
        p.pid, p.pid
    );
    s += &ANSWER_FORMAT.replace("VERDICT: delete", "VERDICT: kill");
    s
}

pub fn workdir_for(path: &Path, home: &Path) -> PathBuf {
    let mut cur = Some(path);
    while let Some(p) = cur {
        if p.is_dir() {
            return p.to_path_buf();
        }
        cur = p.parent();
    }
    home.to_path_buf()
}

const CLAUDE_TOOLS: &str = "Read,Glob,Grep,Bash";

const CLAUDE_ALLOWED: &str = "Read,Glob,Grep,Bash(ls:*),Bash(du:*),Bash(stat:*),Bash(head:*),\
Bash(git worktree list:*),Bash(ps:*),Bash(lsof:*),Bash(xcrun simctl list:*)";

const LOCKDOWN_ENV: &[(&str, &str)] = &[
    ("GIT_OPTIONAL_LOCKS", "0"),
    ("GIT_CONFIG_COUNT", "1"),
    ("GIT_CONFIG_KEY_0", "core.fsmonitor"),
    ("GIT_CONFIG_VALUE_0", "false"),
];

pub fn claude_args(cfg: &AiConfig, prompt: &str, dir: &str) -> Vec<String> {
    let mut args: Vec<String> = ["-p", prompt, "--output-format", "text", "--add-dir", dir]
        .map(String::from)
        .into();
    if !cfg.claude_model.is_empty() {
        args.extend(["--model".into(), cfg.claude_model.clone()]);
    }
    if !cfg.effort.is_empty() {
        args.extend(["--effort".into(), cfg.effort.clone()]);
    }
    args.extend(
        [
            "--tools",
            CLAUDE_TOOLS,
            "--allowedTools",
            CLAUDE_ALLOWED,
            "--permission-mode",
            "dontAsk",
            "--setting-sources",
            "",
            "--strict-mcp-config",
        ]
        .map(String::from),
    );
    args
}

pub fn codex_args(cfg: &AiConfig, prompt: &str, dir: &str, out_file: &str) -> Vec<String> {
    let mut args: Vec<String> = [
        "exec",
        "--sandbox",
        "read-only",
        "--skip-git-repo-check",
        "--ignore-user-config",
        "--ignore-rules",
        "--disable",
        "plugins",
        "--disable",
        "apps",
        "--disable",
        "hooks",
        "-c",
        "project_doc_max_bytes=0",
        "-C",
        dir,
        "-o",
        out_file,
    ]
    .map(String::from)
    .into();
    if !cfg.codex_model.is_empty() {
        args.extend(["-m".into(), cfg.codex_model.clone()]);
    }
    if !cfg.effort.is_empty() {
        args.extend([
            "-c".into(),
            format!("model_reasoning_effort=\"{}\"", cfg.effort),
        ]);
    }
    args.push(prompt.into());
    args
}

fn codex_out_file() -> Result<NamedTempFile> {
    tempfile::Builder::new()
        .prefix("dustpan-codex-")
        .suffix(".txt")
        .tempfile()
        .context("could not create a temp file for the codex answer")
}

pub fn ask(cfg: &AiConfig, prompt: &str, dir: &Path) -> Result<String> {
    let timeout = Duration::from_secs(cfg.timeout_secs);
    let dir_s = dir.display().to_string();
    match cfg.provider.as_str() {
        "codex" => {
            let out_file = codex_out_file()?;
            let args = codex_args(cfg, prompt, &dir_s, &out_file.path().display().to_string());
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let out = run_with("codex", &args, Some(dir), LOCKDOWN_ENV, None, timeout)
                .context("codex did not finish (not installed or timed out)")?;
            let text = std::fs::read_to_string(out_file.path()).unwrap_or_default();
            if !out.ok && text.trim().is_empty() {
                bail!("codex failed: {}", out.stderr.trim());
            }
            Ok(text.trim().to_string())
        }
        _ => {
            let args = claude_args(cfg, prompt, &dir_s);
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let out = run_with("claude", &args, Some(dir), LOCKDOWN_ENV, None, timeout)
                .context("claude did not finish (not installed or timed out)")?;
            if !out.ok {
                bail!("claude failed: {}", out.stderr.trim());
            }
            Ok(out.stdout.trim().to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Action, Category};

    #[test]
    fn prompt_includes_facts_and_format() {
        let mut item = Item::new(
            Category::DerivedData,
            "MyApp · ~/gone",
            "/Users/me/Library/Developer/Xcode/DerivedData/MyApp-abc",
        );
        item.bytes = 20 << 30;
        item.reclaimable = 20 << 30;
        item.reasons = vec!["project ~/gone no longer exists".into()];
        item.action = Action::delete("/Users/me/Library/Developer/Xcode/DerivedData/MyApp-abc");
        let p = item_prompt(&item, Path::new("/Users/me"));
        assert!(p.contains("20.0G"));
        assert!(p.contains("no longer exists"));
        assert!(p.contains("VERDICT: delete"));
        assert!(p.contains("Never modify"));
    }

    #[test]
    fn claude_args_are_exact_and_locked_down() {
        let cfg = AiConfig::default();
        let args = claude_args(&cfg, "hi", "/w");
        let expected: Vec<String> = [
            "-p", "hi", "--output-format", "text", "--add-dir", "/w", "--model", "claude-opus-5", "--effort", "medium",
            "--tools", "Read,Glob,Grep,Bash",
            "--allowedTools",
            "Read,Glob,Grep,Bash(ls:*),Bash(du:*),Bash(stat:*),Bash(head:*),Bash(git worktree list:*),\
Bash(ps:*),Bash(lsof:*),Bash(xcrun simctl list:*)",
            "--permission-mode", "dontAsk", "--setting-sources", "", "--strict-mcp-config",
        ]
        .map(String::from)
        .into();
        assert_eq!(args, expected);
    }

    #[test]
    fn claude_allowlist_has_no_write_or_exec_capable_commands() {
        let args = claude_args(&AiConfig::default(), "hi", "/w");
        let pos = |flag: &str| args.iter().position(|a| a == flag).unwrap();
        let allowed = &args[pos("--allowedTools") + 1];
        for bad in [
            "sqlite3",
            "find",
            "git status",
            "git log",
            "git branch",
            "plutil",
            "Bash(file",
            "Edit",
            "Write",
            "Bash(git:",
            "Bash(*",
        ] {
            assert!(!allowed.contains(bad), "{bad} is allowed");
        }
        assert_eq!(args[pos("--tools") + 1], "Read,Glob,Grep,Bash");
        assert_eq!(args[pos("--permission-mode") + 1], "dontAsk");
        assert_eq!(args[pos("--setting-sources") + 1], "");
        assert!(args.iter().any(|a| a == "--strict-mcp-config"));
        assert!(!args.iter().any(|a| a == "--mcp-config"));
    }

    #[test]
    fn codex_args_are_exact_and_locked_down() {
        let cfg = AiConfig::default();
        let args = codex_args(&cfg, "hi", "/w", "/tmp/out");
        let expected: Vec<String> = [
            "exec",
            "--sandbox",
            "read-only",
            "--skip-git-repo-check",
            "--ignore-user-config",
            "--ignore-rules",
            "--disable",
            "plugins",
            "--disable",
            "apps",
            "--disable",
            "hooks",
            "-c",
            "project_doc_max_bytes=0",
            "-C",
            "/w",
            "-o",
            "/tmp/out",
            "-m",
            "gpt-5.6-sol",
            "-c",
            "model_reasoning_effort=\"medium\"",
            "hi",
        ]
        .map(String::from)
        .into();
        assert_eq!(args, expected);
    }

    #[test]
    fn empty_model_and_effort_fall_back_to_cli_defaults() {
        let cfg = AiConfig {
            claude_model: String::new(),
            codex_model: String::new(),
            effort: String::new(),
            ..AiConfig::default()
        };
        assert!(
            !claude_args(&cfg, "hi", "/w")
                .iter()
                .any(|a| a == "--model" || a == "--effort")
        );
        let codex = codex_args(&cfg, "hi", "/w", "/o");
        assert!(
            !codex
                .iter()
                .any(|a| a == "-m" || a.starts_with("model_reasoning_effort"))
        );
        assert!(codex.iter().any(|a| a == "--ignore-user-config"));
    }

    #[test]
    fn concurrent_codex_out_files_differ() {
        let (a, b) = std::thread::scope(|s| {
            let a = s.spawn(|| codex_out_file().unwrap());
            let b = s.spawn(|| codex_out_file().unwrap());
            (a.join().unwrap(), b.join().unwrap())
        });
        assert_ne!(a.path(), b.path());
        assert!(a.path().exists() && b.path().exists());
        let path = a.path().to_path_buf();
        drop(a);
        assert!(!path.exists());
    }

    #[test]
    fn lockdown_env_stops_git_status_running_repo_fsmonitor() {
        let d = tempfile::tempdir().unwrap();
        let repo = d.path().join("repo");
        let marker = d.path().join("ran");
        let hook = d.path().join("fsmon.sh");
        std::fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
        std::fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let git = |args: &[&str], env: &[(&str, &str)]| {
            let mut all = vec!["-C", repo.to_str().unwrap()];
            all.extend(args);
            run_with("git", &all, None, env, None, Duration::from_secs(10)).unwrap()
        };
        std::fs::create_dir(&repo).unwrap();
        assert!(git(&["init", "-q"], &[]).ok);
        assert!(git(&["config", "core.fsmonitor", hook.to_str().unwrap()], &[]).ok);
        assert!(git(&["status"], LOCKDOWN_ENV).ok);
        assert!(!marker.exists());
        git(&["status"], &[]);
        assert!(marker.exists());
    }

    #[test]
    fn workdir_falls_back_to_existing_ancestor() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("gone/deeper");
        assert_eq!(workdir_for(&p, Path::new("/")), d.path());
    }
}
