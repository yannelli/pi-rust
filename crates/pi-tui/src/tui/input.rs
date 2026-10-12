//! Terminal input routing, terminal color queries, and cell size replies.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;

use super::TuiInputListenerResult;
use super::base::{FocusRestore, TuiInner};
use super::focus::RestorePolicy;
use crate::component::same_component;
use crate::keys::{is_key_release, matches_key};
use crate::terminal_colors::{
    OscColorTarget, RgbColor, TerminalColors, parse_osc_color_response, parse_terminal_color_scheme_report,
};
use crate::terminal_image::{CellDimensions, set_cell_dimensions};
use crate::ui_loop::{self, TimerId};

const TERMINAL_PALETTE_SIZE: usize = 16;
/// OSC 10 and 11 plus OSC 4 for every palette color.
const TERMINAL_COLOR_REPLY_COUNT: usize = 2 + TERMINAL_PALETTE_SIZE;

static DEVICE_ATTRIBUTES_RESPONSE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\x1b\[\?[\d;]*c$").expect("valid regex"));
static CELL_SIZE_RESPONSE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\x1b\[6;(\d+);(\d+)t$").expect("valid regex"));

/// Default colors, palette colors 0-15, and a trailing DA1 request. Terminals answer in order,
/// so the DA1 reply marks the end of the color replies.
fn terminal_color_query() -> String {
    let mut query = String::from("\x1b]10;?\x07\x1b]11;?\x07");
    for index in 0..TERMINAL_PALETTE_SIZE {
        query.push_str(&format!("\x1b]4;{index};?\x07"));
    }
    query.push_str("\x1b[c");
    query
}

type ColorsCallback = Box<dyn FnOnce(TerminalColors)>;

pub(crate) struct PendingColorQuery {
    foreground: Option<RgbColor>,
    background: Option<RgbColor>,
    palette: Vec<Option<RgbColor>>,
    replied: HashSet<String>,
    /// The result callback until the timeout, then the late reply callback. `None` once done.
    deliver: Option<ColorsCallback>,
    timer: Option<TimerId>,
}

impl PendingColorQuery {
    fn result(&self) -> TerminalColors {
        let palette =
            self.palette.iter().all(Option::is_some).then(|| self.palette.iter().flatten().copied().collect());
        TerminalColors { foreground: self.foreground, background: self.background, palette }
    }
}

fn complete_color_query(query: &Rc<RefCell<PendingColorQuery>>) {
    let (deliver, result) = {
        let mut query = query.borrow_mut();
        if let Some(timer) = query.timer.take() {
            ui_loop::clear_timer(timer);
        }
        (query.deliver.take(), query.result())
    };
    if let Some(deliver) = deliver {
        deliver(result);
    }
}

impl TuiInner {
    pub(crate) fn handle_terminal_input(&self, data: String) {
        let depth = self.dispatch_depth.get();
        self.dispatch_depth.set(depth + 1);
        self.route_input(data);
        self.dispatch_depth.set(depth);
        if depth == 0 {
            self.apply_pending_focus_flags();
        }
    }

    fn route_input(&self, mut data: String) {
        if self.consume_terminal_color_response(&data) || self.consume_color_scheme_report(&data) {
            return;
        }
        // The fullscreen viewport listener is registered first upstream, so it runs first.
        let viewport = self.is_alt();
        if viewport && crate::alt_screen::handle_viewport_input(self, &data) {
            return;
        }
        let listeners: Vec<_> = self.state.borrow().input_listeners.iter().map(|(_, l)| l.clone()).collect();
        if viewport || !listeners.is_empty() {
            for listener in listeners {
                let result = match listener.try_borrow_mut() {
                    Ok(mut listener) => listener(&data),
                    Err(_) => TuiInputListenerResult::Pass,
                };
                match result {
                    TuiInputListenerResult::Consume => return,
                    TuiInputListenerResult::Replace(replacement) => data = replacement,
                    TuiInputListenerResult::Pass => {}
                }
            }
            if data.is_empty() {
                return;
            }
        }
        if self.consume_cell_size_response(&data) {
            return;
        }
        let on_debug = self.on_debug.borrow().clone();
        if let Some(on_debug) = on_debug
            && matches_key(&data, "shift+ctrl+d")
        {
            (on_debug.borrow_mut())();
            return;
        }

        // A focused overlay may have become invisible (resize or visibility callback).
        let focused = self.state.borrow().focused.clone();
        let stack = self.state.borrow().overlay_stack.clone();
        let focused_overlay =
            focused.as_ref().and_then(|f| stack.iter().find(|o| same_component(&o.component, f)).cloned());
        if let Some(overlay) = focused_overlay
            && !self.is_overlay_visible(&overlay)
        {
            match self.topmost_visible_overlay() {
                Some(top) => self.set_focus(Some(top.component.clone())),
                None => {
                    let pre_focus = overlay.pre_focus.borrow().clone();
                    self.set_focus_internal(pre_focus, RestorePolicy::Preserve);
                }
            }
        }

        let focused = self.state.borrow().focused.clone();
        let stack = self.state.borrow().overlay_stack.clone();
        let focus_is_overlay = focused.as_ref().is_some_and(|f| stack.iter().any(|o| same_component(&o.component, f)));
        if !focus_is_overlay {
            match self.visible_overlay_focus_restore() {
                FocusRestore::Eligible { overlay } => self.set_focus(Some(overlay.component.clone())),
                FocusRestore::Blocked { overlay, blocked_by, resume } => {
                    let blocked_is_focused = focused.as_ref().is_some_and(|f| same_component(f, &blocked_by));
                    if !blocked_is_focused {
                        match resume {
                            super::base::BlockedResume::RestoreOverlay => {
                                self.set_focus(Some(overlay.component.clone()));
                            }
                            super::base::BlockedResume::FocusTarget(target) => {
                                self.clear_overlay_focus_restore();
                                self.set_focus(target);
                            }
                        }
                    }
                }
                FocusRestore::Inactive => {}
            }
        }

        let Some(focused) = self.state.borrow().focused.clone() else { return };
        let (handles, wants_release) = match focused.try_borrow() {
            Ok(component) => (component.handles_input(), component.wants_key_release()),
            Err(_) => return,
        };
        if !handles {
            return;
        }
        if is_key_release(&data) && !wants_release {
            return;
        }
        if let Ok(mut component) = focused.try_borrow_mut() {
            component.handle_input(&data);
        }
        // Keyboard input is latency-sensitive, so skip the throttled path.
        self.request_immediate_render();
    }

    fn consume_terminal_color_response(&self, data: &str) -> bool {
        let Some(query) = self.state.borrow().pending_color_queries.front().cloned() else { return false };
        if DEVICE_ATTRIBUTES_RESPONSE.is_match(data) {
            self.state.borrow_mut().pending_color_queries.pop_front();
            complete_color_query(&query);
            return true;
        }
        let Some(response) = parse_osc_color_response(data) else { return false };
        let complete = {
            let mut pending = query.borrow_mut();
            let key = match response.target {
                OscColorTarget::Foreground => "foreground".to_string(),
                OscColorTarget::Background => "background".to_string(),
                OscColorTarget::Palette(index) => index.to_string(),
            };
            if pending.deliver.is_none() || pending.replied.contains(&key) {
                return true;
            }
            pending.replied.insert(key);
            match response.target {
                OscColorTarget::Foreground => pending.foreground = response.rgb,
                OscColorTarget::Background => pending.background = response.rgb,
                OscColorTarget::Palette(index) => {
                    if let Some(slot) = pending.palette.get_mut(index as usize) {
                        *slot = response.rgb;
                    }
                }
            }
            pending.replied.len() == TERMINAL_COLOR_REPLY_COUNT
        };
        if complete {
            complete_color_query(&query);
        }
        true
    }

    fn consume_color_scheme_report(&self, data: &str) -> bool {
        let Some(scheme) = parse_terminal_color_scheme_report(data) else { return false };
        let listeners: Vec<_> = self.state.borrow().color_scheme_listeners.iter().map(|(_, l)| l.clone()).collect();
        for listener in listeners {
            if let Ok(mut listener) = listener.try_borrow_mut() {
                listener(scheme);
            }
        }
        true
    }

    fn consume_cell_size_response(&self, data: &str) -> bool {
        let Some(captures) = CELL_SIZE_RESPONSE.captures(data) else { return false };
        let height_px: u64 = captures[1].parse().unwrap_or(0);
        let width_px: u64 = captures[2].parse().unwrap_or(0);
        if height_px == 0 || width_px == 0 {
            return true;
        }
        set_cell_dimensions(CellDimensions {
            width_px: width_px.min(u32::MAX as u64) as u32,
            height_px: height_px.min(u32::MAX as u64) as u32,
        });
        // Images depend on the cell size, so re-render everything.
        self.invalidate_all();
        self.request_render(false);
        true
    }

    pub(crate) fn query_terminal_colors(
        &self,
        timeout: Duration,
        on_result: ColorsCallback,
        on_late_reply: Option<ColorsCallback>,
    ) {
        let query = Rc::new(RefCell::new(PendingColorQuery {
            foreground: None,
            background: None,
            palette: vec![None; TERMINAL_PALETTE_SIZE],
            replied: HashSet::new(),
            deliver: Some(on_result),
            timer: None,
        }));
        let weak = Rc::downgrade(&query);
        let mut late = on_late_reply;
        let timer = ui_loop::set_timeout(timeout, move || {
            let Some(query) = weak.upgrade() else { return };
            let (deliver, result) = {
                let mut query = query.borrow_mut();
                query.timer = None;
                let deliver = query.deliver.take();
                query.deliver = late.take();
                (deliver, query.result())
            };
            if let Some(deliver) = deliver {
                deliver(result);
            }
        });
        query.borrow_mut().timer = Some(timer);
        self.state.borrow_mut().pending_color_queries.push_back(query);
        self.write(&terminal_color_query());
    }
}
