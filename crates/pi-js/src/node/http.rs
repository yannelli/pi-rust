//! Natives for `node:http`: the HTTP/1.x message head parser on httparse.

use rquickjs::{Ctx, Object, TypedArray, Value};
use serde_json::json;

use super::set_fn;
use crate::error::JsError;
use crate::runtime::convert::{bytes_of, from_json, throw};

fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&byte| byte as char).collect()
}

/// llhttp's code and reason for an httparse error, as Node reports them.
fn parse_error(error: httparse::Error, request: bool) -> JsError {
    let (code, reason) = match error {
        httparse::Error::HeaderName => ("HPE_INVALID_HEADER_TOKEN", "Invalid header token"),
        httparse::Error::HeaderValue => ("HPE_INVALID_HEADER_TOKEN", "Invalid header value char"),
        httparse::Error::NewLine => ("HPE_LF_EXPECTED", "Missing expected LF after header value"),
        httparse::Error::Status => ("HPE_INVALID_STATUS", "Invalid status code"),
        httparse::Error::Token if request => ("HPE_INVALID_METHOD", "Invalid method encountered"),
        httparse::Error::Token => ("HPE_INVALID_CONSTANT", "Expected HTTP/"),
        httparse::Error::TooManyHeaders => ("HPE_HEADER_OVERFLOW", "Header overflow"),
        httparse::Error::Version => ("HPE_INVALID_VERSION", "Invalid HTTP version"),
    };
    JsError::new(format!("Parse Error: {reason}")).with_code(code).with_data("reason", reason)
}

fn complete(status: httparse::Status<usize>) -> Option<usize> {
    match status {
        httparse::Status::Complete(length) => Some(length),
        httparse::Status::Partial => None,
    }
}

fn header_list(headers: &[httparse::Header<'_>]) -> Vec<String> {
    headers.iter().flat_map(|header| [header.name.to_string(), latin1(header.value)]).collect()
}

/// Parse a request or response head. Returns undefined while the head is incomplete.
fn parse_head<'js>(ctx: Ctx<'js>, data: TypedArray<'js, u8>, request: bool) -> rquickjs::Result<Value<'js>> {
    let bytes = bytes_of(&data);
    let mut size = 128;
    loop {
        let mut headers = vec![httparse::EMPTY_HEADER; size];
        let result = if request {
            let mut parsed = httparse::Request::new(&mut headers);
            parsed.parse(&bytes).map(|status| {
                complete(status).map(|length| {
                    json!({
                        "length": length,
                        "method": parsed.method.unwrap_or_default(),
                        "url": parsed.path.unwrap_or_default(),
                        "minor": parsed.version.unwrap_or(1),
                        "headers": header_list(parsed.headers),
                    })
                })
            })
        } else {
            let mut parsed = httparse::Response::new(&mut headers);
            parsed.parse(&bytes).map(|status| {
                complete(status).map(|length| {
                    json!({
                        "length": length,
                        "statusCode": parsed.code.unwrap_or_default(),
                        "statusMessage": parsed.reason.unwrap_or_default(),
                        "minor": parsed.version.unwrap_or(1),
                        "headers": header_list(parsed.headers),
                    })
                })
            })
        };
        return match result {
            Ok(Some(head)) => from_json(&ctx, &head),
            Ok(None) => Ok(Value::new_undefined(ctx.clone())),
            Err(httparse::Error::TooManyHeaders) if size < 8192 => {
                size *= 4;
                continue;
            }
            Err(error) => Err(throw(&ctx, &parse_error(error, request))),
        };
    }
}

/// Parse a chunk size line: `[consumed, size]`, or undefined while incomplete.
fn chunk_size<'js>(ctx: Ctx<'js>, data: TypedArray<'js, u8>) -> rquickjs::Result<Value<'js>> {
    match httparse::parse_chunk_size(&bytes_of(&data)) {
        Ok(httparse::Status::Complete((consumed, size))) => from_json(&ctx, &json!([consumed, size])),
        Ok(httparse::Status::Partial) => Ok(Value::new_undefined(ctx.clone())),
        Err(_) => {
            let error =
                JsError::new("Parse Error: Invalid character in chunk size").with_code("HPE_INVALID_CHUNK_SIZE");
            Err(throw(&ctx, &error.with_data("reason", "Invalid character in chunk size")))
        }
    }
}

pub(crate) fn natives<'js>(ctx: &Ctx<'js>) -> rquickjs::Result<Object<'js>> {
    let object = Object::new(ctx.clone())?;
    set_fn(&object, "parseHead", parse_head)?;
    set_fn(&object, "chunkSize", chunk_size)?;
    Ok(object)
}
