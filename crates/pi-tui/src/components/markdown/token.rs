//! Markdown tokens, mirroring the shapes the upstream renderer reads from `marked`.

use std::cell::RefCell;
use std::rc::Rc;

/// Inline tokens of a block token. They are filled after block lexing (marked's inline queue),
/// so the block token and the queue share the list.
pub(crate) type Children = Rc<RefCell<Vec<Token>>>;

pub(crate) fn children(tokens: Vec<Token>) -> Children {
    Rc::new(RefCell::new(tokens))
}

#[derive(Debug, Clone)]
pub(crate) struct ListItem {
    pub raw: String,
    pub task: bool,
    pub checked: Option<bool>,
    pub text: String,
    pub tokens: Vec<Token>,
}

#[derive(Debug, Clone)]
pub(crate) struct ListToken {
    pub raw: String,
    pub ordered: bool,
    /// Start number of an ordered list (`None` for bullets).
    pub start: Option<u64>,
    pub loose: bool,
    pub items: Vec<ListItem>,
}

#[derive(Debug, Clone)]
pub(crate) struct TableCell {
    pub tokens: Children,
}

#[derive(Debug, Clone)]
pub(crate) struct TableToken {
    pub raw: String,
    pub header: Vec<TableCell>,
    pub rows: Vec<Vec<TableCell>>,
}

#[derive(Debug, Clone)]
pub(crate) enum Token {
    Space {
        raw: String,
    },
    Code {
        raw: String,
        lang: String,
        text: String,
    },
    Heading {
        raw: String,
        depth: usize,
        tokens: Children,
    },
    Hr {
        raw: String,
    },
    Blockquote {
        raw: String,
        text: String,
        tokens: Vec<Token>,
    },
    List(ListToken),
    Html {
        raw: String,
    },
    Def {
        raw: String,
    },
    Table(TableToken),
    Paragraph {
        raw: String,
        text: String,
        tokens: Children,
    },
    /// Block text (with inline children) or inline text (without).
    Text {
        raw: String,
        text: String,
        tokens: Option<Children>,
    },
    Escape {
        raw: String,
        text: String,
    },
    Link {
        raw: String,
        href: String,
        text: String,
        tokens: Vec<Token>,
        image: bool,
    },
    Strong {
        raw: String,
        tokens: Vec<Token>,
    },
    Em {
        raw: String,
        tokens: Vec<Token>,
    },
    Codespan {
        raw: String,
        text: String,
    },
    Br {
        raw: String,
    },
    Del {
        raw: String,
        tokens: Vec<Token>,
    },
    Checkbox {
        raw: String,
    },
    Latex {
        raw: String,
        text: String,
        pending: bool,
    },
    LatexBlock {
        raw: String,
        text: String,
        pending: bool,
    },
}

impl Token {
    /// The marked token type name.
    pub fn kind(&self) -> &'static str {
        match self {
            Token::Space { .. } => "space",
            Token::Code { .. } => "code",
            Token::Heading { .. } => "heading",
            Token::Hr { .. } => "hr",
            Token::Blockquote { .. } => "blockquote",
            Token::List(_) => "list",
            Token::Html { .. } => "html",
            Token::Def { .. } => "def",
            Token::Table(_) => "table",
            Token::Paragraph { .. } => "paragraph",
            Token::Text { .. } => "text",
            Token::Escape { .. } => "escape",
            Token::Link { image: false, .. } => "link",
            Token::Link { image: true, .. } => "image",
            Token::Strong { .. } => "strong",
            Token::Em { .. } => "em",
            Token::Codespan { .. } => "codespan",
            Token::Br { .. } => "br",
            Token::Del { .. } => "del",
            Token::Checkbox { .. } => "checkbox",
            Token::Latex { .. } => "latex",
            Token::LatexBlock { .. } => "latexBlock",
        }
    }

    pub fn raw(&self) -> &str {
        match self {
            Token::List(list) => &list.raw,
            Token::Table(table) => &table.raw,
            Token::Space { raw }
            | Token::Code { raw, .. }
            | Token::Heading { raw, .. }
            | Token::Hr { raw }
            | Token::Blockquote { raw, .. }
            | Token::Html { raw }
            | Token::Def { raw }
            | Token::Paragraph { raw, .. }
            | Token::Text { raw, .. }
            | Token::Escape { raw, .. }
            | Token::Link { raw, .. }
            | Token::Strong { raw, .. }
            | Token::Em { raw, .. }
            | Token::Codespan { raw, .. }
            | Token::Br { raw }
            | Token::Del { raw, .. }
            | Token::Checkbox { raw }
            | Token::Latex { raw, .. }
            | Token::LatexBlock { raw, .. } => raw,
        }
    }

    pub fn raw_mut(&mut self) -> &mut String {
        match self {
            Token::List(list) => &mut list.raw,
            Token::Table(table) => &mut table.raw,
            Token::Space { raw }
            | Token::Code { raw, .. }
            | Token::Heading { raw, .. }
            | Token::Hr { raw }
            | Token::Blockquote { raw, .. }
            | Token::Html { raw }
            | Token::Def { raw }
            | Token::Paragraph { raw, .. }
            | Token::Text { raw, .. }
            | Token::Escape { raw, .. }
            | Token::Link { raw, .. }
            | Token::Strong { raw, .. }
            | Token::Em { raw, .. }
            | Token::Codespan { raw, .. }
            | Token::Br { raw }
            | Token::Del { raw, .. }
            | Token::Checkbox { raw }
            | Token::Latex { raw, .. }
            | Token::LatexBlock { raw, .. } => raw,
        }
    }

    /// The `text` field of paragraph and text tokens, which marked extends when merging blocks.
    pub fn text_mut(&mut self) -> Option<&mut String> {
        match self {
            Token::Paragraph { text, .. } | Token::Text { text, .. } => Some(text),
            _ => None,
        }
    }
}
