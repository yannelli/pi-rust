//! `google.protobuf.Value` for JSON: tool schemas sent to Cursor and tool arguments it returns.

use serde_json::{Map, Number, Value as Json};

use super::proto::{Value, Writer, fields};

pub(crate) fn encode(json: &Json) -> Writer {
    match json {
        Json::Null => Writer::new().varint(1, 0),
        Json::Number(number) => Writer::new().double(2, number.as_f64().unwrap_or_default()),
        Json::String(text) => Writer::new().string(3, text),
        Json::Bool(flag) => Writer::new().bool(4, *flag),
        Json::Object(object) => Writer::new().message(5, encode_struct(object)),
        Json::Array(items) => {
            let list = items.iter().fold(Writer::new(), |list, item| list.message(1, encode(item)));
            Writer::new().message(6, list)
        }
    }
}

pub(crate) fn encode_struct(object: &Map<String, Json>) -> Writer {
    object
        .iter()
        .fold(Writer::new(), |out, (key, value)| out.message(1, Writer::new().string(1, key).message(2, encode(value))))
}

pub(crate) fn decode(data: &[u8]) -> Result<Json, String> {
    let mut decoded = Json::Null;
    for (field, value) in fields(data)? {
        decoded = match (field, value) {
            (2, Value::Fixed64(bits)) => {
                let number = f64::from_bits(bits);
                if number.fract() == 0.0 && number.abs() < 9e15 {
                    Json::from(number as i64)
                } else {
                    Number::from_f64(number).map_or(Json::Null, Json::Number)
                }
            }
            (3, Value::Bytes(text)) => Json::String(String::from_utf8_lossy(text).into_owned()),
            (4, Value::Varint(flag)) => Json::Bool(flag != 0),
            (5, Value::Bytes(object)) => Json::Object(decode_struct(object)?),
            (6, Value::Bytes(list)) => {
                let items = fields(list)?.into_iter().filter(|(field, _)| *field == 1);
                Json::Array(items.filter_map(|(_, item)| item.as_bytes()).map(decode).collect::<Result<_, _>>()?)
            }
            _ => Json::Null,
        };
    }
    Ok(decoded)
}

pub(crate) fn decode_struct(data: &[u8]) -> Result<Map<String, Json>, String> {
    let mut object = Map::new();
    for (_, entry) in fields(data)?.into_iter().filter(|(field, _)| *field == 1) {
        let entry = fields(entry.as_bytes().unwrap_or_default())?;
        let key = entry.iter().find(|(field, _)| *field == 1).and_then(|(_, key)| key.as_str()).unwrap_or_default();
        let value = entry.iter().find(|(field, _)| *field == 2).and_then(|(_, value)| value.as_bytes());
        object.insert(key.to_string(), value.map(decode).transpose()?.unwrap_or(Json::Null));
    }
    Ok(object)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn matches_the_documented_bytes_and_round_trips() {
        let schema = json!({"type": "object"});
        let bytes = encode(&schema).finish();
        let expected = [
            0x2a, 0x12, 0x0a, 0x10, 0x0a, 0x04, b't', b'y', b'p', b'e', 0x12, 0x08, 0x1a, 0x06, b'o', b'b', b'j', b'e',
            b'c', b't',
        ];
        assert_eq!(bytes, expected);
        let value = json!({"a": [1, 2.5, "x", true, null], "b": {"c": -3}});
        assert_eq!(decode(&encode(&value).finish()).unwrap(), value);
    }
}
