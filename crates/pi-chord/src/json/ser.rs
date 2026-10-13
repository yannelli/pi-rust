//! A serde serializer that builds strict [`Json`] directly and rejects non-finite numbers.

use serde::ser::{self, Impossible, Serialize};

use super::{Json, JsonMap};
use crate::error::ChordError;

#[derive(Debug)]
pub(crate) struct SerError(pub String);

impl std::fmt::Display for SerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SerError {}

impl ser::Error for SerError {
    fn custom<T: std::fmt::Display>(message: T) -> Self {
        Self(message.to_string())
    }
}

impl From<SerError> for ChordError {
    fn from(error: SerError) -> Self {
        ChordError::Type(error.0)
    }
}

/// How an absent `Option` (the analog of JavaScript `undefined`) is written.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoneMode {
    Null,
    Omit,
    Reject,
}

/// Builds a value. `None` marks an absent `Option` so object properties can omit it.
#[derive(Clone, Copy)]
pub(crate) struct JsonSerializer {
    pub none: NoneMode,
}

fn undefined() -> SerError {
    SerError("Value contains a non-JSON undefined; expected strict JSON".into())
}

type Out = Result<Option<Json>, SerError>;

fn number(value: f64) -> Out {
    Json::from_f64(value)
        .map(Some)
        .ok_or_else(|| SerError("Value contains a non-finite number and is not strict JSON".into()))
}

fn present(value: Option<Json>) -> Json {
    value.unwrap_or(Json::Null)
}

pub(crate) struct SeqBuilder {
    serializer: JsonSerializer,
    items: Vec<Json>,
    variant: Option<&'static str>,
}

pub(crate) struct MapBuilder {
    serializer: JsonSerializer,
    map: JsonMap,
    key: Option<String>,
    variant: Option<&'static str>,
}

fn wrap_variant(variant: Option<&'static str>, value: Json) -> Option<Json> {
    Some(match variant {
        Some(name) => Json::object(std::iter::once((name, value)).collect()),
        None => value,
    })
}

impl ser::Serializer for JsonSerializer {
    type Ok = Option<Json>;
    type Error = SerError;
    type SerializeSeq = SeqBuilder;
    type SerializeTuple = SeqBuilder;
    type SerializeTupleStruct = SeqBuilder;
    type SerializeTupleVariant = SeqBuilder;
    type SerializeMap = MapBuilder;
    type SerializeStruct = MapBuilder;
    type SerializeStructVariant = MapBuilder;

    fn serialize_bool(self, value: bool) -> Out {
        Ok(Some(Json::Bool(value)))
    }
    fn serialize_i8(self, value: i8) -> Out {
        Ok(Some(value.into()))
    }
    fn serialize_i16(self, value: i16) -> Out {
        Ok(Some(value.into()))
    }
    fn serialize_i32(self, value: i32) -> Out {
        Ok(Some(value.into()))
    }
    fn serialize_i64(self, value: i64) -> Out {
        Ok(Some(value.into()))
    }
    fn serialize_u8(self, value: u8) -> Out {
        Ok(Some(value.into()))
    }
    fn serialize_u16(self, value: u16) -> Out {
        Ok(Some(value.into()))
    }
    fn serialize_u32(self, value: u32) -> Out {
        Ok(Some(value.into()))
    }
    fn serialize_u64(self, value: u64) -> Out {
        Ok(Some(value.into()))
    }
    fn serialize_f32(self, value: f32) -> Out {
        number(f64::from(value))
    }
    fn serialize_f64(self, value: f64) -> Out {
        number(value)
    }
    fn serialize_char(self, value: char) -> Out {
        Ok(Some(Json::from(value.to_string())))
    }
    fn serialize_str(self, value: &str) -> Out {
        Ok(Some(Json::from(value)))
    }
    fn serialize_bytes(self, value: &[u8]) -> Out {
        Ok(Some(Json::array(value.iter().map(|byte| Json::from(*byte)).collect())))
    }
    fn serialize_none(self) -> Out {
        Ok(None)
    }
    fn serialize_some<T: ?Sized + Serialize>(self, value: &T) -> Out {
        value.serialize(self).map(|value| Some(present(value)))
    }
    fn serialize_unit(self) -> Out {
        Ok(Some(Json::Null))
    }
    fn serialize_unit_struct(self, _name: &'static str) -> Out {
        Ok(Some(Json::Null))
    }
    fn serialize_unit_variant(self, _name: &'static str, _index: u32, variant: &'static str) -> Out {
        Ok(Some(Json::from(variant)))
    }
    fn serialize_newtype_struct<T: ?Sized + Serialize>(self, _name: &'static str, value: &T) -> Out {
        value.serialize(self)
    }
    fn serialize_newtype_variant<T: ?Sized + Serialize>(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        value: &T,
    ) -> Out {
        let inner = present(value.serialize(self)?);
        Ok(wrap_variant(Some(variant), inner))
    }
    fn serialize_seq(self, len: Option<usize>) -> Result<SeqBuilder, SerError> {
        Ok(SeqBuilder { serializer: self, items: Vec::with_capacity(len.unwrap_or(0)), variant: None })
    }
    fn serialize_tuple(self, len: usize) -> Result<SeqBuilder, SerError> {
        self.serialize_seq(Some(len))
    }
    fn serialize_tuple_struct(self, _name: &'static str, len: usize) -> Result<SeqBuilder, SerError> {
        self.serialize_seq(Some(len))
    }
    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<SeqBuilder, SerError> {
        Ok(SeqBuilder { serializer: self, items: Vec::with_capacity(len), variant: Some(variant) })
    }
    fn serialize_map(self, _len: Option<usize>) -> Result<MapBuilder, SerError> {
        Ok(MapBuilder { serializer: self, map: JsonMap::new(), key: None, variant: None })
    }
    fn serialize_struct(self, _name: &'static str, len: usize) -> Result<MapBuilder, SerError> {
        self.serialize_map(Some(len))
    }
    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        _len: usize,
    ) -> Result<MapBuilder, SerError> {
        Ok(MapBuilder { serializer: self, map: JsonMap::new(), key: None, variant: Some(variant) })
    }
}

impl SeqBuilder {
    fn push<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), SerError> {
        match value.serialize(self.serializer)? {
            Some(value) => self.items.push(value),
            None if self.serializer.none == NoneMode::Null => self.items.push(Json::Null),
            None => return Err(undefined()),
        }
        Ok(())
    }
    fn finish(self) -> Out {
        Ok(wrap_variant(self.variant, Json::array(self.items)))
    }
}

macro_rules! seq_impl {
    ($trait:ident, $method:ident) => {
        impl ser::$trait for SeqBuilder {
            type Ok = Option<Json>;
            type Error = SerError;
            fn $method<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), SerError> {
                self.push(value)
            }
            fn end(self) -> Out {
                self.finish()
            }
        }
    };
}
seq_impl!(SerializeSeq, serialize_element);
seq_impl!(SerializeTuple, serialize_element);
seq_impl!(SerializeTupleStruct, serialize_field);
seq_impl!(SerializeTupleVariant, serialize_field);

impl MapBuilder {
    fn entry<T: ?Sized + Serialize>(&mut self, key: String, value: &T) -> Result<(), SerError> {
        match value.serialize(self.serializer)? {
            Some(value) => {
                self.map.insert(key, value);
            }
            None => match self.serializer.none {
                NoneMode::Omit => {}
                NoneMode::Null => {
                    self.map.insert(key, Json::Null);
                }
                NoneMode::Reject => return Err(undefined()),
            },
        }
        Ok(())
    }
    fn finish(self) -> Out {
        Ok(wrap_variant(self.variant, Json::object(self.map)))
    }
}

impl ser::SerializeMap for MapBuilder {
    type Ok = Option<Json>;
    type Error = SerError;
    fn serialize_key<T: ?Sized + Serialize>(&mut self, key: &T) -> Result<(), SerError> {
        self.key = Some(key.serialize(KeySerializer)?);
        Ok(())
    }
    fn serialize_value<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), SerError> {
        let key = self.key.take().ok_or_else(|| SerError("map value without a key".into()))?;
        self.entry(key, value)
    }
    fn end(self) -> Out {
        self.finish()
    }
}

macro_rules! struct_impl {
    ($trait:ident) => {
        impl ser::$trait for MapBuilder {
            type Ok = Option<Json>;
            type Error = SerError;
            fn serialize_field<T: ?Sized + Serialize>(&mut self, key: &'static str, value: &T) -> Result<(), SerError> {
                self.entry(key.to_string(), value)
            }
            fn end(self) -> Out {
                self.finish()
            }
        }
    };
}
struct_impl!(SerializeStruct);
struct_impl!(SerializeStructVariant);

/// Object keys: strings, plus integers and booleans stringified as `serde_json` does.
struct KeySerializer;

fn key_error() -> SerError {
    SerError("Value contains a non-string key and is not strict JSON".into())
}

macro_rules! key_display {
    ($($method:ident: $ty:ty),*) => {$(
        fn $method(self, value: $ty) -> Result<String, SerError> {
            Ok(value.to_string())
        }
    )*};
}

impl ser::Serializer for KeySerializer {
    type Ok = String;
    type Error = SerError;
    type SerializeSeq = Impossible<String, SerError>;
    type SerializeTuple = Impossible<String, SerError>;
    type SerializeTupleStruct = Impossible<String, SerError>;
    type SerializeTupleVariant = Impossible<String, SerError>;
    type SerializeMap = Impossible<String, SerError>;
    type SerializeStruct = Impossible<String, SerError>;
    type SerializeStructVariant = Impossible<String, SerError>;

    key_display!(serialize_bool: bool, serialize_i8: i8, serialize_i16: i16, serialize_i32: i32, serialize_i64: i64,
        serialize_u8: u8, serialize_u16: u16, serialize_u32: u32, serialize_u64: u64, serialize_char: char,
        serialize_str: &str);

    fn serialize_f32(self, _value: f32) -> Result<String, SerError> {
        Err(key_error())
    }
    fn serialize_f64(self, _value: f64) -> Result<String, SerError> {
        Err(key_error())
    }
    fn serialize_bytes(self, _value: &[u8]) -> Result<String, SerError> {
        Err(key_error())
    }
    fn serialize_none(self) -> Result<String, SerError> {
        Err(key_error())
    }
    fn serialize_some<T: ?Sized + Serialize>(self, _value: &T) -> Result<String, SerError> {
        Err(key_error())
    }
    fn serialize_unit(self) -> Result<String, SerError> {
        Err(key_error())
    }
    fn serialize_unit_struct(self, _name: &'static str) -> Result<String, SerError> {
        Err(key_error())
    }
    fn serialize_unit_variant(self, _name: &'static str, _i: u32, variant: &'static str) -> Result<String, SerError> {
        Ok(variant.to_string())
    }
    fn serialize_newtype_struct<T: ?Sized + Serialize>(self, _n: &'static str, value: &T) -> Result<String, SerError> {
        value.serialize(self)
    }
    fn serialize_newtype_variant<T: ?Sized + Serialize>(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _value: &T,
    ) -> Result<String, SerError> {
        Err(key_error())
    }
    fn serialize_seq(self, _len: Option<usize>) -> Result<Self::SerializeSeq, SerError> {
        Err(key_error())
    }
    fn serialize_tuple(self, _len: usize) -> Result<Self::SerializeTuple, SerError> {
        Err(key_error())
    }
    fn serialize_tuple_struct(self, _n: &'static str, _len: usize) -> Result<Self::SerializeTupleStruct, SerError> {
        Err(key_error())
    }
    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleVariant, SerError> {
        Err(key_error())
    }
    fn serialize_map(self, _len: Option<usize>) -> Result<Self::SerializeMap, SerError> {
        Err(key_error())
    }
    fn serialize_struct(self, _name: &'static str, _len: usize) -> Result<Self::SerializeStruct, SerError> {
        Err(key_error())
    }
    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStructVariant, SerError> {
        Err(key_error())
    }
}
