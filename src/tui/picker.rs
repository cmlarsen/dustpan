use super::Modal;
use super::SPINNER;
use super::app::App;
use super::modal::modal;
use crate::catalog::Catalog;
use crate::config::{AiConfig, Provider};
use crate::report::safe_inline;
use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};

pub(crate) fn with_current(current: &str, presets: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    if !current.is_empty() {
        v.push(current.to_string());
    }
    for p in presets {
        if !v.iter().any(|x| x == p) {
            v.push(p.to_string());
        }
    }
    v
}

pub(crate) fn current_model(ai: &AiConfig) -> &str {
    if ai.provider == Provider::Codex {
        &ai.codex_model
    } else {
        &ai.claude_model
    }
}

pub(crate) fn model_ids(cat: Option<&Catalog>, provider: Provider, current: &str) -> Vec<String> {
    let ids: Vec<&str> = cat
        .map(|c| c.models(provider).iter().map(|m| m.id.as_str()).collect())
        .unwrap_or_default();
    with_current(current, &ids)
}

pub(crate) fn efforts(cat: Option<&Catalog>, provider: Provider, model: &str) -> Vec<String> {
    match cat {
        Some(c) => c.efforts_for(provider, model),
        None => Catalog::default().efforts_for(provider, model),
    }
}

pub(crate) fn cycle(list: &[String], current: &str, dir: isize) -> String {
    if list.is_empty() {
        return current.to_string();
    }
    let pos = list.iter().position(|x| x == current).unwrap_or(0) as isize;
    let n = list.len() as isize;
    list[((pos + dir).rem_euclid(n)) as usize].clone()
}

impl App {
    pub(crate) fn picker_key(&mut self, k: KeyEvent, row: usize) {
        let ai = &mut self.cfg.ai;
        let dir = match k.code {
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Char(' ') => 1,
            KeyCode::Left | KeyCode::Char('h') => -1,
            KeyCode::Down | KeyCode::Char('j') => {
                self.modal = Modal::AiPicker((row + 1).min(2));
                return;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.modal = Modal::AiPicker(row.saturating_sub(1));
                return;
            }
            _ => {
                self.state.ai = crate::state::AiChoice {
                    provider: Some(ai.provider),
                    claude_model: Some(ai.claude_model.clone()),
                    codex_model: Some(ai.codex_model.clone()),
                    effort: Some(ai.effort.clone()),
                };
                if self.persist {
                    let _ = self.state.save();
                }
                self.status = format!("AI: {} · saved for next time", self.ai_label());
                return;
            }
        };
        match row {
            0 => {
                ai.provider = match ai.provider {
                    Provider::Claude => Provider::Codex,
                    Provider::Codex => Provider::Claude,
                };
            }
            1 => {
                let origin = if ai.provider == Provider::Codex {
                    &self.picker_origin.1
                } else {
                    &self.picker_origin.0
                };
                let ids = model_ids(self.catalog.as_ref(), ai.provider, origin);
                let next = cycle(&ids, current_model(ai), dir);
                if ai.provider == Provider::Codex {
                    ai.codex_model = next;
                } else {
                    ai.claude_model = next;
                }
            }
            _ => {
                let efforts = efforts(self.catalog.as_ref(), ai.provider, current_model(ai));
                ai.effort = cycle(&efforts, &ai.effort, dir);
            }
        }
        if row < 2 {
            let supported = efforts(self.catalog.as_ref(), ai.provider, current_model(ai));
            if !supported.contains(&ai.effort) {
                let default = self
                    .catalog
                    .as_ref()
                    .and_then(|c| c.default_effort(ai.provider, current_model(ai)));
                ai.effort = ["medium".to_string()]
                    .into_iter()
                    .chain(default)
                    .find(|e| supported.contains(e))
                    .or_else(|| supported.first().cloned())
                    .unwrap_or_default();
            }
        }
        self.modal = Modal::AiPicker(row);
    }

    pub(crate) fn open_picker(&mut self) {
        self.modal = Modal::AiPicker(0);
        self.picker_origin = (
            self.cfg.ai.claude_model.clone(),
            self.cfg.ai.codex_model.clone(),
        );
        if self.catalog.is_none() && !self.catalog_loading {
            self.catalog_loading = true;
            let tx = self.tx.clone();
            std::thread::spawn(move || {
                let _ = tx.send(super::Msg::Catalog(Box::new(crate::catalog::fetch())));
            });
        }
    }

    pub(crate) fn ai_label(&self) -> String {
        let ai = &self.cfg.ai;
        let model = if ai.provider == Provider::Codex {
            &ai.codex_model
        } else {
            &ai.claude_model
        };
        let model = if model.is_empty() {
            "default model"
        } else {
            model.as_str()
        };
        let effort = if ai.effort.is_empty() {
            "default effort"
        } else {
            ai.effort.as_str()
        };
        format!("{} {model} · {effort}", ai.provider)
    }

    pub(crate) fn draw_ai_picker(&self, f: &mut Frame, row: usize) {
        let ai = &self.cfg.ai;
        let model = current_model(ai);
        let rows = [
            ("agent", ai.provider.as_str()),
            ("model", model),
            ("effort", ai.effort.as_str()),
        ];
        let mut lines = vec![
            Line::from("Who answers when you press x on an item or process."),
            Line::from(""),
        ];
        let dim = Style::new().fg(Color::DarkGray);
        for (i, (label, value)) in rows.iter().enumerate() {
            let selected = i == row;
            let style = if selected {
                Style::new()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::new()
            };
            let value = if value.is_empty() {
                "(CLI default)"
            } else {
                value
            };
            lines.push(Line::from(vec![
                Span::raw(if selected { " ▸ " } else { "   " }),
                Span::styled(format!("{label:<8}"), Style::new().fg(Color::DarkGray)),
                Span::styled(format!(" ◂ {} ▸ ", safe_inline(value)), style),
            ]));
        }
        lines.push(Line::from(""));
        match &self.catalog {
            None => lines.push(Line::from(Span::styled(
                format!(
                    "{} asking claude and codex for their models…",
                    SPINNER[self.tick % SPINNER.len()]
                ),
                Style::new().fg(Color::Cyan),
            ))),
            Some(cat) => {
                let listed = cat.models(ai.provider);
                match listed.iter().find(|m| m.id == model) {
                    Some(m) if !m.about.is_empty() => {
                        lines.push(Line::from(Span::styled(safe_inline(&m.about), dim)))
                    }
                    Some(_) => {}
                    None => lines.push(Line::from(Span::styled(
                        format!(
                            "{} is from your config; {} doesn't list it",
                            safe_inline(model),
                            safe_inline(ai.provider.as_str())
                        ),
                        Style::new().fg(Color::Yellow),
                    ))),
                }
                lines.push(Line::from(Span::styled(
                    format!(
                        "{} models · efforts: {}",
                        listed.len(),
                        safe_inline(&efforts(Some(cat), ai.provider, model).join(", "))
                    ),
                    dim,
                )));
                for n in &cat.notes {
                    lines.push(Line::from(Span::styled(
                        format!("note: {}", safe_inline(n)),
                        dim,
                    )));
                }
            }
        }
        let keys = Line::from(Span::styled(
            "↑↓ choose · ←→ change · any other key saves and closes",
            Style::new().fg(Color::DarkGray),
        ));
        modal(f, " AI agent ", Text::from(lines), keys, 72);
    }
}

#[cfg(test)]
mod tests {
    use super::super::app::test_support::{app_with_items, key, render};
    use super::*;
    use ratatui::crossterm::event::KeyCode;

    #[test]
    fn ai_picker_switches_agent_and_effort() {
        let mut app = app_with_items();
        key(&mut app, KeyCode::Char('A'));
        assert!(render(&mut app, 120, 30).contains("AI agent"));
        key(&mut app, KeyCode::Right);
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Right);
        key(&mut app, KeyCode::Enter);
        assert!(matches!(app.modal, Modal::None));
        assert_eq!(app.cfg.ai.provider, Provider::Codex);
        assert_eq!(app.cfg.ai.effort, "high");
        assert_eq!(app.state.ai.provider, Some(Provider::Codex));
        assert_eq!(app.ai_label(), "codex gpt-5.6-sol · high");
    }

    fn catalog() -> Catalog {
        use crate::catalog::ModelOption;
        let m = |id: &str, efforts: &[&str], default: &str| ModelOption {
            id: id.into(),
            about: format!("{id} about"),
            efforts: efforts.iter().map(|e| e.to_string()).collect(),
            default_effort: Some(default.into()),
        };
        Catalog {
            codex: vec![
                m("gpt-a", &["low", "medium", "high"], "medium"),
                m("gpt-b", &["low", "xhigh"], "low"),
            ],
            claude: vec![m("opus", &[], ""), m("sonnet", &[], "")],
            claude_efforts: vec![
                "low".into(),
                "medium".into(),
                "high".into(),
                "xhigh".into(),
                "max".into(),
            ],
            notes: vec!["test note".into()],
        }
    }

    #[test]
    fn picker_lists_queried_models_and_snaps_unsupported_effort() {
        let mut app = app_with_items();
        app.catalog = Some(catalog());
        app.cfg.ai.codex_model = "gpt-a".into();
        key(&mut app, KeyCode::Char('A'));
        assert!(
            !app.catalog_loading,
            "an existing catalog must not be refetched"
        );
        key(&mut app, KeyCode::Right);
        assert_eq!(app.cfg.ai.provider, Provider::Codex);
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Right);
        assert_eq!(app.cfg.ai.codex_model, "gpt-b");
        assert_eq!(
            app.cfg.ai.effort, "low",
            "medium isn't offered by gpt-b, so it snaps to gpt-b's default"
        );
        let screen = render(&mut app, 120, 30);
        assert!(screen.contains("gpt-b about"));
        assert!(screen.contains("efforts: low, xhigh"));
        assert!(screen.contains("test note"));
    }

    #[test]
    fn picker_keeps_a_configured_model_the_catalog_lacks() {
        let mut app = app_with_items();
        app.catalog = Some(catalog());
        app.cfg.ai.claude_model = "claude-opus-5".into();
        key(&mut app, KeyCode::Char('A'));
        let screen = render(&mut app, 120, 30);
        assert!(screen.contains("claude-opus-5 is from your config"));
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Right);
        assert_eq!(app.cfg.ai.claude_model, "opus");
        key(&mut app, KeyCode::Left);
        assert_eq!(app.cfg.ai.claude_model, "claude-opus-5");
    }

    #[test]
    fn cycle_wraps() {
        let list: Vec<String> = ["a", "b", "c"].map(String::from).into();
        assert_eq!(cycle(&list, "c", 1), "a");
        assert_eq!(cycle(&list, "a", -1), "c");
        assert_eq!(with_current("x", &["a", "x"]), vec!["x", "a"]);
    }
}
