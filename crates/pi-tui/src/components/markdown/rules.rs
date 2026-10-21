//! The GFM rules of `marked` 18, translated from the JavaScript regex sources.
//! JavaScript's ASCII `\d` and `\w` are spelled out; `\b_` in the text rule is redundant and dropped.

use std::sync::LazyLock;

use fancy_regex::{Regex as FancyRegex, RegexInput};
use regex::Regex;

fn fancy(pattern: &str) -> FancyRegex {
    FancyRegex::new(pattern).unwrap_or_else(|err| panic!("invalid markdown rule {pattern:?}: {err}"))
}

fn plain(pattern: &str) -> Regex {
    Regex::new(pattern).unwrap_or_else(|err| panic!("invalid markdown rule {pattern:?}: {err}"))
}

macro_rules! rule {
    ($name:ident, fancy, $pattern:expr) => {
        pub(crate) static $name: LazyLock<FancyRegex> = LazyLock::new(|| fancy(&$pattern));
    };
    ($name:ident, plain, $pattern:expr) => {
        pub(crate) static $name: LazyLock<Regex> = LazyLock::new(|| plain(&$pattern));
    };
}

const TAG_NAMES: &str = "address|article|aside|base|basefont|blockquote|body|caption|center|col|colgroup|dd|details|dialog|dir|div|dl|dt|fieldset|figcaption|figure|footer|form|frame|frameset|h[1-6]|head|header|hr|html|iframe|legend|li|link|main|menu|menuitem|meta|nav|noframes|ol|optgroup|option|p|param|search|section|summary|table|tbody|td|tfoot|th|thead|title|tr|track|ul";
const HR_BODY: &str = r" {0,3}((?:-[\t ]*){3,}|(?:_[ \t]*){3,}|(?:\*[ \t]*){3,})(?:\n+|$)";
const HEADING_START: &str = r" {0,3}#{1,6}(?:\s|$)";
const FENCE_START: &str = r" {0,3}(?:`{3,}(?=[^`\n]*(?:\n|$))|~~~)[^\n]*(?:\n|$)";
const PUNCT: &str = r"[\p{P}\p{S}]";

fn html_start() -> String {
    format!(r"<\/?(?:{TAG_NAMES})(?: +|\n|\/?>)|<(?:script|pre|style|textarea|!--)")
}

fn table_body() -> String {
    format!(
        r" *([^\n ].*)\n {{0,3}}((?:\| *)?:?-+:? *(?:\| *:?-+:? *)*(?:\| *)?)(?:\n((?:(?! *\n|{HR_BODY}|{HEADING_START}| {{0,3}}>|(?: {{4}}| {{0,3}}\t)[^\n]|{FENCE_START}| {{0,3}}(?:[*+-]|1[.)])[ \t]|{html}).*(?:\n|$))*)\n*|$)",
        html = html_start()
    )
}

// Block rules.
rule!(NEWLINE, plain, r"^(?:[ \t]*(?:\n|$))+");
rule!(CODE, plain, r"^((?: {4}| {0,3}\t)[^\n]+(?:\n(?:[ \t]*(?:\n|$))*)?)+");
rule!(
    FENCES,
    fancy,
    r"^ {0,3}(`{3,}(?=[^`\n]*(?:\n|$))|~{3,})([^\n]*)(?:\n|$)(?:|([\s\S]*?)(?:\n|$))(?: {0,3}\1[~`]* *(?=\n|$)|$)"
);
rule!(HR, plain, format!("^{HR_BODY}"));
rule!(HEADING, fancy, r"^ {0,3}(#{1,6})(?=\s|$)(.*)(?:\n+|$)");
rule!(
    BLOCKQUOTE,
    fancy,
    format!(
        r"^( {{0,3}}> ?(([^\n]+(?:\n(?!{HR_BODY}|{HEADING_START}| {{0,3}}>|{FENCE_START}| {{0,3}}(?:[*+-]|[0-9]{{1,9}}[.)])(?:[ \t]|\n|$)|{html}|[ \t]+\n)[^\n]+)*)|[^\n]*)(?:\n|$))+",
        html = html_start()
    )
);
rule!(LIST, plain, r"^( {0,3}(?:[*+-]|[0-9]{1,9}[.)]))([ \t][^\n]*?)?(?:\n|$)");
rule!(
    HTML,
    fancy,
    format!(
        r#"(?i)^ {{0,3}}(?:<(script|pre|style|textarea)[\s>][\s\S]*?(?:<\/\1>[^\n]*\n*|$)|<!--(?:-?>|[\s\S]*?(?:-->|$))[^\n]*(\n+|$)|<\?[\s\S]*?(?:\?>[^\n]*\n*|$)|<![A-Z][\s\S]*?(?:>[^\n]*\n*|$)|<!\[CDATA\[[\s\S]*?(?:\]\]>[^\n]*\n*|$)|<\/?({TAG_NAMES})(?: +|\n|\/?>)[\s\S]*?(?:(?:\n[ \t]*)+\n|$)|<(?!script|pre|style|textarea)([a-z][A-Za-z0-9_-]*)(?: +[a-zA-Z:_][A-Za-z0-9_.:-]*(?: *= *"[^"\n]*"| *= *'[^'\n]*'| *= *[^\s"'=<>`]+)?)*? *\/?>(?=[ \t]*(?:\n|$))[\s\S]*?(?:(?:\n[ \t]*)+\n|$)|<\/(?!script|pre|style|textarea)[a-z][A-Za-z0-9_-]*\s*>(?=[ \t]*(?:\n|$))[\s\S]*?(?:(?:\n[ \t]*)+\n|$))"#
    )
);
rule!(
    DEF,
    fancy,
    r#"^ {0,3}\[((?!\s*\])(?:\\[\s\S]|[^\[\]\\])+)\]: *(?:\n[ \t]*)?([^<\s][^\s]*|<.*?>)(?:(?: +(?:\n[ \t]*)?| *\n[ \t]*)((?:"(?:\\"?|[^"\\])*"|'[^'\n]*(?:\n[^'\n]+)*\n?'|\([^()]*\))))? *(?:\n+|$)"#
);
rule!(TABLE, fancy, format!("^{}", table_body()));
rule!(LHEADING, fancy, {
    let interrupt = r" {0,3}(?:[*+-]|[0-9]{1,9}[.)]) |(?: {4}| {0,3}\t)| {0,3}(?:`{3,}|~{3,})| {0,3}>| {0,3}#{1,6}(?:\s|$)| {0,3}<[^\n>]+>\n| {0,3}\|?(?:[:\- ]*\|)+[\:\- ]*\n";
    format!(r"^(?!{interrupt})((?:.|\n(?!\s*?\n|{interrupt}))+?)\n {{0,3}}(=+|-+) *(?:\n+|$)")
});
rule!(
    PARAGRAPH,
    fancy,
    format!(
        r"^([^\n]+(?:\n(?!{HR_BODY}|{HEADING_START}| {{0,3}}>|{FENCE_START}| {{0,3}}(?:[*+-]|1[.)])[ \t]+[^ \t\n]|{html}|{table}|[ \t]+\n)[^\n]+)*)",
        html = html_start(),
        table = table_body()
    )
);
rule!(TEXT, plain, r"^[^\n]+");

// Helpers of the block tokenizers.
rule!(BLOCKQUOTE_START, plain, r"^ {0,3}>");
rule!(BLOCKQUOTE_SETEXT, fancy, r"\n {0,3}((?:=+|-+) *)(?=\n|$)");
rule!(BLOCKQUOTE_PREFIX, plain, r"(?m)^ {0,3}>[ \t]?");
rule!(CODE_REMOVE_INDENT, plain, r"(?m)^(?: {1,4}| {0,3}\t)");
rule!(INDENT_CODE_COMPENSATION, plain, r"^(\s+)(?:```)");
rule!(BLANK_LINE, plain, r"^[ \t]*$");
rule!(DOUBLE_BLANK_LINE, plain, r"\n[ \t]*\n[ \t]*$");
rule!(LIST_IS_TASK, plain, r"^\[[ xX]\] +\S");
rule!(LIST_REPLACE_TASK, plain, r"^\[[ xX]\] +");
rule!(LIST_TASK_CHECKBOX, plain, r"\[[ xX]\]");
rule!(ANY_LINE, plain, r"\n.*\n");
rule!(TABLE_ALIGN_CHARS, plain, r"^\||\| *$");
rule!(TABLE_ROW_BLANK_LINE, plain, r"\n[ \t]*$");
rule!(HREF_BRACKETS, plain, r"^<(.*)>$");
rule!(MULTIPLE_SPACE, plain, r"\s+");

/// marked's per-indent list regexes (`nextBulletRegex` and friends), for indents 0 to 3.
pub(crate) struct IndentRules {
    pub next_bullet: Regex,
    pub hr: Regex,
    pub fences_begin: Regex,
    pub heading_begin: Regex,
    pub html_begin: Regex,
    pub blockquote_begin: Regex,
}

static INDENT_RULES: LazyLock<Vec<IndentRules>> = LazyLock::new(|| {
    (0..4)
        .map(|n| IndentRules {
            next_bullet: plain(&format!(r"^ {{0,{n}}}(?:[*+-]|[0-9]{{1,9}}[.)])((?:[ \t][^\n]*)?(?:\n|$))")),
            hr: plain(&format!(r"^ {{0,{n}}}((?:- *){{3,}}|(?:_ *){{3,}}|(?:\* *){{3,}})(?:\n+|$)")),
            fences_begin: plain(&format!(r"^ {{0,{n}}}(?:```|~~~)")),
            heading_begin: plain(&format!(r"^ {{0,{n}}}#")),
            html_begin: plain(&format!(r"(?i)^ {{0,{n}}}<(?:[a-z].*>|!--)")),
            blockquote_begin: plain(&format!(r"^ {{0,{n}}}>")),
        })
        .collect()
});

/// The rules for an item content indent (marked caches `min(3, indent - 1)`).
pub(crate) fn indent_rules(indent: usize) -> &'static IndentRules {
    &INDENT_RULES[indent.saturating_sub(1).min(3)]
}

// Inline rules.
rule!(ESCAPE, plain, r##"^\\([!"#$%&'()*+,\-./:;<=>?@\[\]\\^_`{|}~])"##);
rule!(
    TAG,
    plain,
    r#"^<!--(?:-?>|[\s\S]*?-->)|^<\/[a-zA-Z][A-Za-z0-9_:-]*\s*>|^<[a-zA-Z][A-Za-z0-9_-]*(?:\s+[a-zA-Z:_][A-Za-z0-9_.:-]*(?:\s*=\s*"[^"]*"|\s*=\s*'[^']*'|\s*=\s*[^\s"'=<>`]+)?)*?\s*\/?>|^<\?[\s\S]*?\?>|^<![a-zA-Z]+\s[\s\S]*?>|^<!\[CDATA\[[\s\S]*?\]\]>"#
);
const LINK_LABEL: &str = r"(?:\[(?:\\[\s\S]|[^\[\]\\])*\]|\\[\s\S]|`+(?!`)[^`]*?`+(?!`)|``+(?=\])|[^\[\]\\`])*?";
const REF_LABEL: &str = r"(?!\s*\])(?:\\[\s\S]|[^\[\]\\])+";
rule!(
    LINK,
    fancy,
    format!(
        r#"^!?\[({LINK_LABEL})\]\(\s*(<(?:\\.|[^\n<>\\])+>|[^ \t\n\x00-\x1f]+|(?=\)))(?:(?:[ \t]+(?:\n[ \t]*)?|\n[ \t]*)("(?:\\"?|[^"\\])*"|'(?:\\'?|[^'\\])*'|\((?:\\\)?|[^)\\])*\)))?\s*\)"#
    )
);
rule!(REFLINK, fancy, format!(r"^!?\[({LINK_LABEL})\]\[({REF_LABEL})\]"));
rule!(NOLINK, fancy, format!(r"^!?\[({REF_LABEL})\](?:\[\])?"));
rule!(REFLINK_SEARCH, fancy, format!(r"!?\[({LINK_LABEL})\]\[({REF_LABEL})\]|!?\[({REF_LABEL})\](?:\[\])?(?!\()"));
rule!(CODESPAN, fancy, r"^(`+)([^`]|[^`][\s\S]*?[^`])\1(?!`)");
rule!(BR, fancy, r"^( {2,}|\\)\n(?!\s*$)");
rule!(STRICT_DEL, fancy, r"^(~~)(?=[^\s~])((?:\\.|[^\\])*?(?:\\.|[^\s~\\]))\1(?=[^~]|$)");
rule!(
    AUTOLINK,
    fancy,
    r"^<([a-zA-Z][a-zA-Z0-9+.-]{1,31}:[^\s\x00-\x1f<>]*|[a-zA-Z0-9.!#$%&'*+/=?_`{|}~-]+(@)[a-zA-Z0-9](?:[a-zA-Z0-9-]{0,61}[a-zA-Z0-9])?(?:\.[a-zA-Z0-9](?:[a-zA-Z0-9-]{0,61}[a-zA-Z0-9])?)+(?![-_]))>"
);
rule!(
    URL,
    fancy,
    r"^((?:[hH][tT][tT][pP][sS]?|[fF][tT][pP]):\/\/|www\.)(?:[a-zA-Z0-9\-]+\.?)+[^\s<]*|^[A-Za-z0-9._+-]+(@)[a-zA-Z0-9_-]+(?:\.[a-zA-Z0-9_-]*[a-zA-Z0-9])+(?![-_])"
);
rule!(BACKPEDAL, fancy, r#"(?:[^?!.,:;*_'"~()&]+|\([^)]*\)|&(?![a-zA-Z0-9]+;$)|[?!.,:;*_'"~)]+(?!$))+"#);
const EMAIL_CHARS: &str = r"[a-zA-Z0-9.!#$%&'*+\/=?_`{\|}~-]";
rule!(
    INLINE_TEXT,
    fancy,
    format!(
        r"^(`+|~+|[^`~])(?:(?=[`~])|(?= {{2,}}\n)|(?={EMAIL_CHARS}+@)|[\s\S]*?(?:(?=[\\<!\[`*~_]|[hH][tT][tT][pP][sS]?|[fF][tT][pP]:\/\/|www\.|$)|[^ ](?= {{2,}}\n)|[^a-zA-Z0-9.!#$%&'*+\/=?_`{{\|}}~-](?={EMAIL_CHARS}+@)))"
    )
);
rule!(
    EM_STRONG_LDELIM,
    fancy,
    format!(r"^(?:\*+(?:((?!\*)(?!~){PUNCT})|([^\s*]))?)|^_+(?:((?!_)(?!~){PUNCT})|([^\s_]))?")
);

/// East Asian ranges that `marked-cjk-friendly` treats like punctuation when it decides which
/// delimiter runs can open or close emphasis.
const CJK: &str = concat!(
    r"\x{1100}-\x{11ff}\x{20a9}\x{2329}-\x{232a}\x{2630}-\x{2637}\x{268a}-\x{268f}",
    r"\x{2e80}-\x{2e99}\x{2e9b}-\x{2ef3}\x{2f00}-\x{2fd5}\x{2ff0}-\x{303e}\x{3041}-\x{3096}",
    r"\x{3099}-\x{30ff}\x{3105}-\x{312f}\x{3131}-\x{318e}\x{3190}-\x{31e5}\x{31ef}-\x{321e}",
    r"\x{3220}-\x{3247}\x{3250}-\x{a48c}\x{a490}-\x{a4c6}\x{a960}-\x{a97c}\x{ac00}-\x{d7a3}",
    r"\x{d7b0}-\x{d7c6}\x{d7cb}-\x{d7fb}\x{f900}-\x{faff}\x{fe10}-\x{fe19}\x{fe30}-\x{fe52}",
    r"\x{fe54}-\x{fe66}\x{fe68}-\x{fe6b}\x{ff01}-\x{ffbe}\x{ffc2}-\x{ffc7}\x{ffca}-\x{ffcf}",
    r"\x{ffd2}-\x{ffd7}\x{ffda}-\x{ffdc}\x{ffe0}-\x{ffe6}\x{ffe8}-\x{ffee}\x{16fe0}-\x{16fe4}",
    r"\x{16ff0}-\x{16ff6}\x{17000}-\x{18cda}\x{18cff}-\x{18d20}\x{18d80}-\x{18df2}",
    r"\x{18e00}-\x{19191}\x{191a0}-\x{191d2}\x{1aff0}-\x{1aff3}\x{1aff5}-\x{1affb}",
    r"\x{1affd}-\x{1affe}\x{1b000}-\x{1b128}\x{1b132}\x{1b150}-\x{1b152}\x{1b155}",
    r"\x{1b164}-\x{1b168}\x{1b170}-\x{1b2fb}\x{1d300}-\x{1d356}\x{1d360}-\x{1d376}\x{1f1ae}",
    r"\x{1f200}\x{1f202}\x{1f210}-\x{1f219}\x{1f21b}-\x{1f22e}\x{1f230}-\x{1f231}\x{1f237}",
    r"\x{1f23b}\x{1f240}-\x{1f248}\x{1f260}-\x{1f265}\x{1f7da}\x{20000}-\x{3fffd}",
);

/// The `marked-cjk-friendly` right-delimiter rule for `*`, built like upstream from the
/// punctuation, punctuation-or-space, and not-punctuation-or-space classes.
fn cjk_rdelim_ast(punct: &str, punct_space: &str, not_punct_space: &str) -> String {
    format!(
        r"^[^_*]*?__[^_*]*?\*[^_*]*?(?=__)|[^*]+(?=[^*])|(?!\*){punct}(\*+)(?=[\s]|$)|{not_punct_space}(\*+)(?!\*)(?={punct_space}|$)|(?!\*){punct_space}(\*+)(?={not_punct_space})|[\s](\*+)(?!\*)(?={punct})|(?!\*){punct}(\*+)(?!\*)(?={punct})|{not_punct_space}(\*+)(?={not_punct_space})"
    )
}

rule!(CJK_CHAR, plain, format!("[{CJK}]"));
rule!(PUNCTUATION_CJK, fancy, format!(r"^((?![*_])[\s\p{{P}}\p{{S}}{CJK}])"));
rule!(
    EM_STRONG_RDELIM_AST_CJK,
    fancy,
    cjk_rdelim_ast(
        &format!(r"(?!~)[\p{{P}}\p{{S}}{CJK}]"),
        &format!(r"(?!~)[\s\p{{P}}\p{{S}}{CJK}]"),
        &format!(r"(?:[^\s\p{{P}}\p{{S}}{CJK}]|~)")
    )
);
rule!(EM_STRONG_RDELIM_UND_CJK, fancy, {
    let (punct, punct_space, not_punct_space) =
        (format!(r"[\p{{P}}\p{{S}}{CJK}]"), format!(r"[\s\p{{P}}\p{{S}}{CJK}]"), format!(r"[^\s\p{{P}}\p{{S}}{CJK}]"));
    format!(
        r"^[^_*]*?\*\*[^_*]*?_[^_*]*?(?=\*\*)|[^_]+(?=[^_])|(?!_){punct}(_+)(?=[\s]|$)|{not_punct_space}(_+)(?!_)(?={punct_space}|$)|(?!_){punct_space}(_+)(?={not_punct_space})|[\s](_+)(?!_)(?={punct})|(?!_){punct}(_+)(?!_)(?={punct})"
    )
});
rule!(PUNCTUATION, fancy, r"^((?![*_])[\s\p{P}\p{S}])");
rule!(ANY_PUNCTUATION, plain, r"\\([\p{P}\p{S}])");
rule!(
    BLOCK_SKIP,
    fancy,
    r"\[(?:[^\[\]`]|(?<a>`+)[^`]+\k<a>(?!`))*?\]\((?:\\[\s\S]|[^\\\(\)]|\((?:\\[\s\S]|[^\\\(\)])*\))*\)|(?<!`)()(?<b>`+)[^`]+\k<b>(?!`)|<(?! )[^<>]*?>"
);
rule!(UNICODE_ALPHANUMERIC, plain, r"[\p{L}\p{N}]");
rule!(OUTPUT_LINK_REPLACE, plain, r"\\([\[\]])");
rule!(START_A_TAG, plain, r"(?i)^<a ");
rule!(END_A_TAG, plain, r"(?i)^</a>");
rule!(START_PRE_SCRIPT_TAG, plain, r"(?i)^<(pre|code|kbd|script)(\s|>)");
rule!(END_PRE_SCRIPT_TAG, plain, r"(?i)^</(pre|code|kbd|script)(\s|>)");

/// Matches a `^`-anchored rule at the start of `text` only, without scanning the rest.
/// Engine errors (backtrack limits) count as no match.
pub(crate) fn captures<'t>(regex: &FancyRegex, text: &'t str) -> Option<fancy_regex::Captures<'t, str>> {
    regex.captures_input(RegexInput::new(text).anchored(true)).ok().flatten()
}

/// Anchored like [`captures`].
pub(crate) fn is_match(regex: &FancyRegex, text: &str) -> bool {
    regex.is_match_input(RegexInput::new(text).anchored(true)).unwrap_or(false)
}

/// The first match anywhere in `text`.
pub(crate) fn captures_anywhere<'t>(regex: &FancyRegex, text: &'t str) -> Option<fancy_regex::Captures<'t, str>> {
    regex.captures(text).ok().flatten()
}
