use super::draw::verdict_style;
use crate::model::Verdict;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};

pub(crate) fn confirm_keys(verb: &str, color: Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            " y ",
            Style::new()
                .fg(Color::Black)
                .bg(color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(" {verb}   ")),
        Span::styled(
            " any other key ",
            Style::new().fg(Color::Black).bg(Color::DarkGray),
        ),
        Span::raw(" cancel"),
    ])
}

pub fn wrapped_rows(text: &Text, width: u16) -> u16 {
    let width = width.max(1) as usize;
    text.lines
        .iter()
        .map(|l| {
            let w = l.width();
            if w <= width { 1 } else { w.div_ceil(width) + 1 }
        })
        .sum::<usize>()
        .min(u16::MAX as usize) as u16
}

pub(crate) fn modal(f: &mut Frame, title: &str, body: Text, keys: Line, max_w: u16) {
    let area = f.area();
    let w = max_w.min(area.width.saturating_sub(4)).max(20);
    let inner_w = w.saturating_sub(2);
    let h = (wrapped_rows(&body, inner_w) + 4)
        .min(area.height.saturating_sub(2))
        .max(5);
    let rect = Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    };
    f.render_widget(Clear, rect);
    let block = Block::bordered()
        .title(title.to_string())
        .border_style(Style::new().fg(Color::Yellow));
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    let [body_area, _, keys_area] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    f.render_widget(Paragraph::new(body).wrap(Wrap { trim: false }), body_area);
    f.render_widget(Paragraph::new(keys), keys_area);
}

pub(crate) fn draw_help(f: &mut Frame) {
    let rows = [
        ("", "STATES"),
        (
            "SAFE",
            "leftover with nothing depending on it (gone project, merged branch, orphan cache)",
        ),
        (
            "REVIEW",
            "probably removable, but it costs something (rebuild, re-download) or is unrecognized",
        ),
        ("IN USE", "tied to recent work or a running process"),
        (
            "KEEP",
            "uncommitted or unpushed work, dedicated to a live worktree, or pinned by you",
        ),
        ("", ""),
        ("", "SELECT"),
        ("space", "mark / unmark the current row"),
        (
            "J K ⇧↑⇧↓",
            "mark the current row and move (hold to sweep a run of rows)",
        ),
        (
            "v",
            "start a range at this row; v or space again marks everything in between",
        ),
        (
            "a / *",
            "mark every SAFE item in view / every cleanable item in view",
        ),
        ("u", "clear all marks"),
        (
            "d",
            "clean the marked items (or the current one) after a confirm; kills on Processes",
        ),
        ("", ""),
        ("", "OTHER KEYS"),
        ("j/k ↑↓", "move · g/G top/bottom · PgUp/PgDn"),
        ("x / X", "ask the AI about the current / every marked item"),
        ("A", "choose the AI agent, model, and effort"),
        ("p", "pin: never clean this item (saved across runs)"),
        ("o / y", "reveal in Finder / copy path"),
        (
            "f c s /",
            "filter by state · filter by kind · change sort · search",
        ),
        ("r", "rescan (disk) · refresh (processes)"),
        (
            "1 2 3 tab",
            "switch tabs · q quits · esc closes dialogs and clears a range",
        ),
        ("", ""),
        (
            "",
            "Config: ~/.config/dustpan/config.toml · state + history: ~/.local/state/dustpan",
        ),
    ];
    let lines: Vec<Line> = rows
        .iter()
        .map(|(k, v)| {
            if k.is_empty() {
                Line::from(Span::styled(
                    v.to_string(),
                    Style::new().add_modifier(Modifier::BOLD),
                ))
            } else {
                let style = match *k {
                    "SAFE" => verdict_style(Verdict::Safe),
                    "REVIEW" => verdict_style(Verdict::Review),
                    "IN USE" => verdict_style(Verdict::Active),
                    "KEEP" => verdict_style(Verdict::Keep),
                    _ => Style::new().fg(Color::Cyan),
                };
                Line::from(vec![
                    Span::styled(format!("  {k:<11}"), style),
                    Span::raw(v.to_string()),
                ])
            }
        })
        .collect();
    let keys = Line::from(Span::styled(
        "any key closes",
        Style::new().fg(Color::DarkGray),
    ));
    modal(f, " Help ", Text::from(lines), keys, 108);
}

#[cfg(test)]
mod tests {
    use super::super::app::test_support::{app_with_items, key, render, safe_item};
    use ratatui::crossterm::event::KeyCode;

    #[test]
    fn confirm_keys_stay_visible_when_lines_wrap() {
        let mut app = app_with_items();
        app.items.extend((0..12).map(safe_item));
        key(&mut app, KeyCode::Char('a'));
        key(&mut app, KeyCode::Char('d'));
        for (w, h) in [(80, 24), (100, 30), (190, 48)] {
            let screen = render(&mut app, w, h);
            assert!(
                screen.contains(" y  clean"),
                "confirm keys missing at {w}x{h}\n{screen}"
            );
        }
    }
}
