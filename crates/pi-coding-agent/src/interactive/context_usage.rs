//! `/context` (pi-rust extension): the shared report painted with the current theme.

use pi_tui::colors::{Color, color_to_oklch, oklch_color};

use super::mode::InteractiveMode;
use crate::agent_session::context_breakdown::ContextBreakdown;
use crate::headless_commands::{ContextReport, ContextStyle};
use crate::theme::{StyleFg, Theme, ThemeColor, ThemeStyle, theme};

/// Mild variations of the accent, one per part: the hue turns and the lightness steps a little,
/// so the parts also differ when the accent is gray.
fn part_colors(theme: &Theme) -> Vec<Color> {
    let accent_color = theme.color(ThemeColor::Accent);
    let accent = color_to_oklch(&accent_color);
    let middle = (ContextBreakdown::default().parts().len() - 1) as f64 / 2.0;
    (0..ContextBreakdown::default().parts().len())
        .map(|part| {
            let offset = part as f64 - middle;
            let lightness = (accent.l + offset * 0.03).clamp(0.0, 1.0);
            oklch_color(lightness, accent.c, accent.h + offset * 12.0).unwrap_or(accent_color)
        })
        .collect()
}

impl InteractiveMode {
    /// `/context`: the report is built once and painted with the current theme.
    pub(super) fn handle_context_command(&self) {
        let report = ContextReport::of_session(&self.session());
        let render = move || {
            let t = theme();
            let colors = part_colors(&t);
            let style = |style: ContextStyle, text: &str| match style {
                ContextStyle::Title => t.bold(text),
                ContextStyle::Label => t.fg(ThemeColor::Dim, text),
                ContextStyle::Part(part) => {
                    let fg = colors.get(part).copied().map(StyleFg::Color);
                    t.style(text, &ThemeStyle { fg, ..ThemeStyle::default() })
                }
            };
            report.text(&style)
        };
        self.add_spacer();
        self.add_text(render, 1, 0);
        self.request_render();
    }
}
