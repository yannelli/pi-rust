//! Platform support: shells, WSL and Termux detection, version checks, the changelog, install
//! telemetry, and retrying management HTTP (upstream `utils/shell.ts`, `wsl.ts`,
//! `version-check.ts`, `changelog.ts`, `management-http.ts`, and `core/telemetry.ts`).

pub mod changelog;
pub mod http;
pub mod shell;
pub mod system;
pub mod telemetry;
pub mod version_check;

#[cfg(test)]
pub(crate) mod test_http;

pub use http::{FetchError, FetchRetryOptions, VERSION, fetch_with_retry, pi_user_agent};
pub use system::{
    PlatformEnv, app_key_defaults, is_termux, is_wsl, termux_package_for_tool, termux_tool_warnings,
    use_windows_keybindings,
};
