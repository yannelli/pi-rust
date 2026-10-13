//! Maps TextMate/Sublime scopes from syntect onto the highlight.js classes upstream themes use.
//! Upstream colors code through highlight.js; these rules reproduce its classes for the same tokens.

use syntect::parsing::Scope;

/// Which language family a syntax belongs to, for the places where highlight.js grammars differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Family {
    #[default]
    Other,
    /// Primitive types are keywords; class references are plain text.
    Java,
    /// Primitive types such as `int` are keywords (C, C++, Go).
    CLike,
    /// Variables are variables; interpolation is a variable; commands like `local` are built-ins.
    Shell,
    /// Variables with sigils are variables (PHP).
    Sigils,
    /// Variables are plain text and `use` lines are statements.
    Perl,
    /// Only instance, class, and global variables are variables.
    Ruby,
    /// Docstrings are strings.
    Python,
    /// Identifiers and inherited attributes are plain text.
    Nix,
}

impl Family {
    pub(crate) fn for_syntax(name: &str) -> Family {
        match name.to_ascii_lowercase().as_str() {
            "java" => Family::Java,
            "c" | "c++" | "go" | "objective-c" | "objective-c++" => Family::CLike,
            "php" => Family::Sigils,
            "perl" => Family::Perl,
            "ruby" => Family::Ruby,
            "python" => Family::Python,
            "nix" => Family::Nix,
            name if name.contains("bash") || name.contains("shell") || name == "fish" || name == "makefile" => {
                Family::Shell
            }
            _ => Family::Other,
        }
    }

    fn primitives_are_keywords(self) -> bool {
        matches!(self, Family::Java | Family::CLike | Family::Sigils)
    }

    fn has_preprocessor(self) -> bool {
        self != Family::Perl
    }
}

const PRIMITIVE_TYPES: &[&str] = &[
    "bool", "boolean", "byte", "char", "decimal", "double", "f32", "f64", "float", "i128", "i16", "i32", "i64", "i8",
    "int", "isize", "long", "object", "sbyte", "short", "signed", "str", "string", "u128", "u16", "u32", "u64", "u8",
    "uint", "ulong", "unsigned", "ushort", "usize", "void",
];

const TYPE_NAMES: &[&str] = &[
    "entity.name.class",
    "entity.name.struct",
    "entity.name.enum",
    "entity.name.trait",
    "entity.name.interface",
    "entity.name.union",
    "entity.name.type",
];

/// `Option` and `Result` variants, which highlight.js lists as literals.
const VARIANT_LITERALS: &[&str] = &["Some", "None", "Ok", "Err"];

const PARAMETER_LISTS: &[&str] = &["meta.function.parameters", "meta.parameters", "meta.parameter-clause"];

const SELF_WORDS: &[&str] = &["this", "super", "self", "$this", "Self"];

/// Shell commands highlight.js lists as built-ins although grammars mark them as keywords.
const SHELL_BUILTINS: &[&str] =
    &["local", "export", "declare", "readonly", "typeset", "unset", "alias", "source", "set"];

enum Mapping {
    Class(&'static str),
    /// No class of its own: the enclosing scope decides.
    Skip,
    /// Plain text, regardless of enclosing scopes.
    Plain,
}

fn is_word(text: &str) -> bool {
    let text = text.trim();
    !text.is_empty() && text.chars().all(|c| c.is_alphanumeric() || c == '_')
}

fn has(scope: &str, prefix: &str) -> bool {
    scope.strip_prefix(prefix).is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
}

fn map_variable(scope: &str, word: &str, family: Family) -> Option<Mapping> {
    let sigil = word.starts_with(['$', '@', '%']);
    let variable = has(scope, "variable.other") || has(scope, "variable.language") || has(scope, "variable.parameter");
    let found = match family {
        Family::Shell => {
            let readwrite = has(scope, "variable.other") && !scope.contains(".assignment");
            readwrite || has(scope, "meta.group.expansion") || has(scope, "punctuation.definition.variable")
        }
        Family::Sigils => variable || has(scope, "punctuation.definition.variable"),
        Family::Ruby => {
            variable && (sigil || [".instance", ".global", ".class"].iter().any(|kind| scope.contains(kind)))
        }
        _ => false,
    };
    found.then_some(Mapping::Class("variable"))
}

fn map_scope(scope: &str, text: &str, family: Family) -> Mapping {
    use Mapping::*;
    let has = |prefix: &str| has(scope, prefix);
    let word = text.trim();
    if has("string") || has("storage.type.string") {
        return Class("string");
    }
    if has("constant.numeric") {
        return Class("number");
    }
    if has("constant.language") {
        return Class("literal");
    }
    if has("keyword.operator") {
        return if is_word(text) { Class("keyword") } else { Plain };
    }
    if family == Family::Shell && SHELL_BUILTINS.contains(&word) && (has("keyword") || has("storage")) {
        return Class("built_in");
    }
    if has("keyword") || has("storage.modifier") {
        return Class("keyword");
    }
    if has("storage.type.annotation") || (has("punctuation.section.embedded") && word.starts_with("<?")) {
        return Class("meta");
    }
    if has("storage.type") {
        let capitalized = word.starts_with(|c: char| c.is_ascii_uppercase());
        return match (is_word(text), capitalized, family) {
            (false, _, _) => Skip,
            (true, true, Family::Java) => Skip,
            (true, true, _) => Class("type"),
            (true, false, _) => Class("keyword"),
        };
    }
    if has("support") {
        let member = scope.contains(".exception") || scope.contains(".property") || scope.contains(".dom");
        let java_class = family == Family::Java && !PRIMITIVE_TYPES.contains(&word);
        return if member || java_class { Skip } else { Class("built_in") };
    }
    if has("entity.name.function") || has("entity.type.function") {
        return Class("title.function");
    }
    if TYPE_NAMES.iter().any(|prefix| has(prefix)) {
        return Class("title.class");
    }
    if has("entity.other.inherited-class") {
        return Class("title.class.inherited");
    }
    if has("entity.name.tag") {
        return Class("name");
    }
    if has("entity.other.attribute-name") {
        return if family == Family::Nix && has("entity.other.attribute-name.single") { Skip } else { Class("attr") };
    }
    if let Some(mapping) = map_variable(scope, word, family) {
        return mapping;
    }
    if has("variable.parameter") {
        return if matches!(family, Family::Nix | Family::Shell) { Skip } else { Class("params") };
    }
    if has("variable.language") {
        return if SELF_WORDS.contains(&word) { Class("built_in") } else { Skip };
    }
    if has("meta.interpolation") || has("meta.template.expression") {
        return Class(if family == Family::Shell { "variable" } else { "subst" });
    }
    let markup = [
        ("markup.inserted", "addition"),
        ("markup.deleted", "deletion"),
        ("markup.bold", "strong"),
        ("markup.italic", "emphasis"),
        ("markup.underline.link", "link"),
        ("markup.heading", "section"),
        ("markup.quote", "quote"),
        ("markup.raw", "code"),
        ("meta.diff.header", "meta"),
        ("meta.diff.range", "meta"),
    ];
    match markup.iter().find(|(prefix, _)| has(prefix)) {
        Some((_, class)) => Class(class),
        None => Skip,
    }
}

/// Scopes whose whole extent highlight.js marks as `meta` (preprocessor lines, annotations, attributes).
fn is_meta_context(scope: &str, family: Family) -> bool {
    (family.has_preprocessor() && scope.starts_with("meta.preprocessor"))
        || ["meta.annotation", "meta.attribute", "meta.decorator"].iter().any(|prefix| scope.starts_with(prefix))
}

/// Outer scopes that color everything inside them, whatever inner scopes say.
fn dominant(names: &[String], family: Family) -> Option<&'static str> {
    for name in names {
        if family == Family::Python && has(name, "comment.block.documentation") {
            return Some("string");
        }
        if has(name, "comment") {
            return Some("comment");
        }
        if has(name, "string.regexp") {
            return Some("regexp");
        }
    }
    None
}

/// The highlight.js class for a token with the given scope stack (outermost first).
pub(crate) fn classify(stack: &[Scope], text: &str, family: Family) -> Option<&'static str> {
    let names: Vec<String> = stack.iter().map(|scope| scope.build_string()).collect();
    if let Some(class) = dominant(&names, family) {
        return Some(class);
    }
    if names.iter().any(|name| is_meta_context(name, family)) {
        return Some("meta");
    }
    let mut class = None;
    for name in names.iter().rev() {
        match map_scope(name, text, family) {
            Mapping::Class(found) => {
                class = Some(found);
                break;
            }
            Mapping::Plain => break,
            Mapping::Skip => {}
        }
    }
    if names.iter().any(|name| name.starts_with("meta.mapping.key")) && class != Some("subst") {
        return Some("attr");
    }
    let word = text.trim();
    if PRIMITIVE_TYPES.contains(&word) && matches!(class, Some("keyword" | "type" | "built_in")) {
        return Some(if family.primitives_are_keywords() { "keyword" } else { "type" });
    }
    if VARIANT_LITERALS.contains(&word) && matches!(class, Some("type" | "built_in")) {
        return Some("literal");
    }
    // highlight.js colors a whole parameter list, punctuation included.
    if class.is_none()
        && !word.is_empty()
        && names.iter().any(|name| PARAMETER_LISTS.iter().any(|list| has(name, list)))
    {
        return Some("params");
    }
    class
}
