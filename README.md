# Dustpan (`dp`)

Dustpan finds the dev leftovers eating your Mac's disk and memory: git worktrees for merged branches, DerivedData for projects you deleted, 25 GB simulators, stale caches, and agent processes whose folder is gone. For each one it says why it's safe to remove (or why it isn't), and it cleans only after you confirm.

It was built for people juggling many projects at once. Anything with uncommitted or unpushed work, or with a process running in it, is marked KEEP or IN USE and left alone.

**macOS only.** It reads Xcode, simulator, and `~/Library` state directly.

## Install

```bash
brew install cmlarsen/tap/dustpan
# or
cargo install dustpan
```

Your terminal needs Full Disk Access (System Settings → Privacy & Security) to size other apps' containers in `~/Library/Containers`. Without it, macOS may ask for access app by app, and anything Dustpan cannot read is left out of the totals.

## Use

```bash
dp            # interactive TUI
dp report     # plain summary: SAFE / REVIEW / IN USE / KEEP (--all, --json)
dp mem        # memory by app + dev processes worth a look (--json)
dp clean      # clean every SAFE item after one confirm (--dry-run, --yes)
dp ask <path> # ask Claude or Codex what a folder is (--provider codex)
```

`x` in the TUI and `dp ask` need the [Claude Code](https://claude.com/claude-code) CLI or the [Codex](https://github.com/openai/codex) CLI. Everything else works without them.

## States

| State | Meaning |
|---|---|
| SAFE | Nothing depends on it: its project is gone, its branch or PR is merged, or it's an orphaned cache or download |
| REVIEW | Probably removable, but removing it costs a rebuild or re-download, or no rule recognizes it |
| IN USE | Tied to recent work (inside `stale_days`) or a running process |
| KEEP | Has uncommitted or unpushed work, belongs to a live worktree, or you pinned it |

Only SAFE items get cleaned automatically. `dp clean` never touches anything else, and the TUI flags any non-SAFE items in the confirm dialog.

## What it checks

- **Git worktrees** in every repo under your roots: dirty, unpushed, merged (via `git merge-base` and `gh pr list`), idle, or has a process running in it. Removal goes through `git worktree remove`, so git refuses to remove a checkout with changes.
- **Xcode DerivedData**: reads each folder's `info.plist` to see whether the workspace that built it still exists.
- **Simulators**: last use (`device.plist`), size of the largest app data, and `(wt-name)` clones whose worktree is gone. It runs `simctl erase` or `simctl delete`.
- **Simulator runtimes** (the multi-GB disk images): betas superseded by a release, and older runtimes no simulator uses. It runs `simctl runtime delete`. The newest runtime per platform is left alone.
- **iOS DeviceSupport**: older symbol sets for a device model once a newer one exists.
- **node_modules**, grouped by project and flagged when the project is idle. "Frees" counts only files that aren't hardlinked from the pnpm store.
- **Package caches**: pnpm, uv, Homebrew, CocoaPods, npm, bun, cargo, Playwright, Android emulators and system images, Hugging Face, and others. Prune commands (`pnpm store prune`, `uv cache prune`, `brew cleanup`) are SAFE. A cache that gets deleted outright is REVIEW while you've used it within `stale_days`, because deleting it only buys a re-download.
- **Docker** (when the daemon is running): images no container uses (`docker rmi`) and the build cache (`docker builder prune -f`). Volumes are never touched.
- **Downloads**: installers (`.dmg`, `.pkg`, `.xip`, `.iso`) and archives in `~/Downloads` older than two weeks.
- **Known leftovers**: Codex's abandoned marketplace upgrade folders (throwaway git clones, so the git-repo guard is relaxed for exactly those paths) and interrupted CFNetwork downloads in app containers.
- **Catch-all**: any unrecognized folder over `catch_all_min_gb` in `~/Library/{Containers,Application Support,Caches}`, `~/.cache`, `~/.local/share`, or a `~/.dotdir`. Press `x` to have an AI explain it.
- **Processes**: agents and dev servers whose working directory was deleted, that have been detached for days, or that listen on ports.

## Selecting

| Key | Does |
|---|---|
| `space` | mark / unmark the current row |
| `J` `K` or `⇧↓` `⇧↑` | mark the current row and move, to sweep a run of rows |
| `v` … `v` | mark every row between the two presses (`esc` cancels) |
| `a` / `*` | mark every SAFE item in view / every cleanable item in view |
| `u` | clear marks |
| `d` | clean the marks (or the current row) after a confirm |

Filter first (`f` state, `c` kind, `/` search) to keep `a` and `*` to what you mean.

## Safety

- Each item's action goes through the guard during the scan. Anything it would refuse is shown as REVIEW, with the reason, instead of looking cleanable.
- Every delete path goes through a guard: it must be inside `~` (or `$TMPDIR`), at least two levels deep, outside Documents/Desktop/Pictures/iCloud/Mail/Messages/keychains/`.ssh`/`.config`, not an ancestor of a project root, and not a git repository. If any path fails, nothing is deleted.
- Pin an item with `p`, or add globs to `protect` in `~/.config/dustpan/config.toml`. A folder glob also protects everything inside it.
- Every clean and kill is appended to `~/.local/state/dustpan/history.jsonl` (the History tab).

## AI context

Press `A` in the TUI to pick the agent (Claude or Codex), model, and effort. The choice is saved for later runs and used by `dp ask` too, which also takes `--provider`, `--model`, and `--effort`. `x` on an item (or `dp ask`) runs `claude -p` or `codex exec --sandbox read-only` in that folder. The call includes the facts Dustpan gathered and asks for a verdict. Claude's tools are limited to read-only ones (Read/Glob/Grep, `ls`, `du`, `git status/log`, `ps`, `lsof`…). Answers are saved per item in `~/.local/state/dustpan/state.json` and show in the details pane on later runs.

## Config

`~/.config/dustpan/config.toml` is created on first run:

```toml
roots = []            # empty = every folder in ~ that holds a git repo
stale_days = 30
min_size_mb = 100
catch_all_min_gb = 2
protect = ["~/Work/MyApp"]

[ai]
provider = "claude"   # or "codex"
claude_model = "claude-opus-5"   # exact IDs: verdicts stay stable when an alias moves
codex_model = "gpt-5.6-sol"
effort = "medium"                # low | medium | high | xhigh, passed to both CLIs
timeout_secs = 240
```

A full scan walks roughly 800 GB in about a minute. The TUI streams results as they arrive, so you can start browsing right away.

## Development

```bash
cargo test
cargo install --path . --force     # installs `dp`
cargo test --release render_real_scan -- --ignored --nocapture   # render the TUI from a scan of your machine
```

Releases: bump `version` in `Cargo.toml`, commit, then `git tag v0.x.y && git push --tags`. The release workflow builds both Mac architectures, publishes a GitHub Release, and updates the Homebrew tap.

## License

MIT or Apache-2.0, at your option. Dustpan deletes files, and it does so with no warranty: read the confirm dialog.
