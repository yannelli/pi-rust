//! [`CountdownTimer`]: a once-per-second countdown for dialogs with a timeout
//! (upstream `countdown-timer.ts`).

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use pi_tui::Tui;
use pi_tui::ui_loop::{self, TimerId};

/// Calls `on_tick` with the remaining seconds now and every second, then `on_expire` at zero.
/// Dropping the timer stops it.
pub struct CountdownTimer {
    timer: Rc<Cell<Option<TimerId>>>,
}

impl CountdownTimer {
    pub fn new(
        timeout: Duration,
        tui: Option<Tui>,
        mut on_tick: impl FnMut(u64) + 'static,
        on_expire: impl FnOnce() + 'static,
    ) -> Self {
        let mut remaining = timeout.as_millis().div_ceil(1000) as u64;
        on_tick(remaining);
        let timer: Rc<Cell<Option<TimerId>>> = Rc::default();
        let on_expire = RefCell::new(Some(on_expire));
        let handle = timer.clone();
        let id = ui_loop::set_interval(Duration::from_secs(1), move || {
            remaining = remaining.saturating_sub(1);
            on_tick(remaining);
            if let Some(tui) = &tui {
                tui.request_render(false);
            }
            if remaining == 0 {
                if let Some(id) = handle.take() {
                    ui_loop::clear_timer(id);
                }
                if let Some(expire) = on_expire.borrow_mut().take() {
                    expire();
                }
            }
        });
        timer.set(Some(id));
        Self { timer }
    }

    /// Stops the countdown without calling `on_expire`.
    pub fn dispose(&self) {
        if let Some(id) = self.timer.take() {
            ui_loop::clear_timer(id);
        }
    }

    /// A handle that stops this countdown, usable while its owner is borrowed.
    pub fn disposer(&self) -> impl Fn() + 'static {
        let timer = self.timer.clone();
        move || {
            if let Some(id) = timer.take() {
                ui_loop::clear_timer(id);
            }
        }
    }
}

impl Drop for CountdownTimer {
    fn drop(&mut self) {
        self.dispose();
    }
}
