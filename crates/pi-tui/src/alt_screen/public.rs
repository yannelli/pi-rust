//! Fullscreen-only methods of [`Tui`] and the clipboard copy callback types.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use super::{selection, viewport, with_alt};
use crate::component::ComponentRef;
use crate::tui::{Tui, TuiInner};
use crate::ui_loop::{self, TuiWaker};
use crate::wheel_scroll::WheelScrollLines;

const COPY_ERROR_FLASH_DURATION: Duration = Duration::from_millis(5000);

/// Result of a clipboard copy started by the fullscreen renderer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopyOutcome {
    /// The text reached the system clipboard.
    Copied,
    /// The copy failed; the message is shown, or "Copy failed" without one.
    Failed(Option<String>),
}

/// Completes a copy request. `Send`, so the copy can finish on another thread.
#[derive(Debug)]
pub struct CopyCompletion {
    waker: TuiWaker,
    tui: u64,
    request: u64,
}

impl CopyCompletion {
    /// Reports the outcome; the UI thread shows "Copied!" or the error as a flash.
    pub fn complete(self, outcome: CopyOutcome) {
        let (tui, request) = (self.tui, self.request);
        self.waker.post(move || {
            let copied = outcome == CopyOutcome::Copied;
            if let Some(inner) = crate::tui::find_tui(tui) {
                finish_copy(&inner, outcome);
            }
            let pending = PENDING_COPIES.with(|pending| pending.borrow_mut().remove(&request));
            if let Some(pending) = pending {
                pending.resolve(copied);
            }
        });
    }
}

#[derive(Default)]
struct CopyRequestState {
    result: Option<bool>,
    waker: Option<Waker>,
}

/// Result of [`Tui::copy_active_selection_to_clipboard`] (upstream `Promise<boolean>`). The copy
/// starts when the request is created; await it or poll [`CopyRequest::result`] for the outcome.
#[derive(Clone, Default)]
pub struct CopyRequest {
    state: Rc<RefCell<CopyRequestState>>,
}

impl CopyRequest {
    fn resolved(copied: bool) -> Self {
        let request = Self::default();
        request.state.borrow_mut().result = Some(copied);
        request
    }

    fn resolve(&self, copied: bool) {
        let waker = {
            let mut state = self.state.borrow_mut();
            state.result = Some(copied);
            state.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// True once the text reached the clipboard, false when nothing was selected or the copy
    /// failed, and `None` while the copy function has not completed yet.
    pub fn result(&self) -> Option<bool> {
        self.state.borrow().result
    }
}

impl Future for CopyRequest {
    type Output = bool;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<bool> {
        let mut state = self.state.borrow_mut();
        match state.result {
            Some(copied) => Poll::Ready(copied),
            None => {
                state.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

thread_local! {
    static PENDING_COPIES: RefCell<HashMap<u64, CopyRequest>> = RefCell::new(HashMap::new());
    static NEXT_COPY_REQUEST: Cell<u64> = const { Cell::new(1) };
}

/// Copies selected text to the system clipboard and reports through the completion.
pub type CopySelectionFn = Rc<dyn Fn(String, CopyCompletion)>;

fn finish_copy(inner: &TuiInner, outcome: CopyOutcome) {
    let (message, duration) = match outcome {
        CopyOutcome::Copied => ("Copied!".to_string(), None),
        CopyOutcome::Failed(message) => {
            (message.unwrap_or_else(|| "Copy failed".to_string()), Some(COPY_ERROR_FLASH_DURATION))
        }
    };
    with_alt(inner, |alt| alt.flashes.flash(&message, duration));
}

/// Copies text with the configured copy function, or with OSC 52 when there is none.
pub(super) fn copy_text_to_clipboard(inner: &TuiInner, text: String) -> CopyRequest {
    let copy = with_alt(inner, |alt| alt.copy_selection.clone()).flatten();
    match copy {
        Some(copy) => {
            let request = NEXT_COPY_REQUEST.with(|next| next.replace(next.get() + 1));
            let pending = CopyRequest::default();
            PENDING_COPIES.with(|pending_copies| pending_copies.borrow_mut().insert(request, pending.clone()));
            copy(text, CopyCompletion { waker: ui_loop::waker(), tui: inner.id, request });
            pending
        }
        None => {
            use base64::Engine;
            let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
            inner.write(&format!("\x1b]52;c;{encoded}\x07"));
            with_alt(inner, |alt| alt.flashes.flash("Copied!", None));
            CopyRequest::resolved(true)
        }
    }
}

impl Tui {
    /// Fullscreen: first content row shown by the primary scroll view.
    pub fn viewport_top(&self) -> usize {
        with_alt(&self.inner, |alt| alt.primary_scroll_view()).map_or(0, |view| view.scroll_top())
    }

    /// Fullscreen: whether the primary scroll view follows new output.
    pub fn is_following_output(&self) -> bool {
        with_alt(&self.inner, |alt| alt.primary_scroll_view()).is_some_and(|view| view.is_following_end())
    }

    /// Fullscreen: lines moved per wheel event.
    pub fn set_wheel_scroll_lines(&self, lines: WheelScrollLines) {
        with_alt(&self.inner, |alt| alt.wheel_scroll.set_lines(lines));
    }

    pub fn get_copy_on_select(&self) -> bool {
        with_alt(&self.inner, |alt| alt.copy_on_select).unwrap_or(false)
    }

    pub fn set_copy_on_select(&self, enabled: bool) {
        with_alt(&self.inner, |alt| alt.copy_on_select = enabled);
    }

    /// Fullscreen: whether a non-empty text selection exists.
    pub fn has_active_selection(&self) -> bool {
        selection::active_selection_text(&self.inner).is_some()
    }

    /// Fullscreen: copies the active selection. The request resolves to false when nothing is
    /// selected or the copy fails.
    pub fn copy_active_selection_to_clipboard(&self) -> CopyRequest {
        match selection::active_selection_text(&self.inner) {
            Some(text) => copy_text_to_clipboard(&self.inner, text),
            None => CopyRequest::resolved(false),
        }
    }

    /// Fullscreen: drops the selection and click history, for example before replacing the transcript.
    pub fn reset_text_selection(&self) {
        selection::clear_text_selection(&self.inner);
        with_alt(&self.inner, |alt| alt.last_click = None);
    }

    /// Fullscreen: lines of the last frame, one per row, as written to the terminal.
    pub fn get_screen_lines(&self) -> Vec<String> {
        with_alt(&self.inner, |alt| alt.previous_screen.clone()).unwrap_or_default()
    }

    /// Fullscreen: renders `component` as the layout root instead of the root children.
    pub fn set_layout_root(&self, component: Option<ComponentRef>) {
        {
            let mut root = self.inner.layout_root.borrow_mut();
            let same = match (&*root, &component) {
                (Some(a), Some(b)) => crate::component::same_component(a, b),
                (None, None) => true,
                _ => false,
            };
            if same {
                return;
            }
            *root = component;
        }
        with_alt(&self.inner, |alt| alt.current_layout = None);
        self.request_render(false);
    }

    /// Fullscreen: scrolls the primary scroll view by `lines` (negative scrolls up).
    pub fn scroll_by(&self, lines: i64) {
        viewport::scroll_by(&self.inner, lines);
    }

    pub fn scroll_to_top(&self) {
        viewport::scroll_to_top(&self.inner);
    }

    pub fn scroll_to_bottom(&self) {
        viewport::scroll_to_bottom(&self.inner);
    }

    /// Fullscreen: shows a transient message (default one second).
    pub fn flash(&self, message: &str, duration: Option<Duration>) {
        with_alt(&self.inner, |alt| alt.flashes.flash(message, duration));
    }
}
