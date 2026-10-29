//! The canonical JSON form whose SHA-256 the device checks on every alert
//! snapshot (spec §3.1.8). The firmware implements it independently in
//! `device/internal/alerts/canonical.go`; both match CPython's
//! `json.dumps(v, sort_keys=True, separators=(",", ":"), ensure_ascii=False)`:
//!
//! - object keys sorted by code point, no whitespace;
//! - strings as UTF-8, escaping only `"`, `\` and control characters below
//!   0x20 (`\b \f \n \r \t` by name, the rest as `\u00xx`);
//! - integers as exact decimals;
//! - floats in Python's `repr` form: the shortest digits that round-trip,
//!   positional for decimal exponents −4 to 15, otherwise `d.ddde±XX` with at
//!   least two exponent digits; `-0.0` stays `-0.0`; NaN and infinity are
//!   refused.

use std::collections::BTreeMap;
use std::fmt;
use std::fmt::Write as _;

use serde_json::Value;

use crate::vocab::Sha256;

/// Why a value has no canonical form.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CanonicalError {
    /// A NaN or infinite number, including a literal too large for a double.
    NonFinite,
    /// Text that is not one JSON value, at this byte offset.
    Syntax {
        /// The byte offset.
        offset: usize,
        /// What the parser expected there.
        expected: &'static str,
    },
    /// A `\u` escape of an unpaired UTF-16 surrogate, at this byte offset.
    LoneSurrogate {
        /// The byte offset.
        offset: usize,
    },
    /// Arrays and objects nested deeper than [`MAX_DEPTH`].
    TooDeep,
}

impl fmt::Display for CanonicalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CanonicalError::NonFinite => f.write_str("NaN and infinity have no canonical form"),
            CanonicalError::Syntax { offset, expected } => {
                write!(f, "expected {expected} at byte {offset}")
            }
            CanonicalError::LoneSurrogate { offset } => {
                write!(f, "unpaired surrogate escape at byte {offset}")
            }
            CanonicalError::TooDeep => write!(f, "nesting deeper than {MAX_DEPTH}"),
        }
    }
}

impl std::error::Error for CanonicalError {}

/// The deepest nesting [`canonical_json_text`] accepts.
pub const MAX_DEPTH: usize = 128;

/// The canonical form of a value built in process. Its integers are exact
/// whatever their size; a JSON text parsed into a `Value` first may already
/// have lost an integer above `u64::MAX`, so re-encode text with
/// [`canonical_json_text`].
pub fn canonical_json(value: &Value) -> Result<String, CanonicalError> {
    let mut out = String::new();
    write_value(&mut out, value)?;
    Ok(out)
}

/// The SHA-256 of a value's canonical form.
pub fn canonical_sha256(value: &Value) -> Result<Sha256, CanonicalError> {
    Ok(Sha256::of(canonical_json(value)?.as_bytes()))
}

/// Re-encodes one JSON text in canonical form, as the firmware does with
/// what it receives: integers keep every digit, floats are read as doubles,
/// and a duplicate key keeps its last value. Trailing data is refused.
pub fn canonical_json_text(text: &str) -> Result<String, CanonicalError> {
    let mut parser = Parser {
        text,
        pos: 0,
        depth: 0,
    };
    let node = parser.value()?;
    parser.whitespace();
    if parser.pos != text.len() {
        return Err(CanonicalError::Syntax {
            offset: parser.pos,
            expected: "end of input",
        });
    }
    let mut out = String::with_capacity(text.len());
    write_node(&mut out, &node);
    Ok(out)
}

fn write_value(out: &mut String, value: &Value) -> Result<(), CanonicalError> {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => {
            if n.is_f64() {
                write_float(out, n.as_f64().ok_or(CanonicalError::NonFinite)?)?;
            } else {
                write!(out, "{n}").expect("writing to a String");
            }
        }
        Value::String(s) => write_string(out, s),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(out, item)?;
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
            out.push('{');
            for (i, (key, item)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(out, key);
                out.push(':');
                write_value(out, item)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

/// Writes `s` escaped as Python's encoder does with `ensure_ascii=False`.
fn write_string(out: &mut String, s: &str) {
    out.push('"');
    let mut plain = 0;
    for (i, b) in s.bytes().enumerate() {
        let escape = match b {
            b'"' => "\\\"",
            b'\\' => "\\\\",
            b'\n' => "\\n",
            b'\r' => "\\r",
            b'\t' => "\\t",
            0x08 => "\\b",
            0x0c => "\\f",
            0x00..=0x1f => "",
            _ => continue,
        };
        out.push_str(&s[plain..i]);
        if escape.is_empty() {
            write!(out, "\\u{b:04x}").expect("writing to a String");
        } else {
            out.push_str(escape);
        }
        plain = i + 1;
    }
    out.push_str(&s[plain..]);
    out.push('"');
}

/// Writes `f` in Python's `repr` form.
fn write_float(out: &mut String, f: f64) -> Result<(), CanonicalError> {
    if !f.is_finite() {
        return Err(CanonicalError::NonFinite);
    }
    if f == 0.0 {
        out.push_str(if f.is_sign_negative() { "-0.0" } else { "0.0" });
        return Ok(());
    }
    if f < 0.0 {
        out.push('-');
    }
    // Rust's `{:e}` writes the shortest digits that round-trip: "1.234e-5".
    let scientific = format!("{:e}", f.abs());
    let (mantissa, exponent) = scientific.split_once('e').expect("exponent form");
    let exponent: i32 = exponent.parse().expect("decimal exponent");
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    if (-4..16).contains(&exponent) {
        if exponent < 0 {
            out.push_str("0.");
            out.extend(std::iter::repeat_n('0', (-exponent - 1) as usize));
            out.push_str(&digits);
        } else {
            let point = exponent as usize + 1;
            if digits.len() <= point {
                out.push_str(&digits);
                out.extend(std::iter::repeat_n('0', point - digits.len()));
                out.push_str(".0");
            } else {
                out.push_str(&digits[..point]);
                out.push('.');
                out.push_str(&digits[point..]);
            }
        }
    } else {
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        let sign = if exponent < 0 { '-' } else { '+' };
        write!(out, "e{sign}{:02}", exponent.unsigned_abs()).expect("writing to a String");
    }
    Ok(())
}

/// A parsed JSON value that keeps integers as their decimal digits.
enum Node {
    Null,
    Bool(bool),
    Integer(String),
    Float(f64),
    Text(String),
    Array(Vec<Node>),
    Object(BTreeMap<String, Node>),
}

fn write_node(out: &mut String, node: &Node) {
    match node {
        Node::Null => out.push_str("null"),
        Node::Bool(true) => out.push_str("true"),
        Node::Bool(false) => out.push_str("false"),
        Node::Integer(digits) => out.push_str(digits),
        Node::Float(f) => write_float(out, *f).expect("finite by construction"),
        Node::Text(s) => write_string(out, s),
        Node::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_node(out, item);
            }
            out.push(']');
        }
        Node::Object(map) => {
            out.push('{');
            for (i, (key, item)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(out, key);
                out.push(':');
                write_node(out, item);
            }
            out.push('}');
        }
    }
}

/// A strict RFC 8259 parser over one text.
struct Parser<'a> {
    text: &'a str,
    pos: usize,
    depth: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.text.as_bytes().get(self.pos).copied()
    }

    fn syntax(&self, expected: &'static str) -> CanonicalError {
        CanonicalError::Syntax {
            offset: self.pos,
            expected,
        }
    }

    fn whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn literal(&mut self, word: &'static str, node: Node) -> Result<Node, CanonicalError> {
        if self.text[self.pos..].starts_with(word) {
            self.pos += word.len();
            Ok(node)
        } else {
            Err(self.syntax(word))
        }
    }

    fn value(&mut self) -> Result<Node, CanonicalError> {
        self.whitespace();
        match self.peek() {
            Some(b'n') => self.literal("null", Node::Null),
            Some(b't') => self.literal("true", Node::Bool(true)),
            Some(b'f') => self.literal("false", Node::Bool(false)),
            Some(b'"') => self.string().map(Node::Text),
            Some(b'[') => self.nested(Self::array),
            Some(b'{') => self.nested(Self::object),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => Err(self.syntax("a JSON value")),
        }
    }

    fn nested(
        &mut self,
        parse: fn(&mut Self) -> Result<Node, CanonicalError>,
    ) -> Result<Node, CanonicalError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(CanonicalError::TooDeep);
        }
        let node = parse(self)?;
        self.depth -= 1;
        Ok(node)
    }

    fn array(&mut self) -> Result<Node, CanonicalError> {
        self.pos += 1;
        let mut items = Vec::new();
        self.whitespace();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Node::Array(items));
        }
        loop {
            items.push(self.value()?);
            self.whitespace();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Node::Array(items));
                }
                _ => return Err(self.syntax("',' or ']'")),
            }
        }
    }

    fn object(&mut self) -> Result<Node, CanonicalError> {
        self.pos += 1;
        let mut map = BTreeMap::new();
        self.whitespace();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Node::Object(map));
        }
        loop {
            self.whitespace();
            if self.peek() != Some(b'"') {
                return Err(self.syntax("an object key"));
            }
            let key = self.string()?;
            self.whitespace();
            if self.peek() != Some(b':') {
                return Err(self.syntax("':'"));
            }
            self.pos += 1;
            let item = self.value()?;
            map.insert(key, item);
            self.whitespace();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Node::Object(map));
                }
                _ => return Err(self.syntax("',' or '}'")),
            }
        }
    }

    fn digits(&mut self) -> usize {
        let start = self.pos;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        self.pos - start
    }

    fn number(&mut self) -> Result<Node, CanonicalError> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => {
                self.digits();
            }
            _ => return Err(self.syntax("a digit")),
        }
        let mut float = false;
        if self.peek() == Some(b'.') {
            self.pos += 1;
            if self.digits() == 0 {
                return Err(self.syntax("a fraction digit"));
            }
            float = true;
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if self.digits() == 0 {
                return Err(self.syntax("an exponent digit"));
            }
            float = true;
        }
        let literal = &self.text[start..self.pos];
        if !float {
            let digits = if literal == "-0" { "0" } else { literal };
            return Ok(Node::Integer(digits.to_owned()));
        }
        let value: f64 = literal.parse().map_err(|_| CanonicalError::Syntax {
            offset: start,
            expected: "a number",
        })?;
        if !value.is_finite() {
            return Err(CanonicalError::NonFinite);
        }
        Ok(Node::Float(value))
    }

    fn hex4(&mut self) -> Result<u32, CanonicalError> {
        let hex = self
            .text
            .get(self.pos..self.pos + 4)
            .filter(|h| h.bytes().all(|b| b.is_ascii_hexdigit()));
        let hex = hex.ok_or_else(|| self.syntax("four hex digits"))?;
        self.pos += 4;
        Ok(u32::from_str_radix(hex, 16).expect("hex digits"))
    }

    fn string(&mut self) -> Result<String, CanonicalError> {
        self.pos += 1;
        let mut out = String::new();
        loop {
            let rest = &self.text[self.pos..];
            let stop = rest
                .bytes()
                .position(|b| b == b'"' || b == b'\\' || b < 0x20);
            let Some(stop) = stop else {
                return Err(CanonicalError::Syntax {
                    offset: self.text.len(),
                    expected: "'\"'",
                });
            };
            out.push_str(&rest[..stop]);
            self.pos += stop;
            match self.peek() {
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    let at = self.pos;
                    self.pos += 1;
                    let escape = self.peek().ok_or_else(|| self.syntax("an escape"))?;
                    self.pos += 1;
                    let c = match escape {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => {
                            let unit = self.hex4()?;
                            let scalar = match unit {
                                0xd800..=0xdbff => {
                                    if !self.text[self.pos..].starts_with("\\u") {
                                        return Err(CanonicalError::LoneSurrogate { offset: at });
                                    }
                                    self.pos += 2;
                                    let low = self.hex4()?;
                                    if !(0xdc00..=0xdfff).contains(&low) {
                                        return Err(CanonicalError::LoneSurrogate { offset: at });
                                    }
                                    0x10000 + ((unit - 0xd800) << 10) + (low - 0xdc00)
                                }
                                0xdc00..=0xdfff => {
                                    return Err(CanonicalError::LoneSurrogate { offset: at });
                                }
                                _ => unit,
                            };
                            char::from_u32(scalar).expect("a Unicode scalar value")
                        }
                        _ => {
                            self.pos = at;
                            return Err(self.syntax("a valid escape"));
                        }
                    };
                    out.push(c);
                }
                _ => return Err(self.syntax("no raw control character in a string")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// `TestCanonicalJSONMatchesPython` (device/internal/alerts/canonical_test.go).
    #[test]
    fn the_firmware_canonical_vector_matches() {
        let input = "{ \"z\": 1, \"a\": \"é\\u2028\\\"\\\\\\n\\u0001\",\n\t  \"m\": [1.5, 1e20, 1e-5, 0.1, -0.0, 100000000000000000000000, true, null],\n\t  \"b\": {\"y\": 2, \"x\": \"日本\"} }";
        let want = "{\"a\":\"é\u{2028}\\\"\\\\\\n\\u0001\",\"b\":{\"x\":\"日本\",\"y\":2},\"m\":[1.5,1e+20,1e-05,0.1,-0.0,100000000000000000000000,true,null],\"z\":1}";
        assert_eq!(canonical_json_text(input).unwrap(), want);
        assert!(
            canonical_json_text("{\"a\":1} {\"b\":2}").is_err(),
            "trailing value"
        );
    }

    /// `TestPythonFloatRepr`.
    #[test]
    fn the_firmware_float_vectors_match() {
        let vectors = [
            ("1.0", "1.0"),
            ("0.75", "0.75"),
            ("1e16", "1e+16"),
            ("123456789012345.6", "123456789012345.6"),
            ("0.0001", "0.0001"),
            ("0.00001234", "1.234e-05"),
            ("2.5e-300", "2.5e-300"),
            ("-3.0", "-3.0"),
        ];
        for (input, want) in vectors {
            assert_eq!(canonical_json_text(input).unwrap(), want, "{input}");
        }
    }

    /// Values checked against CPython 3.14's `json.dumps` with the canonical
    /// options; they cover the positional and exponent boundaries.
    #[test]
    fn floats_match_python_repr_at_the_boundaries() {
        let vectors = [
            (1e15, "1000000000000000.0"),
            (9999999999999998.0, "9999999999999998.0"),
            (1e16, "1e+16"),
            (1.5e16, "1.5e+16"),
            (0.0001, "0.0001"),
            (0.00009999, "9.999e-05"),
            (1e100, "1e+100"),
            (-1.25e-7, "-1.25e-07"),
            (5e-324, "5e-324"),
            (1.7976931348623157e308, "1.7976931348623157e+308"),
            (0.1 + 0.2, "0.30000000000000004"),
            (100.0, "100.0"),
            (123.456, "123.456"),
            (-0.0, "-0.0"),
            (0.0, "0.0"),
        ];
        for (value, want) in vectors {
            assert_eq!(canonical_json(&json!(value)).unwrap(), want, "{value:e}");
        }
    }

    #[test]
    fn non_finite_numbers_have_no_canonical_form() {
        let mut out = String::new();
        for f in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(write_float(&mut out, f), Err(CanonicalError::NonFinite));
        }
        assert_eq!(canonical_json_text("1e400"), Err(CanonicalError::NonFinite));
        assert_eq!(
            canonical_json_text("[-1e999]"),
            Err(CanonicalError::NonFinite)
        );
        assert_eq!(
            canonical_json_text("1e-400").unwrap(),
            "0.0",
            "underflow reads as zero, as in Python and Go"
        );
    }

    #[test]
    fn values_built_in_process_and_their_wire_text_agree() {
        let value = json!({"volume": 0.5, "label": "Wake\tup \"now\"", "revision": 18_446_744_073_709_551_615u64,
                           "due": "1790000000000", "neg": -42, "nested": {"b": [], "a": {}}, "big": 1e20});
        let direct = canonical_json(&value).unwrap();
        assert_eq!(
            direct,
            "{\"big\":1e+20,\"due\":\"1790000000000\",\"label\":\"Wake\\tup \\\"now\\\"\",\"neg\":-42,\"nested\":{\"a\":{},\"b\":[]},\"revision\":18446744073709551615,\"volume\":0.5}"
        );
        assert_eq!(
            canonical_json_text(&serde_json::to_string_pretty(&value).unwrap()).unwrap(),
            direct
        );
    }

    #[test]
    fn keys_sort_by_code_point_and_duplicates_keep_the_last() {
        assert_eq!(
            canonical_json_text("{\"b\":1,\"a\":2,\"b\":3}").unwrap(),
            "{\"a\":2,\"b\":3}"
        );
        assert_eq!(
            canonical_json_text("{\"é\":1,\"z\":2,\"Z\":3,\"\u{1f600}\":4}").unwrap(),
            "{\"Z\":3,\"z\":2,\"é\":1,\"\u{1f600}\":4}"
        );
    }

    #[test]
    fn escapes_follow_python_exactly() {
        let all_controls: String = (0u8..0x20).map(char::from).collect();
        let want = "\"\\u0000\\u0001\\u0002\\u0003\\u0004\\u0005\\u0006\\u0007\\b\\t\\n\\u000b\\f\\r\\u000e\\u000f\\u0010\\u0011\\u0012\\u0013\\u0014\\u0015\\u0016\\u0017\\u0018\\u0019\\u001a\\u001b\\u001c\\u001d\\u001e\\u001f\"";
        assert_eq!(canonical_json(&json!(all_controls)).unwrap(), want);
        assert_eq!(
            canonical_json(&json!("\u{7f}/\u{e9}")).unwrap(),
            "\"\u{7f}/\u{e9}\"",
            "DEL, slash and non-ASCII stay raw"
        );
        assert_eq!(
            canonical_json_text("\"\\ud83d\\ude00 \\/\"").unwrap(),
            "\"\u{1f600} /\""
        );
    }

    #[test]
    fn integers_keep_every_digit_and_minus_zero_is_zero() {
        assert_eq!(canonical_json_text("-0").unwrap(), "0");
        assert_eq!(
            canonical_json_text("-123456789012345678901234567890").unwrap(),
            "-123456789012345678901234567890"
        );
        assert_eq!(canonical_json_text("-0.0").unwrap(), "-0.0");
        assert_eq!(canonical_json_text("1E2").unwrap(), "100.0");
    }

    #[test]
    fn invalid_text_is_refused() {
        for text in [
            "",
            "01",
            "1.",
            ".5",
            "1e",
            "+1",
            "[1,]",
            "{\"a\"}",
            "{a:1}",
            "\"\\x\"",
            "\"tab\there\"",
            "nul",
            "\"open",
            "NaN",
            "Infinity",
        ] {
            assert!(
                matches!(
                    canonical_json_text(text),
                    Err(CanonicalError::Syntax { .. })
                ),
                "{text:?}"
            );
        }
        assert!(matches!(
            canonical_json_text("\"\\ud83d\""),
            Err(CanonicalError::LoneSurrogate { .. })
        ));
        assert!(matches!(
            canonical_json_text("\"\\ude00\""),
            Err(CanonicalError::LoneSurrogate { .. })
        ));
        assert!(matches!(
            canonical_json_text("\"\\ud83d\\u0041\""),
            Err(CanonicalError::LoneSurrogate { .. })
        ));
        let deep = format!("{}{}", "[".repeat(MAX_DEPTH + 1), "]".repeat(MAX_DEPTH + 1));
        assert_eq!(canonical_json_text(&deep), Err(CanonicalError::TooDeep));
        let ok = format!("{}{}", "[".repeat(MAX_DEPTH), "]".repeat(MAX_DEPTH));
        assert!(canonical_json_text(&ok).is_ok());
    }
}
