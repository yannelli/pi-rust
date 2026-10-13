//! The mode-independent extension UI (upstream `ExtensionUIContext`). The extension host calls
//! it; the TUI implements it with dialogs and RPC mode with `extension_ui_request` records.
//! Methods that hand over TUI components stay on the TUI side, since they need a terminal.

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

/// The run mode an extension sees as `ctx.mode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExtensionMode {
    Tui,
    Rpc,
    Json,
    Print,
}

impl ExtensionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tui => "tui",
            Self::Rpc => "rpc",
            Self::Json => "json",
            Self::Print => "print",
        }
    }

    /// Whether dialogs work (`ctx.hasUI`): true in the TUI and RPC modes.
    pub fn has_ui(self) -> bool {
        matches!(self, Self::Tui | Self::Rpc)
    }
}

/// Options of a dialog (upstream `ExtensionUIDialogOptions`).
#[derive(Clone, Debug, Default)]
pub struct DialogOptions {
    /// Dismiss the dialog with its default value after this long.
    pub timeout: Option<Duration>,
    /// Dismiss the dialog with its default value when cancelled.
    pub cancel: Option<CancellationToken>,
}

impl DialogOptions {
    pub fn with_timeout(timeout: Duration) -> Self {
        Self { timeout: Some(timeout), cancel: None }
    }
}

/// Severity of a notification; upstream defaults to `info`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NotifyType {
    #[default]
    Info,
    Warning,
    Error,
}

impl NotifyType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

/// Where a widget goes; upstream defaults to `aboveEditor`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WidgetPlacement {
    #[default]
    AboveEditor,
    BelowEditor,
}

impl WidgetPlacement {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AboveEditor => "aboveEditor",
            Self::BelowEditor => "belowEditor",
        }
    }
}

/// The streaming loader's look (upstream `WorkingIndicatorOptions`): empty frames hide it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkingIndicatorOptions {
    pub frames: Option<Vec<String>>,
    pub interval_ms: Option<u64>,
}

/// A theme the UI can switch to (upstream `getAllThemes()` entries).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThemeInfo {
    pub name: String,
    pub path: Option<PathBuf>,
}

/// UI requests from extensions. Dialogs resolve to their default value (`None` or `false`)
/// when dismissed, cancelled, or timed out; the other methods do not wait.
#[async_trait]
pub trait ExtensionUi: Send + Sync {
    /// Pick one of `options`.
    async fn select(&self, title: &str, options: &[String], opts: DialogOptions) -> Option<String>;
    /// Yes or no; `false` when dismissed.
    async fn confirm(&self, title: &str, message: &str, opts: DialogOptions) -> bool;
    /// One line of free text.
    async fn input(&self, title: &str, placeholder: Option<&str>, opts: DialogOptions) -> Option<String>;
    /// Multi-line text, starting from `prefill`.
    async fn editor(&self, title: &str, prefill: Option<&str>, opts: DialogOptions) -> Option<String>;

    fn notify(&self, message: &str, kind: Option<NotifyType>);
    /// Set (or clear with `None`) a footer status entry.
    fn set_status(&self, key: &str, text: Option<&str>);
    /// Set (or clear with `None`) a block of text lines around the editor.
    fn set_widget(&self, key: &str, lines: Option<&[String]>, placement: Option<WidgetPlacement>);
    /// The terminal window or tab title.
    fn set_title(&self, title: &str);
    fn set_editor_text(&self, text: &str);

    /// Paste into the editor; without paste handling this is [`Self::set_editor_text`].
    fn paste_to_editor(&self, text: &str) {
        self.set_editor_text(text);
    }

    fn get_editor_text(&self) -> String {
        String::new()
    }

    fn set_working_message(&self, _message: Option<&str>) {}

    fn set_working_visible(&self, _visible: bool) {}

    fn set_working_indicator(&self, _options: Option<WorkingIndicatorOptions>) {}

    fn set_hidden_thinking_label(&self, _label: Option<&str>) {}

    fn get_all_themes(&self) -> Vec<ThemeInfo> {
        Vec::new()
    }

    /// Switch the theme by name; the error is shown to the extension.
    fn set_theme(&self, _name: &str) -> Result<(), String> {
        Err("UI not available".into())
    }

    fn get_tools_expanded(&self) -> bool {
        false
    }

    fn set_tools_expanded(&self, _expanded: bool) {}
}

/// The UI of modes without one (print and JSON): dialogs return their defaults at once.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoOpExtensionUi;

#[async_trait]
impl ExtensionUi for NoOpExtensionUi {
    async fn select(&self, _title: &str, _options: &[String], _opts: DialogOptions) -> Option<String> {
        None
    }

    async fn confirm(&self, _title: &str, _message: &str, _opts: DialogOptions) -> bool {
        false
    }

    async fn input(&self, _title: &str, _placeholder: Option<&str>, _opts: DialogOptions) -> Option<String> {
        None
    }

    async fn editor(&self, _title: &str, _prefill: Option<&str>, _opts: DialogOptions) -> Option<String> {
        None
    }

    fn notify(&self, _message: &str, _kind: Option<NotifyType>) {}

    fn set_status(&self, _key: &str, _text: Option<&str>) {}

    fn set_widget(&self, _key: &str, _lines: Option<&[String]>, _placement: Option<WidgetPlacement>) {}

    fn set_title(&self, _title: &str) {}

    fn set_editor_text(&self, _text: &str) {}
}
