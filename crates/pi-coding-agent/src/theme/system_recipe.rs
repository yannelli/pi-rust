//! The `system` theme recipe: color families, contrast levels, and the rules tying tokens to surfaces.
//! Ported from upstream `system-theme.ts`; the curve coefficients are the upstream fitted values.

use super::terminal::TerminalTheme;
use super::tokens::{ThemeBg, ThemeColor, ThemeToken};

/// A family's OKHSL hue and saturation: `max` at mid lightness, falling toward `min` at black and white.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Family {
    pub hue: f64,
    pub min: f64,
    pub max: f64,
    /// ANSI palette slot the family takes its hue and saturation from.
    pub slot: usize,
}

const fn family(hue: f64, min: f64, max: f64, slot: usize) -> Family {
    Family { hue, min, max, slot }
}

pub(crate) const NEUTRAL: Family = family(231.49, 0.02, 0.08, 8);
const BLUE: Family = family(231.49, 0.1, 0.68, 4);
const GREEN: Family = family(158.68, 0.1, 0.76, 2);
const RED: Family = family(20.0, 0.1, 0.92, 1);
const YELLOW: Family = family(82.36, 0.5, 1.0, 3);
const ORANGE: Family = family(52.0, 0.12, 0.85, 3);
const VIOLET: Family = family(295.0, 0.2, 0.6, 5);
const CALAMINE: Family = family(202.43, 0.1, 0.74, 6);
const THINKING_SLATE: Family = family(231.49, 0.08, 0.2, 4);
const THINKING_BLUE: Family = family(231.49, 0.2, 0.45, 4);
const THINKING_PERIWINKLE: Family = family(263.25, 0.3, 0.6, 6);
const THINKING_VIOLET: Family = family(295.0, 0.4, 0.75, 5);
const THINKING_MAGENTA: Family = family(337.5, 0.5, 0.85, 13);
const THINKING_RED: Family = family(20.0, 0.95, 1.0, 1);

/// The family of a token, and whether it is the neutral family.
pub(crate) fn token_family(token: ThemeToken) -> (Family, bool) {
    use ThemeToken::*;
    let family = match token {
        SelectedBg | UserMessageBg | Border | MdLink | SyntaxKeyword => BLUE,
        SearchMatchBg | SyntaxString => ORANGE,
        CustomMessageBg | Accent | BorderAccent | CustomMessageLabel | MdCode | MdListBullet | SyntaxType => VIOLET,
        ToolSuccessBg | Success | MdCodeBlock | ToolDiffAdded | BashMode | SyntaxNumber => GREEN,
        ToolErrorBg | Error | ToolDiffRemoved => RED,
        Warning | MdHeading | SyntaxFunction => YELLOW,
        SyntaxVariable => CALAMINE,
        ThinkingMinimal => THINKING_SLATE,
        ThinkingLow => THINKING_BLUE,
        ThinkingMedium => THINKING_PERIWINKLE,
        ThinkingHigh => THINKING_VIOLET,
        ThinkingXhigh => THINKING_MAGENTA,
        ThinkingMax => THINKING_RED,
        _ => return (NEUTRAL, true),
    };
    (family, false)
}

/// The palette slot of a token: its family's slot unless it would share a hue with a similar token.
pub(crate) fn token_slot(token: ThemeToken) -> usize {
    match token {
        ThemeToken::SyntaxString => 2,
        ThemeToken::SyntaxNumber => 5,
        ThemeToken::SearchMatchBg => 3,
        _ => token_family(token).0.slot,
    }
}

/// A target-lightness curve: a polynomial in the surface's OKLab lightness, and the surface range where it applies.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Curve {
    pub coefficients: [f64; 6],
    pub reachable: [f64; 2],
}

const fn curve(coefficients: [f64; 6], reachable: [f64; 2]) -> Curve {
    Curve { coefficients, reachable }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Level {
    Panel,
    Track,
    Thinking(usize),
    Subtle,
    Thumb,
    Readable,
    Emphasis,
    TextOnPanel,
    Text,
}

const THINKING_CURVES: [[Curve; 2]; 7] = [
    [
        curve([0.52988, -0.05809, -0.30924, 4.63567, -6.52933, 2.89108], [0.0, 0.873]),
        curve([-28.27749, 182.85284, -469.62416, 603.15916, -384.59976, 97.35147], [0.51, 1.0]),
    ],
    [
        curve([0.55278, -0.03667, -0.45659, 4.95347, -6.90265, 3.0706], [0.0, 0.858]),
        curve([-37.10484, 235.86282, -596.62344, 754.3633, -474.00763, 118.3551], [0.535, 1.0]),
    ],
    [
        curve([0.57486, -0.01765, -0.58987, 5.25227, -7.27175, 3.25532], [0.0, 0.842]),
        curve([-59.89653, 377.05024, -945.07843, 1182.03145, -734.96375, 181.68658], [0.556, 1.0]),
    ],
    [
        curve([0.59621, -0.00062, -0.71148, 5.53588, -7.6392, 3.44606], [0.0, 0.827]),
        curve([-72.07122, 445.84082, -1099.57352, 1353.88793, -829.53392, 202.26164], [0.58, 1.0]),
    ],
    [
        curve([0.61691, 0.01462, -0.82288, 5.80651, -8.00641, 3.64333], [0.0, 0.811]),
        curve([-110.14338, 674.21488, -1645.75941, 2004.32367, -1215.15899, 293.3183], [0.6, 1.0]),
    ],
    [
        curve([0.63702, 0.02826, -0.92498, 6.06465, -8.37246, 3.84651], [0.0, 0.795]),
        curve([-175.47701, 1063.54495, -2570.70594, 3098.80776, -1860.15527, 444.76392], [0.62, 1.0]),
    ],
    [
        curve([0.65658, 0.04044, -1.01835, 6.30989, -8.73529, 4.05439], [0.0, 0.779]),
        curve([-183.81712, 1094.70055, -2602.68539, 3088.71276, -1826.91131, 430.75931], [0.643, 1.0]),
    ],
];

/// The curve of a level for a terminal appearance.
pub(crate) fn level_curve(level: Level, appearance: TerminalTheme) -> Curve {
    let [dark, light] = match level {
        Level::Panel => [
            curve([0.29131, -0.39746, 2.33185, -0.85524, -1.2076, 0.86276], [0.0, 0.979]),
            curve([-3.74073, 27.94549, -78.44258, 112.6798, -79.60015, 22.11277], [0.348, 1.0]),
        ],
        Level::Track => [
            curve([0.39028, -0.23015, 0.83573, 2.43829, -4.38292, 2.01582], [0.0, 0.946]),
            curve([-5.24921, 38.37322, -107.28833, 152.10005, -106.17127, 29.18061], [0.368, 1.0]),
        ],
        Level::Thinking(index) => THINKING_CURVES[index],
        Level::Subtle => [
            curve([0.56762, -0.02475, -0.5383, 5.12628, -7.10931, 3.17324], [0.0, 0.848]),
            curve([-232.85459, 1376.54473, -3249.11801, 3827.91186, -2248.29472, 526.55751], [0.657, 1.0]),
        ],
        Level::Thumb => [
            curve([0.60323, 0.00278, -0.73328, 5.57157, -7.68067, 3.46933], [0.0, 0.823]),
            curve([-82.89897, 511.01355, -1255.98095, 1540.76821, -940.68087, 228.58523], [0.586, 1.0]),
        ],
        Level::Readable => [
            curve([0.66937, 0.04704, -1.06871, 6.43941, -8.9332, 4.17229], [0.0, 0.77]),
            curve([-1554.52576, 8733.56817, -19604.93507, 21977.72696, -12300.99599, 2749.81288], [0.751, 1.0]),
        ],
        Level::Emphasis => [
            curve([0.7303, 0.07695, -1.31626, 7.1681, -10.14436, 4.92846], [0.0, 0.712]),
            curve([-4948.31942, 26870.91986, -58334.48399, 63280.17197, -34298.01053, 7430.30146], [0.811, 1.0]),
        ],
        Level::TextOnPanel => [
            curve([0.86713, 0.05232, -0.89428, 4.79014, -5.5432, 1.75023], [0.0, 0.542]),
            curve([-8570.89457, 43954.60805, -90084.00702, 92220.6791, -47152.15802, 9632.27113], [0.867, 1.0]),
        ],
        Level::Text => [
            curve([0.89242, 0.02311, -0.44862, 2.34417, -0.06084, -2.63844], [0.0, 0.5]),
            curve([-2004.67048, 6664.47299, -6060.70202, -1792.61209, 5133.82359, -1939.85583], [0.894, 1.0]),
        ],
    };
    match appearance {
        TerminalTheme::Dark => dark,
        TerminalTheme::Light => light,
    }
}

/// A surface a token is drawn on: the terminal background or another token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Surface {
    Background,
    Token(ThemeToken),
}

pub(crate) struct Rule {
    pub token: ThemeToken,
    pub on: Vec<Surface>,
    pub level: Level,
}

pub(crate) const PANELS: [ThemeBg; 7] = [
    ThemeBg::UserMessageBg,
    ThemeBg::ToolPendingBg,
    ThemeBg::ToolSuccessBg,
    ThemeBg::ToolErrorBg,
    ThemeBg::SelectedBg,
    ThemeBg::SearchMatchBg,
    ThemeBg::CustomMessageBg,
];

/// Text-level tokens that take the terminal's foreground.
pub(crate) const FOREGROUND_TOKENS: [ThemeColor; 3] =
    [ThemeColor::Text, ThemeColor::UserMessageText, ThemeColor::ToolTitle];

pub(crate) fn is_panel(token: ThemeToken) -> bool {
    PANELS.iter().any(|panel| panel.token() == token)
}

/// The contrast rules, in upstream order.
pub(crate) fn rules() -> Vec<Rule> {
    use ThemeToken::*;
    let s = Surface::Token;
    let bg = Surface::Background;
    let tool = [s(ToolPendingBg), s(ToolSuccessBg), s(ToolErrorBg)];
    let message = [s(UserMessageBg), s(CustomMessageBg)];
    let join = |parts: &[&[Surface]]| -> Vec<Surface> { parts.concat() };
    let mut rules = Vec::new();
    let mut each = |tokens: &[ThemeToken], on: Vec<Surface>, level: Level| {
        for &token in tokens {
            rules.push(Rule { token, on: on.clone(), level });
        }
    };
    let panels: Vec<ThemeToken> = PANELS.iter().map(|panel| panel.token()).collect();
    each(&panels, vec![bg], Level::Panel);
    each(&[Text], vec![bg], Level::Text);
    each(&[Text], vec![s(SelectedBg)], Level::TextOnPanel);
    each(&[UserMessageText], vec![s(UserMessageBg)], Level::TextOnPanel);
    each(&[ToolTitle], tool.to_vec(), Level::TextOnPanel);
    each(&[Accent, Success, Error, Warning], join(&[&[bg, s(SelectedBg)], &tool]), Level::Readable);
    let muted_surfaces = join(&[&[bg, s(SelectedBg), s(CustomMessageBg)], &tool]);
    each(&[Muted], muted_surfaces.clone(), Level::Readable);
    each(&[Dim], muted_surfaces, Level::Subtle);
    each(&[ThinkingText], vec![bg], Level::Readable);
    each(&[CustomMessageText], join(&[&[s(CustomMessageBg)], &tool]), Level::Readable);
    each(&[CustomMessageLabel], join(&[&[bg, s(CustomMessageBg), s(SelectedBg)], &tool]), Level::Readable);
    each(&[ToolOutput], join(&[&[bg], &tool]), Level::Readable);
    let markdown = [MdHeading, MdLink, MdLinkUrl, MdCode, MdQuote, MdCodeBlockBorder, MdListBullet];
    each(&markdown, join(&[&[bg], &message]), Level::Readable);
    let everywhere = join(&[&[bg], &message, &tool]);
    each(&[MdCodeBlock], everywhere.clone(), Level::Readable);
    each(&[ToolDiffAdded, ToolDiffRemoved, ToolDiffContext], join(&[&[bg], &tool]), Level::Readable);
    let syntax = [
        SyntaxComment,
        SyntaxKeyword,
        SyntaxFunction,
        SyntaxVariable,
        SyntaxString,
        SyntaxNumber,
        SyntaxType,
        SyntaxOperator,
        SyntaxPunctuation,
    ];
    each(&syntax, everywhere.clone(), Level::Readable);
    each(&[SearchMatchText], vec![s(SearchMatchBg)], Level::Readable);
    each(&[BashMode, Border, BorderAccent], vec![bg], Level::Readable);
    each(&[BorderMuted], vec![bg], Level::Subtle);
    each(&[MdQuoteBorder, MdHr], everywhere, Level::Readable);
    each(&[ScrollbarTrack], vec![bg], Level::Track);
    each(&[ScrollbarThumb], vec![s(ScrollbarTrack)], Level::Thumb);
    let thinking =
        [ThinkingOff, ThinkingMinimal, ThinkingLow, ThinkingMedium, ThinkingHigh, ThinkingXhigh, ThinkingMax];
    for (index, token) in thinking.into_iter().enumerate() {
        each(&[token], vec![bg], Level::Thinking(index));
    }
    rules
}

/// Tokens in dependency order: every surface before the tokens drawn on it.
pub(crate) fn solve_order(rules: &[Rule]) -> Vec<ThemeToken> {
    fn visit(token: ThemeToken, rules: &[Rule], order: &mut Vec<ThemeToken>) {
        if order.contains(&token) {
            return;
        }
        for rule in rules.iter().filter(|rule| rule.token == token) {
            for surface in &rule.on {
                if let Surface::Token(surface) = surface {
                    visit(*surface, rules, order);
                }
            }
        }
        order.push(token);
    }
    let mut order = Vec::new();
    for rule in rules {
        visit(rule.token, rules, &mut order);
    }
    order
}
