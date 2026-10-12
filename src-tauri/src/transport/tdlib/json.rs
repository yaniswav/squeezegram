//! Reading TDLib JSON objects without `unwrap`: a missing or mistyped field is
//! `TransportError::Protocol` naming the object type and the field, never its value.
//!
//! TDLib writes `int53` as a JSON number and `int64` as a string (`td_api.tl` 1.8.68); `int`
//! accepts both.

use serde_json::Value;

use crate::transport::TransportError;

/// `@type` of an object, `?` when there is none.
pub fn kind(object: &Value) -> &str {
    object["@type"].as_str().unwrap_or("?")
}

pub fn missing(object: &Value, field: &str) -> TransportError {
    TransportError::Protocol(format!("{}: missing {field}", kind(object)))
}

/// The object itself if its `@type` is `expected`.
pub fn expect<'a>(object: &'a Value, expected: &str) -> Result<&'a Value, TransportError> {
    if object["@type"] == expected {
        Ok(object)
    } else {
        Err(TransportError::Protocol(format!(
            "expected {expected}, got {}",
            kind(object)
        )))
    }
}

/// An integer written as a number or as a string.
pub fn int_value(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.parse().ok(),
        _ => None,
    }
}

pub fn int(object: &Value, field: &str) -> Result<i64, TransportError> {
    int_value(&object[field]).ok_or_else(|| missing(object, field))
}

/// A missing integer is 0 (TDLib omits nothing, but a default keeps old fields harmless).
pub fn int_or_zero(object: &Value, field: &str) -> i64 {
    int_value(&object[field]).unwrap_or(0)
}

pub fn boolean(object: &Value, field: &str) -> Result<bool, TransportError> {
    object[field]
        .as_bool()
        .ok_or_else(|| missing(object, field))
}

pub fn flag(object: &Value, field: &str) -> bool {
    object[field].as_bool().unwrap_or(false)
}

pub fn text<'a>(object: &'a Value, field: &str) -> Result<&'a str, TransportError> {
    object[field].as_str().ok_or_else(|| missing(object, field))
}

pub fn array<'a>(object: &'a Value, field: &str) -> Result<&'a Vec<Value>, TransportError> {
    object[field]
        .as_array()
        .ok_or_else(|| missing(object, field))
}

/// A non-negative `int53` size; negative or missing is 0.
pub fn size(object: &Value, field: &str) -> u64 {
    u64::try_from(int_or_zero(object, field)).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn integers_as_numbers_or_strings_and_typed_errors() {
        let object = json!({ "@type": "x", "a": 5, "b": "-7", "c": "no", "d": true, "e": "t" });
        assert_eq!(int(&object, "a").unwrap(), 5);
        assert_eq!(int(&object, "b").unwrap(), -7);
        assert!(
            matches!(int(&object, "c"), Err(TransportError::Protocol(m)) if m == "x: missing c")
        );
        assert_eq!(int_or_zero(&object, "zz"), 0);
        assert!(boolean(&object, "d").unwrap());
        assert!(boolean(&object, "a").is_err());
        assert!(!flag(&object, "zz"));
        assert_eq!(text(&object, "e").unwrap(), "t");
        assert!(array(&object, "e").is_err());
        assert_eq!(size(&object, "b"), 0);
        assert!(expect(&object, "x").is_ok());
        assert!(expect(&object, "y").is_err());
        assert_eq!(kind(&json!([1])), "?");
    }
}
