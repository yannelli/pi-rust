//! [`FirstTimeSetupComponent`]: theme choice and analytics opt-in on first start
//! (upstream `first-time-setup.ts`), plus the startup checks from upstream `startup-ui.ts`.

use std::path::Path;

use pi_tui::{Component, Container, get_keybindings};

use super::common::{border, spacer, text};
use super::{key_hint, raw_key_hint};
use crate::resources::brand::{APP_NAME, CONFIG_DIR_NAME};
use crate::settings::SettingsManager;
use crate::theme::{SYSTEM_THEME_NAME, ThemeColor, theme};

/// What the user picked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirstTimeSetupResult {
    pub theme: String,
    pub share_analytics: bool,
}

const THEME_OPTIONS: [(&str, &str); 3] =
    [(SYSTEM_THEME_NAME, "System (matches your terminal colors)"), ("dark", "Dark"), ("light", "Light")];

const ANALYTICS_OPTIONS: [(bool, &str); 2] = [(true, "Share anonymous usage data"), (false, "Don't share")];

const SETUP_LOGO_LINES: [&str; 4] = ["██████", "██  ██", "████  ██", "██    ██"];

const ANALYTICS_DESCRIPTION: &str = "Opting in stores a tracking identifier in settings.json and enables anonymous\nusage analytics. This helps us to better debug, reproduce, and resolve issues\nand bugs within Pi. You can observe what is shared using /privacy and make\nchanges anytime in settings.json.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Theme,
    Analytics,
}

/// First-time setup dialog: theme choice and analytics opt-in.
pub struct FirstTimeSetupComponent {
    container: Container,
    step: Step,
    theme_index: usize,
    analytics_index: usize,
    on_theme_preview: Box<dyn FnMut(&str)>,
    on_submit: Box<dyn FnMut(FirstTimeSetupResult)>,
    on_cancel: Box<dyn FnMut()>,
}

impl FirstTimeSetupComponent {
    /// `on_theme_preview` gets the theme name whenever the highlighted theme changes; the host applies it.
    pub fn new(
        on_theme_preview: impl FnMut(&str) + 'static,
        on_submit: impl FnMut(FirstTimeSetupResult) + 'static,
        on_cancel: impl FnMut() + 'static,
    ) -> Self {
        let mut component = Self {
            container: Container::new(),
            step: Step::Theme,
            theme_index: 0,
            analytics_index: 0,
            on_theme_preview: Box::new(on_theme_preview),
            on_submit: Box::new(on_submit),
            on_cancel: Box::new(on_cancel),
        };
        component.update();
        component
    }

    // Rebuild the whole dialog on every change so theme previews recolor all text.
    fn update(&mut self) {
        let theme = theme();
        let container = &mut self.container;
        container.clear();
        container.add_child(border());
        container.add_child(spacer(1));
        container.add_child(text(&theme.fg(ThemeColor::Accent, &SETUP_LOGO_LINES.join("\n")), 1, 0));
        container.add_child(spacer(1));
        let welcome = format!("Welcome to {APP_NAME}, the minimal coding agent.");
        container.add_child(text(&theme.fg(ThemeColor::Accent, &theme.bold(&welcome)), 1, 0));
        container.add_child(spacer(1));

        if self.step == Step::Theme {
            container.add_child(text(&theme.fg(ThemeColor::Text, "Pick a theme."), 1, 0));
            container.add_child(spacer(1));
            let labels: Vec<&str> = THEME_OPTIONS.iter().map(|(_, label)| *label).collect();
            add_option_list(container, &labels, self.theme_index);
        } else {
            let question = "Opt-in to anonymous usage data sharing?";
            container.add_child(text(&theme.fg(ThemeColor::Text, question), 1, 0));
            container.add_child(text(&theme.fg(ThemeColor::Muted, ANALYTICS_DESCRIPTION), 1, 0));
            container.add_child(spacer(1));
            let labels: Vec<&str> = ANALYTICS_OPTIONS.iter().map(|(_, label)| *label).collect();
            add_option_list(container, &labels, self.analytics_index);
        }

        container.add_child(spacer(1));
        let confirm = if self.step == Step::Theme { "continue" } else { "finish" };
        let hints = format!(
            "{}  {}  {}",
            raw_key_hint("↑↓", "navigate"),
            key_hint("tui.select.confirm", confirm),
            key_hint("tui.select.cancel", "skip setup"),
        );
        container.add_child(text(&hints, 1, 0));
        container.add_child(spacer(1));
        container.add_child(border());
    }

    fn move_selection(&mut self, delta: isize) {
        if self.step == Step::Theme {
            let next = self.theme_index.saturating_add_signed(delta).min(THEME_OPTIONS.len() - 1);
            if next != self.theme_index {
                self.theme_index = next;
                (self.on_theme_preview)(THEME_OPTIONS[next].0);
            }
        } else {
            self.analytics_index = self.analytics_index.saturating_add_signed(delta).min(ANALYTICS_OPTIONS.len() - 1);
        }
        self.update();
    }
}

fn add_option_list(container: &mut Container, labels: &[&str], selected_index: usize) {
    let theme = theme();
    for (index, label) in labels.iter().enumerate() {
        let selected = index == selected_index;
        let prefix = if selected { theme.fg(ThemeColor::Accent, "→ ") } else { "  ".to_string() };
        let label = theme.fg(if selected { ThemeColor::Accent } else { ThemeColor::Text }, label);
        container.add_child(text(&format!("{prefix}{label}"), 1, 0));
    }
}

impl Component for FirstTimeSetupComponent {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.container.render(width)
    }

    fn handle_input(&mut self, data: &str) {
        let kb = get_keybindings();
        if kb.matches(data, "tui.select.up") || data == "k" {
            self.move_selection(-1);
        } else if kb.matches(data, "tui.select.down") || data == "j" {
            self.move_selection(1);
        } else if kb.matches(data, "tui.select.confirm") || data == "\n" {
            if self.step == Step::Theme {
                self.step = Step::Analytics;
                self.update();
            } else {
                (self.on_submit)(FirstTimeSetupResult {
                    theme: THEME_OPTIONS[self.theme_index].0.to_string(),
                    share_analytics: ANALYTICS_OPTIONS[self.analytics_index].0,
                });
            }
        } else if kb.matches(data, "tui.select.cancel") {
            (self.on_cancel)();
        }
    }

    fn handles_input(&self) -> bool {
        true
    }

    /// Rebuild on theme changes, e.g. when the system theme receives the terminal's colors.
    fn invalidate(&mut self) {
        self.update();
        self.container.invalidate();
    }
}

/// Persists a setup result the way upstream `showFirstTimeSetup` does.
pub fn apply_first_time_setup_result(settings: &SettingsManager, result: &FirstTimeSetupResult) {
    settings.set_theme(&result.theme);
    settings.set_enable_analytics(result.share_analytics);
    settings.flush();
}

/// Package, app, and config directory names of a build; forks and rebrands differ from the official ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DistributionMetadata<'a> {
    pub package_name: &'a str,
    pub app_name: &'a str,
    pub config_dir_name: &'a str,
}

const OFFICIAL_PACKAGE_NAME: &str = "@earendil-works/pi-coding-agent";

/// The package name of this build: the official one unless the build sets `PI_PACKAGE_NAME`.
pub const PACKAGE_NAME: &str = match option_env!("PI_PACKAGE_NAME") {
    Some(name) => name,
    None => OFFICIAL_PACKAGE_NAME,
};

/// This build's distribution metadata.
pub const DISTRIBUTION: DistributionMetadata<'static> =
    DistributionMetadata { package_name: PACKAGE_NAME, app_name: APP_NAME, config_dir_name: CONFIG_DIR_NAME };

fn is_official_distribution(distribution: &DistributionMetadata<'_>) -> bool {
    distribution.package_name == OFFICIAL_PACKAGE_NAME
        && distribution.app_name == "pi"
        && distribution.config_dir_name == ".pi"
}

/// Whether to run first-time setup: the official distribution, `PI_EXPERIMENTAL=1`, no custom agent
/// directory, and no `settings.json` yet.
pub fn should_run_first_time_setup(settings_path: &Path) -> bool {
    should_run_first_time_setup_with(&DISTRIBUTION, |name| std::env::var(name).ok(), settings_path)
}

/// [`should_run_first_time_setup`] with an explicit distribution and environment lookup.
pub fn should_run_first_time_setup_with(
    distribution: &DistributionMetadata<'_>,
    env: impl Fn(&str) -> Option<String>,
    settings_path: &Path,
) -> bool {
    if !is_official_distribution(distribution) {
        return false;
    }
    if env("PI_EXPERIMENTAL").as_deref() != Some("1") {
        return false;
    }
    if env(&crate::resources::brand::agent_dir_env_var()).is_some_and(|dir| !dir.is_empty()) {
        return false;
    }
    !settings_path.exists()
}
