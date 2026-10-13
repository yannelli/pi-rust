//! Theme colors for HTML export (upstream `getResolvedThemeColors`, `isLightTheme`, `getThemeExportColors`).

use pi_tui::colors::{ColorValue, color_to_hex, indexed_color_from_number, parse_color};

use super::ThemeError;
use super::registry::{current_theme_name, load_theme, load_theme_json, resolve_var_refs};
use super::system::SYSTEM_THEME_NAME;
use super::terminal::TerminalTheme;
use crate::export::ExportTheme;

/// The explicit `export` colors of a theme, as CSS values. Unset entries derive from `userMessageBg`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThemeExportColors {
    pub page_bg: Option<String>,
    pub card_bg: Option<String>,
    pub info_bg: Option<String>,
}

fn selected_name(name: Option<&str>) -> String {
    name.map(String::from).or_else(current_theme_name).unwrap_or_else(|| SYSTEM_THEME_NAME.to_string())
}

/// Every token's concrete color as `#rrggbb`, in theme order.
pub fn get_resolved_theme_colors(name: Option<&str>) -> Result<Vec<(String, String)>, ThemeError> {
    let theme = load_theme(&selected_name(name), None)?;
    let colors = theme.colors();
    Ok(colors.iter().map(|(token, color)| (token.name().to_string(), color_to_hex(color))).collect())
}

/// Whether a theme is designed for light backgrounds.
pub fn is_light_theme(name: Option<&str>) -> Result<bool, ThemeError> {
    Ok(load_theme(&selected_name(name), None)?.appearance() == TerminalTheme::Light)
}

/// The theme's explicit export colors. CSS understands hex and `oklch()` but not `okhsl()`, so OKHSL
/// values and palette indices become hex. Unknown themes and the system theme have none.
pub fn get_theme_export_colors(name: Option<&str>) -> ThemeExportColors {
    let name = selected_name(name);
    if name == SYSTEM_THEME_NAME {
        return ThemeExportColors::default();
    }
    let Ok(json) = load_theme_json(&name) else { return ThemeExportColors::default() };
    let Some(export) = json.export else { return ThemeExportColors::default() };
    let resolve = |value: &Option<ColorValue>| -> Result<Option<String>, ThemeError> {
        let Some(value) = value else { return Ok(None) };
        match resolve_var_refs(value, &json.vars)? {
            ColorValue::Number(index) => {
                let color = indexed_color_from_number(index).map_err(|error| ThemeError(error.0))?;
                Ok(Some(color_to_hex(&color)))
            }
            ColorValue::Text(text) if text.is_empty() => Ok(None),
            ColorValue::Text(text) if text.to_ascii_lowercase().starts_with("okhsl(") => {
                let color = parse_color(&text).map_err(|error| ThemeError(error.0))?;
                Ok(Some(color_to_hex(&color)))
            }
            ColorValue::Text(text) => Ok(Some(text)),
        }
    };
    let resolved = (|| {
        Ok::<_, ThemeError>(ThemeExportColors {
            page_bg: resolve(&export.page_bg)?,
            card_bg: resolve(&export.card_bg)?,
            info_bg: resolve(&export.info_bg)?,
        })
    })();
    resolved.unwrap_or_default()
}

/// The HTML exporter's theme for a theme name (the active theme when `None`).
pub fn export_theme(name: Option<&str>) -> Result<ExportTheme, ThemeError> {
    let mut theme = ExportTheme::new(get_resolved_theme_colors(name)?);
    let export = get_theme_export_colors(name);
    theme.page_bg = export.page_bg;
    theme.card_bg = export.card_bg;
    theme.info_bg = export.info_bg;
    Ok(theme)
}
