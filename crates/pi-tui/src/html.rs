//! HTML entity decoding, for converting highlighter HTML output to terminal text (upstream
//! coding-agent `utils/html.ts`).

/// A decoded entity and the byte length of its source (`&` through `;`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedHtmlEntity {
    pub text: String,
    pub length: usize,
}

fn decode_code_point(code_point: Option<u32>) -> Option<String> {
    code_point.filter(|cp| *cp <= 0x10ffff).map(|cp| {
        // JavaScript's String.fromCodePoint keeps lone surrogates; Rust strings cannot hold them.
        char::from_u32(cp).unwrap_or(char::REPLACEMENT_CHARACTER).to_string()
    })
}

/// JavaScript `Number.parseInt(text, radix)`: leading digits only, `None` when there are none.
fn parse_int_prefix(text: &str, radix: u32) -> Option<u32> {
    let text = text.trim_start();
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let digits = if radix == 16 {
        digits.strip_prefix("0x").or_else(|| digits.strip_prefix("0X")).unwrap_or(digits)
    } else {
        digits
    };
    let end = digits.find(|c: char| !c.is_digit(radix)).unwrap_or(digits.len());
    if end == 0 {
        return None;
    }
    let value = u64::from_str_radix(&digits[..end], radix).unwrap_or(u64::MAX);
    // Negative values and values past the Unicode range are rejected by the caller.
    if negative && value > 0 {
        return Some(u32::MAX);
    }
    Some(u32::try_from(value).unwrap_or(u32::MAX))
}

/// Decodes the entity name between `&` and `;`: the five XML entities and numeric references.
pub fn decode_html_entity(entity: &str) -> Option<String> {
    match entity {
        "amp" => return Some("&".into()),
        "lt" => return Some("<".into()),
        "gt" => return Some(">".into()),
        "quot" => return Some("\"".into()),
        "apos" => return Some("'".into()),
        _ => {}
    }
    if let Some(hex) = entity.strip_prefix("#x").or_else(|| entity.strip_prefix("#X")) {
        return decode_code_point(parse_int_prefix(hex, 16));
    }
    if let Some(decimal) = entity.strip_prefix('#') {
        return decode_code_point(parse_int_prefix(decimal, 10));
    }
    None
}

/// Decodes the entity starting with the `&` at byte `index` of `html`, if it is a known one
/// that ends with `;` within 16 bytes.
pub fn decode_html_entity_at(html: &str, index: usize) -> Option<DecodedHtmlEntity> {
    let rest = html.get(index + 1..)?;
    let semicolon = rest.find(';')?;
    if semicolon + 1 > 16 {
        return None;
    }
    let text = decode_html_entity(&rest[..semicolon])?;
    Some(DecodedHtmlEntity { text, length: semicolon + 2 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_named_and_numeric_entities() {
        assert_eq!(decode_html_entity("amp").as_deref(), Some("&"));
        assert_eq!(decode_html_entity("#x41").as_deref(), Some("A"));
        assert_eq!(decode_html_entity("#65").as_deref(), Some("A"));
        assert_eq!(decode_html_entity("#x110000"), None);
        assert_eq!(decode_html_entity("nbsp"), None);
        let decoded = decode_html_entity_at("a &lt;b", 2).expect("entity");
        assert_eq!(decoded, DecodedHtmlEntity { text: "<".into(), length: 4 });
        assert_eq!(decode_html_entity_at("&ampersand", 0), None);
    }
}
