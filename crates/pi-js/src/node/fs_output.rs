//! The results of fs requests and their conversion to JavaScript values.

use rquickjs::{Array, Ctx, Object, Value};

use super::fs_ops::StatInfo;
use crate::error::JsError;
use crate::runtime::convert::{bytes_to_js, caught};

pub(crate) enum Output {
    Undefined,
    Bool(bool),
    Number(f64),
    Text(String),
    MaybeText(Option<String>),
    Bytes(Vec<u8>),
    Stat(Box<StatInfo>),
    /// `(name, type)` pairs; names are relative paths for recursive listings.
    Entries(Vec<(String, u8)>),
}

pub(crate) fn to_js<'js>(ctx: &Ctx<'js>, output: Output) -> Result<Value<'js>, JsError> {
    let result: rquickjs::Result<Value<'js>> = (|| match output {
        Output::Undefined => Ok(Value::new_undefined(ctx.clone())),
        Output::Bool(flag) => Ok(Value::new_bool(ctx.clone(), flag)),
        Output::Number(number) => Ok(Value::new_number(ctx.clone(), number)),
        Output::Text(text) => rquickjs::String::from_str(ctx.clone(), &text).map(|text| text.into_value()),
        Output::MaybeText(None) => Ok(Value::new_undefined(ctx.clone())),
        Output::MaybeText(Some(text)) => rquickjs::String::from_str(ctx.clone(), &text).map(|text| text.into_value()),
        Output::Bytes(bytes) => bytes_to_js(ctx, bytes),
        Output::Stat(info) => {
            let object = Object::new(ctx.clone())?;
            let fields = [
                ("dev", info.dev),
                ("ino", info.ino),
                ("mode", info.mode),
                ("nlink", info.nlink),
                ("uid", info.uid),
                ("gid", info.gid),
                ("rdev", info.rdev),
                ("size", info.size),
                ("blksize", info.blksize),
                ("blocks", info.blocks),
                ("atimeMs", info.atime_ms),
                ("mtimeMs", info.mtime_ms),
                ("ctimeMs", info.ctime_ms),
                ("birthtimeMs", info.birthtime_ms),
            ];
            for (key, value) in fields {
                object.set(key, value)?;
            }
            object.set("exact", info.exact.iter().map(i128::to_string).collect::<Vec<_>>())?;
            Ok(object.into_value())
        }
        Output::Entries(entries) => {
            let list = Array::new(ctx.clone())?;
            for (index, (name, kind)) in entries.into_iter().enumerate() {
                let pair = Array::new(ctx.clone())?;
                pair.set(0, name)?;
                pair.set(1, kind)?;
                list.set(index, pair)?;
            }
            Ok(list.into_value())
        }
    })();
    result.map_err(|error| caught(ctx, error))
}
