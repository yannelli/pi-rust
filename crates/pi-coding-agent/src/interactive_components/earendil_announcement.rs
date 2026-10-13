//! [`EarendilAnnouncementComponent`]: the `/dementedelves` announcement (upstream
//! `earendil-announcement.ts`).

use std::rc::Rc;
use std::sync::OnceLock;

use base64::Engine;
use pi_tui::components::{Image, ImageOptions, ImageTheme};
use pi_tui::{Component, Container, component};

use super::DynamicBorder;
use super::common::{spacer, text};
use crate::theme::{ThemeColor, theme};

pub const BLOG_URL: &str = "https://mariozechner.at/posts/2026-04-08-ive-sold-out/";
pub const IMAGE_FILENAME: &str = "clankolas.png";
const IMAGE_BYTES: &[u8] = include_bytes!("assets/clankolas.png");

fn image_base64() -> &'static str {
    static CACHED: OnceLock<String> = OnceLock::new();
    CACHED.get_or_init(|| base64::engine::general_purpose::STANDARD.encode(IMAGE_BYTES))
}

fn accent_border() -> DynamicBorder {
    DynamicBorder::with_color(|text: &str| theme().fg(ThemeColor::Accent, text))
}

/// Accent-bordered notice with the blog link and the bundled image.
pub struct EarendilAnnouncementComponent {
    container: Container,
}

impl Default for EarendilAnnouncementComponent {
    fn default() -> Self {
        Self::new()
    }
}

impl EarendilAnnouncementComponent {
    pub fn new() -> Self {
        let active = theme();
        let mut container = Container::new();
        container.add_child(component(accent_border()));
        let title = active.bold(&active.fg(ThemeColor::Accent, "pi has joined Earendil"));
        container.add_child(text(&title, 1, 0));
        container.add_child(spacer(1));
        container.add_child(text(&active.fg(ThemeColor::Muted, "Read the blog post:"), 1, 0));
        container.add_child(text(&active.fg(ThemeColor::MdLink, BLOG_URL), 1, 0));
        container.add_child(spacer(1));

        let image_theme = ImageTheme { fallback_color: Rc::new(|text: &str| theme().fg(ThemeColor::Muted, text)) };
        let options = ImageOptions {
            max_width_cells: Some(56),
            filename: Some(IMAGE_FILENAME.to_string()),
            ..ImageOptions::default()
        };
        container.add_child(component(Image::new(image_base64(), "image/png", image_theme, options, None)));
        container.add_child(spacer(1));

        container.add_child(component(accent_border()));
        Self { container }
    }
}

impl Component for EarendilAnnouncementComponent {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.container.render(width)
    }

    fn invalidate(&mut self) {
        self.container.invalidate();
    }
}
