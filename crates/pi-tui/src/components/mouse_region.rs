//! [`MouseRegion`]: adds a mouse handler to a component without changing its rendering.

use crate::component::{
    Component, ComponentRef, MouseResponse, TuiMouseEvent, TuiMouseEventResult, dispatch_mouse_event,
};

/// Handler of a [`MouseRegion`]. Returning `None` leaves the event unhandled.
pub type MouseRegionHandler = Box<dyn FnMut(&TuiMouseEvent) -> Option<TuiMouseEventResult>>;

/// Wraps a child: the child sees mouse events first, then the handler (upstream `MouseRegion`).
pub struct MouseRegion {
    child: ComponentRef,
    on_mouse: MouseRegionHandler,
}

impl MouseRegion {
    pub fn new(
        child: ComponentRef,
        on_mouse: impl FnMut(&TuiMouseEvent) -> Option<TuiMouseEventResult> + 'static,
    ) -> Self {
        Self { child, on_mouse: Box::new(on_mouse) }
    }

    pub fn child(&self) -> &ComponentRef {
        &self.child
    }
}

impl Component for MouseRegion {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.child.borrow_mut().render(width)
    }

    fn handle_mouse(&mut self, event: &TuiMouseEvent) -> Option<MouseResponse> {
        if let Some(result) = dispatch_mouse_event(&self.child, event) {
            return Some(MouseResponse::Forwarded(result));
        }
        (self.on_mouse)(event).map(MouseResponse::Handled)
    }

    fn invalidate(&mut self) {
        self.child.borrow_mut().invalidate();
    }
}
