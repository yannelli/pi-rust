//! Chip shapes: how the colored segments on one side of a status line are drawn and joined.

use pi_tui::colors::{TerminalColorMode, background_ansi, foreground_ansi};

use super::themes::{color, ink_for};
use crate::theme::{ThemeColor, theme};

const RESET: &str = "\x1b[0m";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chip {
    /// Colored text joined by the separator.
    Plain,
    /// Text on a colored block.
    Block,
    /// A block with round ends (Nerd Font).
    Rounded,
    /// Blocks joined by arrows (Powerline or Nerd Font).
    Powerline,
    /// Blocks joined by slants (Nerd Font).
    Slanted,
    /// `[text]` in color.
    Brackets,
    /// A colored dot before the text.
    Dot,
}

impl Chip {
    pub const NAMES: &str = "plain, block, rounded, powerline, slanted, brackets, or dot";

    /// The separator for this shape when the settings name none.
    pub fn default_separator(self) -> &'static str {
        match self {
            Chip::Plain => " • ",
            Chip::Dot => "  ",
            _ => " ",
        }
    }

    /// A chip name; `pills` is the older name of `block`.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "plain" => Some(Chip::Plain),
            "block" | "pills" => Some(Chip::Block),
            "rounded" => Some(Chip::Rounded),
            "powerline" => Some(Chip::Powerline),
            "slanted" => Some(Chip::Slanted),
            "brackets" => Some(Chip::Brackets),
            "dot" => Some(Chip::Dot),
            _ => None,
        }
    }
}

/// The text color of a `plain` chip: a pi theme token or a theme color.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Fg {
    Token(ThemeColor),
    Rgb([u8; 3]),
}

/// One segment to draw. `bg` is the block color; `fg` is the color for text-only shapes.
pub struct Piece {
    pub text: String,
    pub fg: Fg,
    pub bg: [u8; 3],
}

/// Draws `pieces`; `right` mirrors the arrows of `powerline` and `slanted` for a right-aligned group.
pub fn join(chip: Chip, pieces: &[Piece], separator: &str, right: bool, mode: TerminalColorMode) -> String {
    let fg = |rgb: [u8; 3]| foreground_ansi(&color(rgb), mode);
    let bg = |rgb: [u8; 3]| background_ansi(&color(rgb), mode);
    let ink = |rgb: [u8; 3]| fg(ink_for(rgb));
    let theme = theme();
    let colored = |piece: &Piece, text: &str| match piece.fg {
        Fg::Token(token) => theme.fg(token, text),
        Fg::Rgb(rgb) => format!("{}{text}{RESET}", fg(rgb)),
    };
    let block = |piece: &Piece| format!("{}{} {} ", bg(piece.bg), ink(piece.bg), piece.text);
    let joined = |parts: Vec<String>| parts.join(&theme.fg(ThemeColor::Dim, separator));
    match chip {
        Chip::Plain => joined(pieces.iter().map(|piece| colored(piece, &piece.text)).collect()),
        Chip::Brackets => joined(pieces.iter().map(|piece| colored(piece, &format!("[{}]", piece.text))).collect()),
        Chip::Dot => joined(pieces.iter().map(|piece| format!("{} {}", colored(piece, "●"), piece.text)).collect()),
        Chip::Block => joined(pieces.iter().map(|piece| format!("{}{RESET}", block(piece))).collect()),
        Chip::Rounded => joined(
            pieces
                .iter()
                .map(|piece| format!("{}\u{e0b6}{}{RESET}{}\u{e0b4}{RESET}", fg(piece.bg), block(piece), fg(piece.bg)))
                .collect(),
        ),
        Chip::Powerline | Chip::Slanted => {
            let (left_cap, right_cap) =
                if chip == Chip::Powerline { ('\u{e0b0}', '\u{e0b2}') } else { ('\u{e0bc}', '\u{e0ba}') };
            let mut out = String::new();
            for (index, piece) in pieces.iter().enumerate() {
                let previous = index.checked_sub(1).map(|index| pieces[index].bg);
                if right {
                    let behind = previous.map_or_else(|| "\x1b[49m".to_string(), bg);
                    out.push_str(&format!("{}{behind}{right_cap}{}", fg(piece.bg), block(piece)));
                } else {
                    if let Some(previous) = previous {
                        out.push_str(&format!("{}{}{left_cap}", fg(previous), bg(piece.bg)));
                    }
                    out.push_str(&block(piece));
                }
            }
            match pieces.last() {
                Some(last) if !right => out.push_str(&format!("{RESET}{}{left_cap}{RESET}", fg(last.bg))),
                Some(_) => out.push_str(RESET),
                None => {}
            }
            out
        }
    }
}
