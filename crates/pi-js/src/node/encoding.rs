//! Natives for Buffer encodings, `TextDecoder`, and the WHATWG URL parser.

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use rquickjs::function::Opt;
use rquickjs::{Ctx, Object};
use url::Url;

use super::set_fn;
use crate::error::JsError;
use crate::runtime::convert::{bytes_to_js, latin1_to_bytes, throw};

/// Bytes for `text` in a Node encoding name.
pub(crate) fn encode(text: &str, encoding: &str) -> Result<Vec<u8>, JsError> {
    Ok(match normalize(encoding)? {
        "utf8" => text.as_bytes().to_vec(),
        "latin1" | "ascii" => text.encode_utf16().map(|unit| unit as u8).collect(),
        "utf16le" => text.encode_utf16().flat_map(u16::to_le_bytes).collect(),
        "base64" | "base64url" => decode_base64(text),
        "hex" => decode_hex(text),
        _ => unreachable!("normalize returns known encodings"),
    })
}

/// Text for `bytes` in a Node encoding name. UTF-8 decoding replaces invalid sequences.
pub(crate) fn decode(bytes: &[u8], encoding: &str) -> Result<String, JsError> {
    Ok(match normalize(encoding)? {
        "utf8" => String::from_utf8_lossy(bytes).into_owned(),
        "latin1" => bytes.iter().map(|byte| *byte as char).collect(),
        "ascii" => bytes.iter().map(|byte| (byte & 0x7f) as char).collect(),
        "utf16le" => {
            let units: Vec<u16> = bytes.chunks_exact(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]])).collect();
            String::from_utf16_lossy(&units)
        }
        "base64" => STANDARD.encode(bytes),
        "base64url" => URL_SAFE_NO_PAD.encode(bytes),
        "hex" => bytes.iter().map(|byte| format!("{byte:02x}")).collect(),
        _ => unreachable!("normalize returns known encodings"),
    })
}

fn normalize(encoding: &str) -> Result<&'static str, JsError> {
    Ok(match encoding.to_ascii_lowercase().as_str() {
        "utf8" | "utf-8" | "" => "utf8",
        "latin1" | "binary" => "latin1",
        "ascii" => "ascii",
        "utf16le" | "utf-16le" | "ucs2" | "ucs-2" => "utf16le",
        "base64" => "base64",
        "base64url" => "base64url",
        "hex" => "hex",
        other => {
            return Err(JsError::type_error(format!("Unknown encoding: {other}")).with_code("ERR_UNKNOWN_ENCODING"));
        }
    })
}

/// Node's lenient base64: both alphabets, whitespace and padding ignored, stops at junk.
fn decode_base64(text: &str) -> Vec<u8> {
    let mut bits = 0u32;
    let mut count = 0;
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    for byte in text.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            b' ' | b'\t' | b'\n' | b'\r' => continue,
            _ => continue,
        };
        bits = (bits << 6) | u32::from(value);
        count += 6;
        if count >= 8 {
            count -= 8;
            out.push((bits >> count) as u8);
        }
    }
    out
}

/// Node's hex: pairs until the first invalid one.
fn decode_hex(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        let digits = std::str::from_utf8(pair).ok().and_then(|digits| u8::from_str_radix(digits, 16).ok());
        match digits {
            Some(byte) => out.push(byte),
            None => break,
        }
    }
    out
}

/// `[href, origin, protocol, username, password, host, hostname, port, pathname, search, hash]`.
fn components(url: &Url) -> Vec<String> {
    use url::quirks;
    vec![
        quirks::href(url).to_string(),
        quirks::origin(url),
        quirks::protocol(url).to_string(),
        quirks::username(url).to_string(),
        quirks::password(url).to_string(),
        quirks::host(url).to_string(),
        quirks::hostname(url).to_string(),
        quirks::port(url).to_string(),
        quirks::pathname(url).to_string(),
        quirks::search(url).to_string(),
        quirks::hash(url).to_string(),
    ]
}

fn parse_url(input: &str, base: Option<&str>) -> Option<Url> {
    match base {
        Some(base) => Url::parse(base).ok()?.join(input).ok(),
        None => Url::parse(input).ok(),
    }
}

/// Apply a WHATWG setter; invalid values leave the URL unchanged, as browsers do.
fn set_component(href: &str, component: &str, value: &str) -> Option<Vec<String>> {
    use url::quirks;
    let mut url = Url::parse(href).ok()?;
    let _ = match component {
        "href" => quirks::set_href(&mut url, value).map_err(|_| ()),
        "protocol" => quirks::set_protocol(&mut url, value),
        "username" => quirks::set_username(&mut url, value),
        "password" => quirks::set_password(&mut url, value),
        "host" => quirks::set_host(&mut url, value),
        "hostname" => quirks::set_hostname(&mut url, value),
        "port" => quirks::set_port(&mut url, value),
        "pathname" => {
            quirks::set_pathname(&mut url, value);
            Ok(())
        }
        "search" => {
            quirks::set_search(&mut url, value);
            Ok(())
        }
        "hash" => {
            quirks::set_hash(&mut url, value);
            Ok(())
        }
        _ => return None,
    };
    Some(components(&url))
}

/// `Intl.Segmenter` segments of `text` (UAX #29 graphemes, words, or sentences).
fn segment(text: &str, granularity: &str) -> Vec<String> {
    use unicode_segmentation::UnicodeSegmentation;
    let segments: Vec<&str> = match granularity {
        "word" => text.split_word_bounds().collect(),
        "sentence" => text.split_sentence_bounds().collect(),
        _ => text.graphemes(true).collect(),
    };
    segments.into_iter().map(str::to_string).collect()
}

pub(crate) fn natives<'js>(ctx: &Ctx<'js>) -> rquickjs::Result<Object<'js>> {
    let object = Object::new(ctx.clone())?;
    set_fn(&object, "encode", |ctx: Ctx<'js>, text: String, encoding: Opt<String>| {
        let bytes = encode(&text, encoding.0.as_deref().unwrap_or("utf8")).map_err(|error| throw(&ctx, &error))?;
        bytes_to_js(&ctx, bytes)
    })?;
    set_fn(&object, "decode", |ctx: Ctx<'js>, bytes: String, encoding: Opt<String>| {
        decode(&latin1_to_bytes(&bytes), encoding.0.as_deref().unwrap_or("utf8")).map_err(|error| throw(&ctx, &error))
    })?;
    set_fn(&object, "utf8Decode", |ctx: Ctx<'js>, bytes: String, fatal: bool, ignore_bom: bool| {
        let bytes = latin1_to_bytes(&bytes);
        let body = if !ignore_bom && bytes.starts_with(&[0xef, 0xbb, 0xbf]) { &bytes[3..] } else { &bytes[..] };
        if fatal {
            return std::str::from_utf8(body).map(str::to_string).map_err(|_| {
                throw(
                    &ctx,
                    &JsError::type_error("The encoded data was not valid for encoding utf-8")
                        .with_code("ERR_ENCODING_INVALID_ENCODED_DATA"),
                )
            });
        }
        Ok(String::from_utf8_lossy(body).into_owned())
    })?;
    set_fn(&object, "isUtf8", |bytes: String| std::str::from_utf8(&latin1_to_bytes(&bytes)).is_ok())?;
    set_fn(&object, "segment", |text: String, granularity: String| segment(&text, &granularity))?;
    set_fn(&object, "urlParse", |input: String, base: Opt<Option<String>>| {
        parse_url(&input, base.0.flatten().as_deref()).map(|url| components(&url))
    })?;
    set_fn(&object, "urlSet", |href: String, component: String, value: String| {
        set_component(&href, &component, &value)
    })?;
    set_fn(&object, "domainToASCII", |domain: String| url::quirks::domain_to_ascii(&domain))?;
    set_fn(&object, "domainToUnicode", |domain: String| url::quirks::domain_to_unicode(&domain))?;
    set_fn(&object, "fileUrlToPath", |ctx: Ctx<'js>, href: String| {
        let invalid =
            || throw(&ctx, &JsError::type_error("The URL must be of scheme file").with_code("ERR_INVALID_URL_SCHEME"));
        let url = Url::parse(&href).map_err(|_| invalid())?;
        if url.scheme() != "file" {
            return Err(invalid());
        }
        url.to_file_path().map(|path| path.display().to_string()).map_err(|_| {
            throw(&ctx, &JsError::type_error("File URL path must be absolute").with_code("ERR_INVALID_FILE_URL_PATH"))
        })
    })?;
    set_fn(&object, "pathToFileUrl", |ctx: Ctx<'js>, path: String| {
        let absolute = crate::resolve::normalize(
            &std::path::Path::new(&crate::runtime::state::state().cwd.borrow().clone()).join(&path),
        );
        let mut url = Url::from_file_path(&absolute).map_err(|_| throw(&ctx, &JsError::type_error("Invalid path")))?;
        if (path.ends_with('/') || path.ends_with(std::path::MAIN_SEPARATOR)) && !url.path().ends_with('/') {
            url.set_path(&format!("{}/", url.path()));
        }
        Ok::<_, rquickjs::Error>(url.to_string())
    })?;
    Ok(object)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_node_encodings() {
        for encoding in ["utf8", "latin1", "utf16le", "base64", "base64url", "hex"] {
            let bytes = b"hi \xff\x00 there!".to_vec();
            let text = decode(&bytes, encoding).unwrap();
            if encoding != "utf8" {
                assert_eq!(encode(&text, encoding).unwrap(), bytes, "{encoding}");
            }
        }
        assert_eq!(encode("aGVsbG8", "base64").unwrap(), b"hello");
        assert_eq!(encode("aGV sbG8=\n", "base64").unwrap(), b"hello");
        assert_eq!(encode("0a0bzz", "hex").unwrap(), vec![10, 11]);
        assert!(encode("x", "nope").is_err());
    }

    #[test]
    fn applies_url_setters() {
        let parts = set_component("https://a.example/x?y#z", "pathname", "/new path").unwrap();
        assert_eq!(parts[0], "https://a.example/new%20path?y#z");
        let unchanged = set_component("https://a.example/", "port", "notaport").unwrap();
        assert_eq!(unchanged[7], "");
        assert!(parse_url("not a url", None).is_none());
        assert_eq!(components(&parse_url("../c", Some("http://h/a/b/")).unwrap())[0], "http://h/a/c");
    }
}
