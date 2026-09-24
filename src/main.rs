#[cfg(not(target_os = "macos"))]
compile_error!("dustpan only supports macOS");

mod actions;
mod ai;
mod catalog;
mod config;
mod git;
mod model;
mod procs;
mod report;
mod scan;
mod scanners;
mod size;
mod state;
mod tui;
mod util;

use anyhow::Result;
use clap::{Parser, Subcommand};
use model::{Category, Item, Verdict};
use serde::Serialize;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "dp",
    version,
    about = "Dustpan: find and clean dev leftovers eating your disk and memory"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Serialize)]
struct AppMemory<'a> {
    name: &'a str,
    bytes: u64,
    processes: usize,
}

fn app_memory(apps: &[(String, u64, usize)]) -> Vec<AppMemory<'_>> {
    apps.iter()
        .map(|(name, bytes, processes)| AppMemory {
            name,
            bytes: *bytes,
            processes: *processes,
        })
        .collect()
}

#[derive(Subcommand)]
enum Cmd {
    /// Interactive TUI (the default)
    Tui,
    /// Print what is safe, what needs review, and what is in use
    Report {
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
        /// List every item, including in-use ones
        #[arg(long)]
        all: bool,
    },
    /// Memory breakdown and dev processes worth a look
    Mem {
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },
    /// Clean every SAFE item (asks first unless --yes)
    Clean {
        /// Skip the interactive confirmation
        #[arg(long)]
        yes: bool,
        /// Show what would be cleaned without changing anything
        #[arg(long)]
        dry_run: bool,
    },
    /// Ask Claude or Codex what a folder is and whether it can go
    Ask {
        path: PathBuf,
        /// claude or codex (defaults to the TUI's last choice, then the config)
        #[arg(long)]
        provider: Option<String>,
        /// Model ID for that provider
        #[arg(long)]
        model: Option<String>,
        /// low, medium, high, or xhigh
        #[arg(long)]
        effort: Option<String>,
    },
    /// Print the config file location
    Config,
}

fn progress(msg: &str) {
    if std::io::stderr().is_terminal() {
        eprint!("\r\x1b[2K{}…", report::safe_inline(msg));
        let _ = std::io::stderr().flush();
    }
}

fn clear_progress() {
    if std::io::stderr().is_terminal() {
        eprint!("\r\x1b[2K");
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cmd = cli.cmd.unwrap_or(Cmd::Tui);
    if let Cmd::Config = cmd {
        println!("{}", config::config_path().display());
        return config::load().map(|_| ());
    }
    let cfg = config::load()?;
    if !matches!(cmd, Cmd::Tui) {
        util::kill_children_on_interrupt();
    }
    match cmd {
        Cmd::Tui => tui::run(cfg),
        Cmd::Report { json, all } => {
            let (items, secs) = scan::collect(cfg, progress);
            clear_progress();
            if json {
                println!("{}", serde_json::to_string_pretty(&items)?);
            } else {
                report::print_disk(&items, secs, all);
            }
            Ok(())
        }
        Cmd::Mem { json } => {
            let snap = procs::snapshot();
            if json {
                let apps = app_memory(&snap.apps);
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "memory": snap.mem,
                        "apps": apps,
                        "dev_processes": snap.dev,
                    }))?
                );
            } else {
                report::print_mem(&snap);
            }
            Ok(())
        }
        Cmd::Clean { yes, dry_run } => clean_safe(cfg, yes, dry_run),
        Cmd::Ask {
            path,
            provider,
            model,
            effort,
        } => {
            let home = util::home();
            let path = std::fs::canonicalize(&path).unwrap_or(path);
            let mut ai_cfg = cfg.ai.clone();
            state::State::load().ai.apply(&mut ai_cfg);
            if let Some(p) = provider {
                ai_cfg.provider = p;
            }
            if let Some(m) = model {
                if ai_cfg.provider == "codex" {
                    ai_cfg.codex_model = m;
                } else {
                    ai_cfg.claude_model = m;
                }
            }
            if let Some(e) = effort {
                ai_cfg.effort = e;
            }
            let stats = size::dir_stats(&path);
            let mut item = Item::new(Category::AppData, path.display().to_string(), &path);
            item.bytes = stats.bytes;
            item.reclaimable = stats.exclusive;
            item.last_used = stats.newest;
            item.reasons = vec!["asked about directly with `dp ask`".into()];
            eprintln!(
                "asking {} about {} ({})…",
                report::safe_inline(&ai_cfg.provider),
                report::safe_inline(&util::tilde(&path, &home)),
                util::human(item.bytes)
            );
            let answer = ai::ask(
                &ai_cfg,
                &ai::item_prompt(&item, &home),
                &ai::workdir_for(&path, &home),
            )?;
            println!("{}", report::safe_multiline(&answer));
            Ok(())
        }
        Cmd::Config => Ok(()),
    }
}

fn clean_safe(cfg: config::Config, yes: bool, dry_run: bool) -> Result<()> {
    let home = util::home();
    let roots = config::resolve_roots(&cfg, &home);
    let protector = config::Protector::new(&cfg.protect, state::State::load().pins, &home)?;
    let (items, _) = scan::collect(cfg, progress);
    clear_progress();
    let safe: Vec<&Item> = items
        .iter()
        .filter(|i| i.effective_verdict() == Verdict::Safe && i.cleanable())
        .collect();
    if safe.is_empty() {
        println!("Nothing is marked SAFE right now.");
        return Ok(());
    }
    let total: u64 = safe.iter().map(|i| i.reclaimable).sum();
    println!(
        "{} SAFE items, about {} to free:",
        safe.len(),
        util::human(total)
    );
    for i in &safe {
        println!(
            "  {:>7}  {}\n           $ {}",
            util::human(i.reclaimable),
            report::safe_inline(&i.name),
            report::safe_inline(&i.action.describe())
        );
    }
    if dry_run {
        return Ok(());
    }
    if !yes {
        if !std::io::stdin().is_terminal() {
            anyhow::bail!("refusing to clean without a terminal; pass --yes");
        }
        print!("\nClean all of these? [y/N] ");
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !answer.trim().eq_ignore_ascii_case("y") {
            println!("Nothing cleaned.");
            return Ok(());
        }
    }
    let mut freed = 0;
    let mut failures = 0;
    for i in safe {
        print!("  {} … ", report::safe_inline(&i.name));
        std::io::stdout().flush()?;
        match actions::clean(i, &home, &roots, &protector) {
            Ok(msg) => {
                freed += i.reclaimable;
                println!("ok ({})", report::safe_inline(&msg));
            }
            Err(e) => {
                failures += 1;
                println!("FAILED: {}", report::safe_inline(&format!("{e:#}")));
            }
        }
    }
    println!("\nFreed about {}.", util::human(freed));
    clean_result(failures)
}

fn clean_result(failures: usize) -> Result<()> {
    if failures == 0 {
        Ok(())
    } else {
        anyhow::bail!("{failures} item(s) failed to clean")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn clean_failures_produce_an_error_exit() {
        assert!(clean_result(0).is_ok());
        assert_eq!(
            clean_result(3).unwrap_err().to_string(),
            "3 item(s) failed to clean"
        );
    }

    #[test]
    fn mem_json_apps_have_named_fields() {
        let apps = vec![("Editor".to_string(), 42, 2)];
        let value = serde_json::to_value(app_memory(&apps)).unwrap();
        assert_eq!(value[0]["name"], "Editor");
        assert_eq!(value[0]["bytes"], 42);
        assert_eq!(value[0]["processes"], 2);
    }

    #[test]
    fn every_cli_flag_has_help_text() {
        let command = Cli::command();
        for subcommand in command.get_subcommands() {
            for argument in subcommand
                .get_arguments()
                .filter(|argument| argument.get_long().is_some())
            {
                assert!(
                    argument.get_help().is_some(),
                    "{} --{} has no help",
                    subcommand.get_name(),
                    argument.get_long().unwrap()
                );
            }
        }
    }
}
