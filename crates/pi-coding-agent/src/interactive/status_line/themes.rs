//! Status line color themes. Each theme gives one color per role; chip shapes use it as the
//! background (with dark or light ink picked by contrast) and `plain` uses it as the text color.

use pi_tui::colors::{Color, rgb_color};

/// What a segment shows, which picks its color in a theme.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Path,
    Git,
    Model,
    Thinking,
    Session,
    Tokens,
    Cache,
    Cost,
    ContextOk,
    ContextWarn,
    ContextHigh,
    Time,
    Meta,
    State,
}

const ROLES: usize = 14;

/// `(name, colors by role in [`Role`] order)`.
const THEMES: [(&str, [&str; ROLES]); 15] = [
    (
        "pi-rust",
        [
            "#5FA0BF", "#F2B44C", "#EF6F3A", "#C4552A", "#9A8FBF", "#9A8FBF", "#6FB3B8", "#8FBF7F", "#8FBF7F",
            "#F2B44C", "#E5534B", "#6FB3B8", "#7A6A60", "#C4552A",
        ],
    ),
    (
        "pi",
        [
            "#4D9ABF", "#F1BE58", "#F09082", "#E4A08E", "#A3C4D9", "#A3C4D9", "#8FC1B5", "#9CC58F", "#9CC58F",
            "#F1BE58", "#E5534B", "#4D9ABF", "#8A8F98", "#F09082",
        ],
    ),
    (
        "claude",
        [
            "#E8E6DC", "#CC9B7A", "#D97757", "#C15F3C", "#CFC9B8", "#A3A08F", "#B9B49F", "#8A9A5B", "#8A9A5B",
            "#D9A757", "#BF4D43", "#B0AEA5", "#9C9A92", "#D97757",
        ],
    ),
    (
        "codex",
        [
            "#E5E5E5", "#A3A3A3", "#10A37F", "#0E8A6B", "#D4D4D4", "#737373", "#6B7280", "#10A37F", "#10A37F",
            "#E6A23C", "#EF4444", "#A3A3A3", "#525252", "#10A37F",
        ],
    ),
    (
        "cursor",
        [
            "#374151", "#4B5563", "#F5F5F5", "#9CA3AF", "#6B7280", "#6B7280", "#4B5563", "#3B82F6", "#22C55E",
            "#EAB308", "#EF4444", "#374151", "#1F2937", "#3B82F6",
        ],
    ),
    (
        "dracula",
        [
            "#6272A4", "#F1FA8C", "#BD93F9", "#FF79C6", "#8BE9FD", "#8BE9FD", "#8BE9FD", "#50FA7B", "#50FA7B",
            "#FFB86C", "#FF5555", "#6272A4", "#44475A", "#FF79C6",
        ],
    ),
    (
        "nord",
        [
            "#5E81AC", "#EBCB8B", "#88C0D0", "#81A1C1", "#B48EAD", "#8FBCBB", "#8FBCBB", "#A3BE8C", "#A3BE8C",
            "#EBCB8B", "#BF616A", "#4C566A", "#434C5E", "#88C0D0",
        ],
    ),
    (
        "catppuccin",
        [
            "#89B4FA", "#F9E2AF", "#CBA6F7", "#F5C2E7", "#94E2D5", "#89DCEB", "#94E2D5", "#A6E3A1", "#A6E3A1",
            "#FAB387", "#F38BA8", "#B4BEFE", "#585B70", "#F5C2E7",
        ],
    ),
    (
        "gruvbox",
        [
            "#458588", "#D79921", "#D65D0E", "#B16286", "#689D6A", "#83A598", "#8EC07C", "#98971A", "#98971A",
            "#D79921", "#CC241D", "#504945", "#3C3836", "#FE8019",
        ],
    ),
    (
        "tokyo-night",
        [
            "#7AA2F7", "#E0AF68", "#BB9AF7", "#F7768E", "#7DCFFF", "#2AC3DE", "#73DACA", "#9ECE6A", "#9ECE6A",
            "#E0AF68", "#F7768E", "#565F89", "#414868", "#BB9AF7",
        ],
    ),
    (
        "solarized",
        [
            "#268BD2", "#B58900", "#CB4B16", "#D33682", "#6C71C4", "#2AA198", "#2AA198", "#859900", "#859900",
            "#B58900", "#DC322F", "#586E75", "#073642", "#6C71C4",
        ],
    ),
    (
        "rose-pine",
        [
            "#31748F", "#F6C177", "#EBBCBA", "#C4A7E7", "#9CCFD8", "#9CCFD8", "#9CCFD8", "#9CCFD8", "#9CCFD8",
            "#F6C177", "#EB6F92", "#6E6A86", "#26233A", "#C4A7E7",
        ],
    ),
    (
        "monokai",
        [
            "#66D9EF", "#E6DB74", "#F92672", "#AE81FF", "#A6E22E", "#66D9EF", "#A6E22E", "#A6E22E", "#A6E22E",
            "#FD971F", "#F92672", "#75715E", "#49483E", "#AE81FF",
        ],
    ),
    (
        "one-dark",
        [
            "#61AFEF", "#E5C07B", "#C678DD", "#E06C75", "#56B6C2", "#56B6C2", "#98C379", "#98C379", "#98C379",
            "#D19A66", "#E06C75", "#5C6370", "#3E4451", "#C678DD",
        ],
    ),
    (
        "mono",
        [
            "#D4D4D4", "#A3A3A3", "#F5F5F5", "#BDBDBD", "#8C8C8C", "#8C8C8C", "#8C8C8C", "#D4D4D4", "#BDBDBD",
            "#E0E0E0", "#FFFFFF", "#737373", "#525252", "#F5F5F5",
        ],
    ),
];

/// The theme names, for docs and error messages.
pub fn theme_names() -> impl Iterator<Item = &'static str> {
    THEMES.iter().map(|(name, _)| *name)
}

/// A theme's colors; `None` for an unknown name (the `terminal` theme uses pi's theme tokens).
#[derive(Clone, Debug, PartialEq)]
pub struct Palette {
    colors: [[u8; 3]; ROLES],
}

impl Palette {
    pub fn named(name: &str) -> Option<Self> {
        let (_, hex) = THEMES.iter().find(|(theme, _)| *theme == name)?;
        let mut colors = [[0; 3]; ROLES];
        for (slot, value) in colors.iter_mut().zip(hex) {
            *slot = parse_hex(value)?;
        }
        Some(Self { colors })
    }

    pub fn rgb(&self, role: Role) -> [u8; 3] {
        self.colors[role as usize]
    }
}

/// `#rrggbb` as bytes.
pub fn parse_hex(value: &str) -> Option<[u8; 3]> {
    let hex = value.strip_prefix('#').filter(|hex| hex.len() == 6)?;
    let channel = |at: usize| u8::from_str_radix(hex.get(at..at + 2)?, 16).ok();
    Some([channel(0)?, channel(2)?, channel(4)?])
}

pub fn color([r, g, b]: [u8; 3]) -> Color {
    rgb_color(f64::from(r), f64::from(g), f64::from(b)).unwrap_or(Color::Indexed(7))
}

/// Dark ink on light backgrounds and light ink on dark ones (WCAG relative luminance).
pub fn ink_for([r, g, b]: [u8; 3]) -> [u8; 3] {
    let linear = |channel: u8| {
        let value = f64::from(channel) / 255.0;
        if value <= 0.04045 { value / 12.92 } else { ((value + 0.055) / 1.055).powf(2.4) }
    };
    let luminance = 0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b);
    if luminance > 0.22 { [27, 27, 27] } else { [245, 245, 245] }
}
