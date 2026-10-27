//! Terminal UI library with differential rendering, ported from the upstream `pi-tui` package.
//! <!-- concise-ignore: crate-level documentation of the API and the threading model -->
//!
//! # Building blocks
//!
//! - [`Component`]: `render(width) -> lines`, optional input, mouse, focus, and layout hooks.
//!   Components are shared as [`ComponentRef`] (`Rc<RefCell<dyn Component>>`); identity matters
//!   for focus and overlays.
//! - [`Tui`]: a cheap-clone handle to the renderer. [`Tui::new_main_screen`] renders into the
//!   scrollback with differential line updates; [`Tui::new_alt_screen`] owns a fullscreen viewport
//!   with scroll views, scrollbars, wheel, selection, copy-on-select, and transcript search.
//! - [`Terminal`]: the terminal behind a trait. [`ProcessTerminal`] reads raw stdin bytes on a
//!   background thread and parses them like upstream (Kitty keyboard protocol, xterm
//!   modifyOtherKeys, bracketed paste); [`testing::VirtualTerminal`] emulates a terminal in tests.
//! - Key handling: [`keys`] (parsing and `matches_key`), [`keybindings`] (ids, defaults, user
//!   overrides), [`stdin_buffer`] (splitting batched input).
//! - Text: [`utils`] (visible width, ANSI-aware truncate, wrap, slice, OSC 8 links), [`colors`],
//!   [`terminal_image`] (Kitty and iTerm2 images, capability detection).
//!
//! # Threading model
//!
//! The UI is single-threaded, like upstream. Create the [`Tui`], its components, and the terminal
//! on one thread (the UI thread) and drive that thread's loop with [`ui_loop::run_once`] (blocking)
//! or [`ui_loop::run_once_async`] (on a current-thread runtime or inside a `tokio::task::LocalSet`).
//! Each pass delivers terminal input and resizes, runs due timers ([`ui_loop::set_timeout`],
//! [`ui_loop::set_interval`]), and renders when a render was requested.
//!
//! Other threads (the agent loop, providers, a multi-thread tokio runtime) never touch components.
//! They reach the UI thread through [`TuiWaker`] (`Send + Sync + Clone`, from [`ui_loop::waker`]):
//! [`TuiWaker::request_render`], [`TuiWaker::post`] to run a closure on the UI thread, or
//! [`TuiWaker::send_message`] to hand over a message that [`ui_loop::run_once`] returns. A plain
//! channel to the UI thread plus [`TuiWaker::wake`] works too.
//!
//! ```no_run
//! use pi_tui::{ProcessTerminal, Tui, TuiOptions, ui_loop};
//! use std::{cell::RefCell, rc::Rc};
//!
//! let terminal = Rc::new(RefCell::new(ProcessTerminal::new()));
//! let tui = Tui::new_main_screen(terminal, TuiOptions::default());
//! let waker = ui_loop::waker();
//! std::thread::spawn(move || waker.send_message("agent finished"));
//! tui.start();
//! loop {
//!     let messages = ui_loop::run_once(None).expect("render");
//!     if !messages.is_empty() {
//!         break;
//!     }
//! }
//! tui.stop(Default::default());
//! ```
//!
//! # Environment
//!
//! Read here: `PI_TUI_ESC_TIMEOUT`, `PI_TUI_WRITE_LOG`, `PI_TUI_DEBUG`, `PI_TUI_DEBUG_REDRAW`,
//! `PI_PROGRAM_STATUS`, `PI_IMAGE_PROTOCOL`, `PI_TRUE_COLOR`, `PI_HYPERLINKS`, `TERM`,
//! `TERM_PROGRAM`, `COLORTERM`, `COLORFGBG`, `TMUX`, `ZELLIJ`, `STY`, `KITTY_WINDOW_ID`,
//! `ITERM_SESSION_ID`, `WT_SESSION`, `WEZTERM_PANE`, `GHOSTTY_RESOURCES_DIR`, `TERMUX_VERSION`,
//! `SSH_CONNECTION`, `SSH_CLIENT`, `SSH_TTY`, `COLUMNS`, `LINES`, `DISPLAY`. `PI_HARDWARE_CURSOR`
//! and `PI_CLEAR_ON_SHRINK` are settings of the application: pass them through
//! [`TuiOptions::show_hardware_cursor`] and [`Tui::set_clear_on_shrink`].

pub mod alt_screen;
pub mod alt_screen_search;
pub mod autocomplete;
pub mod colors;
pub mod component;
pub mod components;
mod debug_log;
pub mod editor_component;
pub mod env;
pub mod fuzzy;
pub mod html;
pub mod keybindings;
pub mod keys;
pub mod kill_ring;
pub mod latex;
pub mod layout;
pub mod layout_node;
pub mod main_screen;
pub mod native_platform;
pub mod oklab;
pub mod process_terminal;
pub mod program_status;
pub mod stdin_buffer;
pub mod terminal;
pub mod terminal_colors;
pub mod terminal_image;
pub mod testing;
pub mod tui;
pub mod ui_loop;
pub mod undo_stack;
pub mod utils;
pub mod wheel_scroll;
pub mod word_navigation;

pub use alt_screen::{CopyCompletion, CopyOutcome, CopyRequest, CopySelectionFn, TuiAltScreenOptions};
pub use autocomplete::{AutocompleteItem, AutocompleteProvider, CombinedAutocompleteProvider, SlashCommand};
pub use component::{
    AsAny, CURSOR_MARKER, Component, ComponentRef, Container, FAKE_CURSOR_END, FAKE_CURSOR_START, MouseResponse,
    TuiMouseButton, TuiMouseDispatchResult, TuiMouseDispatchTarget, TuiMouseEvent, TuiMouseEventResult,
    TuiMouseEventType, component, dispatch_mouse_event, is_focusable, render_fake_cursor, retarget_mouse_event,
    same_component,
};
pub use components::{
    AltScreenFlashContainer, HStack, Input, InputOptions, ScrollView, ScrollViewHandle, ScrollViewOptions,
    ScrollViewScrollToOptions, ScrollViewScrollbar, StackChild, StackEntryOptions, StackOptions, VStack,
};
pub use components::{
    BoxComponent, CancellableLoader, Image, Loader, Markdown, MouseRegion, SelectList, SettingsList, Spacer, Text,
    TruncatedText,
};
pub use components::{Editor, EditorOptions, EditorTheme};
pub use editor_component::EditorComponent;
pub use keybindings::{KeybindingsManager, get_keybindings, set_keybindings};
pub use keys::{Key, KeyEventType, is_key_release, is_key_repeat, matches_key, parse_key};
pub use main_screen::TuiMainScreenRenderState;
pub use stdin_buffer::{StdinBuffer, StdinBufferEvent, StdinBufferOptions};
pub use terminal::{ProcessTerminal, Terminal, TerminalRef, is_apple_terminal_session};
pub use tui::{
    OverlayAnchor, OverlayBounds, OverlayHandle, OverlayMargin, OverlayOptions, OverlayUnfocusOptions, SizeValue, Tui,
    TuiError, TuiInputListenerResult, TuiListenerId, TuiMode, TuiOptions, TuiStopOptions, WeakTui, composite_tui_line,
    restore_terminals_on_exit,
};
pub use ui_loop::{TerminalSink, TimerId, TuiWaker};
pub use utils::{
    get_osc8_link_at_column, replace_control_characters, slice_by_column, strip_terminal_sequences, truncate_to_width,
    visible_width, wrap_text_with_ansi,
};
pub use wheel_scroll::WheelScrollLines;
