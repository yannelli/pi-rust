//! Building themes from the theme files the resource loader found (upstream `ResourceLoader.loadThemes`).

use std::sync::Arc;

use pi_tui::colors::TerminalColorMode;

use super::registry::load_theme_from_path;
use super::theme::Theme;
use crate::resources::{LoadedResources, ResourceDiagnostic, ThemeFile};

/// Loaded themes and the problems found while loading them.
#[derive(Debug, Default)]
pub struct LoadedThemes {
    pub themes: Vec<Arc<Theme>>,
    pub diagnostics: Vec<ResourceDiagnostic>,
}

/// Validate and build each theme file. Invalid files become warnings naming the file, like upstream.
pub fn load_theme_files(files: &[ThemeFile], mode: Option<TerminalColorMode>) -> LoadedThemes {
    let mut loaded = LoadedThemes::default();
    for file in files {
        match load_theme_from_path(&file.path, mode) {
            Ok(mut theme) => {
                theme.set_source_info(Some(file.source_info.clone()));
                loaded.themes.push(Arc::new(theme));
            }
            Err(error) => loaded.diagnostics.push(ResourceDiagnostic::warning(error.0, &file.path)),
        }
    }
    loaded
}

/// Load the themes of a resource load, keeping the loader's own diagnostics (missing paths, collisions).
pub fn load_resource_themes(resources: &LoadedResources, mode: Option<TerminalColorMode>) -> LoadedThemes {
    let mut loaded = load_theme_files(&resources.themes, mode);
    let mut diagnostics = resources.theme_diagnostics.clone();
    diagnostics.append(&mut loaded.diagnostics);
    loaded.diagnostics = diagnostics;
    loaded
}
