use super::SPINNER;
use super::app::App;
use super::modal::{confirm_keys, draw_help, modal};
use super::{Modal, Tab};
use crate::model::{Item, Verdict};
use crate::procs::fmt_uptime;
use crate::report::{safe_inline, totals};
use crate::state;
use crate::util::{ago, human, tilde};
use chrono::Utc;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Cell, Paragraph, Row, Table, Tabs, Wrap};
use std::collections::HashSet;

impl App {
    pub(crate) fn draw(&mut self, f: &mut Frame) {
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
            Tabs::new(titles).select(idx).highlight_style(
                Style::new()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
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
            Modal::ConfirmClean { ids, skipped } => self.draw_confirm_clean(f, ids, skipped),
            Modal::AiPicker(row) => self.draw_ai_picker(f, *row),
            Modal::ConfirmKill(pids) => self.draw_confirm_kill(f, pids),
        }
    }

    fn draw_header(&self, f: &mut Frame, area: ratatui::layout::Rect) {
        let mut l1 = vec![Span::styled(
            " Dustpan ",
            Style::new()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )];
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
        for v in [
            Verdict::Safe,
            Verdict::Review,
            Verdict::Active,
            Verdict::Keep,
        ] {
            let (b, n) = t.get(&v).copied().unwrap_or((0, 0));
            l2.push(Span::styled(
                format!("{} ", v.label()),
                verdict_style(v).add_modifier(Modifier::BOLD),
            ));
            l2.push(Span::raw(format!("{} ({n})   ", human(b))));
        }
        if self.scanning {
            l2.push(Span::styled(
                format!(
                    "{} {} ({}s)",
                    SPINNER[self.tick % SPINNER.len()],
                    safe_inline(&self.scan_progress),
                    self.scan_started.elapsed().as_secs()
                ),
                Style::new().fg(Color::Cyan),
            ));
        } else if let Some(s) = self.scan_secs {
            l2.push(Span::styled(
                format!("scanned in {s:.0}s"),
                Style::new().fg(Color::DarkGray),
            ));
        }
        if let (Some(start), Some(d)) = (self.free_at_start, &self.disk)
            && d.free > start + (100 << 20)
        {
            l2.push(Span::styled(
                format!("   free {} → {}", human(start), human(d.free)),
                Style::new().fg(Color::Green),
            ));
        }
        if self.freed_session > 0 {
            l2.push(Span::styled(
                format!("   freed {} this session", human(self.freed_session)),
                Style::new().fg(Color::Green),
            ));
        }
        f.render_widget(Paragraph::new(vec![Line::from(l1), Line::from(l2)]), area);
    }

    fn draw_disk(&mut self, f: &mut Frame, area: ratatui::layout::Rect) {
        let (list_area, detail_area) = if area.width >= 140 {
            let [a, b] =
                Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)])
                    .areas(area);
            (a, b)
        } else {
            let [a, b] = Layout::vertical([Constraint::Min(6), Constraint::Length(14)]).areas(area);
            (a, b)
        };
        let vis = self.visible();
        let now = Utc::now();
        let range: HashSet<String> = self.range_ids().into_iter().collect();
        let rows: Vec<Row> = vis
            .iter()
            .map(|&i| {
                let it = &self.items[i];
                let v = it.effective_verdict();
                let marked = self.disk_sel.marked.contains(&it.id);
                let mark = if self.cleaning.contains(&it.id) {
                    SPINNER[self.tick % SPINNER.len()]
                } else if marked {
                    "✓"
                } else if range.contains(&it.id) {
                    "┃"
                } else {
                    " "
                };
                let row_style = if marked {
                    Style::new().bg(Color::Rgb(28, 52, 34))
                } else if range.contains(&it.id) {
                    Style::new().bg(Color::Rgb(40, 40, 24))
                } else {
                    Style::new()
                };
                let mut name = safe_inline(&it.name);
                if self.pending_ai.contains(&it.id) {
                    name.push_str("  (asking AI…)");
                } else if self.state.ai_notes.contains_key(&it.id) {
                    name.push_str("  ✦");
                }
                Row::new(vec![
                    Cell::from(mark).style(Style::new().fg(Color::Yellow)),
                    Cell::from(if it.protected { "PINNED" } else { v.label() })
                        .style(verdict_style(v)),
                    Cell::from(human(it.bytes)),
                    Cell::from(human(it.reclaimable)).style(Style::new().fg(Color::DarkGray)),
                    Cell::from(ago(it.last_used, now)),
                    Cell::from(it.category.label()).style(Style::new().fg(Color::Blue)),
                    Cell::from(name),
                ])
                .style(row_style)
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
            title += &format!(" · \"{}\"", safe_inline(&self.search));
        }
        if !self.disk_sel.marked.is_empty() {
            let bytes: u64 = self
                .items
                .iter()
                .filter(|i| self.disk_sel.marked.contains(&i.id))
                .map(|i| i.reclaimable)
                .sum();
            title += &format!(
                " · {} marked ({})",
                self.disk_sel.marked.len(),
                human(bytes)
            );
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
        .row_highlight_style(
            Style::new()
                .bg(Color::Rgb(50, 50, 70))
                .add_modifier(Modifier::BOLD),
        )
        .block(Block::bordered().title(title));
        self.disk_sel
            .table
            .select(
                self.selected_index(&vis)
                    .or(if vis.is_empty() { None } else { Some(0) }),
            );
        f.render_stateful_widget(table, list_area, &mut self.disk_sel.table);

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
            Line::from(Span::styled(
                safe_inline(&item.name),
                Style::new().add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                safe_inline(&tilde(&item.path, &self.home)),
                dim,
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled(
                    if item.protected { "PINNED" } else { v.label() },
                    verdict_style(v).add_modifier(Modifier::BOLD),
                ),
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
            lines.push(Line::from(format!("  • {}", safe_inline(r))));
        }
        if let Some(o) = &item.owner {
            lines.push(Line::from(Span::styled(
                format!("  belongs to {}", safe_inline(&tilde(o, &self.home))),
                dim,
            )));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("clean: ", Style::new().add_modifier(Modifier::BOLD)),
            Span::styled(
                safe_inline(&item.action.describe()),
                Style::new().fg(Color::Cyan),
            ),
        ]));
        lines.push(Line::from(""));
        self.push_ai(&mut lines, &item.id);
        Text::from(lines)
    }

    fn push_ai(&self, lines: &mut Vec<Line<'static>>, key: &str) {
        let now = Utc::now();
        if self.pending_ai.contains(key) {
            lines.push(Line::from(Span::styled(
                format!(
                    "{} asking {}…",
                    SPINNER[self.tick % SPINNER.len()],
                    safe_inline(self.cfg.ai.provider.as_str())
                ),
                Style::new().fg(Color::Magenta),
            )));
        } else if let Some(note) = self.state.ai_notes.get(key) {
            lines.push(Line::from(Span::styled(
                format!(
                    "✦ {} · {} ago",
                    safe_inline(&note.provider),
                    ago(Some(note.at), now)
                ),
                Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD),
            )));
        } else {
            lines.push(Line::from(Span::styled(
                format!(
                    "press x to ask {} about this (A to change)",
                    safe_inline(&self.ai_label())
                ),
                Style::new().fg(Color::DarkGray),
            )));
        }
    }

    fn draw_procs(&mut self, f: &mut Frame, area: ratatui::layout::Rect) {
        let [top, detail_area] =
            Layout::vertical([Constraint::Min(6), Constraint::Length(11)]).areas(area);
        let [list_area, apps_area] =
            Layout::horizontal([Constraint::Fill(1), Constraint::Length(42)]).areas(top);
        let list = &self.procs.dev;
        let rows: Vec<Row> = list
            .iter()
            .map(|p| {
                let mut name = safe_inline(&p.name);
                if self.pending_ai.contains(&p.id()) {
                    name.push_str("  (asking AI…)");
                } else if self.state.ai_notes.contains_key(&p.id()) {
                    name.push_str("  ✦");
                }
                Row::new(vec![
                    Cell::from(if self.proc_sel.marked.contains(&p.pid) {
                        "●"
                    } else {
                        " "
                    })
                    .style(Style::new().fg(Color::Yellow)),
                    Cell::from(p.verdict.label()).style(verdict_style(p.verdict)),
                    Cell::from(human(p.rss)),
                    Cell::from(fmt_uptime(p.uptime_secs)),
                    Cell::from(p.pid.to_string()),
                    Cell::from(name),
                    Cell::from(safe_inline(
                        p.reasons.first().map(String::as_str).unwrap_or_default(),
                    ))
                    .style(Style::new().fg(Color::DarkGray)),
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
        .row_highlight_style(
            Style::new()
                .bg(Color::Rgb(50, 50, 70))
                .add_modifier(Modifier::BOLD),
        )
        .block(Block::bordered().title(title));
        let pos = self
            .proc_sel
            .selected
            .and_then(|p| list.iter().position(|x| x.pid == p));
        self.proc_sel
            .table
            .select(pos.or(if list.is_empty() { None } else { Some(0) }));
        f.render_stateful_widget(table, list_area, &mut self.proc_sel.table);

        let m = &self.procs.mem;
        let mut app_lines = vec![
            Line::from(format!("used {} of {}", human(m.used()), human(m.total))),
            Line::from(Span::styled(
                format!("cached files {} (freed on demand)", human(m.cached)),
                Style::new().fg(Color::DarkGray),
            )),
            Line::from(""),
        ];
        for (name, bytes, n) in self
            .procs
            .apps
            .iter()
            .take(apps_area.height.saturating_sub(5) as usize)
        {
            let label = if *n > 1 {
                format!("{} ×{n}", safe_inline(name))
            } else {
                safe_inline(name)
            };
            app_lines.push(Line::from(format!("{:>7}  {}", human(*bytes), label)));
        }
        f.render_widget(
            Paragraph::new(app_lines).block(Block::bordered().title(" Memory by app ")),
            apps_area,
        );

        let mut lines = Vec::new();
        if let Some(p) = self.selected_proc() {
            lines.push(Line::from(Span::styled(
                format!(
                    "{} · pid {} · {} · up {}",
                    safe_inline(&p.name),
                    p.pid,
                    human(p.rss),
                    fmt_uptime(p.uptime_secs)
                ),
                Style::new().add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(Span::styled(
                safe_inline(&p.comm),
                Style::new().fg(Color::DarkGray),
            )));
            for r in &p.reasons {
                lines.push(Line::from(format!("  • {}", safe_inline(r))));
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

    fn draw_history(&self, f: &mut Frame, area: ratatui::layout::Rect) {
        let total: u64 = self.history.iter().filter(|h| h.ok).map(|h| h.bytes).sum();
        let rows: Vec<Row> = self
            .history
            .iter()
            .map(|h| {
                Row::new(vec![
                    Cell::from(
                        h.at.with_timezone(&chrono::Local)
                            .format("%b %-d %H:%M")
                            .to_string(),
                    ),
                    Cell::from(if h.ok { "ok" } else { "FAIL" }).style(if h.ok {
                        Style::new().fg(Color::Green)
                    } else {
                        Style::new().fg(Color::Red)
                    }),
                    Cell::from(if h.bytes > 0 {
                        human(h.bytes)
                    } else {
                        String::new()
                    }),
                    Cell::from(safe_inline(&h.name)),
                    Cell::from(safe_inline(&h.message)).style(Style::new().fg(Color::DarkGray)),
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
            safe_inline(&tilde(
                &state::state_dir().join("history.jsonl"),
                &self.home
            ))
        )));
        f.render_widget(table, area);
    }

    fn draw_footer(&self, f: &mut Frame, area: ratatui::layout::Rect) {
        let line = if self.searching {
            Line::from(vec![
                Span::styled(" / ", Style::new().fg(Color::Black).bg(Color::Yellow)),
                Span::raw(format!(
                    " {}▏  enter keep · esc clear",
                    safe_inline(&self.search)
                )),
            ])
        } else if !self.status.is_empty() {
            Line::from(Span::styled(
                format!(" {}", safe_inline(&self.status)),
                Style::new().fg(Color::Yellow),
            ))
        } else {
            let marks = match self.tab {
                Tab::Disk if !self.disk_sel.marked.is_empty() => {
                    let bytes: u64 = self
                        .items
                        .iter()
                        .filter(|i| self.disk_sel.marked.contains(&i.id))
                        .map(|i| i.reclaimable)
                        .sum();
                    Some(format!(
                        " {} marked ({}) · d clean · u clear ",
                        self.disk_sel.marked.len(),
                        human(bytes)
                    ))
                }
                Tab::Procs if !self.proc_sel.marked.is_empty() => Some(format!(
                    " {} marked · d kill · u clear ",
                    self.proc_sel.marked.len()
                )),
                _ => None,
            };
            let hints = match self.tab {
                Tab::Disk => {
                    "space/J/K/v mark · a SAFE · * all · d clean · x ask · A agent · p pin · o reveal · f c s / filter · r rescan · ? help · q quit"
                }
                Tab::Procs => {
                    "space/J/K mark · * all · d kill · x ask · A agent · r refresh · ? help · q quit"
                }
                Tab::History => "r reload · tab switch · q quit",
            };
            let mut spans = Vec::new();
            if let Some(m) = marks {
                spans.push(Span::styled(
                    m,
                    Style::new()
                        .fg(Color::Black)
                        .bg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            spans.push(Span::styled(
                format!(" {hints}"),
                Style::new().fg(Color::DarkGray),
            ));
            Line::from(spans)
        };
        f.render_widget(Paragraph::new(line), area);
    }

    fn draw_confirm_clean(&self, f: &mut Frame, ids: &[String], skipped: &[String]) {
        let items: Vec<&Item> = ids
            .iter()
            .filter_map(|id| self.items.iter().find(|i| &i.id == id))
            .collect();
        let total: u64 = items.iter().map(|i| i.reclaimable).sum();
        let not_safe = items
            .iter()
            .filter(|i| i.effective_verdict() != Verdict::Safe)
            .count();
        let mut lines = vec![
            Line::from(Span::styled(
                format!(
                    "Clean {} item(s) and free about {}?",
                    items.len(),
                    human(total)
                ),
                Style::new().add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
        ];
        if !skipped.is_empty() {
            lines.push(Line::from(Span::styled(
                format!(
                    "{} marked item(s) are skipped: KEEP, pinned, or no automatic action.",
                    skipped.len()
                ),
                Style::new().fg(Color::Magenta),
            )));
            lines.push(Line::from(""));
        }
        if not_safe > 0 {
            lines.push(Line::from(Span::styled(
                format!("⚠ {not_safe} of these are not marked SAFE. Read their details first."),
                Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(""));
        }
        let home = self.home.display().to_string();
        for i in items.iter().take(12) {
            let v = i.effective_verdict();
            lines.push(Line::from(vec![
                Span::styled(format!("{:<6} ", v.label()), verdict_style(v)),
                Span::raw(format!(
                    "{:>7}  {}",
                    human(i.reclaimable),
                    safe_inline(&i.name)
                )),
            ]));
            lines.push(Line::from(Span::styled(
                format!(
                    "         $ {}",
                    safe_inline(&i.action.describe().replace(&home, "~"))
                ),
                Style::new().fg(Color::DarkGray),
            )));
        }
        if items.len() > 12 {
            lines.push(Line::from(format!("… and {} more", items.len() - 12)));
        }
        modal(
            f,
            " Confirm cleanup ",
            Text::from(lines),
            confirm_keys("clean", Color::Green),
            110,
        );
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
            lines.push(Line::from(format!("  pid {pid:<7} {}", safe_inline(name))));
        }
        if pids.len() > 14 {
            lines.push(Line::from(format!("… and {} more", pids.len() - 14)));
        }
        modal(
            f,
            " Confirm kill ",
            Text::from(lines),
            confirm_keys("kill", Color::Red),
            70,
        );
    }
}

pub(crate) fn verdict_style(v: Verdict) -> Style {
    Style::new().fg(match v {
        Verdict::Safe => Color::Green,
        Verdict::Review => Color::Yellow,
        Verdict::Active => Color::Cyan,
        Verdict::Keep => Color::Magenta,
    })
}

#[cfg(test)]
mod tests {
    use super::super::Tab;
    use super::super::app::App;
    use super::super::app::test_support::{app_with_items, key, render, safe_item};
    use crate::procs;
    use crate::scan;
    use ratatui::crossterm::event::KeyCode;

    #[test]
    fn renders_every_tab_wide_and_narrow() {
        let mut app = app_with_items();
        for (w, h) in [(180, 45), (80, 24), (40, 12)] {
            for tab in [Tab::Disk, Tab::Procs, Tab::History] {
                app.tab = tab;
                let screen = render(&mut app, w, h);
                assert!(screen.contains("Dustpan"), "{w}x{h}\n{screen}");
            }
        }
        app.tab = Tab::Disk;
        let screen = render(&mut app, 180, 45);
        assert!(screen.contains("no longer exists"));
        assert!(screen.contains("SAFE"));
        assert!(screen.contains("rm -rf"));
    }

    #[test]
    fn disk_table_keeps_viewport_while_scrolling_back_up() {
        let mut app = app_with_items();
        app.items = (0..200).map(safe_item).collect();
        for _ in 0..150 {
            key(&mut app, KeyCode::Down);
        }
        render(&mut app, 180, 20);
        let low_offset = app.disk_sel.table.offset();
        assert!(low_offset > 0);
        for _ in 0..140 {
            key(&mut app, KeyCode::Up);
        }
        render(&mut app, 180, 20);
        let vis = app.visible();
        let selected = app.selected_index(&vis).unwrap();
        let offset = app.disk_sel.table.offset();
        assert!(offset < low_offset);
        assert!(selected >= offset && selected < offset + 13);
    }

    #[test]
    fn rendered_external_text_has_no_terminal_controls() {
        let mut app = app_with_items();
        app.items[0].name = "bad\x1b[2J\u{0085}name".into();
        app.items[0].reasons = vec!["reason\x07here".into()];
        let screen = render(&mut app, 180, 45);
        assert!(!screen.contains('\x1b'));
        assert!(!screen.contains('\u{0085}'));
        assert!(!screen.contains('\x07'));
        assert!(screen.contains("bad�[2J�name"));
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
            println!("{}", render(&mut app, 190, 48));
        }
    }
}
