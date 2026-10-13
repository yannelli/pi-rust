//! The component tree of the mode and the renderers it mounts into (upstream `chat-viewport.ts`
//! and `tui-renderer.ts`).

use std::cell::RefCell;
use std::path::Path;
use std::rc::{Rc, Weak};
use std::time::Duration;

use pi_tui::components::{
    ScrollFollow, ScrollOverscroll, ScrollViewScrollbar, StackChild, StackEntryOptions, StackOptions,
};
use pi_tui::layout_node::StackBasis;
use pi_tui::terminal_image::{CapabilityOverrides, ImageProtocol, set_capability_overrides};
use pi_tui::{
    ComponentRef, Container, ScrollView, ScrollViewHandle, ScrollViewOptions, TerminalRef, Tui, TuiAltScreenOptions,
    TuiMode as RendererMode, TuiOptions, VStack, WheelScrollLines, component,
};

use super::mode::InteractiveMode;
use crate::interactive_components::key_display_text;
use crate::settings::SettingsManager;
use crate::settings::schema::{FullscreenScrollbar, TuiMode};
use crate::theme::{TerminalTheme, ThemeBg, ThemeColor, ThemeHost, theme};

/// The containers of the transcript and the input dock, kept across renderer switches.
pub(super) struct Layout {
    pub header: Rc<RefCell<Container>>,
    pub loaded_resources: Rc<RefCell<Container>>,
    pub chat: Rc<RefCell<Container>>,
    pub document: Rc<RefCell<Container>>,
    pub pending: Rc<RefCell<Container>>,
    pub status: Rc<RefCell<Container>>,
    pub editor: Rc<RefCell<Container>>,
    pub footer: Rc<RefCell<Container>>,
    /// Fullscreen layout: the transcript scroll view above the fixed input dock.
    pub root: ComponentRef,
    pub transcript: ScrollViewHandle,
}

fn shared_container() -> Rc<RefCell<Container>> {
    Rc::new(RefCell::new(Container::new()))
}

fn entry(child: ComponentRef, options: StackEntryOptions) -> StackChild {
    StackChild::Entry(child, options)
}

impl Layout {
    pub fn new(editor: Rc<RefCell<Container>>, settings: &SettingsManager) -> Self {
        let (header, loaded_resources, chat) = (shared_container(), shared_container(), shared_container());
        let mut document = Container::new();
        document.add_child(header.clone());
        document.add_child(loaded_resources.clone());
        document.add_child(chat.clone());
        let document = Rc::new(RefCell::new(document));
        let (pending, status, footer) = (shared_container(), shared_container(), shared_container());
        let transcript = ScrollView::new(
            document.clone(),
            ScrollViewOptions {
                follow: ScrollFollow::End,
                primary: true,
                overscroll: ScrollOverscroll::Chain,
                scrollbar: scrollbar_setting(settings.fullscreen_scrollbar()),
                scrollbar_track_style: Some(Rc::new(|text: &str| theme().fg(ThemeColor::ScrollbarTrack, text))),
                scrollbar_thumb_style: Some(Rc::new(|text: &str| theme().fg(ThemeColor::ScrollbarThumb, text))),
                scrollbar_hide_delay_ms: None,
            },
        );
        let transcript_handle = transcript.handle();
        let dock_options =
            |min_size| StackEntryOptions { shrink: Some(1), min_size: Some(min_size), ..Default::default() };
        let dock = VStack::new(
            vec![
                entry(pending.clone(), dock_options(0)),
                entry(status.clone(), dock_options(0)),
                entry(editor.clone(), dock_options(3)),
                entry(footer.clone(), dock_options(0)),
            ],
            StackOptions::default(),
        );
        let transcript_options = StackEntryOptions {
            basis: Some(StackBasis::Fixed(0)),
            grow: Some(1),
            shrink: Some(1),
            min_size: Some(1),
            ..Default::default()
        };
        let dock_root_options = StackEntryOptions {
            basis: Some(StackBasis::Auto),
            grow: Some(0),
            shrink: Some(1),
            min_size: Some(1),
            ..Default::default()
        };
        let root = VStack::new(
            vec![entry(component(transcript), transcript_options), entry(component(dock), dock_root_options)],
            StackOptions::default(),
        );
        Self {
            header,
            loaded_resources,
            chat,
            document,
            pending,
            status,
            editor,
            footer,
            root: component(root),
            transcript: transcript_handle,
        }
    }

    /// The root children of both renderers, top to bottom.
    pub fn roots(&self) -> Vec<ComponentRef> {
        vec![self.document.clone(), self.pending.clone(), self.status.clone(), self.editor.clone(), self.footer.clone()]
    }
}

pub(super) fn scrollbar_setting(setting: FullscreenScrollbar) -> ScrollViewScrollbar {
    match setting {
        FullscreenScrollbar::Auto => ScrollViewScrollbar::Auto,
        FullscreenScrollbar::Always => ScrollViewScrollbar::Always,
        FullscreenScrollbar::Hidden => ScrollViewScrollbar::Hidden,
    }
}

pub(super) fn wheel_lines(settings: &SettingsManager) -> WheelScrollLines {
    match settings.fullscreen_wheel_scroll_lines() {
        Some(lines) => WheelScrollLines::Fixed(lines as f64),
        None => WheelScrollLines::Auto,
    }
}

pub(super) fn renderer_mode(mode: TuiMode) -> RendererMode {
    match mode {
        TuiMode::Regular => RendererMode::Regular,
        TuiMode::Fullscreen => RendererMode::Fullscreen,
    }
}

/// The `terminal.images`, `terminal.trueColor`, and `terminal.hyperlinks` settings.
pub(super) fn apply_capability_overrides(settings: &SettingsManager) {
    let (images, true_color, hyperlinks) = settings.terminal_capability_overrides();
    let images = images.map(|kind| match kind.as_deref() {
        Some("kitty") => Some(ImageProtocol::Kitty),
        Some("iterm2") => Some(ImageProtocol::Iterm2),
        _ => None,
    });
    set_capability_overrides(CapabilityOverrides { images, true_color, hyperlinks });
}

/// A renderer for `mode` on `terminal` (upstream `createInteractiveTui`).
pub(super) fn create_tui(
    mode: TuiMode,
    terminal: TerminalRef,
    settings: &SettingsManager,
    agent_dir: &Path,
    owner: Weak<InteractiveMode>,
) -> Tui {
    let options = TuiOptions {
        show_hardware_cursor: Some(settings.show_hardware_cursor()),
        log_directory: Some(agent_dir.to_path_buf()),
        env: None,
    };
    if mode == TuiMode::Regular {
        return Tui::new_main_screen(terminal, options);
    }
    fn search_match(text: &str) -> String {
        theme().bg(ThemeBg::SearchMatchBg, &theme().fg(ThemeColor::SearchMatchText, text))
    }
    let alt = TuiAltScreenOptions {
        wheel_scroll_lines: Some(wheel_lines(settings)),
        mouse: None,
        search_match_style: Some(Rc::new(|text: &str| theme().underline(&search_match(text)))),
        search_current_match_style: Some(Rc::new(|text: &str| theme().bold(&theme().inverse(&search_match(text))))),
        search_navigation_button_style: Some(Rc::new(|text: &str, hovered: bool| {
            if hovered { theme().underline(text) } else { text.to_string() }
        })),
        scroll_to_end_indicator: Some(Rc::new(|| {
            let shortcut = key_display_text("tui.altScreen.bottom");
            let suffix = if shortcut.is_empty() { String::new() } else { format!(" · {shortcut}") };
            let label = format!(" ↓ Jump to latest message{suffix} ");
            theme().bg(ThemeBg::SelectedBg, &theme().fg(ThemeColor::Text, &label))
        })),
        open_url: Some(Rc::new(|url: &str| pi_ai::auth::oauth::browser::open_browser(url))),
        on_right_click_paste: Some(Rc::new(move || {
            if let Some(mode) = owner.upgrade() {
                mode.handle_right_click_paste();
            }
        })),
        copy_on_select: Some(settings.fullscreen_copy_on_select()),
        copy_selection: Some(Rc::new(|text: String, completion: pi_tui::CopyCompletion| {
            std::thread::spawn(move || {
                let outcome = match crate::clipboard::copy_to_clipboard(&text) {
                    Ok(()) => pi_tui::CopyOutcome::Copied,
                    Err(error) => pi_tui::CopyOutcome::Failed(Some(error.to_string())),
                };
                completion.complete(outcome);
            });
        })),
    };
    Tui::new_alt_screen(terminal, options, alt)
}

/// Theme host that follows the active renderer across mode switches.
pub(super) struct ModeThemeHost(pub Weak<InteractiveMode>);

impl ThemeHost for ModeThemeHost {
    fn query_terminal_colors(
        &self,
        timeout: Duration,
        on_result: Box<dyn FnOnce(pi_tui::terminal_colors::TerminalColors)>,
        on_late_reply: Box<dyn FnOnce(pi_tui::terminal_colors::TerminalColors)>,
    ) {
        match self.0.upgrade() {
            Some(mode) => ThemeHost::query_terminal_colors(&mode.tui(), timeout, on_result, on_late_reply),
            None => on_result(Default::default()),
        }
    }

    fn set_terminal_color_scheme_notifications(&self, enabled: bool) {
        if let Some(mode) = self.0.upgrade() {
            mode.tui().set_terminal_color_scheme_notifications(enabled);
        }
    }

    fn on_terminal_color_scheme_change(&self, listener: Box<dyn FnMut(TerminalTheme)>) -> Box<dyn FnOnce()> {
        match self.0.upgrade() {
            Some(mode) => ThemeHost::on_terminal_color_scheme_change(&mode.tui(), listener),
            None => Box::new(|| {}),
        }
    }

    fn invalidate(&self) {
        if let Some(mode) = self.0.upgrade() {
            mode.tui().invalidate();
        }
    }

    fn request_render(&self) {
        if let Some(mode) = self.0.upgrade() {
            mode.request_render();
        }
    }
}
