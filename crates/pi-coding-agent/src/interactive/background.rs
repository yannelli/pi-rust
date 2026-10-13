//! Suspend with Ctrl+Z, and the update checks started after startup (upstream `handleCtrlZ`,
//! `showNewVersionNotification`, and `showPackageUpdateNotification`).

use pi_tui::component;
use pi_tui::components::markdown::{DefaultTextStyle, MarkdownOptions};
use pi_tui::components::{Markdown, Spacer};
use pi_tui::terminal_image::get_capabilities;
use pi_tui::terminal_image::hyperlink;

use super::components::ThemedText;
use super::mode::InteractiveMode;
use crate::interactive_components::DynamicBorder;
use crate::packages::{PackageManager, PackageManagerOptions};
use crate::platform::http::VERSION;
use crate::platform::version_check::{LatestPiRelease, VersionCheckOptions, check_for_new_pi_version};
use crate::resources::brand::APP_NAME;
use crate::theme::{ThemeColor, theme};

const RELEASES_URL: &str = "https://github.com/yannelli/pi-rust/releases";

fn warning_border() -> DynamicBorder {
    DynamicBorder::with_color(|text: &str| theme().fg(ThemeColor::Warning, text))
}

impl InteractiveMode {
    /// Ctrl+Z: restore the terminal, stop the process group, and redraw on `fg`.
    pub(super) fn handle_ctrl_z(&self) {
        #[cfg(windows)]
        self.show_status("Suspend to background is not supported on Windows");
        #[cfg(unix)]
        self.defer(|mode| {
            let tui = mode.tui();
            tui.stop(pi_tui::TuiStopOptions::default());
            if let Err(error) = rustix::process::kill_current_process_group(rustix::process::Signal::TSTP) {
                mode.show_error(&format!("Could not suspend: {error}"));
            }
            tui.start();
            tui.request_render(true);
        });
    }

    /// The new release and package update checks; each shows a notice when it finds something.
    pub(super) fn start_version_check(&self) {
        let skip = crate::platform::version_check::version_check_skipped();
        self.spawn(move |mode| async move {
            let check = async move { check_for_new_pi_version(VERSION, &VersionCheckOptions::default(), skip).await };
            if let Ok(Some(release)) = tokio::spawn(check).await {
                mode.show_new_version_notification(&release);
            }
        });
        let session = self.session();
        let options = PackageManagerOptions::new(session.cwd(), &self.agent_dir, session.settings().clone());
        self.spawn(move |mode| async move {
            let check = async move { PackageManager::new(options).check_for_available_updates().await };
            let Ok(updates) = tokio::spawn(check).await else { return };
            if !updates.is_empty() {
                let names: Vec<String> = updates.into_iter().map(|update| update.display_name).collect();
                mode.show_package_update_notification(&names);
            }
        });
    }

    pub(super) fn show_new_version_notification(&self, release: &LatestPiRelease) {
        let version = release.version.clone();
        let header = move || {
            let title = theme().bold(&theme().fg(ThemeColor::Warning, "Update Available"));
            let instruction = theme().fg(ThemeColor::Muted, &format!("New version {version} is available. Run "));
            format!("{title}\n{instruction}{}", theme().fg(ThemeColor::Accent, &format!("{APP_NAME} update")))
        };
        self.add_to_chat(component(Spacer::new(1)));
        self.add_to_chat(component(warning_border()));
        self.add_to_chat(component(ThemedText::new(header, 1, 0)));
        if let Some(note) = release.note.as_deref().map(str::trim).filter(|note| !note.is_empty()) {
            let style = DefaultTextStyle {
                color: Some(std::rc::Rc::new(|text: &str| theme().fg(ThemeColor::Muted, text))),
                ..Default::default()
            };
            let markdown = self.render_options().markdown_theme;
            self.add_to_chat(component(Spacer::new(1)));
            self.add_to_chat(component(Markdown::new(note, 1, 0, markdown, Some(style), MarkdownOptions::default())));
            self.add_to_chat(component(Spacer::new(1)));
        }
        let releases = move || {
            let link = theme().fg(ThemeColor::Accent, RELEASES_URL);
            let link = if get_capabilities().hyperlinks { hyperlink(&link, RELEASES_URL) } else { link };
            format!("{}{link}", theme().fg(ThemeColor::Muted, "Changelog: "))
        };
        self.add_to_chat(component(ThemedText::new(releases, 1, 0)));
        self.add_to_chat(component(warning_border()));
        self.request_render();
    }

    pub(super) fn show_package_update_notification(&self, packages: &[String]) {
        let lines: Vec<String> = packages.iter().map(|name| format!("- {name}")).collect();
        let lines = lines.join("\n");
        let text = move || {
            let title = theme().bold(&theme().fg(ThemeColor::Warning, "Package Updates Available"));
            let instruction = theme().fg(ThemeColor::Muted, "Package updates are available. Run ")
                + &theme().fg(ThemeColor::Accent, &format!("{APP_NAME} update --extensions"));
            format!("{title}\n{instruction}\n{}\n{lines}", theme().fg(ThemeColor::Muted, "Packages:"))
        };
        self.add_to_chat(component(Spacer::new(1)));
        self.add_to_chat(component(warning_border()));
        self.add_to_chat(component(ThemedText::new(text, 1, 0)));
        self.add_to_chat(component(warning_border()));
        self.request_render();
    }
}
