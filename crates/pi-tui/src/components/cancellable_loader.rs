//! [`CancellableLoader`]: a [`Loader`] that Escape cancels.

use tokio_util::sync::CancellationToken;

use crate::component::Component;
use crate::components::loader::{Loader, LoaderIndicatorOptions};
use crate::components::scroll_view::StyleFn;
use crate::keybindings::{get_keybindings, ids};
use crate::tui::Tui;

/// A loader with a cancellation token that the cancel key (Escape by default) triggers
/// (upstream `CancellableLoader`, whose `AbortSignal` is a [`CancellationToken`] here).
pub struct CancellableLoader {
    loader: Loader,
    token: CancellationToken,
    /// Called when the user presses the cancel key.
    pub on_abort: Option<Box<dyn FnMut()>>,
}

impl CancellableLoader {
    pub fn new(
        ui: Option<&Tui>,
        spinner_color_fn: StyleFn,
        message_color_fn: StyleFn,
        message: &str,
        indicator: Option<LoaderIndicatorOptions>,
    ) -> Self {
        Self {
            loader: Loader::new(ui, spinner_color_fn, message_color_fn, message, indicator),
            token: CancellationToken::new(),
            on_abort: None,
        }
    }

    /// Token cancelled when the user presses the cancel key. Clones share the cancellation.
    pub fn signal(&self) -> CancellationToken {
        self.token.clone()
    }

    /// Whether the loader was cancelled.
    pub fn aborted(&self) -> bool {
        self.token.is_cancelled()
    }

    pub fn loader(&mut self) -> &mut Loader {
        &mut self.loader
    }

    pub fn set_message(&mut self, message: &str) {
        self.loader.set_message(message);
    }

    /// Stops the animation.
    pub fn dispose(&mut self) {
        self.loader.stop();
    }
}

impl Component for CancellableLoader {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.loader.render(width)
    }

    fn handle_input(&mut self, data: &str) {
        if get_keybindings().matches(data, ids::SELECT_CANCEL) {
            self.token.cancel();
            if let Some(on_abort) = self.on_abort.as_mut() {
                on_abort();
            }
        }
    }

    fn handles_input(&self) -> bool {
        true
    }

    fn invalidate(&mut self) {
        self.loader.invalidate();
    }
}
