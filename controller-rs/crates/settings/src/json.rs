//! JSON that keeps the order of object keys, and writers that reproduce the text the
//! stored documents already use.
//!
//! The settings documents in the database were written by Python's `json.dumps` with its
//! default options: `", "` and `": "` separators, every character outside printable ASCII
//! escaped as `\uXXXX`, and floats in Python's shortest `repr` form. The wake-model
//! registry was written the same way but compact (`","` and `":"`), with keys sorted and
//! a final newline. Writing the same forms means a document read and written back without
//! a change is byte for byte what was stored, and the previous controller still reads
//! everything this one writes (§3.5.1).
//!
//! `serde_json`'s own `Value` sorts object keys unless a crate-wide feature is enabled,
//! which would change other crates' JSON as well, so this module has its own value type.

use std::fmt::{self, Write as _};

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::ser::{Serialize, SerializeMap, SerializeSeq, Serializer};

/// A JSON value whose objects keep their keys in document order.
#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    /// `null`.
    Null,
    /// `true` or `false`.
    Bool(bool),
    /// A number exactly as parsed: an integer stays an integer.
    Number(serde_json::Number),
    /// A string.
    String(String),
    /// An array.
    Array(Vec<Json>),
    /// An object, keys in document order.
    Object(JsonObject),
}

/// A JSON object whose keys keep their document order. A key appears once: when a
/// document repeats a key, the later value replaces the earlier one in the earlier
/// position, as Python's `json.loads` does.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct JsonObject {
    entries: Vec<(String, Json)>,
}

impl JsonObject {
    /// An empty object.
    pub fn new() -> Self {
        Self::default()
    }

    /// The value under `key`.
    pub fn get(&self, key: &str) -> Option<&Json> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// Set `key`: in place when it exists, otherwise at the end.
    pub fn insert(&mut self, key: impl Into<String>, value: Json) {
        let key = key.into();
        match self.entries.iter_mut().find(|(k, _)| *k == key) {
            Some((_, slot)) => *slot = value,
            None => self.entries.push((key, value)),
        }
    }

    /// Remove `key`, returning its value.
    pub fn remove(&mut self, key: &str) -> Option<Json> {
        let index = self.entries.iter().position(|(k, _)| k == key)?;
        Some(self.entries.remove(index).1)
    }

    /// The entries in document order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Json)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// How many keys the object has.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the object has no keys.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl FromIterator<(String, Json)> for JsonObject {
    fn from_iter<I: IntoIterator<Item = (String, Json)>>(iter: I) -> Self {
        let mut object = Self::new();
        for (key, value) in iter {
            object.insert(key, value);
        }
        object
    }
}

impl Json {
    /// Parse JSON text, keeping object key order.
    pub fn parse(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }

    /// A number from a float; `None` when it is not finite.
    pub fn float(value: f64) -> Option<Self> {
        serde_json::Number::from_f64(value).map(Self::Number)
    }

    /// A number from an integer.
    pub fn int(value: i64) -> Self {
        Self::Number(value.into())
    }

    /// A number that is written as an integer when it is whole, the way a browser's
    /// `JSON.stringify` sends it; `None` when it is not finite.
    pub fn number(value: f64) -> Option<Self> {
        const EXACT: f64 = 9_007_199_254_740_992.0; // 2^53
        if value.fract() == 0.0 && value.abs() < EXACT {
            // Whole and inside the exactly representable range, so the cast is exact.
            Some(Self::int(value as i64))
        } else {
            Self::float(value)
        }
    }

    /// The value as a number, when it is one.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Number(n) => n.as_f64(),
            _ => None,
        }
    }

    /// The value as a whole number, when it is one: an integer, or a float with no
    /// fractional part (a browser may send `700` as `700.0`).
    pub fn as_whole(&self) -> Option<i64> {
        match self {
            Self::Number(n) => n.as_i64().or_else(|| {
                let f = n.as_f64()?;
                (f.fract() == 0.0 && f.abs() < 9_007_199_254_740_992.0).then_some(f as i64)
            }),
            _ => None,
        }
    }

    /// The text Python's `json.dumps(value)` writes: `", "` and `": "` separators, keys
    /// in their order, non-ASCII escaped.
    pub fn to_python_text(&self) -> String {
        let mut out = String::new();
        write_value(&mut out, self, Style::Spaced, false);
        out
    }

    /// The text of `json.dumps(value, sort_keys=True, separators=(",", ":"))`.
    pub fn to_compact_sorted_text(&self) -> String {
        let mut out = String::new();
        write_value(&mut out, self, Style::Compact, true);
        out
    }

    /// The equivalent `serde_json` value (its objects may reorder keys).
    pub fn to_value(&self) -> serde_json::Value {
        match self {
            Self::Null => serde_json::Value::Null,
            Self::Bool(b) => serde_json::Value::Bool(*b),
            Self::Number(n) => serde_json::Value::Number(n.clone()),
            Self::String(s) => serde_json::Value::String(s.clone()),
            Self::Array(items) => {
                serde_json::Value::Array(items.iter().map(Self::to_value).collect())
            }
            Self::Object(o) => serde_json::Value::Object(
                o.iter()
                    .map(|(k, v)| (k.to_owned(), v.to_value()))
                    .collect(),
            ),
        }
    }

    /// The same value from a `serde_json` one.
    pub fn from_value(value: &serde_json::Value) -> Self {
        match value {
            serde_json::Value::Null => Self::Null,
            serde_json::Value::Bool(b) => Self::Bool(*b),
            serde_json::Value::Number(n) => Self::Number(n.clone()),
            serde_json::Value::String(s) => Self::String(s.clone()),
            serde_json::Value::Array(items) => {
                Self::Array(items.iter().map(Self::from_value).collect())
            }
            serde_json::Value::Object(o) => Self::Object(
                o.iter()
                    .map(|(k, v)| (k.clone(), Self::from_value(v)))
                    .collect(),
            ),
        }
    }
}

#[derive(Clone, Copy)]
enum Style {
    Spaced,
    Compact,
}

impl Style {
    fn item(self) -> &'static str {
        match self {
            Self::Spaced => ", ",
            Self::Compact => ",",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::Spaced => ": ",
            Self::Compact => ":",
        }
    }
}

fn write_value(out: &mut String, value: &Json, style: Style, sort_keys: bool) {
    match value {
        Json::Null => out.push_str("null"),
        Json::Bool(true) => out.push_str("true"),
        Json::Bool(false) => out.push_str("false"),
        Json::Number(n) => write_number(out, n),
        Json::String(s) => write_string(out, s),
        Json::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(style.item());
                }
                write_value(out, item, style, sort_keys);
            }
            out.push(']');
        }
        Json::Object(object) => {
            let mut entries: Vec<(&str, &Json)> = object.iter().collect();
            if sort_keys {
                // Python sorts by code point; for UTF-8 strings that is byte order.
                entries.sort_by(|a, b| a.0.cmp(b.0));
            }
            out.push('{');
            for (i, (key, item)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push_str(style.item());
                }
                write_string(out, key);
                out.push_str(style.key());
                write_value(out, item, style, sort_keys);
            }
            out.push('}');
        }
    }
}

fn write_number(out: &mut String, number: &serde_json::Number) {
    if number.is_i64() || number.is_u64() {
        let _ = write!(out, "{number}");
    } else if let Some(f) = number.as_f64() {
        out.push_str(&python_float_repr(f));
    }
}

/// Python's `repr` of a finite float: the shortest digits that read back as the same
/// value, in positional form for decimal exponents from -4 to 16 and in exponent form
/// (`1e-05`, `1.5e+17`) outside them; a whole number keeps a `.0`.
pub(crate) fn python_float_repr(value: f64) -> String {
    if value == 0.0 {
        return if value.is_sign_negative() {
            "-0.0"
        } else {
            "0.0"
        }
        .to_owned();
    }
    // Rust's `{:e}` writes the same shortest round-trip digits as `d.ddde<exp>`.
    let scientific = format!("{:e}", value.abs());
    let (mantissa, exponent) = scientific
        .split_once('e')
        .expect("`{:e}` always writes an exponent");
    let exponent: i32 = exponent.parse().expect("`{:e}` writes a decimal exponent");
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    // Python's decimal point position: the number is 0.<digits> × 10^point.
    let point = exponent + 1;
    let mut out = String::new();
    if value.is_sign_negative() {
        out.push('-');
    }
    if !(-4 < point && point <= 16) {
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        let sign = if exponent < 0 { '-' } else { '+' };
        let _ = write!(out, "e{sign}{:02}", exponent.abs());
    } else if point <= 0 {
        out.push_str("0.");
        out.extend(std::iter::repeat_n('0', point.unsigned_abs() as usize));
        out.push_str(&digits);
    } else {
        let point = point as usize;
        if point >= digits.len() {
            out.push_str(&digits);
            out.extend(std::iter::repeat_n('0', point - digits.len()));
            out.push_str(".0");
        } else {
            out.push_str(&digits[..point]);
            out.push('.');
            out.push_str(&digits[point..]);
        }
    }
    out
}

/// A JSON string as Python writes it with `ensure_ascii`: every character outside
/// printable ASCII as `\uXXXX` (astral characters as a surrogate pair), with the short
/// escapes for quote, backslash, newline, carriage return, tab, backspace and form feed.
fn write_string(out: &mut String, text: &str) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ' '..='~' => out.push(c),
            _ => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    let _ = write!(out, "\\u{unit:04x}");
                }
            }
        }
    }
    out.push('"');
}

impl<'de> Deserialize<'de> for Json {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(JsonVisitor)
    }
}

struct JsonVisitor;

impl<'de> Visitor<'de> for JsonVisitor {
    type Value = Json;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any JSON value")
    }

    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Json, E> {
        Ok(Json::Bool(v))
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Json, E> {
        Ok(Json::Number(v.into()))
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Json, E> {
        Ok(Json::Number(v.into()))
    }

    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Json, E> {
        serde_json::Number::from_f64(v)
            .map(Json::Number)
            .ok_or_else(|| E::custom("a number that is not finite"))
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Json, E> {
        Ok(Json::String(v.to_owned()))
    }

    fn visit_string<E: de::Error>(self, v: String) -> Result<Json, E> {
        Ok(Json::String(v))
    }

    fn visit_unit<E: de::Error>(self) -> Result<Json, E> {
        Ok(Json::Null)
    }

    fn visit_none<E: de::Error>(self) -> Result<Json, E> {
        Ok(Json::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Json, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = seq.next_element()? {
            items.push(item);
        }
        Ok(Json::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Json, A::Error> {
        let mut object = JsonObject::new();
        while let Some((key, value)) = map.next_entry::<String, Json>()? {
            object.insert(key, value);
        }
        Ok(Json::Object(object))
    }
}

impl Serialize for Json {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Null => serializer.serialize_unit(),
            Self::Bool(b) => serializer.serialize_bool(*b),
            Self::Number(n) => n.serialize(serializer),
            Self::String(s) => serializer.serialize_str(s),
            Self::Array(items) => {
                let mut seq = serializer.serialize_seq(Some(items.len()))?;
                for item in items {
                    seq.serialize_element(item)?;
                }
                seq.end()
            }
            Self::Object(object) => {
                let mut map = serializer.serialize_map(Some(object.len()))?;
                for (key, value) in object.iter() {
                    map.serialize_entry(key, value)?;
                }
                map.end()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floats_follow_python_repr() {
        for (value, python) in [
            (0.17, "0.17"),
            (2.0, "2.0"),
            (-35.0, "-35.0"),
            (0.009283081628382206, "0.009283081628382206"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (3.3e-5, "3.3e-05"),
            (1e16, "1e+16"),
            (1234567890123456.0, "1234567890123456.0"),
            (1.5e17, "1.5e+17"),
            (0.0, "0.0"),
        ] {
            assert_eq!(python_float_repr(value), python, "{value}");
        }
    }

    #[test]
    fn python_text_round_trips_order_escapes_and_numbers() {
        let text = r#"{"b": 1, "a": [0.2, -35, true, null], "c": {"z": "caf\u00e9 \ud83d\ude00", "y": "q\"\\\n"}}"#;
        let value = Json::parse(text).unwrap();
        assert_eq!(value.to_python_text(), text);
    }

    #[test]
    fn compact_sorted_text() {
        let value = Json::parse(r#"{"b": {"y": 1, "x": 2.5}, "a": []}"#).unwrap();
        assert_eq!(
            value.to_compact_sorted_text(),
            r#"{"a":[],"b":{"x":2.5,"y":1}}"#
        );
    }

    #[test]
    fn repeated_key_keeps_first_position_and_last_value() {
        let value = Json::parse(r#"{"a": 1, "b": 2, "a": 3}"#).unwrap();
        assert_eq!(value.to_python_text(), r#"{"a": 3, "b": 2}"#);
    }

    #[test]
    fn whole_numbers_are_written_as_integers() {
        assert_eq!(Json::number(700.0).unwrap().to_python_text(), "700");
        assert_eq!(Json::number(0.5).unwrap().to_python_text(), "0.5");
        assert_eq!(Json::parse("700.0").unwrap().as_whole(), Some(700));
        assert_eq!(Json::parse("700.5").unwrap().as_whole(), None);
    }
}
