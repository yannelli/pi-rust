//! Extensions (upstream `core/extensions/*`): TypeScript and JavaScript extensions run unchanged
//! on the embedded pi-js runtime, one runtime per session. JavaScript host modules port the
//! upstream API and event runner; host operations in Rust reach the session, models, and UI.

pub mod autocomplete;
pub mod component;
pub(crate) mod convert;
pub mod inline;
pub mod loader;
mod modules;
mod ops;
mod ops_async;
mod ops_host;
mod ops_session;
mod ops_ui;
mod provider;
pub mod registry;
mod replace;
pub mod rpc;
pub mod runner;
mod session;
pub(crate) mod shell;
pub mod tool;
mod tui_hooks;
pub mod vendor;

pub use component::{ComponentFactory, ComponentKind, CustomRequest, JsComponent};
pub use inline::{InlineExtension, inline_path};
pub use loader::{LoadEntry, discover_extensions_in_dir, load_entries, resolve_extension_entries};
pub use registry::{
    BuiltinReplacement, CommandInfo, FlagInfo, LoadedExtension, Registry, ShortcutInfo, ToolDescriptor,
};
pub use runner::{
    ExtensionBindings, ExtensionError, ExtensionErrorFn, ExtensionLoadError, ExtensionRunner, ShutdownFn,
};
pub use session::{LoadOptions, LoadedExtensions, load_extensions};
pub use tool::ExtensionTool;
pub use tui_hooks::{TerminalInputVerdict, ToolRenderRequest, ToolRendererInfo};
