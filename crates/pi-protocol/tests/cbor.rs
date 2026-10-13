//! Port of upstream `test/cbor/cbor.test.ts`, including its RFC 8949 golden vectors.

use pi_protocol::{
    CborOptions, CborValue, DEFAULT_MAX_CBOR_BYTE_LENGTH, DEFAULT_MAX_CBOR_CONTAINER_LENGTH, DEFAULT_MAX_CBOR_DEPTH,
    decode_cbor, decode_json_cbor, encode_cbor, encode_json_cbor,
};

fn from_hex(hex: &str) -> Vec<u8> {
    assert!(hex.len() % 2 == 0, "Hex fixture must contain whole bytes");
    (0..hex.len()).step_by(2).map(|index| u8::from_str_radix(&hex[index..index + 2], 16).unwrap()).collect()
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn num(value: f64) -> CborValue {
    CborValue::Number(value)
}

fn text(value: &str) -> CborValue {
    CborValue::Text(value.to_string())
}

fn known_vectors() -> Vec<(CborValue, &'static str)> {
    use CborValue::{Array, Bool, Bytes, Map, Null};
    vec![
        (Null, "f6"),
        (Bool(false), "f4"),
        (Bool(true), "f5"),
        (num(0.0), "00"),
        (num(1.0), "01"),
        (num(10.0), "0a"),
        (num(23.0), "17"),
        (num(24.0), "1818"),
        (num(25.0), "1819"),
        (num(100.0), "1864"),
        (num(1000.0), "1903e8"),
        (num(1_000_000.0), "1a000f4240"),
        (num(1_000_000_000_000.0), "1b000000e8d4a51000"),
        (num(9_007_199_254_740_991.0), "1b001fffffffffffff"),
        (num(-1.0), "20"),
        (num(-10.0), "29"),
        (num(-24.0), "37"),
        (num(-25.0), "3818"),
        (num(-100.0), "3863"),
        (num(-1000.0), "3903e7"),
        (num(-1_000_000.0), "3a000f423f"),
        (num(-9_007_199_254_740_991.0), "3b001ffffffffffffe"),
        (num(1.1), "fb3ff199999999999a"),
        (num(-0.0), "fb8000000000000000"),
        (Bytes(vec![1, 2, 3, 4]), "4401020304"),
        (text(""), "60"),
        (text("IETF"), "6449455446"),
        (text("ü"), "62c3bc"),
        (text("水"), "63e6b0b4"),
        (text("𐅑"), "64f0908591"),
        (Array(vec![]), "80"),
        (Array(vec![num(1.0), num(2.0), num(3.0)]), "83010203"),
        (Array(vec![num(1.0), Array(vec![num(2.0), num(3.0)]), Array(vec![num(4.0), num(5.0)])]), "8301820203820405"),
        (Map(vec![("a".into(), num(1.0)), ("b".into(), Array(vec![num(2.0), num(3.0)]))]), "a26161016162820203"),
    ]
}

#[test]
fn encodes_and_decodes_rfc_8949_vectors() {
    let defaults = CborOptions::default();
    for (value, wire) in known_vectors() {
        assert_eq!(to_hex(&encode_cbor(&value, &defaults).unwrap()), wire, "{value:?}");
        let decoded = decode_cbor(&from_hex(wire), &defaults).unwrap();
        assert_eq!(decoded, value);
        if let (CborValue::Number(left), CborValue::Number(right)) = (&decoded, &value) {
            assert_eq!(left.is_sign_negative(), right.is_sign_negative());
        }
    }
}

#[test]
fn json_values_encode_like_javascript_objects() {
    let value = pi_chord::json!({ "zero": 0, "empty": "", "no": false, "nil": null, "b": 2, "1": "index" });
    let bytes = encode_json_cbor(&value, &CborOptions::default()).unwrap();
    // JavaScript orders array-index keys first, so "1" leads.
    assert_eq!(&to_hex(&bytes)[..8], "a6613165");
    assert_eq!(decode_json_cbor(&bytes, &CborOptions::default()).unwrap(), value);
}

#[test]
fn preserves_a_leading_bom_and_treats_proto_as_data() {
    assert_eq!(decode_cbor(&from_hex("63efbbbf"), &CborOptions::default()).unwrap(), text("\u{feff}"));
    let value = CborValue::Map(vec![("__proto__".into(), text("safe"))]);
    let encoded = encode_cbor(&value, &CborOptions::default()).unwrap();
    assert_eq!(decode_cbor(&encoded, &CborOptions::default()).unwrap(), value);
}

#[test]
fn rejects_unsupported_encoder_values() {
    let defaults = CborOptions::default();
    for value in [
        num(f64::NAN),
        num(f64::INFINITY),
        num(f64::NEG_INFINITY),
        num(9_007_199_254_740_992.0),
        num(-9_007_199_254_740_992.0),
    ] {
        assert!(encode_cbor(&value, &defaults).is_err(), "{value:?}");
    }
    let duplicate = CborValue::Map(vec![("a".into(), num(1.0)), ("a".into(), num(2.0))]);
    assert!(encode_cbor(&duplicate, &defaults).is_err());
}

#[test]
fn rejects_excessive_encoder_depth() {
    let mut too_deep = CborValue::Null;
    for _ in 0..=DEFAULT_MAX_CBOR_DEPTH {
        too_deep = CborValue::Array(vec![too_deep]);
    }
    let error = encode_cbor(&too_deep, &CborOptions::default()).unwrap_err();
    assert!(error.0.contains("depth"), "{error}");
}

#[test]
fn rejects_invalid_decoder_input() {
    let cases = [
        ("empty input", ""),
        ("truncated integer", "18"),
        ("reserved additional information", "1c"),
        ("indefinite byte string", "5f"),
        ("indefinite text string", "7f"),
        ("indefinite array", "9f"),
        ("indefinite map", "bf"),
        ("tag", "c000"),
        ("undefined", "f7"),
        ("unsupported simple value", "e0"),
        ("break outside an indefinite item", "ff"),
        ("float16", "f93c00"),
        ("float32", "fa3f800000"),
        ("positive infinity", "fb7ff0000000000000"),
        ("NaN", "fb7ff8000000000000"),
        ("truncated float64", "fb3ff00000"),
        ("truncated byte string", "44010203"),
        ("truncated text string", "636162"),
        ("truncated array", "8201"),
        ("truncated map", "a16161"),
        ("trailing data", "0000"),
        ("non-string map key", "a10102"),
        ("duplicate map key", "a2616101616102"),
        ("invalid UTF-8 byte", "61ff"),
        ("overlong UTF-8", "62c080"),
        ("UTF-8 surrogate", "63eda080"),
        ("unsafe positive integer", "1b0020000000000000"),
        ("unsafe negative integer", "3b001fffffffffffff"),
        ("unsafe integer encoded as float64", "fb4340000000000000"),
    ];
    for (label, wire) in cases {
        assert!(decode_cbor(&from_hex(wire), &CborOptions::default()).is_err(), "{label}");
    }
}

#[test]
fn enforces_depth_and_declared_length_limits_before_traversal() {
    let mut too_deep = vec![0x81u8; DEFAULT_MAX_CBOR_DEPTH as usize + 2];
    *too_deep.last_mut().unwrap() = 0xf6;
    assert!(decode_cbor(&too_deep, &CborOptions::default()).unwrap_err().0.contains("depth"));

    let bytes = format!("5a{:08x}", DEFAULT_MAX_CBOR_BYTE_LENGTH + 1);
    let text = format!("7a{:08x}", DEFAULT_MAX_CBOR_BYTE_LENGTH + 1);
    let array = format!("9a{:08x}", DEFAULT_MAX_CBOR_CONTAINER_LENGTH + 1);
    let map = format!("ba{:08x}", DEFAULT_MAX_CBOR_CONTAINER_LENGTH + 1);
    for wire in [bytes, text, array, map] {
        let error = decode_cbor(&from_hex(&wire), &CborOptions::default()).unwrap_err();
        assert!(error.0.contains("limit"), "{error}");
    }
}

#[test]
fn supports_stricter_caller_limits() {
    let containers = CborOptions { max_container_length: Some(2), ..CborOptions::default() };
    let bytes = CborOptions { max_byte_length: Some(2), ..CborOptions::default() };
    assert!(decode_cbor(&from_hex("83010203"), &containers).unwrap_err().0.contains("limit"));
    assert!(decode_cbor(&from_hex("626162"), &bytes).unwrap_err().0.contains("limit"));
    let three = CborValue::Array(vec![num(1.0), num(2.0), num(3.0)]);
    assert!(encode_cbor(&three, &containers).unwrap_err().0.contains("limit"));
    assert!(encode_cbor(&text("ab"), &bytes).unwrap_err().0.contains("limit"));
    let invalid_depth = CborOptions { max_depth: Some(513), ..CborOptions::default() };
    assert!(encode_cbor(&CborValue::Null, &invalid_depth).is_err());
}

#[test]
fn byte_strings_are_not_strict_json() {
    assert!(decode_json_cbor(&from_hex("4401020304"), &CborOptions::default()).is_err());
}
