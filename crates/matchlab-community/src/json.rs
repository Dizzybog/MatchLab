//! Order-preserving JSON values with Python `json` compatible output.
//!
//! The Community Learning files are reviewed in bot PRs and stored as
//! GitHub Release assets. Their bytes must stay identical to what the
//! earlier Python tooling wrote, so key order is preserved on input and
//! `dumps` reproduces `json.dumps` separators, escaping and float repr.
use indexmap::IndexMap;
use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use std::fmt;

pub type Object = IndexMap<String, Json>;

#[derive(Clone, Debug)]
pub enum Json {
    Null,
    Bool(bool),
    Int(i128),
    Float(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Object),
}

impl Json {
    pub fn parse(text: &str) -> Result<Json, String> {
        serde_json::from_str(text).map_err(|error| error.to_string())
    }

    pub fn parse_bytes(raw: &[u8]) -> Result<Json, String> {
        let text = std::str::from_utf8(raw).map_err(|error| error.to_string())?;
        // Python's json.loads accepts a UTF-8 byte order mark on bytes.
        Json::parse(text.strip_prefix('\u{feff}').unwrap_or(text))
    }

    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(map) => map.get(key),
            _ => None,
        }
    }

    /// `dict.get(key)` that yields `None` for absent keys and JSON null.
    pub fn field(&self, key: &str) -> &Json {
        self.get(key).unwrap_or(&Json::Null)
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(text) => Some(text),
            _ => None,
        }
    }

    pub fn as_obj(&self) -> Option<&Object> {
        match self {
            Json::Obj(map) => Some(map),
            _ => None,
        }
    }

    pub fn as_arr(&self) -> Option<&Vec<Json>> {
        match self {
            Json::Arr(items) => Some(items),
            _ => None,
        }
    }

    /// Python `type(value) is int`: booleans are excluded.
    pub fn as_int(&self) -> Option<i128> {
        match self {
            Json::Int(value) => Some(*value),
            _ => None,
        }
    }

    /// Python numeric value, including `bool` as 0/1.
    pub fn as_number(&self) -> Option<f64> {
        match self {
            Json::Int(value) => Some(*value as f64),
            Json::Float(value) => Some(*value),
            Json::Bool(value) => Some(f64::from(u8::from(*value))),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Json::Null)
    }

    pub fn is_true(&self) -> bool {
        matches!(self, Json::Bool(true))
    }

    pub fn is_false(&self) -> bool {
        matches!(self, Json::Bool(false))
    }

    /// Python truthiness.
    pub fn truthy(&self) -> bool {
        match self {
            Json::Null => false,
            Json::Bool(value) => *value,
            Json::Int(value) => *value != 0,
            Json::Float(value) => *value != 0.0,
            Json::Str(text) => !text.is_empty(),
            Json::Arr(items) => !items.is_empty(),
            Json::Obj(map) => !map.is_empty(),
        }
    }

    /// `set(obj) == {keys}` for a JSON object.
    pub fn has_exact_keys(&self, keys: &[&str]) -> bool {
        match self {
            Json::Obj(map) => map.len() == keys.len() && keys.iter().all(|key| map.contains_key(*key)),
            _ => false,
        }
    }

    pub fn str_eq(&self, expected: &str) -> bool {
        self.as_str() == Some(expected)
    }

    pub fn from_strs<I: IntoIterator<Item = S>, S: Into<String>>(items: I) -> Json {
        Json::Arr(items.into_iter().map(|item| Json::Str(item.into())).collect())
    }
}

impl From<&str> for Json {
    fn from(value: &str) -> Self {
        Json::Str(value.to_owned())
    }
}

impl From<String> for Json {
    fn from(value: String) -> Self {
        Json::Str(value)
    }
}

impl From<bool> for Json {
    fn from(value: bool) -> Self {
        Json::Bool(value)
    }
}

impl From<i64> for Json {
    fn from(value: i64) -> Self {
        Json::Int(i128::from(value))
    }
}

impl From<usize> for Json {
    fn from(value: usize) -> Self {
        Json::Int(value as i128)
    }
}

impl From<Object> for Json {
    fn from(value: Object) -> Self {
        Json::Obj(value)
    }
}

impl From<Vec<Json>> for Json {
    fn from(value: Vec<Json>) -> Self {
        Json::Arr(value)
    }
}

impl From<&serde_json::Value> for Json {
    fn from(value: &serde_json::Value) -> Self {
        match value {
            serde_json::Value::Null => Json::Null,
            serde_json::Value::Bool(value) => Json::Bool(*value),
            serde_json::Value::Number(number) => {
                if let Some(value) = number.as_i64() {
                    Json::Int(i128::from(value))
                } else if let Some(value) = number.as_u64() {
                    Json::Int(i128::from(value))
                } else {
                    Json::Float(number.as_f64().unwrap_or(f64::NAN))
                }
            }
            serde_json::Value::String(text) => Json::Str(text.clone()),
            serde_json::Value::Array(items) => Json::Arr(items.iter().map(Json::from).collect()),
            serde_json::Value::Object(map) => {
                Json::Obj(map.iter().map(|(key, value)| (key.clone(), Json::from(value))).collect())
            }
        }
    }
}

/// Python equality: dictionaries ignore order, `1 == 1.0 == True`.
impl PartialEq for Json {
    fn eq(&self, other: &Json) -> bool {
        match (self, other) {
            (Json::Null, Json::Null) => true,
            (Json::Str(left), Json::Str(right)) => left == right,
            (Json::Arr(left), Json::Arr(right)) => left == right,
            (Json::Obj(left), Json::Obj(right)) => left == right,
            (Json::Bool(left), Json::Bool(right)) => left == right,
            (Json::Int(left), Json::Int(right)) => left == right,
            (left, right) => match (left.as_number(), right.as_number()) {
                (Some(left), Some(right)) => left == right,
                _ => false,
            },
        }
    }
}

#[macro_export]
macro_rules! obj {
    ($($key:expr => $value:expr),* $(,)?) => {{
        #[allow(unused_mut)]
        let mut map = $crate::json::Object::new();
        $(map.insert(String::from($key), $crate::json::Json::from($value));)*
        $crate::json::Json::Obj(map)
    }};
}

impl<'de> Deserialize<'de> for Json {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Json, D::Error> {
        struct JsonVisitor;
        impl<'de> Visitor<'de> for JsonVisitor {
            type Value = Json;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("any JSON value")
            }
            fn visit_unit<E>(self) -> Result<Json, E> {
                Ok(Json::Null)
            }
            fn visit_none<E>(self) -> Result<Json, E> {
                Ok(Json::Null)
            }
            fn visit_bool<E>(self, value: bool) -> Result<Json, E> {
                Ok(Json::Bool(value))
            }
            fn visit_i64<E>(self, value: i64) -> Result<Json, E> {
                Ok(Json::Int(i128::from(value)))
            }
            fn visit_u64<E>(self, value: u64) -> Result<Json, E> {
                Ok(Json::Int(i128::from(value)))
            }
            fn visit_f64<E>(self, value: f64) -> Result<Json, E> {
                Ok(Json::Float(value))
            }
            fn visit_str<E: de::Error>(self, value: &str) -> Result<Json, E> {
                Ok(Json::Str(value.to_owned()))
            }
            fn visit_string<E>(self, value: String) -> Result<Json, E> {
                Ok(Json::Str(value))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Json, A::Error> {
                let mut items = Vec::new();
                while let Some(item) = seq.next_element()? {
                    items.push(item);
                }
                Ok(Json::Arr(items))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Json, A::Error> {
                let mut map = Object::new();
                while let Some((key, value)) = access.next_entry::<String, Json>()? {
                    // Python keeps the first position and the last value.
                    map.insert(key, value);
                }
                Ok(Json::Obj(map))
            }
        }
        deserializer.deserialize_any(JsonVisitor)
    }
}

/// Python `json.dumps` options.
#[derive(Clone, Copy)]
pub struct Dump {
    pub indent: Option<usize>,
    pub sort_keys: bool,
    pub ensure_ascii: bool,
    pub compact: bool,
}

impl Dump {
    /// `json.dumps(value)`.
    pub const DEFAULT: Dump = Dump { indent: None, sort_keys: false, ensure_ascii: true, compact: false };
    /// `json.dumps(value, ensure_ascii=False, indent=2)`.
    pub const PRETTY_UTF8: Dump = Dump { indent: Some(2), sort_keys: false, ensure_ascii: false, compact: false };
    /// `json.dumps(value, indent=2)`.
    pub const PRETTY: Dump = Dump { indent: Some(2), sort_keys: false, ensure_ascii: true, compact: false };
    /// `json.dumps(value, ensure_ascii=False, separators=(",", ":"))`.
    pub const COMPACT_UTF8: Dump = Dump { indent: None, sort_keys: false, ensure_ascii: false, compact: true };

    pub fn sorted(mut self) -> Dump {
        self.sort_keys = true;
        self
    }

    pub fn utf8(mut self) -> Dump {
        self.ensure_ascii = false;
        self
    }

    pub fn compact(mut self) -> Dump {
        self.compact = true;
        self
    }
}

pub fn dumps(value: &Json, options: Dump) -> String {
    let mut out = String::new();
    write_value(&mut out, value, options, 0);
    out
}

fn separators(options: Dump) -> (&'static str, &'static str) {
    if options.compact {
        (",", ":")
    } else if options.indent.is_some() {
        (",", ": ")
    } else {
        (", ", ": ")
    }
}

fn newline(out: &mut String, options: Dump, level: usize) {
    if let Some(width) = options.indent {
        out.push('\n');
        out.extend(std::iter::repeat_n(' ', width * level));
    }
}

fn write_value(out: &mut String, value: &Json, options: Dump, level: usize) {
    let (item_sep, key_sep) = separators(options);
    match value {
        Json::Null => out.push_str("null"),
        Json::Bool(true) => out.push_str("true"),
        Json::Bool(false) => out.push_str("false"),
        Json::Int(number) => out.push_str(&number.to_string()),
        Json::Float(number) => out.push_str(&float_repr(*number)),
        Json::Str(text) => write_string(out, text, options.ensure_ascii),
        Json::Arr(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push_str(item_sep);
                }
                newline(out, options, level + 1);
                write_value(out, item, options, level + 1);
            }
            newline(out, options, level);
            out.push(']');
        }
        Json::Obj(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            let mut entries: Vec<(&String, &Json)> = map.iter().collect();
            if options.sort_keys {
                entries.sort_by(|left, right| left.0.cmp(right.0));
            }
            out.push('{');
            for (index, (key, item)) in entries.into_iter().enumerate() {
                if index > 0 {
                    out.push_str(item_sep);
                }
                newline(out, options, level + 1);
                write_string(out, key, options.ensure_ascii);
                out.push_str(key_sep);
                write_value(out, item, options, level + 1);
            }
            newline(out, options, level);
            out.push('}');
        }
    }
}

fn write_string(out: &mut String, text: &str, ensure_ascii: bool) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ch if (ch as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", ch as u32)),
            ch if ensure_ascii && !(' '..='~').contains(&ch) => {
                let mut units = [0u16; 2];
                for unit in ch.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
            ch => out.push(ch),
        }
    }
    out.push('"');
}

/// Python `float.__repr__` (shortest round-trip digits).
pub fn float_repr(value: f64) -> String {
    if value.is_nan() {
        return "NaN".into();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    if value == 0.0 {
        return if value.is_sign_negative() { "-0.0".into() } else { "0.0".into() };
    }
    let scientific = format!("{value:e}");
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((&scientific, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let negative = mantissa.starts_with('-');
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let sign = if negative { "-" } else { "" };
    if (-4..16).contains(&exponent) {
        let point = exponent + 1;
        let body = if point <= 0 {
            format!("0.{}{}", "0".repeat((-point) as usize), digits)
        } else if point as usize >= digits.len() {
            format!("{}{}.0", digits, "0".repeat(point as usize - digits.len()))
        } else {
            let (whole, fraction) = digits.split_at(point as usize);
            format!("{whole}.{fraction}")
        };
        format!("{sign}{body}")
    } else {
        let (first, rest) = digits.split_at(1);
        let fraction = if rest.is_empty() { String::new() } else { format!(".{rest}") };
        let exponent_sign = if exponent < 0 { '-' } else { '+' };
        format!("{sign}{first}{fraction}e{exponent_sign}{:02}", exponent.abs())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_float_repr() {
        for (value, text) in [
            (1.0, "1.0"),
            (0.5, "0.5"),
            (1e-5, "1e-05"),
            (1.5e-5, "1.5e-05"),
            (0.0001, "0.0001"),
            (1e16, "1e+16"),
            (123456789012345.6, "123456789012345.6"),
            (0.8333, "0.8333"),
            (-2.25, "-2.25"),
            (1e22, "1e+22"),
        ] {
            assert_eq!(float_repr(value), text);
        }
    }

    #[test]
    fn python_dumps_layout() {
        let value = Json::parse(r#"{"b":[1,{"x":"é\u0001"}],"a":{},"c":[]}"#).unwrap();
        assert_eq!(dumps(&value, Dump::DEFAULT), r#"{"b": [1, {"x": "\u00e9\u0001"}], "a": {}, "c": []}"#);
        assert_eq!(
            dumps(&value, Dump::PRETTY_UTF8),
            "{\n  \"b\": [\n    1,\n    {\n      \"x\": \"é\\u0001\"\n    }\n  ],\n  \"a\": {},\n  \"c\": []\n}"
        );
        assert_eq!(dumps(&value, Dump::COMPACT_UTF8.sorted()), r#"{"a":{},"b":[1,{"x":"é\u0001"}],"c":[]}"#);
        assert_eq!(dumps(&Json::from("😀\u{7f}"), Dump::DEFAULT), r#""\ud83d\ude00\u007f""#);
    }

    #[test]
    fn python_equality() {
        assert_eq!(Json::parse(r#"{"a":1,"b":2}"#).unwrap(), Json::parse(r#"{"b":2.0,"a":true}"#).unwrap());
        assert_ne!(Json::parse("[1,2]").unwrap(), Json::parse("[2,1]").unwrap());
    }
}
