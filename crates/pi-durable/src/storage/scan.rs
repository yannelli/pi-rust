//! Cursor handling shared by the built-in storages' ordered scans.

use pi_chord::{Json, JsonMap};

use crate::errors::{DurableError, DurableResult};
use crate::types::{Cursor, ScanOrder};

/// Largest integer JavaScript represents exactly; IDs and cursors stay within it.
pub const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

/// Where a built-in storage scan starts: its order and the last ID an earlier page returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanStart {
    pub order: ScanOrder,
    pub after: Option<i64>,
}

fn safe_integer(value: Option<&Json>) -> Option<i64> {
    let number = match value? {
        Json::Number(number) => number.as_f64(),
        _ => return None,
    };
    (number.fract() == 0.0 && number.abs() <= MAX_SAFE_INTEGER as f64).then_some(number as i64)
}

fn parse_order(value: &Json) -> Option<ScanOrder> {
    match value.as_str()? {
        "ascending" => Some(ScanOrder::Ascending),
        "descending" => Some(ScanOrder::Descending),
        _ => None,
    }
}

/// Resolve a scan's order and position. A cursor continues in the order it was created with, whether the query
/// repeats that order or omits it; a different `order` fails. A cursor without an order continues in `fallback`.
pub fn scan_start(
    requested: Option<ScanOrder>,
    cursor: Option<&Cursor>,
    fallback: ScanOrder,
) -> DurableResult<ScanStart> {
    let Some(cursor) = cursor else {
        return Ok(ScanStart { order: requested.unwrap_or(fallback), after: None });
    };
    let after = safe_integer(cursor.get("after")).ok_or_else(invalid_cursor)?;
    let order = match cursor.get("order") {
        None => fallback,
        Some(value) => parse_order(value).ok_or_else(invalid_cursor)?,
    };
    if let Some(requested) = requested
        && requested != order
    {
        return Err(DurableError::storage_request(format!(
            "The cursor continues a {} scan; the query asks for {}",
            order.as_str(),
            requested.as_str()
        )));
    }
    Ok(ScanStart { order, after: Some(after) })
}

/// The `after` position of a cursor whose scan has one fixed order (document scans).
pub fn cursor_id(cursor: Option<&Cursor>) -> DurableResult<Option<i64>> {
    match cursor.and_then(|cursor| cursor.get("after")) {
        None => Ok(None),
        Some(value) => safe_integer(Some(value)).map(Some).ok_or_else(invalid_cursor),
    }
}

/// The continuation of a scan whose last returned item has `id`.
pub fn next_cursor(id: u64, order: ScanOrder) -> Cursor {
    let mut cursor = JsonMap::new();
    cursor.insert("after", Json::from(id as f64));
    cursor.insert("order", Json::from(order.as_str()));
    cursor
}

pub(crate) fn invalid_cursor() -> DurableError {
    DurableError::storage_request("Invalid storage cursor")
}
