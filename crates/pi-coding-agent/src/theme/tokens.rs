//! Theme token metadata, ported from upstream `theme-tokens.ts`. [`ThemeToken`] lists every token in
//! theme-file order; [`ThemeColor`] (foreground) and [`ThemeBg`] (background) keep each token in its slot.

use std::fmt;

/// Whether a token colors text (`foreground`) or the cells behind it (`background`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TokenSlot {
    Foreground,
    Background,
}

/// One token's slot, description (as published in the JSON Schema), and optional fallback token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenDescriptor {
    pub token: ThemeToken,
    pub name: &'static str,
    pub slot: TokenSlot,
    pub description: &'static str,
    pub fallback: Option<ThemeToken>,
}

macro_rules! theme_tokens {
    ($($variant:ident = $name:literal, $slot:ident, $description:literal $(, $fallback:ident)?;)*) => {
        /// Every theme token, in the order of theme files and the published JSON Schema.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum ThemeToken {
            $($variant,)*
        }

        /// Token metadata, indexed by `ThemeToken as usize`.
        pub const THEME_TOKENS: &[TokenDescriptor] = &[
            $(TokenDescriptor {
                token: ThemeToken::$variant,
                name: $name,
                slot: TokenSlot::$slot,
                description: $description,
                fallback: theme_tokens!(@fallback $($fallback)?),
            },)*
        ];
    };
    (@fallback) => { None };
    (@fallback $fallback:ident) => { Some(ThemeToken::$fallback) };
}

theme_tokens! {
    Accent = "accent", Foreground, "Primary accent color (logo, selected items, cursor)";
    Border = "border", Foreground, "Normal borders";
    BorderAccent = "borderAccent", Foreground, "Highlighted borders";
    BorderMuted = "borderMuted", Foreground, "Subtle borders";
    Success = "success", Foreground, "Success states";
    Error = "error", Foreground, "Error states";
    Warning = "warning", Foreground, "Warning states";
    Muted = "muted", Foreground, "Secondary/dimmed text";
    Dim = "dim", Foreground, "Very dimmed text (more subtle than muted)";
    Text = "text", Foreground, "Default text color (usually empty string)";
    ThinkingText = "thinkingText", Foreground, "Thinking block text color";
    ScrollbarTrack = "scrollbarTrack", Foreground,
        "Fullscreen scrollbar track foreground (falls back to muted when omitted)", Muted;
    ScrollbarThumb = "scrollbarThumb", Foreground,
        "Fullscreen scrollbar thumb foreground (falls back to text when omitted)", Text;
    SelectedBg = "selectedBg", Background, "Selected item background";
    SearchMatchBg = "searchMatchBg", Background,
        "Transcript search match background and current-match text (falls back to selectedBg when omitted)",
        SelectedBg;
    SearchMatchText = "searchMatchText", Foreground,
        "Transcript search match text and current-match background (falls back to text when omitted)", Text;
    UserMessageBg = "userMessageBg", Background, "User message background";
    UserMessageText = "userMessageText", Foreground, "User message text color";
    CustomMessageBg = "customMessageBg", Background, "Custom message background (hook-injected messages)";
    CustomMessageText = "customMessageText", Foreground, "Custom message text color";
    CustomMessageLabel = "customMessageLabel", Foreground, "Custom message type label color";
    ToolPendingBg = "toolPendingBg", Background, "Tool execution box (pending state)";
    ToolSuccessBg = "toolSuccessBg", Background, "Tool execution box (success state)";
    ToolErrorBg = "toolErrorBg", Background, "Tool execution box (error state)";
    ToolTitle = "toolTitle", Foreground, "Tool execution box title color";
    ToolOutput = "toolOutput", Foreground, "Tool execution box output text color";
    MdHeading = "mdHeading", Foreground, "Markdown heading text";
    MdLink = "mdLink", Foreground, "Markdown link text";
    MdLinkUrl = "mdLinkUrl", Foreground, "Markdown link URL";
    MdCode = "mdCode", Foreground, "Markdown inline code";
    MdCodeBlock = "mdCodeBlock", Foreground, "Markdown code block content";
    MdCodeBlockBorder = "mdCodeBlockBorder", Foreground, "Markdown code block fences";
    MdQuote = "mdQuote", Foreground, "Markdown blockquote text";
    MdQuoteBorder = "mdQuoteBorder", Foreground, "Markdown blockquote border";
    MdHr = "mdHr", Foreground, "Markdown horizontal rule";
    MdListBullet = "mdListBullet", Foreground, "Markdown list bullets/numbers";
    ToolDiffAdded = "toolDiffAdded", Foreground, "Added lines in tool diffs";
    ToolDiffRemoved = "toolDiffRemoved", Foreground, "Removed lines in tool diffs";
    ToolDiffContext = "toolDiffContext", Foreground, "Context lines in tool diffs";
    SyntaxComment = "syntaxComment", Foreground, "Syntax highlighting: comments";
    SyntaxKeyword = "syntaxKeyword", Foreground, "Syntax highlighting: keywords";
    SyntaxFunction = "syntaxFunction", Foreground, "Syntax highlighting: function names";
    SyntaxVariable = "syntaxVariable", Foreground, "Syntax highlighting: variable names";
    SyntaxString = "syntaxString", Foreground, "Syntax highlighting: string literals";
    SyntaxNumber = "syntaxNumber", Foreground, "Syntax highlighting: number literals";
    SyntaxType = "syntaxType", Foreground, "Syntax highlighting: type names";
    SyntaxOperator = "syntaxOperator", Foreground, "Syntax highlighting: operators";
    SyntaxPunctuation = "syntaxPunctuation", Foreground, "Syntax highlighting: punctuation";
    ThinkingOff = "thinkingOff", Foreground, "Thinking level border: off";
    ThinkingMinimal = "thinkingMinimal", Foreground, "Thinking level border: minimal";
    ThinkingLow = "thinkingLow", Foreground, "Thinking level border: low";
    ThinkingMedium = "thinkingMedium", Foreground, "Thinking level border: medium";
    ThinkingHigh = "thinkingHigh", Foreground, "Thinking level border: high";
    ThinkingXhigh = "thinkingXhigh", Foreground, "Thinking level border: xhigh";
    ThinkingMax = "thinkingMax", Foreground,
        "Thinking level border: max (falls back to thinkingXhigh when omitted)", ThinkingXhigh;
    BashMode = "bashMode", Foreground, "Editor border color in bash mode";
}

/// Number of theme tokens.
pub const TOKEN_COUNT: usize = THEME_TOKENS.len();

impl ThemeToken {
    /// All tokens in theme-file order.
    pub fn all() -> impl Iterator<Item = ThemeToken> {
        THEME_TOKENS.iter().map(|descriptor| descriptor.token)
    }

    pub fn descriptor(self) -> &'static TokenDescriptor {
        &THEME_TOKENS[self as usize]
    }

    /// The camelCase name used in theme files.
    pub fn name(self) -> &'static str {
        self.descriptor().name
    }

    pub fn slot(self) -> TokenSlot {
        self.descriptor().slot
    }

    pub fn fallback(self) -> Option<ThemeToken> {
        self.descriptor().fallback
    }

    pub fn from_name(name: &str) -> Option<ThemeToken> {
        THEME_TOKENS.iter().find(|descriptor| descriptor.name == name).map(|descriptor| descriptor.token)
    }

    pub fn index(self) -> usize {
        self as usize
    }
}

impl fmt::Display for ThemeToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

macro_rules! slot_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident),* $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum $name {
            $($variant,)*
        }

        impl $name {
            /// Every token of this slot, in theme-file order.
            pub const ALL: &'static [$name] = &[$($name::$variant,)*];

            pub const fn token(self) -> ThemeToken {
                match self {
                    $($name::$variant => ThemeToken::$variant,)*
                }
            }

            pub fn name(self) -> &'static str {
                self.token().name()
            }

            pub fn from_name(name: &str) -> Option<Self> {
                ThemeToken::from_name(name).and_then(|token| Self::try_from(token).ok())
            }
        }

        impl From<$name> for ThemeToken {
            fn from(value: $name) -> Self {
                value.token()
            }
        }

        impl TryFrom<ThemeToken> for $name {
            type Error = ThemeToken;

            fn try_from(token: ThemeToken) -> Result<Self, ThemeToken> {
                match token {
                    $(ThemeToken::$variant => Ok($name::$variant),)*
                    #[allow(unreachable_patterns)]
                    other => Err(other),
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.name())
            }
        }
    };
}

slot_enum! {
    /// A foreground token (upstream `ThemeColor`).
    ThemeColor {
        Accent, Border, BorderAccent, BorderMuted, Success, Error, Warning, Muted, Dim, Text, ThinkingText,
        ScrollbarTrack, ScrollbarThumb, SearchMatchText, UserMessageText, CustomMessageText, CustomMessageLabel,
        ToolTitle, ToolOutput, MdHeading, MdLink, MdLinkUrl, MdCode, MdCodeBlock, MdCodeBlockBorder, MdQuote,
        MdQuoteBorder, MdHr, MdListBullet, ToolDiffAdded, ToolDiffRemoved, ToolDiffContext, SyntaxComment,
        SyntaxKeyword, SyntaxFunction, SyntaxVariable, SyntaxString, SyntaxNumber, SyntaxType, SyntaxOperator,
        SyntaxPunctuation, ThinkingOff, ThinkingMinimal, ThinkingLow, ThinkingMedium, ThinkingHigh, ThinkingXhigh,
        ThinkingMax, BashMode,
    }
}

slot_enum! {
    /// A background token (upstream `ThemeBg`).
    ThemeBg {
        SelectedBg, SearchMatchBg, UserMessageBg, CustomMessageBg, ToolPendingBg, ToolSuccessBg, ToolErrorBg,
    }
}

/// A value for every token, indexed by `ThemeToken as usize` (upstream `ResolvedThemeColorValues`).
#[derive(Debug, Clone, PartialEq)]
pub struct TokenMap<V>(pub(crate) Vec<V>);

impl<V> TokenMap<V> {
    pub fn get(&self, token: impl Into<ThemeToken>) -> &V {
        &self.0[token.into().index()]
    }

    pub fn set(&mut self, token: impl Into<ThemeToken>, value: V) {
        self.0[token.into().index()] = value;
    }

    /// `(token, value)` pairs in theme-file order.
    pub fn iter(&self) -> impl Iterator<Item = (ThemeToken, &V)> {
        ThemeToken::all().zip(self.0.iter())
    }

    pub fn from_fn(mut value: impl FnMut(ThemeToken) -> V) -> Self {
        TokenMap(ThemeToken::all().map(&mut value).collect())
    }
}

impl<V> std::ops::Index<ThemeToken> for TokenMap<V> {
    type Output = V;

    fn index(&self, token: ThemeToken) -> &V {
        &self.0[token.index()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_partition_the_tokens_and_fallbacks_share_a_slot() {
        assert_eq!(ThemeColor::ALL.len() + ThemeBg::ALL.len(), TOKEN_COUNT);
        for (index, descriptor) in THEME_TOKENS.iter().enumerate() {
            assert_eq!(descriptor.token.index(), index);
            let in_fg = ThemeColor::try_from(descriptor.token).is_ok();
            assert_eq!(in_fg, descriptor.slot == TokenSlot::Foreground, "{}", descriptor.name);
            if let Some(fallback) = descriptor.fallback {
                assert_eq!(fallback.slot(), descriptor.slot, "{}", descriptor.name);
                assert!(fallback.fallback().is_none());
            }
        }
        assert_eq!(ThemeToken::from_name("thinkingMax"), Some(ThemeToken::ThinkingMax));
        assert_eq!(ThemeBg::from_name("accent"), None);
        assert_eq!(ThemeColor::from_name("accent"), Some(ThemeColor::Accent));
    }
}
