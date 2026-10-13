//! Shared test helpers. The renderer tests use the real `Text`, `MouseRegion`, and `Image`
//! components, re-exported here so test files can keep importing them from `support`.
#![allow(dead_code, unused_imports)]

use std::cell::RefCell;
use std::rc::Rc;

pub use pi_tui::components::{Image, ImageOptions, ImageTheme, MouseRegion, Text};
use pi_tui::{Component, ComponentRef, component};

/// Wraps a component as a shared handle typed as [`ComponentRef`].
pub fn shared<C: Component + 'static>(value: C) -> (Rc<RefCell<C>>, ComponentRef) {
    let typed = component(value);
    let erased: ComponentRef = typed.clone();
    (typed, erased)
}
