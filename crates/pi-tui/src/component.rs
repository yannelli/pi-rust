//! The [`Component`] trait, shared component handles, mouse events, and [`Container`].

use std::any::Any;
use std::cell::RefCell;
use std::rc::Rc;

use crate::layout_node::LayoutNode;

/// Shared, mutable handle to a component. Identity matters (focus, overlays), so compare with
/// [`same_component`].
pub type ComponentRef = Rc<RefCell<dyn Component>>;

/// Wraps a component in a shared handle. Keep the typed `Rc` for direct access; it coerces to
/// [`ComponentRef`] when passed to the TUI.
pub fn component<C: Component + 'static>(value: C) -> Rc<RefCell<C>> {
    Rc::new(RefCell::new(value))
}

/// Compares two handles by identity.
pub fn same_component(a: &ComponentRef, b: &ComponentRef) -> bool {
    std::ptr::addr_eq(Rc::as_ptr(a), Rc::as_ptr(b))
}

/// Compares optional handles by identity.
pub fn same_component_opt(a: Option<&ComponentRef>, b: Option<&ComponentRef>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => same_component(a, b),
        (None, None) => true,
        _ => false,
    }
}

/// Stable identity key for maps keyed by component.
pub fn component_key(component: &ComponentRef) -> usize {
    Rc::as_ptr(component) as *const () as usize
}

/// Upcast helper so trait objects can be downcast to their concrete type.
pub trait AsAny: Any {
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

impl<T: Any> AsAny for T {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// Mouse event kinds delivered to components.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TuiMouseEventType {
    Press,
    Release,
    Move,
    Drag,
    Click,
    Wheel,
}

/// Mouse button of an event. Wheel and move events use `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TuiMouseButton {
    Left,
    Middle,
    Right,
    None,
}

/// Normalized cell-based mouse event. Coordinates are zero-based.
#[derive(Debug, Clone, PartialEq)]
pub struct TuiMouseEvent {
    pub kind: TuiMouseEventType,
    pub button: TuiMouseButton,
    /// Column local to the receiving component. Negative while a captured drag leaves it.
    pub x: i32,
    /// Row local to the receiving component.
    pub y: i32,
    /// Absolute terminal column.
    pub screen_x: i32,
    /// Absolute terminal row.
    pub screen_y: i32,
    /// Current component width.
    pub width: i32,
    /// Current component height.
    pub height: i32,
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
    /// Logical lines for wheel events. Negative values scroll up.
    pub wheel_delta: Option<i32>,
    /// Consecutive click count for click events.
    pub click_count: Option<u32>,
}

/// What a component did with a mouse event.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TuiMouseEventResult {
    /// Stop propagation and suppress renderer-level fallback behavior.
    pub handled: bool,
    /// Route the following drag and release events to this component. Implies handled.
    pub capture: bool,
    /// Give keyboard focus to this component. Implies handled.
    pub focus: bool,
    /// Request (`Some(true)`) or suppress (`Some(false)`) a render. Move and release default to no
    /// render; press, click, drag, and wheel default to a render.
    pub render: Option<bool>,
}

impl TuiMouseEventResult {
    /// A result that only marks the event as handled.
    pub fn handled() -> Self {
        Self { handled: true, ..Self::default() }
    }
}

/// Target of a dispatched mouse event, with the transform to its local coordinates.
#[derive(Clone)]
pub struct TuiMouseDispatchTarget {
    pub component: ComponentRef,
    pub origin_x: i32,
    pub origin_y: i32,
    pub width: i32,
    pub height: i32,
}

/// Result of dispatching to a concrete component.
#[derive(Clone)]
pub struct TuiMouseDispatchResult {
    pub result: TuiMouseEventResult,
    pub target: TuiMouseDispatchTarget,
    /// Keyboard focus target, which may be a delegating parent container.
    pub focus_target: Option<ComponentRef>,
}

/// Return value of [`Component::handle_mouse`].
pub enum MouseResponse {
    /// The component handled the event itself.
    Handled(TuiMouseEventResult),
    /// The component forwarded the event to a child it hosts.
    Forwarded(TuiMouseDispatchResult),
}

/// A renderable UI element. Mirrors the upstream `Component` interface: `render` is required,
/// everything else has a default.
pub trait Component: AsAny {
    /// Renders the component to lines for the given viewport width.
    fn render(&mut self, width: usize) -> Vec<String>;

    /// Handles keyboard input while the component has focus. Only called when
    /// [`Component::handles_input`] returns true.
    fn handle_input(&mut self, _data: &str) {}

    /// Whether this component implements [`Component::handle_input`] (upstream: `handleInput` exists).
    fn handles_input(&self) -> bool {
        false
    }

    /// Optional normalized mouse handler.
    fn handle_mouse(&mut self, _event: &TuiMouseEvent) -> Option<MouseResponse> {
        None
    }

    /// If true, the component receives Kitty key release events. Default false.
    fn wants_key_release(&self) -> bool {
        false
    }

    /// Invalidates cached rendering state, for example after a theme change.
    fn invalidate(&mut self) {}

    /// Whether the component can show a hardware cursor (upstream `Focusable`).
    fn is_focusable(&self) -> bool {
        false
    }

    /// Set by the TUI when focus changes. Focusable components emit [`CURSOR_MARKER`] while focused.
    fn set_focused(&mut self, _focused: bool) {}

    /// Children for containers, used for focus and mount checks (upstream `instanceof Container`).
    fn container_children(&self) -> Option<Vec<ComponentRef>> {
        None
    }

    /// Layout description for stacks and scroll views rendered by the fullscreen layout engine.
    fn layout_node(&self) -> Option<LayoutNode> {
        None
    }

    /// Layout nodes are skipped by fullscreen mouse hit testing (their children are hit instead)
    /// unless this returns true.
    fn intercepts_layout_mouse(&self) -> bool {
        false
    }
}

/// Returns true when the component is focusable (upstream `isFocusable`).
pub fn is_focusable(component: &ComponentRef) -> bool {
    component.try_borrow().map(|c| c.is_focusable()).unwrap_or(false)
}

/// Cursor position marker: a zero-width APC sequence that terminals ignore. Focused components
/// emit it at the cursor position; the TUI strips it and places the hardware cursor there.
pub const CURSOR_MARKER: &str = "\x1b_pi:c\x07";
/// Start of text drawn as a fake cursor.
pub const FAKE_CURSOR_START: &str = "\x1b_pi:fc\x07";
/// End of text drawn as a fake cursor.
pub const FAKE_CURSOR_END: &str = "\x1b_pi:/fc\x07";

/// Wraps text in fake cursor markers.
pub fn render_fake_cursor(text: &str) -> String {
    format!("{FAKE_CURSOR_START}{text}{FAKE_CURSOR_END}")
}

/// Dispatches an event to a component and records the exact target and coordinate transform.
/// Containers use this when forwarding events to nested children.
pub fn dispatch_mouse_event(component: &ComponentRef, event: &TuiMouseEvent) -> Option<TuiMouseDispatchResult> {
    let response = component.try_borrow_mut().ok()?.handle_mouse(event)?;
    match response {
        MouseResponse::Forwarded(mut forwarded) => {
            // A host that forwarded to its own child keeps keyboard focus, like a delegating container.
            if forwarded.result.focus && component.try_borrow().map(|c| c.handles_input()).unwrap_or(false) {
                forwarded.focus_target = Some(component.clone());
            }
            Some(forwarded)
        }
        MouseResponse::Handled(result) => {
            if !result.handled && !result.capture && !result.focus {
                return None;
            }
            Some(TuiMouseDispatchResult {
                result: TuiMouseEventResult { handled: true, ..result },
                focus_target: result.focus.then(|| component.clone()),
                target: TuiMouseDispatchTarget {
                    component: component.clone(),
                    origin_x: event.screen_x - event.x,
                    origin_y: event.screen_y - event.y,
                    width: event.width,
                    height: event.height,
                },
            })
        }
    }
}

/// Recreates local coordinates for a previously dispatched mouse target.
pub fn retarget_mouse_event(event: &TuiMouseEvent, target: &TuiMouseDispatchTarget) -> TuiMouseEvent {
    TuiMouseEvent {
        x: event.screen_x - target.origin_x,
        y: event.screen_y - target.origin_y,
        width: target.width,
        height: target.height,
        ..event.clone()
    }
}

/// Child heights from the last render, keyed by render width, for mouse hit testing.
pub type MouseLayout = Option<(usize, Vec<(ComponentRef, usize)>)>;

/// A component that renders its children top to bottom.
#[derive(Default)]
pub struct Container {
    pub children: Vec<ComponentRef>,
    mouse_layout: MouseLayout,
}

impl Container {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_child(&mut self, component: ComponentRef) {
        self.children.push(component);
    }

    pub fn remove_child(&mut self, component: &ComponentRef) {
        if let Some(index) = self.children.iter().position(|c| same_component(c, component)) {
            self.children.remove(index);
        }
    }

    pub fn clear(&mut self) {
        self.children.clear();
    }

    /// Renders all children and remembers their heights for mouse hit testing.
    pub fn render_children(&mut self, width: usize) -> Vec<String> {
        render_children_with_layout(&self.children, width, &mut self.mouse_layout)
    }

    /// Container mouse dispatch: finds the child under the pointer and forwards the event.
    pub fn handle_children_mouse(&mut self, event: &TuiMouseEvent) -> Option<MouseResponse> {
        dispatch_children_mouse(&self.children, self.mouse_layout.as_ref(), event).map(MouseResponse::Forwarded)
    }

    /// Invalidates every child.
    pub fn invalidate_children(&mut self) {
        invalidate_all(&self.children);
    }
}

pub(crate) fn invalidate_all(children: &[ComponentRef]) {
    for child in children {
        if let Ok(mut child) = child.try_borrow_mut() {
            child.invalidate();
        }
    }
}

/// Renders children in order and records their heights in `mouse_layout`.
pub fn render_children_with_layout(
    children: &[ComponentRef],
    width: usize,
    mouse_layout: &mut MouseLayout,
) -> Vec<String> {
    let mut lines = Vec::new();
    let mut heights = Vec::with_capacity(children.len());
    for child in children {
        let child_lines = child.borrow_mut().render(width);
        heights.push((child.clone(), child_lines.len()));
        lines.extend(child_lines);
    }
    *mouse_layout = Some((width, heights));
    lines
}

/// Upstream `Container.handleMouse`: hit tests stacked children and forwards the event.
pub fn dispatch_children_mouse(
    children: &[ComponentRef],
    mouse_layout: Option<&(usize, Vec<(ComponentRef, usize)>)>,
    event: &TuiMouseEvent,
) -> Option<TuiMouseDispatchResult> {
    if event.y < 0 || event.y >= event.height {
        return None;
    }
    let width = event.width.max(0) as usize;
    let heights: Vec<(ComponentRef, usize)> = match mouse_layout {
        Some((layout_width, heights)) if *layout_width == width => heights.clone(),
        _ => children.iter().map(|c| (c.clone(), c.borrow_mut().render(width).len())).collect(),
    };
    let mut child_y = 0i32;
    for (child, height) in heights {
        let height = height as i32;
        if event.y >= child_y && event.y < child_y + height {
            let local = TuiMouseEvent { y: event.y - child_y, height, ..event.clone() };
            return dispatch_mouse_event(&child, &local);
        }
        child_y += height;
    }
    None
}

impl Component for Container {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.render_children(width)
    }

    fn handle_mouse(&mut self, event: &TuiMouseEvent) -> Option<MouseResponse> {
        self.handle_children_mouse(event)
    }

    fn invalidate(&mut self) {
        self.invalidate_children();
    }

    fn container_children(&self) -> Option<Vec<ComponentRef>> {
        Some(self.children.clone())
    }
}
