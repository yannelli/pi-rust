//! Embedded JavaScript runtime for pi extensions: QuickJS (rquickjs) on its own thread with an
//! event loop, TypeScript loading through oxc, jiti-compatible module resolution, Node and web
//! globals, and an async bridge between Rust and JavaScript.

pub mod error;
pub mod host;
mod modules;
mod node;
pub mod resolve;
pub mod runner;
pub mod runtime;
pub mod transpile;
pub mod unsupported;
pub mod virtual_modules;

pub use error::{JsError, UNSUPPORTED_CODE};
pub use host::{HostFunction, HostFunctions, HostValue};
pub use rquickjs;
pub use runtime::{
    ExitCallback, JsHandle, JsRef, JsRuntime, JsRuntimeOptions, JsScope, OutputSink, OutputStream, StdOutput,
    UncaughtCallback, UncaughtKind,
};
pub use transpile::{ModuleFormat, TranspileCache, TranspileOptions, Transpiled};
pub use virtual_modules::{PI_VIRTUAL_MODULES, VIRTUAL_MODULE_ALIASES, VirtualModule, VirtualModules};
