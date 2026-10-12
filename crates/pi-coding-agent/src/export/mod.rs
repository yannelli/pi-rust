//! Session export: HTML (`/export`, `--export`, RPC `export_html`), branch JSONL, and `/share`.
//! Sessions are read from disk as `serde_json::Value` entries, so every field survives export.

pub mod ansi;
mod html;
pub mod json;
mod jsonl;
pub mod session_file;
pub mod share;
mod theme;
mod themes;
pub mod tool_renderer;

#[cfg(test)]
mod tests;

pub use html::{
    APP_NAME, ExportTool, HtmlExportOptions, SessionData, default_html_output_path, export_from_file,
    export_session_html, generate_html,
};
pub use jsonl::{TrailingEntries, default_jsonl_output_path, export_session_to_jsonl, serialize_session_branch};
pub use session_file::SessionSnapshot;
pub use theme::{ExportTheme, derive_export_colors};
pub use tool_renderer::{
    AnsiToolRenderer, AnsiToolRendererAdapter, RenderedResultHtml, RenderedToolHtml, RenderedTools,
    TEMPLATE_RENDERED_TOOLS, ToolHtmlRenderer, pre_render_custom_tools,
};
