//! Status line segments: what each one shows, its theme role, and its icon. Codex
//! `[tui] status_line` item names work as aliases, so a Codex list renders as is.

use std::sync::OnceLock;

use pi_ai::ThinkingLevel;
use serde_json::Value;

use super::themes::Role;
use super::{RunState, StatusSnapshot};
use crate::interactive::components::footer::format_cwd_for_footer;
use crate::interactive::util::format_tokens;

const BAR_CELLS: usize = 8;

macro_rules! segments {
    ($($variant:ident => $name:literal $(| $alias:literal)*),* $(,)?) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum Segment { $($variant),* }

        impl Segment {
            pub const ALL: &[Segment] = &[$(Segment::$variant),*];

            /// The name used in `statusLine.segments` and `statusLine.colors`.
            pub fn name(self) -> &'static str {
                match self { $(Segment::$variant => $name),* }
            }

            /// A segment name or a Codex item name.
            pub fn from_name(name: &str) -> Option<Self> {
                match name { $($name $(| $alias)* => Some(Segment::$variant),)* _ => None }
            }
        }
    };
}

segments! {
    Cwd => "cwd" | "current-dir",
    Dir => "dir",
    Project => "project" | "project-name",
    Branch => "branch" | "git-branch",
    Session => "session" | "thread-title" | "session-name",
    SessionId => "sessionId" | "session-id" | "thread-id",
    Model => "model",
    ModelWithReasoning => "modelWithReasoning" | "model-with-reasoning",
    Provider => "provider",
    Thinking => "thinking" | "reasoning",
    Tokens => "tokens",
    TokensIn => "tokensIn" | "total-input-tokens",
    TokensOut => "tokensOut" | "total-output-tokens",
    TokensUsed => "tokensUsed" | "used-tokens",
    Cache => "cache",
    CacheHit => "cacheHit",
    Cost => "cost",
    Context => "context",
    ContextUsed => "contextUsed" | "context-used",
    ContextRemaining => "contextRemaining" | "context-remaining",
    ContextWindow => "contextWindow" | "context-window-size",
    ContextBar => "contextBar",
    Version => "version" | "codex-version",
    RunState => "runState" | "run-state",
    Time => "time",
    User => "user",
    Host => "host",
    Logo => "logo",
    Fill => "fill",
}

impl Segment {
    pub fn role(self, snapshot: &StatusSnapshot) -> Role {
        let percent = snapshot.context_percent().unwrap_or(0.0);
        match self {
            Segment::Cwd | Segment::Dir | Segment::Project => Role::Path,
            Segment::Branch => Role::Git,
            Segment::Session | Segment::SessionId => Role::Session,
            Segment::Model | Segment::ModelWithReasoning | Segment::Provider | Segment::Logo => Role::Model,
            Segment::Thinking => Role::Thinking,
            Segment::Tokens | Segment::TokensIn | Segment::TokensOut | Segment::TokensUsed => Role::Tokens,
            Segment::Cache | Segment::CacheHit => Role::Cache,
            Segment::Cost => Role::Cost,
            Segment::Context
            | Segment::ContextUsed
            | Segment::ContextRemaining
            | Segment::ContextWindow
            | Segment::ContextBar => {
                if percent > 90.0 {
                    Role::ContextHigh
                } else if percent > 70.0 {
                    Role::ContextWarn
                } else {
                    Role::ContextOk
                }
            }
            Segment::Version | Segment::User | Segment::Host | Segment::Fill => Role::Meta,
            Segment::RunState => Role::State,
            Segment::Time => Role::Time,
        }
    }
}

/// One entry of `segments`: a segment, or literal `text`, with an optional color and icon.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub kind: ItemKind,
    pub color: Option<String>,
    pub icon: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ItemKind {
    Segment(Segment),
    Text(String),
}

impl Item {
    pub fn segment(segment: Segment) -> Self {
        Self { kind: ItemKind::Segment(segment), color: None, icon: None }
    }

    /// `"model"`, `{"segment": "model", "color": "#hex", "icon": "◆"}`, or `{"text": "λ"}`.
    pub fn parse(value: &Value) -> Option<Self> {
        if let Some(name) = value.as_str() {
            return Segment::from_name(name).map(Self::segment);
        }
        let object = value.as_object()?;
        let field = |key: &str| object.get(key).and_then(Value::as_str).map(str::to_string);
        let kind = match (field("segment"), field("text")) {
            (Some(name), _) => ItemKind::Segment(Segment::from_name(&name)?),
            (None, Some(text)) => ItemKind::Text(text),
            (None, None) => return None,
        };
        Some(Self { kind, color: field("color"), icon: field("icon") })
    }

    pub fn is_fill(&self) -> bool {
        self.kind == ItemKind::Segment(Segment::Fill)
    }
}

/// `unicode` (the default), `nerd` (needs a Nerd Font), `emoji`, or `none`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icons {
    Unicode,
    Nerd,
    Emoji,
    None,
}

impl Icons {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "unicode" => Some(Icons::Unicode),
            "nerd" => Some(Icons::Nerd),
            "emoji" => Some(Icons::Emoji),
            "none" => Some(Icons::None),
            _ => None,
        }
    }

    /// The icon before a segment, if the set has one.
    pub fn icon(self, segment: Segment) -> Option<&'static str> {
        use Segment::*;
        match (self, segment) {
            (Icons::None, _) => None,
            (Icons::Unicode, Branch) => Some("⎇"),
            (Icons::Unicode, Time) => Some("◷"),
            (Icons::Unicode, _) => None,
            (Icons::Nerd, Cwd | Dir | Project) => Some("\u{f07b}"),
            (Icons::Nerd, Branch) => Some("\u{e0a0}"),
            (Icons::Nerd, Model | ModelWithReasoning) => Some("\u{f06a9}"),
            (Icons::Nerd, Thinking) => Some("\u{f0335}"),
            (Icons::Nerd, Cost) => Some("\u{f155}"),
            (Icons::Nerd, Context | ContextUsed | ContextRemaining | ContextWindow | ContextBar) => Some("\u{f080}"),
            (Icons::Nerd, Tokens | TokensIn | TokensOut | TokensUsed) => Some("\u{f0e7}"),
            (Icons::Nerd, Time) => Some("\u{f017}"),
            (Icons::Nerd, Session | SessionId) => Some("\u{f075}"),
            (Icons::Nerd, User) => Some("\u{f007}"),
            (Icons::Nerd, Host) => Some("\u{f108}"),
            (Icons::Emoji, Cwd | Dir | Project) => Some("📁"),
            (Icons::Emoji, Branch) => Some("🌿"),
            (Icons::Emoji, Model | ModelWithReasoning) => Some("🤖"),
            (Icons::Emoji, Thinking) => Some("🧠"),
            (Icons::Emoji, Cost) => Some("💰"),
            (Icons::Emoji, Context | ContextUsed | ContextRemaining | ContextWindow | ContextBar) => Some("📊"),
            (Icons::Emoji, Tokens | TokensIn | TokensOut | TokensUsed) => Some("⚡"),
            (Icons::Emoji, Time) => Some("⏱"),
            (Icons::Emoji, Session | SessionId) => Some("💬"),
            _ => None,
        }
    }
}

fn dir_name(path: &std::path::Path, home: Option<&std::path::Path>) -> String {
    if home.is_some_and(|home| home == path) {
        return "~".to_string();
    }
    path.file_name().map_or_else(|| path.display().to_string(), |name| name.to_string_lossy().into_owned())
}

fn user_and_host() -> &'static (Option<String>, Option<String>) {
    static CELL: OnceLock<(Option<String>, Option<String>)> = OnceLock::new();
    CELL.get_or_init(|| {
        let env = |names: &[&str]| names.iter().find_map(|name| std::env::var(name).ok().filter(|v| !v.is_empty()));
        let host = env(&["HOSTNAME", "COMPUTERNAME"]).or_else(|| {
            std::fs::read_to_string("/etc/hostname").ok().map(|name| name.trim().to_string()).filter(|n| !n.is_empty())
        });
        (env(&["USER", "USERNAME"]), host.map(|host| host.split('.').next().unwrap_or(&host).to_string()))
    })
}

/// What a segment shows, without its icon; `None` when it has nothing to show.
pub fn segment_text(segment: Segment, snapshot: &StatusSnapshot) -> Option<String> {
    let totals = &snapshot.totals;
    let percent = snapshot.context_percent();
    let join = |parts: Vec<Option<String>>| {
        let parts: Vec<String> = parts.into_iter().flatten().collect();
        (!parts.is_empty()).then(|| parts.join(" "))
    };
    let level = || {
        snapshot.model.as_ref().filter(|model| model.reasoning).map(|_| match snapshot.thinking {
            ThinkingLevel::Off => "thinking off".to_string(),
            level => level.as_str().to_string(),
        })
    };
    match segment {
        Segment::Cwd => Some(format_cwd_for_footer(&snapshot.cwd, snapshot.home.as_deref())),
        Segment::Dir => Some(dir_name(&snapshot.cwd, snapshot.home.as_deref())),
        Segment::Project => snapshot.project.clone().or_else(|| Some(dir_name(&snapshot.cwd, None))),
        Segment::Branch => snapshot.branch.clone(),
        Segment::Session => snapshot.session_name.clone(),
        Segment::SessionId => Some(snapshot.session_id.chars().take(8).collect()).filter(|id: &String| !id.is_empty()),
        Segment::Model => Some(match (&snapshot.model, &snapshot.routed) {
            (Some(model), Some((routed, _))) => format!("{} → {routed}", model.id),
            (Some(model), None) => model.id.clone(),
            (None, _) => "no-model".to_string(),
        }),
        Segment::ModelWithReasoning => {
            let model = segment_text(Segment::Model, snapshot)?;
            Some(match level() {
                Some(level) => format!("{model} {level}"),
                None => model,
            })
        }
        Segment::Provider => snapshot.model.as_ref().map(|model| model.provider.clone()),
        Segment::Thinking => level(),
        Segment::Tokens => join(vec![
            (totals.input > 0).then(|| format!("↑{}", format_tokens(totals.input))),
            (totals.output > 0).then(|| format!("↓{}", format_tokens(totals.output))),
        ]),
        Segment::TokensIn => Some(format!("{} in", format_tokens(totals.input))),
        Segment::TokensOut => Some(format!("{} out", format_tokens(totals.output))),
        Segment::TokensUsed => {
            let used = totals.input + totals.output + totals.cache_read + totals.cache_write;
            Some(format!("{} used", format_tokens(used)))
        }
        Segment::Cache => join(vec![
            (totals.cache_read > 0).then(|| format!("R{}", format_tokens(totals.cache_read))),
            (totals.cache_write > 0).then(|| format!("W{}", format_tokens(totals.cache_write))),
            segment_text(Segment::CacheHit, snapshot),
        ]),
        Segment::CacheHit => snapshot
            .cache_hit_rate
            .filter(|_| totals.cache_read > 0 || totals.cache_write > 0)
            .map(|rate| format!("CH{rate:.1}%")),
        Segment::Cost => (totals.cost != 0.0 || snapshot.subscription)
            .then(|| format!("${:.3}{}", totals.cost, if snapshot.subscription { " (sub)" } else { "" })),
        Segment::Context => {
            let window = format_tokens(snapshot.context_window?);
            let auto = if snapshot.auto_compact { " (auto)" } else { "" };
            Some(match percent {
                Some(percent) => format!("{percent:.1}%/{window}{auto}"),
                None => format!("?/{window}{auto}"),
            })
        }
        Segment::ContextUsed => percent.map(|percent| format!("{percent:.0}% used")),
        Segment::ContextRemaining => percent.map(|percent| format!("{:.0}% left", (100.0 - percent).max(0.0))),
        Segment::ContextWindow => snapshot.context_window.map(|window| format!("{} window", format_tokens(window))),
        Segment::ContextBar => {
            snapshot.context_window?;
            let percent = percent.unwrap_or(0.0).clamp(0.0, 100.0);
            let filled = ((percent / 100.0) * BAR_CELLS as f64).round() as usize;
            Some(format!("{}{} {percent:.0}%", "▰".repeat(filled), "▱".repeat(BAR_CELLS - filled)))
        }
        Segment::Version => Some(format!("v{}", env!("CARGO_PKG_VERSION"))),
        Segment::RunState => Some(
            match snapshot.run_state {
                RunState::Idle => "○ idle",
                RunState::Working => "● working",
                RunState::Compacting => "◐ compacting",
            }
            .to_string(),
        ),
        Segment::Time => jiff::Zoned::now().strftime("%H:%M").to_string().into(),
        Segment::User => user_and_host().0.clone(),
        Segment::Host => user_and_host().1.clone(),
        Segment::Logo => Some("π".to_string()),
        Segment::Fill => None,
    }
}
