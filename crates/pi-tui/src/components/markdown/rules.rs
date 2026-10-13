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
rule!(
    EM_STRONG_RDELIM_AST,
    fancy,
    r"^[^_*]*?__[^_*]*?\*[^_*]*?(?=__)|[^*]+(?=[^*])|(?!\*)(?!~)[\p{P}\p{S}](\*+)(?=[\s]|$)|(?:[^\s\p{P}\p{S}]|~)(\*+)(?!\*)(?=(?!~)[\s\p{P}\p{S}]|$)|(?!\*)(?!~)[\s\p{P}\p{S}](\*+)(?=(?:[^\s\p{P}\p{S}]|~))|[\s](\*+)(?!\*)(?=(?!~)[\p{P}\p{S}])|(?!\*)(?!~)[\p{P}\p{S}](\*+)(?!\*)(?=(?!~)[\p{P}\p{S}])|(?:[^\s\p{P}\p{S}]|~)(\*+)(?=(?:[^\s\p{P}\p{S}]|~))"
);
rule!(
    EM_STRONG_RDELIM_UND,
    fancy,
    r"^[^_*]*?\*\*[^_*]*?_[^_*]*?(?=\*\*)|[^_]+(?=[^_])|(?!_)[\p{P}\p{S}](_+)(?=[\s]|$)|[^\s\p{P}\p{S}](_+)(?!_)(?=[\s\p{P}\p{S}]|$)|(?!_)[\s\p{P}\p{S}](_+)(?=[^\s\p{P}\p{S}])|[\s](_+)(?!_)(?=[\p{P}\p{S}])|(?!_)[\p{P}\p{S}](_+)(?!_)(?=[\p{P}\p{S}])"
);
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
