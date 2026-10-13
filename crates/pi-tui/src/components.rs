//! Components: stacks, scroll views, flashes, inputs, text, lists, loaders, images, and Markdown.

pub mod alt_screen_flash;
pub mod box_component;
pub mod cancellable_loader;
pub mod editor;
pub mod image;
pub mod input;
pub mod loader;
pub mod markdown;
pub mod mouse_region;
pub mod scroll_view;
pub mod select_list;
pub mod settings_list;
pub mod spacer;
pub mod stack;
pub mod stacks;
pub mod text;
pub mod truncated_text;

pub use alt_screen_flash::AltScreenFlashContainer;
pub use box_component::BoxComponent;
pub use cancellable_loader::CancellableLoader;
pub use editor::{Editor, EditorCursor, EditorOptions, EditorTheme};
pub use image::{Image, ImageOptions, ImageTheme, ImageTranscoder, set_image_transcoder};
pub use input::{Input, InputOptions};
pub use loader::{Loader, LoaderIndicatorOptions};
pub use markdown::{DefaultTextStyle, HighlightCodeFn, Markdown, MarkdownOptions, MarkdownTheme, MarkdownTransformFn};
pub use mouse_region::{MouseRegion, MouseRegionHandler};
pub use scroll_view::{
    ScrollFollow, ScrollOverscroll, ScrollView, ScrollViewHandle, ScrollViewOptions, ScrollViewScrollToOptions,
    ScrollViewScrollbar, StyleFn,
};
pub use select_list::{SelectItem, SelectList, SelectListLayoutOptions, SelectListTruncatePrimaryContext};
pub use settings_list::{
    SettingItem, SettingsList, SettingsListOptions, SettingsListTheme, SubmenuDone, SubmenuDoneOptions,
};
pub use spacer::Spacer;
pub use stack::{Stack, StackChild, StackEntryOptions, StackOptions, allocate_stack_sizes, visible_stack_entries};
pub use stacks::{HStack, VStack};
pub use text::Text;
pub use truncated_text::TruncatedText;
