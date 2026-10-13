//! ANSI SGR text to HTML with inline styles (upstream `ansi-to-html.ts`): 16 colors,
//! the 256-color palette, true color, bold, dim, italic, underline, and resets.

const ANSI_COLORS: [&str; 16] = [
    "#000000", "#800000", "#008000", "#808000", "#000080", "#800080", "#008080", "#c0c0c0", "#808080", "#ff0000",
    "#00ff00", "#ffff00", "#0000ff", "#ff00ff", "#00ffff", "#ffffff",
];

fn color_256_to_hex(index: i64) -> String {
    if index < 16 {
        // JavaScript indexes past the table (negative values) as `undefined`.
        return usize::try_from(index).ok().and_then(|i| ANSI_COLORS.get(i)).map_or("undefined", |c| c).to_string();
    }
    if index < 232 {
        let cube = index - 16;
        let component = |n: i64| if n == 0 { 0 } else { 55 + n * 40 };
        return format!("#{:02x}{:02x}{:02x}", component(cube / 36), component(cube % 36 / 6), component(cube % 6));
    }
    let gray = 8 + (index - 232) * 10;
    format!("#{gray:02x}{gray:02x}{gray:02x}")
}

/// Escape `& < > " '` for HTML text and attributes.
pub fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#039;"),
            _ => out.push(c),
        }
    }
    out
}

#[derive(Default)]
struct TextStyle {
    fg: Option<String>,
    bg: Option<String>,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
}

impl TextStyle {
    fn css(&self) -> String {
        let mut parts = Vec::new();
        if let Some(fg) = &self.fg {
            parts.push(format!("color:{fg}"));
        }
        if let Some(bg) = &self.bg {
            parts.push(format!("background-color:{bg}"));
        }
        for (on, css) in [
            (self.bold, "font-weight:bold"),
            (self.dim, "opacity:0.6"),
            (self.italic, "font-style:italic"),
            (self.underline, "text-decoration:underline"),
        ] {
            if on {
                parts.push(css.to_string());
            }
        }
        parts.join(";")
    }

    fn is_styled(&self) -> bool {
        self.fg.is_some() || self.bg.is_some() || self.bold || self.dim || self.italic || self.underline
    }

    /// Extended color after `38`/`48`: returns the color and how many params it used.
    fn extended(params: &[i64], i: usize) -> Option<(String, usize)> {
        match params.get(i + 1) {
            Some(5) if params.len() > i + 2 => Some((color_256_to_hex(params[i + 2]), 2)),
            Some(2) if params.len() > i + 4 => {
                Some((format!("rgb({},{},{})", params[i + 2], params[i + 3], params[i + 4]), 4))
            }
            _ => None,
        }
    }

    fn apply(&mut self, params: &[i64]) {
        let mut i = 0;
        while i < params.len() {
            match params[i] {
                0 => *self = Self::default(),
                1 => self.bold = true,
                2 => self.dim = true,
                3 => self.italic = true,
                4 => self.underline = true,
                22 => (self.bold, self.dim) = (false, false),
                23 => self.italic = false,
                24 => self.underline = false,
                code @ 30..=37 => self.fg = Some(ANSI_COLORS[(code - 30) as usize].into()),
                38 => {
                    if let Some((color, used)) = Self::extended(params, i) {
                        self.fg = Some(color);
                        i += used;
                    }
                }
                39 => self.fg = None,
                code @ 40..=47 => self.bg = Some(ANSI_COLORS[(code - 40) as usize].into()),
                48 => {
                    if let Some((color, used)) = Self::extended(params, i) {
                        self.bg = Some(color);
                        i += used;
                    }
                }
                49 => self.bg = None,
                code @ 90..=97 => self.fg = Some(ANSI_COLORS[(code - 90 + 8) as usize].into()),
                code @ 100..=107 => self.bg = Some(ANSI_COLORS[(code - 100 + 8) as usize].into()),
                _ => {}
            }
            i += 1;
        }
    }
}

/// JavaScript `parseInt(p, 10) || 0` for a run of ASCII digits.
fn parse_param(param: &str) -> i64 {
    if param.is_empty() {
        return 0;
    }
    param.parse::<f64>().map_or(0, |v| if v > i64::MAX as f64 { i64::MAX } else { v as i64 })
}

/// Find the next `ESC [ digits-and-semicolons m` sequence: (start, end, params).
fn next_sgr(text: &str, from: usize) -> Option<(usize, usize, &str)> {
    let bytes = text.as_bytes();
    let mut start = from;
    while let Some(offset) = text[start..].find('\x1b') {
        let esc = start + offset;
        if bytes.get(esc + 1) == Some(&b'[') {
            let params_start = esc + 2;
            let params_len = bytes[params_start..].iter().take_while(|b| b.is_ascii_digit() || **b == b';').count();
            let end = params_start + params_len;
            if bytes.get(end) == Some(&b'm') {
                return Some((esc, end + 1, &text[params_start..end]));
            }
        }
        start = esc + 1;
    }
    None
}

/// Convert ANSI-styled text to HTML with inline `<span style>` runs.
pub fn ansi_to_html(text: &str) -> String {
    let mut style = TextStyle::default();
    let mut result = String::new();
    let mut last = 0;
    let mut in_span = false;
    while let Some((start, end, params)) = next_sgr(text, last) {
        result.push_str(&escape_html(&text[last..start]));
        let params: Vec<i64> = if params.is_empty() { vec![0] } else { params.split(';').map(parse_param).collect() };
        if in_span {
            result.push_str("</span>");
            in_span = false;
        }
        style.apply(&params);
        if style.is_styled() {
            result.push_str(&format!("<span style=\"{}\">", style.css()));
            in_span = true;
        }
        last = end;
    }
    result.push_str(&escape_html(&text[last..]));
    if in_span {
        result.push_str("</span>");
    }
    result
}

/// Each line becomes `<div class="ansi-line">`, with `&nbsp;` for empty lines.
pub fn ansi_lines_to_html<S: AsRef<str>>(lines: &[S]) -> String {
    lines
        .iter()
        .map(|line| {
            let html = ansi_to_html(line.as_ref());
            format!("<div class=\"ansi-line\">{}</div>", if html.is_empty() { "&nbsp;" } else { &html })
        })
        .collect()
}
