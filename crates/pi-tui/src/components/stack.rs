//! Shared stack container and size allocation for [`super::VStack`] and [`super::HStack`].

use crate::component::{ComponentRef, Container, same_component};
use crate::layout_node::{
    LayoutViewport, StackAlign, StackBasis, StackDirection, StackLayoutEntry, StackLayoutNode, StackVisibility,
};

/// Sizing rules for one stack child.
#[derive(Clone, Default)]
pub struct StackEntryOptions {
    pub basis: Option<StackBasis>,
    pub grow: Option<usize>,
    pub shrink: Option<usize>,
    pub min_size: Option<usize>,
    pub max_size: Option<usize>,
    pub visible: Option<StackVisibility>,
}

/// A stack child given at construction: a plain component or one with sizing rules.
pub enum StackChild {
    Component(ComponentRef),
    Entry(ComponentRef, StackEntryOptions),
}

impl From<ComponentRef> for StackChild {
    fn from(component: ComponentRef) -> Self {
        StackChild::Component(component)
    }
}

/// Stack-wide options.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StackOptions {
    /// Cells between children along the stack axis.
    pub gap: usize,
    pub align: StackAlign,
}

/// Children with sizing rules, shared by vertical and horizontal stacks.
pub struct Stack {
    pub(crate) container: Container,
    pub(crate) entries: Vec<StackLayoutEntry>,
    pub(crate) gap: usize,
    pub(crate) align: StackAlign,
    direction: StackDirection,
}

impl Stack {
    pub(crate) fn new(direction: StackDirection, children: Vec<StackChild>, options: StackOptions) -> Self {
        let mut stack = Self {
            container: Container::new(),
            entries: Vec::new(),
            gap: options.gap,
            align: options.align,
            direction,
        };
        for child in children {
            match child {
                StackChild::Component(component) => stack.add_child(component),
                StackChild::Entry(component, options) => stack.add_child_with(component, options),
            }
        }
        stack
    }

    pub fn add_child(&mut self, component: ComponentRef) {
        self.add_child_with(component, StackEntryOptions::default());
    }

    pub fn add_child_with(&mut self, component: ComponentRef, options: StackEntryOptions) {
        self.container.add_child(component.clone());
        self.entries.push(StackLayoutEntry {
            component,
            basis: options.basis,
            grow: options.grow,
            shrink: options.shrink,
            min_size: options.min_size,
            max_size: options.max_size,
            visible: options.visible,
        });
    }

    pub fn remove_child(&mut self, component: &ComponentRef) {
        self.container.remove_child(component);
        if let Some(index) = self.entries.iter().position(|e| same_component(&e.component, component)) {
            self.entries.remove(index);
        }
    }

    pub fn clear(&mut self) {
        self.container.clear();
        self.entries.clear();
    }

    pub fn children(&self) -> &[ComponentRef] {
        &self.container.children
    }

    pub fn entries(&self) -> &[StackLayoutEntry] {
        &self.entries
    }

    pub(crate) fn layout_node(&self) -> StackLayoutNode {
        StackLayoutNode { direction: self.direction, entries: self.entries.clone(), gap: self.gap, align: self.align }
    }
}

/// Entries whose visibility predicate accepts the viewport.
pub fn visible_stack_entries(entries: &[StackLayoutEntry], viewport: &LayoutViewport) -> Vec<StackLayoutEntry> {
    entries.iter().filter(|e| e.visible.as_ref().is_none_or(|visible| visible(viewport))).cloned().collect()
}

fn clamp_size(size: usize, entry: &StackLayoutEntry) -> usize {
    let min = entry.min_size.unwrap_or(0);
    let max = entry.max_size.unwrap_or(usize::MAX).max(min);
    size.clamp(min, max)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Grow,
    Shrink,
}

fn weight(entry: &StackLayoutEntry, size: usize, mode: Mode) -> u128 {
    match mode {
        Mode::Grow => entry.grow.unwrap_or(0) as u128,
        Mode::Shrink => entry.shrink.unwrap_or(1) as u128 * size.max(1) as u128,
    }
}

fn distribute(sizes: &mut [usize], entries: &[StackLayoutEntry], amount: usize, mode: Mode) {
    let mut remaining = amount;
    while remaining > 0 {
        let candidates: Vec<usize> = (0..entries.len())
            .filter(|&i| {
                let entry = &entries[i];
                match mode {
                    Mode::Grow => entry.grow.unwrap_or(0) > 0 && sizes[i] < entry.max_size.unwrap_or(usize::MAX),
                    Mode::Shrink => entry.shrink.unwrap_or(1) > 0 && sizes[i] > entry.min_size.unwrap_or(0),
                }
            })
            .collect();
        if candidates.is_empty() {
            return;
        }
        let total_weight: u128 = candidates.iter().map(|&i| weight(&entries[i], sizes[i], mode)).sum();
        let mut distributed = 0;
        for &i in &candidates {
            if remaining == 0 {
                break;
            }
            let entry = &entries[i];
            let proposed = ((remaining as u128 * weight(entry, sizes[i], mode)) / total_weight.max(1)).max(1);
            let capacity = match mode {
                Mode::Grow => entry.max_size.unwrap_or(usize::MAX) - sizes[i],
                Mode::Shrink => sizes[i] - entry.min_size.unwrap_or(0),
            };
            let delta = (remaining as u128).min(proposed).min(capacity as u128) as usize;
            if delta == 0 {
                continue;
            }
            match mode {
                Mode::Grow => sizes[i] += delta,
                Mode::Shrink => sizes[i] -= delta,
            }
            remaining -= delta;
            distributed += delta;
        }
        if distributed == 0 {
            return;
        }
    }
}

/// Sizes along the stack axis: basis or intrinsic size, clamped, then grown or shrunk to fill
/// `available` minus the gaps.
pub fn allocate_stack_sizes(
    entries: &[StackLayoutEntry],
    intrinsic_sizes: &[usize],
    available: Option<usize>,
    gap: usize,
) -> Vec<usize> {
    let mut sizes: Vec<usize> = entries
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let base = match entry.basis {
                Some(StackBasis::Fixed(size)) => size,
                Some(StackBasis::Auto) | None => intrinsic_sizes.get(i).copied().unwrap_or(0),
            };
            clamp_size(base, entry)
        })
        .collect();
    let Some(available) = available else { return sizes };
    let content = available.saturating_sub(entries.len().saturating_sub(1) * gap);
    let total: usize = sizes.iter().sum();
    if total < content {
        distribute(&mut sizes, entries, content - total, Mode::Grow);
    } else if total > content {
        distribute(&mut sizes, entries, total - content, Mode::Shrink);
    }
    sizes
}
