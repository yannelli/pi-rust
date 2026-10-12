//! Layout descriptions that stacks and scroll views expose to the fullscreen layout engine.

use std::rc::Rc;

use crate::component::ComponentRef;
use crate::components::scroll_view::ScrollViewHandle;

/// Size of the area a stack lays out into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutViewport {
    pub width: usize,
    pub height: usize,
}

/// Base size of a stack entry along the stack axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackBasis {
    /// Use the measured size of the component.
    Auto,
    /// A fixed number of cells.
    Fixed(usize),
}

/// Predicate deciding whether a stack entry is shown for a viewport.
pub type StackVisibility = Rc<dyn Fn(&LayoutViewport) -> bool>;

/// One child of a stack with its sizing rules.
#[derive(Clone)]
pub struct StackLayoutEntry {
    pub component: ComponentRef,
    pub basis: Option<StackBasis>,
    pub grow: Option<usize>,
    pub shrink: Option<usize>,
    pub min_size: Option<usize>,
    pub max_size: Option<usize>,
    pub visible: Option<StackVisibility>,
}

/// Stack direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackDirection {
    Vertical,
    Horizontal,
}

/// Cross-axis alignment of stack children.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StackAlign {
    #[default]
    Stretch,
    Start,
    Center,
    End,
}

/// Layout of a vertical or horizontal stack.
#[derive(Clone)]
pub struct StackLayoutNode {
    pub direction: StackDirection,
    pub entries: Vec<StackLayoutEntry>,
    pub gap: usize,
    pub align: StackAlign,
}

/// Layout of a scroll view: its content component and its scroll state.
#[derive(Clone)]
pub struct ScrollLayoutNode {
    pub component: ComponentRef,
    pub state: ScrollViewHandle,
}

/// A layout node (upstream `LayoutNode`).
#[derive(Clone)]
pub enum LayoutNode {
    Stack(StackLayoutNode),
    Scroll(ScrollLayoutNode),
}

/// Returns the layout node of a component, if it has one (upstream `getLayoutNode`).
pub fn get_layout_node(component: &ComponentRef) -> Option<LayoutNode> {
    component.try_borrow().ok()?.layout_node()
}
