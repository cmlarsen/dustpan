use crate::util::run;
use serde::Deserialize;
use std::time::Duration;

const FALLBACK_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh"];
const FALLBACK_ALIASES: &[&str] = &["opus", "sonnet", "haiku"];

#[derive(Debug, Clone, PartialEq)]
pub struct ModelOption {
    pub id: String,
    pub about: String,
    pub efforts: Vec<String>,
    pub default_effort: Option<String>,
}

impl ModelOption {
    fn plain(id: &str, about: &str) -> Self {
        ModelOption {
            id: id.to_string(),
            about: about.to_string(),
            efforts: Vec::new(),
            default_effort: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Catalog {
    pub claude: Vec<ModelOption>,
    pub claude_efforts: Vec<String>,
    pub codex: Vec<ModelOption>,
    pub notes: Vec<String>,
}

impl Catalog {
    pub fn models(&self, provider: &str) -> &[ModelOption] {
        if provider == "codex" { &self.codex } else { &self.claude }
    }

    pub fn efforts_for(&self, provider: &str, model: &str) -> Vec<String> {
        let from_model = self
            .models(provider)
            .iter()
            .find(|m| m.id == model)
            .map(|m| m.efforts.clone())
            .unwrap_or_default();
        if !from_model.is_empty() {
            return from_model;
        }
        if provider != "codex" && !self.claude_efforts.is_empty() {
            return self.claude_efforts.clone();
        }
        FALLBACK_EFFORTS.iter().map(|s| s.to_string()).collect()
    }

    pub fn default_effort(&self, provider: &str, model: &str) -> Option<String> {
        self.models(provider)
            .iter()
            .find(|m| m.id == model)
            .and_then(|m| m.default_effort.clone())
    }
}

pub fn parse_codex_catalog(json: &str) -> Vec<ModelOption> {
    #[derive(Deserialize)]
    struct Level {
        effort: String,
    }
    #[derive(Deserialize)]
    struct Model {
        slug: String,
        #[serde(default)]
        description: String,
        #[serde(default)]
        visibility: String,
        #[serde(default)]
        default_reasoning_level: Option<String>,
        #[serde(default)]
        supported_reasoning_levels: Vec<Level>,
    }
    #[derive(Deserialize)]
    struct Root {
        models: Vec<Model>,
    }
    let Ok(root) = serde_json::from_str::<Root>(json) else { return Vec::new() };
    root.models
        .into_iter()
        .filter(|m| m.visibility != "hide")
        .map(|m| ModelOption {
            id: m.slug,
            about: m.description,
            efforts: m.supported_reasoning_levels.into_iter().map(|l| l.effort).collect(),
            default_effort: m.default_reasoning_level,
        })
        .collect()
}

pub fn parse_claude_help(help: &str) -> (Vec<String>, Vec<String>) {
    let flat = help.split_whitespace().collect::<Vec<_>>().join(" ");
    let section = |flag: &str| -> String {
        flat.split(flag)
            .nth(1)
            .map(|rest| rest.split(" --").next().unwrap_or("").to_string())
            .unwrap_or_default()
    };
    let model_text = section("--model <model>");
    let aliases: Vec<String> = model_text
        .split_whitespace()
        .map(|t| t.trim_end_matches([',', '.', ')', ';']))
        .filter_map(|t| t.strip_prefix('\'')?.strip_suffix('\''))
        .filter(|w| !w.is_empty() && w.chars().all(|c| c.is_ascii_alphanumeric()))
        .map(String::from)
        .collect();
    let effort_text = section("--effort <level>");
    let efforts: Vec<String> = effort_text
        .split_once('(')
        .and_then(|(_, r)| r.split_once(')'))
        .map(|(inside, _)| {
            inside
                .split(',')
                .map(|e| e.trim().to_string())
                .filter(|e| !e.is_empty() && !e.contains(' '))
                .collect()
        })
        .unwrap_or_default();
    (aliases, efforts)
}

pub fn parse_anthropic_models(json: &str) -> Vec<ModelOption> {
    #[derive(Deserialize)]
    struct M {
        id: String,
        #[serde(default)]
        display_name: String,
    }
    #[derive(Deserialize)]
    struct Page {
        data: Vec<M>,
    }
    serde_json::from_str::<Page>(json)
        .map(|p| p.data.into_iter().map(|m| ModelOption::plain(&m.id, &m.display_name)).collect())
        .unwrap_or_default()
}

fn anthropic_api_models() -> Option<Vec<ModelOption>> {
    let key = std::env::var("ANTHROPIC_API_KEY").ok().filter(|k| !k.is_empty())?;
    let header = std::env::temp_dir().join(format!("dustpan-hdr-{}", std::process::id()));
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&header)
            .ok()?;
        writeln!(f, "x-api-key: {key}\nanthropic-version: 2023-06-01").ok()?;
    }
    let header_arg = format!("@{}", header.display());
    let out = run(
        "curl",
        &["-sf", "--max-time", "8", "-H", &header_arg, "https://api.anthropic.com/v1/models?limit=100"],
        None,
        Duration::from_secs(10),
    );
    let _ = std::fs::remove_file(&header);
    let models = parse_anthropic_models(&out?.stdout);
    (!models.is_empty()).then_some(models)
}

pub fn fetch() -> Catalog {
    let mut cat = Catalog::default();
    let (codex, claude_help, api) = std::thread::scope(|s| {
        let codex = s.spawn(|| run("codex", &["debug", "models"], None, Duration::from_secs(15)));
        let help = s.spawn(|| run("claude", &["--help"], None, Duration::from_secs(15)));
        let api = s.spawn(anthropic_api_models);
        (
            codex.join().ok().flatten(),
            help.join().ok().flatten(),
            api.join().ok().flatten(),
        )
    });

    match codex {
        Some(out) if out.ok => {
            cat.codex = parse_codex_catalog(&out.stdout);
            if cat.codex.is_empty() {
                cat.notes.push("codex returned no models".into());
            }
        }
        _ => cat.notes.push("codex CLI not found or `codex debug models` failed".into()),
    }

    let (mut aliases, efforts) = claude_help
        .filter(|o| o.ok)
        .map(|o| parse_claude_help(&o.stdout))
        .unwrap_or_else(|| {
            cat.notes.push("claude CLI not found".into());
            (Vec::new(), Vec::new())
        });
    for a in FALLBACK_ALIASES {
        if !aliases.iter().any(|x| x == a) {
            aliases.push(a.to_string());
        }
    }
    cat.claude_efforts = efforts;
    cat.claude = aliases
        .iter()
        .map(|a| ModelOption::plain(a, &format!("alias: the latest {a} model (can change when a new one ships)")))
        .collect();
    match api {
        Some(models) => cat.claude.extend(models),
        None => cat.notes.push(
            "Claude Code can't list model IDs; set ANTHROPIC_API_KEY to load them from the Models API".into(),
        ),
    }
    cat
}

#[cfg(test)]
mod tests {
    use super::*;

    const CODEX: &str = r#"{"models":[
      {"slug":"gpt-6-astra","description":"Frontier","visibility":"list","default_reasoning_level":"low",
       "supported_reasoning_levels":[{"effort":"low"},{"effort":"medium"},{"effort":"max"}]},
      {"slug":"codex-auto-review","visibility":"hide","supported_reasoning_levels":[]},
      {"slug":"gpt-5.5","description":"Legacy","visibility":"list","default_reasoning_level":"medium",
       "supported_reasoning_levels":[{"effort":"low"},{"effort":"xhigh"}]}]}"#;

    const HELP: &str = "  --effort <level>                      Effort level for the current session\n                                        (low, medium, high, xhigh, max)\n  --environment <id>   stuff\n  --model <model>                       Model for the current session. Provide\n                                        an alias for the latest model (e.g.\n                                        'fable', 'opus', or 'sonnet') or a\n                                        model's full name (e.g.\n                                        'claude-fable-5').\n  --name <n>  x\n";

    #[test]
    fn codex_catalog_skips_hidden_models_and_keeps_efforts() {
        let m = parse_codex_catalog(CODEX);
        assert_eq!(m.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(), ["gpt-6-astra", "gpt-5.5"]);
        assert_eq!(m[0].efforts, ["low", "medium", "max"]);
        assert_eq!(m[0].default_effort.as_deref(), Some("low"));
        assert!(parse_codex_catalog("not json").is_empty());
    }

    #[test]
    fn claude_help_yields_aliases_and_efforts() {
        let (aliases, efforts) = parse_claude_help(HELP);
        assert_eq!(aliases, ["fable", "opus", "sonnet"]);
        assert_eq!(efforts, ["low", "medium", "high", "xhigh", "max"]);
        assert_eq!(parse_claude_help("nothing here"), (vec![], vec![]));
    }

    #[test]
    fn anthropic_models_page() {
        let json = r#"{"data":[{"id":"claude-opus-5","display_name":"Claude Opus 5","type":"model"}],"has_more":false}"#;
        assert_eq!(parse_anthropic_models(json)[0].id, "claude-opus-5");
    }

    #[test]
    fn efforts_come_from_the_model_then_the_provider_then_a_fallback() {
        let cat = Catalog {
            codex: parse_codex_catalog(CODEX),
            claude: vec![ModelOption::plain("opus", "")],
            claude_efforts: vec!["low".into(), "max".into()],
            notes: vec![],
        };
        assert_eq!(cat.efforts_for("codex", "gpt-5.5"), ["low", "xhigh"]);
        assert_eq!(cat.efforts_for("claude", "opus"), ["low", "max"]);
        assert_eq!(cat.efforts_for("codex", "unknown"), FALLBACK_EFFORTS);
        assert_eq!(cat.default_effort("codex", "gpt-6-astra").as_deref(), Some("low"));
    }

    #[test]
    #[ignore = "runs the real claude and codex CLIs"]
    fn fetch_real_catalog() {
        let cat = fetch();
        println!("{cat:#?}");
        assert!(!cat.claude.is_empty());
    }
}
