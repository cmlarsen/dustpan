use crate::config::AiConfig;
use crate::model::Item;
use crate::procs::{fmt_uptime, ProcItem};
use crate::util::{human, run, tilde};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;

const PREAMBLE: &str = "You are helping a developer decide whether to clean something up on their Mac. \
They work on many projects at once, so anything tied to active work must be kept. \
Investigate read-only: list folders, check sizes, read files, run git status/log. \
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
    s += &format!("- Dustpan's rule-based verdict: {}\n", item.effective_verdict().label());
    for r in &item.reasons {
        s += &format!("  - {r}\n");
    }
    s += &format!("- cleanup action Dustpan would run: {}\n\n", item.action.describe());
    s += ANSWER_FORMAT;
    s
}

pub fn proc_prompt(p: &ProcItem) -> String {
    let mut s = format!("{PREAMBLE}\n\nProcess flagged by the Dustpan cleanup tool (it may be using memory for nothing):\n");
    s += &format!("- pid {} · {}\n- executable: {}\n", p.pid, p.name, p.comm);
    s += &format!("- memory: {} · running {}\n", human(p.rss), fmt_uptime(p.uptime_secs));
    if let Some(c) = &p.cwd {
        s += &format!("- working directory: {}\n", c.display());
    }
    if !p.ports.is_empty() {
        s += &format!("- listening on: {}\n", p.ports.join(", "));
    }
    for r in &p.reasons {
        s += &format!("  - {r}\n");
    }
    s += &format!("\nYou can inspect it with `ps -o pid,ppid,etime,command -p {}` and `lsof -p {}`.\n", p.pid, p.pid);
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

const CLAUDE_TOOLS: &str = "Read,Glob,Grep,Bash(ls:*),Bash(du:*),Bash(stat:*),Bash(file:*),Bash(head:*),\
Bash(git status:*),Bash(git log:*),Bash(git branch:*),Bash(git worktree list:*),Bash(plutil:*),\
Bash(ps:*),Bash(lsof:*),Bash(sqlite3:*),Bash(find:*),Bash(xcrun simctl list:*)";

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
    args.extend(["--allowedTools".into(), CLAUDE_TOOLS.into()]);
    args
}

pub fn codex_args(cfg: &AiConfig, prompt: &str, dir: &str, out_file: &str) -> Vec<String> {
    let mut args: Vec<String> = [
        "exec", "--sandbox", "read-only", "--skip-git-repo-check", "-C", dir, "-o", out_file,
    ]
    .map(String::from)
    .into();
    if !cfg.codex_model.is_empty() {
        args.extend(["-m".into(), cfg.codex_model.clone()]);
    }
    if !cfg.effort.is_empty() {
        args.extend(["-c".into(), format!("model_reasoning_effort=\"{}\"", cfg.effort)]);
    }
    args.push(prompt.into());
    args
}

pub fn ask(cfg: &AiConfig, prompt: &str, dir: &Path) -> Result<String> {
    let timeout = Duration::from_secs(cfg.timeout_secs);
    let dir_s = dir.display().to_string();
    match cfg.provider.as_str() {
        "codex" => {
            let out_file = std::env::temp_dir().join(format!("dustpan-codex-{}.txt", std::process::id()));
            let args = codex_args(cfg, prompt, &dir_s, &out_file.display().to_string());
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let out = run("codex", &args, Some(dir), timeout)
                .context("codex did not finish (not installed or timed out)")?;
            let text = std::fs::read_to_string(&out_file).unwrap_or_default();
            let _ = std::fs::remove_file(&out_file);
            if !out.ok && text.trim().is_empty() {
                bail!("codex failed: {}", out.stderr.trim());
            }
            Ok(text.trim().to_string())
        }
        _ => {
            let args = claude_args(cfg, prompt, &dir_s);
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let out = run("claude", &args, Some(dir), timeout)
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
        let mut item = Item::new(Category::DerivedData, "MyApp · ~/gone", "/Users/me/Library/Developer/Xcode/DerivedData/MyApp-abc");
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
    fn claude_args_pin_model_and_effort() {
        let cfg = AiConfig::default();
        let args = claude_args(&cfg, "hi", "/w");
        let pos = |flag: &str| args.iter().position(|a| a == flag).unwrap();
        assert_eq!(args[pos("--model") + 1], "claude-opus-5");
        assert_eq!(args[pos("--effort") + 1], "medium");
        assert_eq!(args[pos("-p") + 1], "hi");
    }

    #[test]
    fn codex_args_pin_model_and_effort() {
        let cfg = AiConfig::default();
        let args = codex_args(&cfg, "hi", "/w", "/tmp/out");
        let pos = |flag: &str| args.iter().position(|a| a == flag).unwrap();
        assert_eq!(args[pos("-m") + 1], "gpt-5.6-sol");
        assert_eq!(args[pos("-c") + 1], "model_reasoning_effort=\"medium\"");
        assert_eq!(args.last().unwrap(), "hi");
    }

    #[test]
    fn empty_model_and_effort_fall_back_to_cli_defaults() {
        let cfg = AiConfig { claude_model: String::new(), codex_model: String::new(), effort: String::new(), ..AiConfig::default() };
        assert!(!claude_args(&cfg, "hi", "/w").iter().any(|a| a == "--model" || a == "--effort"));
        assert!(!codex_args(&cfg, "hi", "/w", "/o").iter().any(|a| a == "-m" || a == "-c"));
    }

    #[test]
    fn workdir_falls_back_to_existing_ancestor() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("gone/deeper");
        assert_eq!(workdir_for(&p, Path::new("/")), d.path());
    }
}
