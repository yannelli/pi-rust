//! Accessors for resources, models, tools, analytics, and terminal display settings.

use serde_json::{Value, json};

use super::accessors::ResourceKind;
use super::manager::SettingsManager;
use super::schema::*;
use super::storage::SettingsScope;
use super::tools::resolve_default_tools;

fn uuid_v4() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut bytes = [0u8; 16];
    for chunk in bytes.chunks_mut(8) {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u128(
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0),
        );
        chunk.copy_from_slice(&hasher.finish().to_le_bytes()[..chunk.len()]);
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..])
}

impl ResourceKind {
    pub fn key(self) -> &'static str {
        match self {
            Self::Extensions => "extensions",
            Self::Skills => "skills",
            Self::Prompts => "prompts",
            Self::Themes => "themes",
        }
    }

    fn of(self, settings: &Settings) -> Vec<String> {
        let list = match self {
            Self::Extensions => &settings.extensions,
            Self::Skills => &settings.skills,
            Self::Prompts => &settings.prompts,
            Self::Themes => &settings.themes,
        };
        list.clone().unwrap_or_default()
    }
}

impl SettingsManager {
    pub fn enable_analytics(&self) -> bool {
        self.state().merged.enable_analytics.unwrap_or(false)
    }

    pub fn tracking_id(&self) -> Option<String> {
        self.state().merged.tracking_id.clone()
    }

    /// Opting in creates a tracking id the first time.
    pub fn set_enable_analytics(&self, enabled: bool) {
        self.set_global_value("enableAnalytics", Some(json!(enabled)));
        if enabled && self.state().global_typed.tracking_id.is_none() {
            self.set_global_value("trackingId", Some(json!(uuid_v4())));
        }
    }

    /// Stable id of this installation, created on first use in global settings only.
    pub fn get_or_create_device_id(&self) -> String {
        if let Some(id) = self.state().global_typed.device_id.clone() {
            return id;
        }
        let id = uuid_v4();
        self.set_global_value("deviceId", Some(json!(id)));
        id
    }

    /// `httpProxy` from global settings only.
    pub fn http_proxy(&self) -> Option<String> {
        self.state().global_typed.http_proxy.clone().filter(|proxy| !proxy.is_empty())
    }

    /// Package sources of one scope.
    pub fn packages(&self, scope: SettingsScope) -> Vec<PackageSource> {
        let state = self.state();
        let settings = if scope == SettingsScope::Global { &state.global_typed } else { &state.project_typed };
        settings.packages.clone().unwrap_or_default()
    }

    /// Packages of both scopes, project entries first so they win collisions.
    pub fn all_packages(&self) -> Vec<(SettingsScope, PackageSource)> {
        let project = self.packages(SettingsScope::Project).into_iter().map(|p| (SettingsScope::Project, p));
        project.chain(self.packages(SettingsScope::Global).into_iter().map(|p| (SettingsScope::Global, p))).collect()
    }

    pub fn set_packages(&self, packages: &[PackageSource]) {
        self.set_global_value("packages", serde_json::to_value(packages).ok());
    }

    pub fn set_project_packages(&self, packages: &[PackageSource]) -> Result<(), String> {
        self.set_project_value("packages", serde_json::to_value(packages).ok())
    }

    /// Entries of one resource list in one scope, relative to that scope's base directory.
    pub fn resource_paths(&self, kind: ResourceKind, scope: SettingsScope) -> Vec<String> {
        let state = self.state();
        kind.of(if scope == SettingsScope::Global { &state.global_typed } else { &state.project_typed })
    }

    /// Entries of one resource list from both scopes combined, project entries first.
    pub fn combined_resource_paths(&self, kind: ResourceKind) -> Vec<(SettingsScope, String)> {
        let project =
            self.resource_paths(kind, SettingsScope::Project).into_iter().map(|p| (SettingsScope::Project, p));
        let global = self.resource_paths(kind, SettingsScope::Global).into_iter().map(|p| (SettingsScope::Global, p));
        project.chain(global).collect()
    }

    /// The merged list (project replaces global), as upstream `getExtensionPaths` and friends.
    pub fn merged_resource_paths(&self, kind: ResourceKind) -> Vec<String> {
        kind.of(&self.state().merged)
    }

    pub fn set_resource_paths(&self, kind: ResourceKind, paths: &[String]) {
        self.set_global_value(kind.key(), Some(json!(paths)));
    }

    pub fn set_project_resource_paths(&self, kind: ResourceKind, paths: &[String]) -> Result<(), String> {
        self.set_project_value(kind.key(), Some(json!(paths)))
    }

    pub fn thinking_budgets(&self) -> Option<ThinkingBudgetsSettings> {
        self.state().merged.thinking_budgets
    }

    pub fn enabled_models(&self) -> Option<Vec<String>> {
        self.state().merged.enabled_models.clone()
    }

    pub fn set_enabled_models(&self, patterns: Option<&[String]>) {
        self.set_global_value("enabledModels", patterns.map(|patterns| json!(patterns)));
    }

    /// The resolved `defaultTools` selection, or `None` when no layer sets it.
    pub fn default_tools(&self) -> Option<Vec<String>> {
        self.state().merged.default_tools.as_deref().map(resolve_default_tools)
    }

    pub fn image_width_cells(&self) -> u64 {
        let width = self.state().merged.terminal.as_ref().and_then(|terminal| terminal.image_width_cells);
        width.filter(|width| width.is_finite()).map_or(60, |width| width.floor().max(1.0) as u64)
    }

    pub fn set_image_width_cells(&self, width: u64) {
        self.set_global_nested("terminal", "imageWidthCells", Some(json!(width.max(1))));
    }

    /// Setting first, then `PI_CLEAR_ON_SHRINK=1`, then off.
    pub fn clear_on_shrink(&self) -> bool {
        let setting = self.state().merged.terminal.as_ref().and_then(|terminal| terminal.clear_on_shrink);
        setting.unwrap_or_else(|| std::env::var("PI_CLEAR_ON_SHRINK").is_ok_and(|value| value == "1"))
    }

    pub fn set_clear_on_shrink(&self, enabled: bool) {
        self.set_global_nested("terminal", "clearOnShrink", Some(json!(enabled)));
    }

    /// `(images, trueColor, hyperlinks)` overrides; `Some(None)` for images disables them.
    pub fn terminal_capability_overrides(&self) -> (Option<Option<String>>, Option<bool>, Option<bool>) {
        let terminal = self.state().merged.terminal.clone().unwrap_or_default();
        let images = match terminal.images {
            Some(Value::String(kind)) if kind == "kitty" || kind == "iterm2" => Some(Some(kind)),
            Some(Value::Bool(false)) => Some(None),
            _ => None,
        };
        (images, terminal.true_color.and_then(|v| v.as_bool()), terminal.hyperlinks.and_then(|v| v.as_bool()))
    }

    /// Lines per wheel event, clamped to 1..=100; `None` means `"auto"`.
    pub fn fullscreen_wheel_scroll_lines(&self) -> Option<u64> {
        let value = self.state().merged.fullscreen_wheel_scroll_lines.clone()?;
        value.as_f64().filter(|lines| lines.is_finite()).map(|lines| lines.floor().clamp(1.0, 100.0) as u64)
    }

    pub fn set_fullscreen_wheel_scroll_lines(&self, lines: Option<u64>) {
        let value = lines.map_or(json!("auto"), |lines| json!(lines.clamp(1, 100)));
        self.set_global_value("fullscreenWheelScrollLines", Some(value));
    }

    /// Setting first, then `PI_HARDWARE_CURSOR=1`.
    pub fn show_hardware_cursor(&self) -> bool {
        let setting = self.state().merged.show_hardware_cursor;
        setting.unwrap_or_else(|| std::env::var("PI_HARDWARE_CURSOR").is_ok_and(|value| value == "1"))
    }

    pub fn set_show_hardware_cursor(&self, enabled: bool) {
        self.set_global_value("showHardwareCursor", Some(json!(enabled)));
    }

    pub fn editor_padding_x(&self) -> u64 {
        self.state().merged.editor_padding_x.filter(|n| n.is_finite() && *n >= 0.0).map_or(0, |n| n as u64)
    }

    pub fn set_editor_padding_x(&self, padding: u64) {
        self.set_global_value("editorPaddingX", Some(json!(padding.min(3))));
    }

    /// `0` or `1`; anything else is the default `1`.
    pub fn output_pad(&self) -> u8 {
        if self.state().merged.output_pad.as_ref().and_then(Value::as_f64) == Some(0.0) { 0 } else { 1 }
    }

    pub fn set_output_pad(&self, padding: u8) {
        self.set_global_value("outputPad", Some(json!(padding.min(1))));
    }

    pub fn autocomplete_max_visible(&self) -> u64 {
        self.state().merged.autocomplete_max_visible.filter(|n| n.is_finite() && *n >= 0.0).map_or(5, |n| n as u64)
    }

    pub fn set_autocomplete_max_visible(&self, max_visible: u64) {
        self.set_global_value("autocompleteMaxVisible", Some(json!(max_visible.clamp(3, 20))));
    }

    pub fn code_block_indent(&self) -> String {
        let markdown = self.state().merged.markdown.clone().unwrap_or_default();
        markdown.code_block_indent.unwrap_or_else(|| "  ".into())
    }

    pub fn mermaid_rendering_mode(&self) -> MermaidRenderingMode {
        self.state().merged.markdown.as_ref().and_then(|markdown| markdown.mermaid).unwrap_or_default()
    }

    pub fn set_mermaid_rendering_mode(&self, mode: MermaidRenderingMode) {
        self.set_global_nested("markdown", "mermaid", serde_json::to_value(mode).ok());
    }

    pub fn codemode_mode(&self) -> CodemodeMode {
        self.state().merged.codemode.as_ref().and_then(|codemode| codemode.mode).unwrap_or_default()
    }

    pub fn codemode_inline_budget(&self) -> u64 {
        let budget = self.state().merged.codemode.as_ref().and_then(|codemode| codemode.inline_budget);
        budget.filter(|n| n.is_finite() && *n >= 0.0).map_or(3000, |n| n as u64)
    }
}
