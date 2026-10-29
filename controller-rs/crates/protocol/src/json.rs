//! Reading typed fields out of a JSON object at the link boundary, and the
//! uint64 decimal string form (docs/protocol-v1.md §2).

use serde::Serializer;
use serde_json::{Map, Value};

use crate::error::ProtocolError;

pub(crate) type Object = Map<String, Value>;

/// Parses a uint64 decimal string the way the firmware's `strconv.ParseUint`
/// does: one or more ASCII digits (leading zeros allowed), no sign, no
/// spaces, at most `u64::MAX`.
pub(crate) fn parse_u64_decimal(text: &str) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// Serializes a `u64` as a JSON decimal string.
pub(crate) fn u64_string<S: Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_str(value)
}

/// The fields of one JSON object, read with `malformed_message` errors that
/// name the field.
pub(crate) struct Fields<'a> {
    object: &'a Object,
    owner: &'static str,
}

impl<'a> Fields<'a> {
    /// Fields of `object`, the body or sub-object named `owner`.
    pub(crate) fn new(object: &'a Object, owner: &'static str) -> Self {
        Self { object, owner }
    }

    pub(crate) fn error(&self, key: &str, rule: &str) -> ProtocolError {
        ProtocolError::malformed(format!("{}.{key} {rule}", self.owner))
    }

    /// The value under `key`, `None` when absent. JSON null is returned as a value.
    pub(crate) fn raw(&self, key: &str) -> Option<&'a Value> {
        self.object.get(key)
    }

    /// The value under `key`, `None` when absent or null.
    fn present(&self, key: &str) -> Option<&'a Value> {
        self.object.get(key).filter(|v| !v.is_null())
    }

    pub(crate) fn string(&self, key: &str) -> Result<&'a str, ProtocolError> {
        self.raw(key)
            .and_then(Value::as_str)
            .ok_or_else(|| self.error(key, "must be a string"))
    }

    pub(crate) fn nonempty_string(&self, key: &str) -> Result<&'a str, ProtocolError> {
        match self.raw(key).and_then(Value::as_str) {
            Some(text) if !text.is_empty() => Ok(text),
            _ => Err(self.error(key, "must be a non-empty string")),
        }
    }

    /// A string or `None` when absent or null.
    pub(crate) fn optional_string(&self, key: &str) -> Result<Option<&'a str>, ProtocolError> {
        match self.present(key) {
            None => Ok(None),
            Some(Value::String(text)) => Ok(Some(text)),
            Some(_) => Err(self.error(key, "must be a string or null")),
        }
    }

    /// A string, with absent, null and empty all read as `None`: the device
    /// sends an empty string for a value it could not read.
    pub(crate) fn optional_text(&self, key: &str) -> Result<Option<String>, ProtocolError> {
        Ok(self
            .optional_string(key)?
            .filter(|t| !t.is_empty())
            .map(str::to_owned))
    }

    pub(crate) fn optional_bool(&self, key: &str) -> Result<Option<bool>, ProtocolError> {
        match self.present(key) {
            None => Ok(None),
            Some(Value::Bool(b)) => Ok(Some(*b)),
            Some(_) => Err(self.error(key, "must be a boolean or null")),
        }
    }

    /// A non-negative JSON integer, `None` when absent or null.
    pub(crate) fn optional_count(&self, key: &str) -> Result<Option<u64>, ProtocolError> {
        match self.present(key) {
            None => Ok(None),
            Some(value) => value
                .as_u64()
                .map(Some)
                .ok_or_else(|| self.error(key, "must be a non-negative integer")),
        }
    }

    pub(crate) fn u64_string(&self, key: &str) -> Result<u64, ProtocolError> {
        self.raw(key)
            .and_then(Value::as_str)
            .and_then(parse_u64_decimal)
            .ok_or_else(|| self.error(key, "must be a uint64 decimal string"))
    }

    /// A uint64 decimal string, `None` when absent or null.
    pub(crate) fn optional_u64_string(&self, key: &str) -> Result<Option<u64>, ProtocolError> {
        match self.present(key) {
            None => Ok(None),
            Some(_) => self.u64_string(key).map(Some),
        }
    }

    pub(crate) fn object(&self, key: &str) -> Result<&'a Object, ProtocolError> {
        self.raw(key)
            .and_then(Value::as_object)
            .ok_or_else(|| self.error(key, "must be an object"))
    }

    /// An object, `None` when absent or null.
    pub(crate) fn optional_object(&self, key: &str) -> Result<Option<&'a Object>, ProtocolError> {
        match self.present(key) {
            None => Ok(None),
            Some(Value::Object(object)) => Ok(Some(object)),
            Some(_) => Err(self.error(key, "must be an object or null")),
        }
    }

    pub(crate) fn array(&self, key: &str) -> Result<&'a [Value], ProtocolError> {
        self.raw(key)
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .ok_or_else(|| self.error(key, "must be an array"))
    }

    /// An array, `None` when absent or null.
    pub(crate) fn optional_array(&self, key: &str) -> Result<Option<&'a [Value]>, ProtocolError> {
        match self.present(key) {
            None => Ok(None),
            Some(Value::Array(items)) => Ok(Some(items)),
            Some(_) => Err(self.error(key, "must be an array or null")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn u64_decimal_strings_follow_parse_uint() {
        assert_eq!(parse_u64_decimal("0"), Some(0));
        assert_eq!(parse_u64_decimal("007"), Some(7));
        assert_eq!(parse_u64_decimal("18446744073709551615"), Some(u64::MAX));
        for bad in [
            "",
            "18446744073709551616",
            "+1",
            "-1",
            " 1",
            "1 ",
            "1.0",
            "1e3",
            "0x10",
            "١",
        ] {
            assert_eq!(parse_u64_decimal(bad), None, "{bad:?}");
        }
    }
}
