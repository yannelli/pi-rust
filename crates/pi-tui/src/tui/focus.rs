//! Focus management and overlay focus restoration (upstream `setFocusInternal` and overlay handles).

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use super::base::{BlockedResume, FocusRestore, TuiInner};
use super::overlay::{OverlayEntry, OverlayHandle, OverlayOptions, OverlayUnfocusOptions};
use crate::component::{ComponentRef, same_component, same_component_opt};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum RestorePolicy {
    Clear,
    Preserve,
}

impl TuiInner {
    pub(crate) fn set_focus(&self, component: Option<ComponentRef>) {
        self.set_focus_internal(component, RestorePolicy::Clear);
    }

    fn overlay_for(&self, component: &ComponentRef) -> Option<Rc<OverlayEntry>> {
        let stack = self.state.borrow().overlay_stack.clone();
        stack.into_iter().find(|e| same_component(&e.component, component))
    }

    fn visible_overlay_for(&self, component: &ComponentRef) -> Option<Rc<OverlayEntry>> {
        self.overlay_for(component).filter(|e| self.is_overlay_visible(e))
    }

    pub(crate) fn set_focus_internal(&self, component: Option<ComponentRef>, policy: RestorePolicy) {
        let previous = self.state.borrow().focused.clone();
        let mut next = component;
        let previous_overlay = previous.as_ref().and_then(|p| self.visible_overlay_for(p));
        let next_is_overlay = next.as_ref().is_some_and(|n| self.overlay_for(n).is_some());
        let restore = self.visible_overlay_focus_restore();
        if let Some(next_component) = next.clone().filter(|_| !next_is_overlay) {
            match &restore {
                FocusRestore::Blocked { overlay, blocked_by, resume }
                    if same_component_opt(Some(blocked_by), previous.as_ref()) =>
                {
                    if matches!(resume, BlockedResume::FocusTarget(_)) || !self.is_component_mounted(blocked_by) {
                        next = self.resolve_blocked_resume(overlay, resume);
                    } else {
                        self.state.borrow_mut().overlay_focus_restore = FocusRestore::Blocked {
                            overlay: overlay.clone(),
                            blocked_by: next_component,
                            resume: resume.clone(),
                        };
                    }
                }
                _ => {
                    if let (Some(prev_overlay), Some(restore_overlay)) = (&previous_overlay, restore.overlay())
                        && Rc::ptr_eq(prev_overlay, restore_overlay)
                        && !self.is_overlay_focus_ancestor(prev_overlay, &next_component)
                    {
                        self.state.borrow_mut().overlay_focus_restore = FocusRestore::Blocked {
                            overlay: prev_overlay.clone(),
                            blocked_by: next_component,
                            resume: BlockedResume::RestoreOverlay,
                        };
                    }
                }
            }
        } else if next.is_none() {
            match &restore {
                FocusRestore::Blocked { overlay, blocked_by, resume }
                    if same_component_opt(Some(blocked_by), previous.as_ref()) =>
                {
                    next = self.resolve_blocked_resume(overlay, resume);
                }
                _ => {
                    if policy == RestorePolicy::Clear {
                        self.clear_overlay_focus_restore();
                    }
                }
            }
        }

        if let Some(previous) = &previous {
            self.set_focus_flag(previous, false);
        }
        self.state.borrow_mut().focused = next.clone();
        if let Some(next) = &next {
            self.set_focus_flag(next, true);
            if let Some(overlay) = self.visible_overlay_for(next) {
                self.state.borrow_mut().overlay_focus_restore = FocusRestore::Eligible { overlay };
            }
        }
    }

    pub(crate) fn clear_overlay_focus_restore(&self) {
        self.state.borrow_mut().overlay_focus_restore = FocusRestore::Inactive;
    }

    fn clear_overlay_focus_restore_for(&self, overlay: &Rc<OverlayEntry>) {
        let matches = self.state.borrow().overlay_focus_restore.overlay().is_some_and(|o| Rc::ptr_eq(o, overlay));
        if matches {
            self.clear_overlay_focus_restore();
        }
    }

    fn resolve_blocked_resume(&self, overlay: &Rc<OverlayEntry>, resume: &BlockedResume) -> Option<ComponentRef> {
        match resume {
            BlockedResume::RestoreOverlay => Some(overlay.component.clone()),
            BlockedResume::FocusTarget(target) => {
                self.clear_overlay_focus_restore();
                target.clone()
            }
        }
    }

    pub(crate) fn visible_overlay_focus_restore(&self) -> FocusRestore {
        let restore = self.state.borrow().overlay_focus_restore.clone();
        let Some(overlay) = restore.overlay() else { return restore };
        let in_stack = self.state.borrow().overlay_stack.iter().any(|e| Rc::ptr_eq(e, overlay));
        if !in_stack || !self.is_overlay_visible(overlay) {
            return FocusRestore::Inactive;
        }
        restore
    }

    fn is_overlay_focus_ancestor(&self, entry: &Rc<OverlayEntry>, component: &ComponentRef) -> bool {
        let mut visited: Vec<ComponentRef> = Vec::new();
        let mut current = entry.pre_focus.borrow().clone();
        while let Some(candidate) = current {
            if visited.iter().any(|v| same_component(v, &candidate)) {
                break;
            }
            if same_component(&candidate, component) {
                return true;
            }
            visited.push(candidate.clone());
            current = self.overlay_for(&candidate).and_then(|o| o.pre_focus.borrow().clone());
        }
        false
    }

    fn retarget_overlay_pre_focus(&self, removed: &Rc<OverlayEntry>) {
        let stack = self.state.borrow().overlay_stack.clone();
        let removed_pre_focus = removed.pre_focus.borrow().clone();
        for overlay in stack {
            if Rc::ptr_eq(&overlay, removed) {
                continue;
            }
            let points_to_removed =
                overlay.pre_focus.borrow().as_ref().is_some_and(|p| same_component(p, &removed.component));
            if points_to_removed {
                *overlay.pre_focus.borrow_mut() = removed_pre_focus.clone();
            }
        }
    }

    pub(crate) fn is_component_mounted(&self, component: &ComponentRef) -> bool {
        self.mounted_roots().iter().any(|root| contains_component(root, component))
    }

    fn is_focused(&self, component: &ComponentRef) -> bool {
        self.state.borrow().focused.as_ref().is_some_and(|f| same_component(f, component))
    }

    pub(crate) fn topmost_visible_overlay(&self) -> Option<Rc<OverlayEntry>> {
        let stack = self.state.borrow().overlay_stack.clone();
        let mut topmost: Option<Rc<OverlayEntry>> = None;
        for overlay in stack {
            if overlay.options.non_capturing || !self.is_overlay_visible(&overlay) {
                continue;
            }
            if topmost.as_ref().is_none_or(|t| overlay.focus_order.get() > t.focus_order.get()) {
                topmost = Some(overlay);
            }
        }
        topmost
    }

    fn next_focus_order(&self) -> u64 {
        let mut state = self.state.borrow_mut();
        state.focus_order_counter += 1;
        state.focus_order_counter
    }

    pub(crate) fn show_overlay(&self, component: ComponentRef, options: OverlayOptions) -> OverlayHandle {
        let entry = Rc::new(OverlayEntry {
            component: component.clone(),
            pre_focus: RefCell::new(self.state.borrow().focused.clone()),
            hidden: Cell::new(false),
            focus_order: Cell::new(self.next_focus_order()),
            bounds: Cell::new(None),
            options,
        });
        self.state.borrow_mut().overlay_stack.push(entry.clone());
        if !entry.options.non_capturing && self.is_overlay_visible(&entry) {
            self.set_focus(Some(component));
        }
        self.hide_terminal_cursor();
        self.request_render(false);
        OverlayHandle { tui: self.weak.clone(), entry }
    }

    pub(crate) fn hide_overlay_entry(&self, entry: &Rc<OverlayEntry>) {
        let index = self.state.borrow().overlay_stack.iter().position(|e| Rc::ptr_eq(e, entry));
        let Some(index) = index else { return };
        self.clear_overlay_focus_restore_for(entry);
        self.retarget_overlay_pre_focus(entry);
        self.state.borrow_mut().overlay_stack.remove(index);
        if self.is_focused(&entry.component) {
            let target = match self.topmost_visible_overlay() {
                Some(top) => Some(top.component.clone()),
                None => entry.pre_focus.borrow().clone(),
            };
            self.set_focus(target);
        }
        if self.state.borrow().overlay_stack.is_empty() {
            self.hide_terminal_cursor();
        }
        self.request_render(false);
    }

    pub(crate) fn hide_top_overlay(&self) {
        let top = self.state.borrow().overlay_stack.last().cloned();
        if let Some(top) = top {
            self.hide_overlay_entry(&top);
        }
    }

    pub(crate) fn set_overlay_hidden(&self, entry: &Rc<OverlayEntry>, hidden: bool) {
        if entry.hidden.get() == hidden {
            return;
        }
        entry.hidden.set(hidden);
        if hidden {
            self.clear_overlay_focus_restore_for(entry);
            if self.is_focused(&entry.component) {
                let target = match self.topmost_visible_overlay() {
                    Some(top) => Some(top.component.clone()),
                    None => entry.pre_focus.borrow().clone(),
                };
                self.set_focus(target);
            }
        } else if !entry.options.non_capturing && self.is_overlay_visible(entry) {
            entry.focus_order.set(self.next_focus_order());
            self.set_focus(Some(entry.component.clone()));
        }
        self.request_render(false);
    }

    pub(crate) fn focus_overlay_entry(&self, entry: &Rc<OverlayEntry>) {
        let in_stack = self.state.borrow().overlay_stack.iter().any(|e| Rc::ptr_eq(e, entry));
        if !in_stack || !self.is_overlay_visible(entry) {
            return;
        }
        entry.focus_order.set(self.next_focus_order());
        self.set_focus(Some(entry.component.clone()));
        self.request_render(false);
    }

    pub(crate) fn unfocus_overlay_entry(&self, entry: &Rc<OverlayEntry>, options: Option<OverlayUnfocusOptions>) {
        let is_focused = self.is_focused(&entry.component);
        let restore = self.state.borrow().overlay_focus_restore.clone();
        let has_pending_restore = restore.overlay().is_some_and(|o| Rc::ptr_eq(o, entry));
        if !is_focused && !has_pending_restore {
            return;
        }
        if let FocusRestore::Blocked { overlay, blocked_by, .. } = &restore
            && Rc::ptr_eq(overlay, entry)
            && self.is_focused(blocked_by)
        {
            match options {
                Some(options) => {
                    self.state.borrow_mut().overlay_focus_restore = FocusRestore::Blocked {
                        overlay: entry.clone(),
                        blocked_by: blocked_by.clone(),
                        resume: BlockedResume::FocusTarget(options.target),
                    };
                }
                None => self.clear_overlay_focus_restore(),
            }
            self.request_render(false);
            return;
        }
        self.clear_overlay_focus_restore_for(entry);
        if is_focused || options.is_some() {
            let target = match options {
                Some(options) => options.target,
                None => match self.topmost_visible_overlay() {
                    Some(top) if !Rc::ptr_eq(&top, entry) => Some(top.component.clone()),
                    _ => entry.pre_focus.borrow().clone(),
                },
            };
            self.set_focus(target);
        }
        self.request_render(false);
    }

    pub(crate) fn has_visible_overlay(&self) -> bool {
        let stack = self.state.borrow().overlay_stack.clone();
        stack.iter().any(|o| self.is_overlay_visible(o))
    }

    /// Whether the focused component is a visible overlay.
    pub(crate) fn is_overlay_focused(&self) -> bool {
        let focused = self.state.borrow().focused.clone();
        focused.is_some_and(|f| self.visible_overlay_for(&f).is_some())
    }

    /// Keeps overlay containers as keyboard focus owners when a nested control is clicked.
    pub(crate) fn resolve_mouse_focus_target(&self, component: &ComponentRef) -> ComponentRef {
        let stack = self.state.borrow().overlay_stack.clone();
        for overlay in stack.iter().rev() {
            if self.is_overlay_visible(overlay) && contains_component(&overlay.component, component) {
                return overlay.component.clone();
            }
        }
        component.clone()
    }
}

/// Whether `target` is `root` or a descendant through container children.
pub(crate) fn contains_component(root: &ComponentRef, target: &ComponentRef) -> bool {
    if same_component(root, target) {
        return true;
    }
    let children = root.try_borrow().ok().and_then(|r| r.container_children());
    children.is_some_and(|children| children.iter().any(|child| contains_component(child, target)))
}
