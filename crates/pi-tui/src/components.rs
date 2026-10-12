//! Components the renderer itself needs: stacks, scroll views, flashes, and the single-line input.

pub mod alt_screen_flash;
pub mod input;
pub mod scroll_view;
pub mod stack;
pub mod stacks;

pub use alt_screen_flash::AltScreenFlashContainer;
pub use input::{Input, InputOptions};
pub use scroll_view::{
    ScrollFollow, ScrollOverscroll, ScrollView, ScrollViewHandle, ScrollViewOptions, ScrollViewScrollToOptions,
    ScrollViewScrollbar, StyleFn,
};
pub use stack::{Stack, StackChild, StackEntryOptions, StackOptions, allocate_stack_sizes, visible_stack_entries};
pub use stacks::{HStack, VStack};
