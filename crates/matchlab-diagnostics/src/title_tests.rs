//! Conservative extraction of explicit Rust parser-test expectations.
//!
//! This is deliberately a restricted Rust reader, never an evaluator of
//! arbitrary source. Unsupported assertions are recorded for manual review.
//! Observed parser outputs are never used to derive labels. Runtime tracing
//! supplies the complete executed-input inventory separately.
use indexmap::IndexMap;
use matchlab_community::json::{dumps, Dump, Json, Object};
use matchlab_community::obj;
use matchlab_community::util::{sha256_hex, split_lines, Result};
use regex::Regex;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

// ---- tokenizer ----------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Token {
    pub text: String,
    pub pos: usize,
}

fn is_space(ch: char) -> bool {
    ch.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&ch)
}

fn ident_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

fn ident_continue(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Length of the token at `at`, trying the alternatives in the original
/// regular expression's order; `None` for whitespace.
fn token_len(source: &str, at: usize) -> Option<usize> {
    let rest = &source[at..];
    let bytes = rest.as_bytes();
    let first = rest.chars().next()?;
    if is_space(first) {
        return None;
    }
    if rest.starts_with("//") {
        return Some(rest.find('\n').unwrap_or(rest.len()));
    }
    if rest.starts_with("/*")
        && let Some(end) = rest[2..].find("*/") {
            return Some(end + 4);
        }
    if bytes[0] == b'r' {
        let hashes = bytes[1..].iter().take_while(|byte| **byte == b'#').count();
        if bytes.get(1 + hashes) == Some(&b'"') {
            let closing = format!("\"{}", "#".repeat(hashes));
            if let Some(end) = rest[2 + hashes..].find(&closing) {
                return Some(2 + hashes + end + closing.len());
            }
        }
    }
    if bytes[0] == b'"' {
        let mut chars = rest.char_indices().skip(1);
        while let Some((index, ch)) = chars.next() {
            match ch {
                '\\' => {
                    if chars.next().is_none() {
                        break;
                    }
                }
                '"' => return Some(index + 1),
                _ => {}
            }
        }
    }
    if bytes[0] == b'\'' {
        let mut chars = rest.char_indices().skip(1);
        let body = match chars.next() {
            Some((_, '\\')) => chars.next().filter(|(_, ch)| *ch != '\n').map(|_| ()),
            Some((_, '\'')) | None => None,
            Some(_) => Some(()),
        };
        if body.is_some()
            && let Some((index, '\'')) = chars.next() {
                return Some(index + 1);
            }
    }
    if ident_start(bytes[0]) {
        return Some(bytes.iter().take_while(|byte| ident_continue(**byte)).count());
    }
    if bytes[0].is_ascii_digit() {
        return Some(bytes.iter().take_while(|byte| byte.is_ascii_digit() || **byte == b'_').count());
    }
    for operator in ["::", "->", "=>", "==", "!=", "&&", "||", "..=", ".."] {
        if rest.starts_with(operator) {
            return Some(operator.len());
        }
    }
    Some(first.len_utf8())
}

pub fn tokens(source: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < source.len() {
        match token_len(source, at) {
            None => at += source[at..].chars().next().map_or(1, char::len_utf8),
            Some(length) => {
                let text = &source[at..at + length];
                if !text.starts_with("//") && !text.starts_with("/*") {
                    out.push(Token { text: text.to_owned(), pos: at });
                }
                at += length;
            }
        }
    }
    out
}

fn split(ts: &[Token], sep: &str) -> Vec<Vec<Token>> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut depth = 0i64;
    for (index, token) in ts.iter().enumerate() {
        match token.text.as_str() {
            "(" | "[" | "{" => depth += 1,
            ")" | "]" | "}" => depth -= 1,
            text if text == sep && depth == 0 => {
                out.push(ts[start..index].to_vec());
                start = index + 1;
            }
            _ => {}
        }
    }
    if start < ts.len() {
        out.push(ts[start..].to_vec());
    }
    out
}

/// Index of the bracket closing `ts[start]`; `None` when it is no bracket.
fn close(ts: &[Token], start: usize) -> Option<usize> {
    let opening = ts.get(start)?.text.as_str();
    let closing = match opening {
        "(" => ")",
        "[" => "]",
        "{" => "}",
        _ => return None,
    };
    let mut depth = 0;
    for (index, token) in ts.iter().enumerate().skip(start) {
        if token.text == opening {
            depth += 1;
        } else if token.text == closing {
            depth -= 1;
            if depth == 0 {
                return Some(index);
            }
        }
    }
    Some(ts.len() - 1)
}

fn text(ts: &[Token]) -> String {
    ts.iter().map(|token| token.text.as_str()).collect()
}

/// Python `re.fullmatch(r"[A-Za-z_]\w*", text)`.
fn is_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    chars.next().is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '_') && chars.all(|ch| ch.is_alphanumeric() || ch == '_')
}

static UNICODE_ESCAPE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\\u\{([0-9a-fA-F_]+)\}").unwrap());
static BYTE_ESCAPE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\\x([0-9a-fA-F]{2})").unwrap());

fn json_escaped(ch: char) -> String {
    let quoted = dumps(&Json::Str(ch.to_string()), Dump::DEFAULT);
    quoted[1..quoted.len() - 1].to_owned()
}

/// A Rust string literal's value (JSON covers the common escapes).
pub fn rust_string(raw: &str) -> Option<String> {
    if raw.starts_with('r') {
        let start = raw.find('"')? + 1;
        let end = raw.rfind('"')?;
        return Some(raw.get(start..end)?.to_owned());
    }
    let mut failed = false;
    let replaced = UNICODE_ESCAPE.replace_all(raw, |found: &regex::Captures| {
        match u32::from_str_radix(&found[1].replace('_', ""), 16).ok().and_then(char::from_u32) {
            Some(ch) => json_escaped(ch),
            None => {
                failed = true;
                String::new()
            }
        }
    });
    let replaced = BYTE_ESCAPE.replace_all(&replaced, |found: &regex::Captures| {
        json_escaped(char::from(u8::from_str_radix(&found[1], 16).unwrap_or(0)))
    });
    if failed {
        return None;
    }
    serde_json::from_str::<String>(&replaced.replace("\\0", "\\u0000")).ok()
}

// ---- values -------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct Input {
    pub title: String,
    pub method: String,
    pub translate: bool,
}

#[derive(Clone, Debug)]
pub struct Field {
    pub input: Input,
    pub path: Vec<V>,
    pub coercion: String,
}

/// A value of the restricted reader; `Unknown` is never a label.
#[derive(Clone, Debug)]
pub enum V {
    Unknown,
    None,
    Bool(bool),
    Int(i128),
    Str(String),
    List(Vec<V>),
    Field(Box<Field>),
}

impl V {
    fn int(&self) -> Option<i128> {
        match self {
            V::Int(value) => Some(*value),
            V::Bool(value) => Some(i128::from(*value)),
            _ => None,
        }
    }

    fn truthy(&self) -> bool {
        match self {
            V::None => false,
            V::Bool(value) => *value,
            V::Int(value) => *value != 0,
            V::Str(text) => !text.is_empty(),
            V::List(items) => !items.is_empty(),
            V::Unknown | V::Field(_) => true,
        }
    }

    fn is_unknown(&self) -> bool {
        matches!(self, V::Unknown)
    }

    fn is_field(&self) -> bool {
        matches!(self, V::Field(_))
    }

    /// Serializable: no unknown parts and no parser references.
    fn safe(&self) -> bool {
        match self {
            V::Unknown | V::Field(_) => false,
            V::List(items) => items.iter().all(V::safe),
            _ => true,
        }
    }

    fn to_json(&self) -> Json {
        match self {
            V::None | V::Unknown | V::Field(_) => Json::Null,
            V::Bool(value) => Json::Bool(*value),
            V::Int(value) => Json::Int(*value),
            V::Str(text) => Json::Str(text.clone()),
            V::List(items) => Json::Arr(items.iter().map(V::to_json).collect()),
        }
    }
}

/// Python `==`.
impl PartialEq for V {
    fn eq(&self, other: &V) -> bool {
        match (self, other) {
            (V::Unknown, V::Unknown) | (V::None, V::None) => true,
            (V::Str(left), V::Str(right)) => left == right,
            (V::List(left), V::List(right)) => left == right,
            (V::Field(left), V::Field(right)) => {
                left.input == right.input && left.path == right.path && left.coercion == right.coercion
            }
            (left, right) => matches!((left.int(), right.int()), (Some(a), Some(b)) if a == b),
        }
    }
}

fn py_repr_str(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') { '"' } else { '\'' };
    let mut out = String::from(quote);
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch == quote => {
                out.push('\\');
                out.push(ch);
            }
            ch if (ch as u32) < 0x20 || ch == '\u{7f}' => out.push_str(&format!("\\x{:02x}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out.push(quote);
    out
}

fn py_repr(value: &V) -> String {
    match value {
        V::Str(text) => py_repr_str(text),
        V::List(items) => format!("[{}]", items.iter().map(py_repr).collect::<Vec<_>>().join(", ")),
        other => py_str(other),
    }
}

/// Python `str()` used by `format!` expansion.
fn py_str(value: &V) -> String {
    match value {
        V::Unknown => "<object>".into(),
        V::None => "None".into(),
        V::Bool(true) => "True".into(),
        V::Bool(false) => "False".into(),
        V::Int(number) => number.to_string(),
        V::Str(text) => text.clone(),
        V::List(items) => format!("[{}]", items.iter().map(py_repr).collect::<Vec<_>>().join(", ")),
        V::Field(field) => {
            let path: Vec<String> = field.path.iter().map(py_repr).collect();
            let tuple = if path.len() == 1 { format!("({},)", path[0]) } else { format!("({})", path.join(", ")) };
            format!(
                "Field(input=Input(title={}, method={}, translate={}), path={tuple}, coercion={})",
                py_repr_str(&field.input.title),
                py_repr_str(&field.input.method),
                if field.input.translate { "True" } else { "False" },
                py_repr_str(&field.coercion)
            )
        }
    }
}

// ---- reader -------------------------------------------------------------

type Env = HashMap<String, V>;

#[derive(Clone, Debug, PartialEq)]
struct Context {
    file: String,
    test: String,
    line: usize,
}

impl Context {
    fn to_json(&self) -> Json {
        obj! {"file" => self.file.as_str(), "test" => self.test.as_str(), "line" => self.line}
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Expectation {
    path: Vec<V>,
    op: &'static str,
    value: Option<V>,
    coercion: Option<String>,
}

impl Expectation {
    fn safe(&self) -> bool {
        self.path.iter().all(V::safe) && self.value.as_ref().is_none_or(V::safe)
    }

    fn to_json(&self) -> Json {
        let mut map = Object::new();
        map.insert("path".into(), Json::Arr(self.path.iter().map(V::to_json).collect()));
        map.insert("op".into(), Json::from(self.op));
        if let Some(value) = &self.value {
            map.insert("value".into(), value.to_json());
        }
        if let Some(coercion) = &self.coercion {
            map.insert("coercion".into(), Json::from(coercion.as_str()));
        }
        Json::Obj(map)
    }
}

#[derive(Debug)]
struct Row {
    title: String,
    translate: bool,
    expected: Vec<Expectation>,
    sources: Vec<Option<Context>>,
    manual: Vec<String>,
}

pub struct Reader {
    root: PathBuf,
    rows: IndexMap<(String, bool), Row>,
    inventory: Vec<Context>,
    failures: Vec<Json>,
    context: Option<Context>,
}

const SUFFIXES: [&str; 10] = [
    "1080p.WEB-DL.x264-GROUP",
    "2160p.BluRay.REMUX.HEVC",
    "720p.WEBRip.AAC",
    "1080p.NF.WEB-DL.DDP5.1",
    "2160p.UHD.HDR.DV",
    "480p.DVDRip.XviD",
    "1080p.BluRay.x265.10bit",
    "720p.HDTV.x264",
    "1080p.AMZN.WEB-DL.H264",
    "2160p.WEB.H265.Atmos",
];

static FORMAT_FIELD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\{([A-Za-z_0-9]*)\}").unwrap());
static IS_NONE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\A(.+)\.is_none\(\)\z").unwrap());

/// Python `f"{value:02}"` for an integer.
fn two_digits(value: &V) -> Option<String> {
    let number = value.int()?;
    Some(if number < 0 { format!("-{:01}", -number) } else { format!("{number:02}") })
}

impl Reader {
    pub fn new(root: &Path) -> Reader {
        Reader { root: root.to_path_buf(), rows: IndexMap::new(), inventory: Vec::new(), failures: Vec::new(), context: None }
    }

    fn row(&mut self, input: &Input) -> &mut Row {
        let key = (input.title.clone(), input.translate);
        let context = self.context.clone();
        let row = self.rows.entry(key).or_insert_with(|| Row {
            title: input.title.clone(),
            translate: input.translate,
            expected: Vec::new(),
            sources: Vec::new(),
            manual: Vec::new(),
        });
        if !row.sources.contains(&context) {
            row.sources.push(context);
        }
        row
    }

    fn expectation(&mut self, field: &V, op: &'static str, value: Option<V>) {
        let V::Field(field) = field else { return };
        if field.path.is_empty() {
            return;
        }
        let coercion = (!field.coercion.is_empty()).then(|| field.coercion.clone());
        let item = Expectation { path: field.path.clone(), op, value, coercion };
        let row = self.row(&field.input);
        if !row.expected.contains(&item) {
            row.expected.push(item);
        }
    }

    fn evaluate(&mut self, ts: &[Token], env: &Env) -> V {
        self.evaluate_inner(ts, env).unwrap_or(V::Unknown)
    }

    fn evaluate_all(&mut self, parts: &[Vec<Token>], env: &Env) -> Vec<V> {
        parts.iter().map(|part| self.evaluate(part, env)).collect()
    }

    fn evaluate_inner(&mut self, ts: &[Token], env: &Env) -> Option<V> {
        let mut ts = ts;
        while ts.first().is_some_and(|token| token.text == "&") {
            ts = &ts[1..];
        }
        if ts.is_empty() {
            return Some(V::Unknown);
        }
        if ts[0].text == "if" {
            let Some(a) = ts.iter().position(|token| token.text == "{") else { return Some(V::Unknown) };
            let Some(ae) = close(ts, a) else { return Some(V::Unknown) };
            let condition = self.evaluate(&ts[1..a], env);
            if condition.is_unknown() {
                return Some(V::Unknown);
            }
            if condition.truthy() {
                return Some(self.evaluate(&ts[a + 1..ae], env));
            }
            let b = ae + 2;
            let Some(be) = close(ts, b) else { return Some(V::Unknown) };
            return Some(self.evaluate(ts.get(b + 1..be)?, env));
        }
        // Arithmetic is used only for integer range counts in test labels.
        for op in ["+", "-"] {
            let parts = split(ts, op);
            if parts.len() == 2 {
                let values = self.evaluate_all(&parts, env);
                if let (Some(a), Some(b)) = (values[0].int(), values[1].int()) {
                    return Some(V::Int(if op == "+" { a + b } else { a - b }));
                }
            }
        }
        let first = ts[0].text.as_str();
        let mut consumed = 1;
        let next_is = |index: usize, wanted: &str| ts.get(index).is_some_and(|token| token.text == wanted);
        let mut value = if first.starts_with('"') || first.starts_with("r\"") || first.starts_with("r#") {
            V::Str(rust_string(first)?)
        } else if first.starts_with('\'') && first.ends_with('\'') {
            V::Str(first.chars().skip(1).take(first.chars().count().saturating_sub(2)).collect())
        } else if first.as_bytes()[0].is_ascii_digit() && first.bytes().all(|b| b.is_ascii_digit() || b == b'_') {
            V::Int(first.replace('_', "").parse().ok()?)
        } else if first.chars().all(char::is_numeric) {
            return None;
        } else if first == "true" || first == "false" {
            V::Bool(first == "true")
        } else if first == "None" || first == "null" {
            V::None
        } else if first == "(" || first == "[" {
            let end = close(ts, 0)?;
            let values = self.evaluate_all(&split(&ts[1..end], ","), env);
            consumed = end + 1;
            if first == "(" && values.len() == 1 { values.into_iter().next()? } else { V::List(values) }
        } else if matches!(first, "json" | "vec" | "format") && ts.len() > 2 && next_is(1, "!") {
            let end = close(ts, 2)?;
            consumed = end + 1;
            let values = self.evaluate_all(&split(&ts[3..end], ","), env);
            match first {
                "json" => values.into_iter().next().unwrap_or(V::Unknown),
                "vec" => V::List(values),
                _ => match values.first() {
                    Some(V::Str(format)) => {
                        let mut rest = values[1..].iter();
                        let mut unresolved = false;
                        let expanded = FORMAT_FIELD.replace_all(format, |found: &regex::Captures| {
                            let key = &found[1];
                            let value = if key.is_empty() { rest.next().cloned() } else { env.get(key).cloned() };
                            match value {
                                Some(value) if !value.is_unknown() => py_str(&value),
                                _ => {
                                    unresolved = true;
                                    found[0].to_owned()
                                }
                            }
                        });
                        if unresolved { V::Unknown } else { V::Str(expanded.into_owned()) }
                    }
                    _ => V::Unknown,
                },
            }
        } else if first == "variants" && next_is(1, "(") {
            let end = close(ts, 1)?;
            consumed = end + 1;
            let args = self.evaluate_all(&split(&ts[2..end], ","), env);
            if args.len() == 5 && args.iter().all(|arg| !arg.is_unknown()) {
                let V::Str(title) = &args[0] else { return None };
                let dot = title.replace(' ', ".");
                let dash = title.replace(' ', "-");
                let mut out = Vec::new();
                for (index, suffix) in SUFFIXES.iter().enumerate() {
                    let prefix = if index % 2 == 0 { &dot } else { &dash };
                    if args[4].truthy() {
                        let (season, episode) = (two_digits(&args[2])?, two_digits(&args[3])?);
                        out.push(V::Str(match index % 4 {
                            0 => format!("{prefix}.S{season}E{episode}.{suffix}"),
                            1 => format!("{prefix} S{season}E{episode} {suffix}"),
                            2 => format!("{prefix}.S{season}.E{episode}.{suffix}"),
                            _ => format!("{prefix} - S{season}E{episode} - {suffix}"),
                        }));
                    } else {
                        let year = py_str(&args[1]);
                        out.push(V::Str(match index % 3 {
                            0 => format!("{prefix}.{year}.{suffix}"),
                            1 => format!("{prefix} ({year}) {suffix}"),
                            _ => format!("{prefix}-{year}-{suffix}"),
                        }));
                    }
                }
                V::List(out)
            } else {
                V::Unknown
            }
        } else if first == "Some" && next_is(1, "(") {
            let end = close(ts, 1)?;
            consumed = end + 1;
            self.evaluate(&ts[2..end], env)
        } else if matches!(first, "parse" | "parsed" | "parse_title" | "parse_title_canonical") && next_is(1, "(") {
            let end = close(ts, 1)?;
            consumed = end + 1;
            let args = self.evaluate_all(&split(&ts[2..end], ","), env);
            match args.first() {
                Some(V::Str(title)) => {
                    let translate = args.get(1).is_some_and(|arg| !arg.is_unknown() && arg.truthy());
                    let input = Input { title: title.clone(), method: first.to_owned(), translate };
                    self.row(&input);
                    V::Field(Box::new(Field { input, path: Vec::new(), coercion: String::new() }))
                }
                _ => V::Unknown,
            }
        } else {
            env.get(first).cloned().unwrap_or(V::Unknown)
        };
        // Field paths, simple pure methods, and parser result wrappers.
        while consumed < ts.len() {
            let token = ts[consumed].text.as_str();
            if token == "[" {
                let end = close(ts, consumed)?;
                let index = self.evaluate(&ts[consumed + 1..end], env);
                value = match (value, index) {
                    (V::Field(mut field), index) if !index.is_unknown() => {
                        field.path.push(index);
                        V::Field(field)
                    }
                    (V::List(items), index) if index.int().is_some() => {
                        let index = index.int()?;
                        let position = if index < 0 { items.len() as i128 + index } else { index };
                        items.get(usize::try_from(position).ok()?)?.clone()
                    }
                    _ => V::Unknown,
                };
                consumed = end + 1;
            } else if token == "." && consumed + 1 < ts.len() {
                let method = ts[consumed + 1].text.as_str();
                consumed += 2;
                let mut args = Vec::new();
                if next_is(consumed, "(") {
                    let end = close(ts, consumed)?;
                    args = self.evaluate_all(&split(&ts[consumed + 1..end], ","), env);
                    consumed = end + 1;
                }
                const ACCESSORS: [&str; 5] = ["as_str", "as_i64", "as_u64", "as_bool", "as_array"];
                value = match (method, value) {
                    (
                        "unwrap" | "expect" | "unwrap_or_else" | "to_owned" | "to_string" | "into" | "iter" | "copied"
                        | "as_ref",
                        value,
                    ) => value,
                    ("get", V::Field(mut field)) if args.first().is_some_and(|arg| !arg.is_unknown()) => {
                        field.path.push(args[0].clone());
                        V::Field(field)
                    }
                    (method, V::Field(field)) if ACCESSORS.contains(&method) => V::Field(field),
                    ("and_then", V::Field(field)) if ts.iter().any(|token| ACCESSORS.contains(&token.text.as_str())) => {
                        V::Field(field)
                    }
                    ("len", V::Field(mut field)) => {
                        field.coercion = "len".into();
                        V::Field(field)
                    }
                    ("contains", V::Str(text)) if matches!(args.first(), Some(V::Str(_))) => {
                        let V::Str(needle) = &args[0] else { return None };
                        V::Bool(text.contains(needle.as_str()))
                    }
                    ("to_ascii_lowercase", V::Str(text)) => V::Str(text.to_lowercase()),
                    ("clone" | "as_slice", value) => value,
                    _ => V::Unknown,
                };
            } else {
                return Some(V::Unknown);
            }
        }
        Some(value)
    }

    fn assertion(&mut self, name: &str, args: &[Vec<Token>], env: &Env) {
        let Some(first) = args.first() else { return };
        let a = self.evaluate(first, env);
        if matches!(name, "assert_eq" | "assert_ne") && args.len() > 1 {
            let b = self.evaluate(&args[1], env);
            let op = if name == "assert_eq" { "eq" } else { "ne" };
            if a.is_field() && !b.is_unknown() && !b.is_field() {
                self.expectation(&a, op, Some(b));
                return;
            }
            if b.is_field() && !a.is_unknown() && !a.is_field() {
                self.expectation(&b, op, Some(a));
                return;
            }
        }
        let condition = text(first);
        // Only simple absence predicates are scored automatically.
        if name == "assert"
            && let Some(found) = IS_NONE.captures(&condition) {
                let prefix = tokens(&found[1]);
                let value = self.evaluate(&prefix, env);
                if value.is_field() {
                    self.expectation(&value, "absent", None);
                    return;
                }
            }
        // Preserve source assertions even when not executable by this reader.
        let names: BTreeSet<&str> = first.iter().map(|token| token.text.as_str()).filter(|text| is_identifier(text)).collect();
        let inputs: Vec<Input> = names
            .into_iter()
            .filter_map(|name| match env.get(name) {
                Some(V::Field(field)) => Some(field.input.clone()),
                _ => None,
            })
            .collect();
        for input in inputs {
            let row = self.row(&input);
            if !row.manual.contains(&condition) {
                row.manual.push(condition.clone());
            }
        }
    }

    fn block(&mut self, ts: &[Token], env: &mut Env) {
        let mut i = 0;
        while i < ts.len() {
            let token = ts[i].text.as_str();
            if token == "let" {
                let mut end = i + 1;
                let mut depth = 0i64;
                let mut equal = None;
                while end < ts.len() {
                    let current = ts[end].text.as_str();
                    match current {
                        "(" | "[" | "{" => depth += 1,
                        ")" | "]" | "}" => depth -= 1,
                        _ => {}
                    }
                    if current == "=" && depth == 0 && equal.is_none() {
                        equal = Some(end);
                    }
                    if current == ";" && depth == 0 {
                        break;
                    }
                    end += 1;
                }
                if let Some(equal) = equal {
                    let name = ts[i + 1..equal]
                        .iter()
                        .map(|token| token.text.as_str())
                        .find(|text| is_identifier(text) && *text != "mut" && *text != "ref");
                    if let Some(name) = name {
                        let value = self.evaluate(&ts[equal + 1..end.min(ts.len())], env);
                        env.insert(name.to_owned(), value);
                    }
                }
                i = end + 1;
                continue;
            }
            if token == "for" {
                let mut j = i + 1;
                while j < ts.len() && ts[j].text != "in" {
                    j += 1;
                }
                let mut body = j + 1;
                let mut depth = 0i64;
                while body < ts.len() {
                    let current = ts[body].text.as_str();
                    if current == "{" && depth == 0 {
                        break;
                    }
                    match current {
                        "(" | "[" => depth += 1,
                        ")" | "]" => depth -= 1,
                        _ => {}
                    }
                    body += 1;
                }
                if body >= ts.len() {
                    break;
                }
                let end = close(ts, body).unwrap_or(ts.len() - 1);
                let values = self.evaluate(&ts[(j + 1).min(body)..body], env);
                let names: Vec<String> =
                    ts[i + 1..j.min(ts.len())].iter().map(|token| token.text.clone()).filter(|text| is_identifier(text)).collect();
                if let V::List(values) = values {
                    for value in values {
                        let mut local = env.clone();
                        let bindings = match (&value, names.len() > 1) {
                            (V::List(items), true) => items.clone(),
                            _ => vec![value.clone()],
                        };
                        for (name, binding) in names.iter().zip(bindings) {
                            local.insert(name.clone(), binding);
                        }
                        self.block(&ts[body + 1..end], &mut local);
                    }
                } else {
                    let mut failure = self.context.as_ref().map(Context::to_json).unwrap_or(Json::Obj(Object::new()));
                    if let Json::Obj(map) = &mut failure {
                        map.insert("unexpanded_loop".into(), Json::from(text(&ts[i..body])));
                    }
                    self.failures.push(failure);
                }
                i = end + 1;
                continue;
            }
            if matches!(token, "assert_eq" | "assert_ne" | "assert") && i + 2 < ts.len() && ts[i + 1].text == "!" {
                let Some(end) = close(ts, i + 2) else {
                    i += 1;
                    continue;
                };
                let args = split(&ts[(i + 3).min(end)..end], ",");
                self.assertion(token, &args, env);
                i = end + 1;
                continue;
            }
            if matches!(token, "if" | "match" | "while" | "loop") {
                // Conditional/unsupported control flow cannot establish labels.
                while i < ts.len() && ts[i].text != "{" {
                    i += 1;
                }
                if i < ts.len() {
                    i = close(ts, i).unwrap_or(ts.len() - 1) + 1;
                }
                if i < ts.len() && ts[i].text == "else" {
                    i += 1;
                    if i < ts.len() && ts[i].text == "{" {
                        i = close(ts, i).unwrap_or(ts.len() - 1) + 1;
                    }
                }
                continue;
            }
            i += 1;
        }
    }

    pub fn file(&mut self, path: &Path) -> Result<()> {
        let source = std::fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
        let ts = tokens(&source);
        let mut globals = Env::new();
        for i in 0..ts.len() {
            if ts[i].text != "const" || i + 1 >= ts.len() {
                continue;
            }
            let mut j = i + 2;
            while j < ts.len() && ts[j].text != "=" && ts[j].text != ";" {
                j += 1;
            }
            if j < ts.len() && ts[j].text == "=" {
                let mut k = j + 1;
                let mut depth = 0i64;
                while k < ts.len() {
                    match ts[k].text.as_str() {
                        "[" | "(" | "{" => depth += 1,
                        "]" | ")" | "}" => depth -= 1,
                        _ => {}
                    }
                    if ts[k].text == ";" && depth == 0 {
                        break;
                    }
                    k += 1;
                }
                let value = self.evaluate(&ts[j + 1..k.min(ts.len())], &globals);
                globals.insert(ts[i + 1].text.clone(), value);
            }
        }
        let relative = path.strip_prefix(&self.root).unwrap_or(path).to_string_lossy().into_owned();
        for i in 0..ts.len() {
            if ts[i].text != "#" || i + 3 >= ts.len() || text(&ts[i..i + 4]) != "#[test]" {
                continue;
            }
            let mut j = i + 4;
            while j < ts.len() && ts[j].text != "fn" {
                j += 1;
            }
            if j + 1 >= ts.len() {
                continue;
            }
            let name = ts[j + 1].text.clone();
            while j < ts.len() && ts[j].text != "{" {
                j += 1;
            }
            let end = close(&ts, j).unwrap_or(ts.len().saturating_sub(1));
            let line = source[..ts[i].pos].matches('\n').count() + 1;
            let context = Context { file: relative.clone(), test: name, line };
            self.context = Some(context.clone());
            self.inventory.push(context);
            let mut env = globals.clone();
            if j < end {
                self.block(&ts[j + 1..end], &mut env);
            }
        }
        Ok(())
    }

    pub fn run(mut self) -> Result<Json> {
        let tests = self.root.join("crates/matchlab-parser/tests");
        let mut files: Vec<PathBuf> = std::fs::read_dir(&tests)
            .map_err(|error| format!("{}: {error}", tests.display()))?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.extension().is_some_and(|ext| ext == "rs") && path.is_file())
            .collect();
        files.sort();
        for path in files {
            self.file(&path)?;
        }
        let root = self.root.clone();
        self.file(&root.join("crates/matchlab-parser/src/lib.rs"))?;
        self.file(&root.join("crates/matchlab-parser/src/prefilter.rs"))?;
        Ok(self.payload())
    }

    pub fn payload(self) -> Json {
        let rows: Vec<Json> = self
            .rows
            .into_values()
            .map(|row| {
                // Unsupported intermediate expressions are never emitted as labels.
                let expected: Vec<Json> = row.expected.iter().filter(|item| item.safe()).map(Expectation::to_json).collect();
                obj! {
                    "title" => row.title.as_str(),
                    "translate_languages" => row.translate,
                    "expected" => expected,
                    "sources" => Json::Arr(row.sources.iter().map(|source| source.as_ref().map_or(Json::Null, Context::to_json)).collect()),
                    "assertions_manual" => Json::from_strs(row.manual),
                }
            })
            .collect();
        let count = |key: &str| -> usize { rows.iter().map(|row| row.field(key).as_arr().map_or(0, Vec::len)).sum() };
        let summary = obj! {
            "test_functions" => self.inventory.len(),
            "unique_inputs" => rows.len(),
            "inputs_with_scored_expectations" => rows.iter().filter(|row| row.field("expected").truthy()).count(),
            "scored_assertions" => count("expected"),
            "manual_assertions" => count("assertions_manual"),
            "unexpanded_loops" => self.failures.len(),
        };
        obj! {
            "schema" => "matchlab.parser-test-expectations.v1",
            "provenance" => "Explicit source assertions only; parser outputs are not ground truth. Restricted extraction is incomplete; runtime trace is the executed-input inventory.",
            "tests" => Json::Arr(self.inventory.iter().map(Context::to_json).collect()),
            "unexpanded_loops" => self.failures,
            "cases" => rows,
            "summary" => summary,
        }
    }
}

// ---- release_300 generator audit ----------------------------------------

/// The small `variants()` mirror above is audited against this exact
/// tokenized source function. Fail closed after semantic changes instead of
/// silently manufacturing incorrect test labels.
pub const VARIANT_SOURCE_SHA256: &str = "c8951c84c7afbe963a86fa8ac63b5fcf102cdae72074c722feba8ec6e0719cb8";

pub fn verify_variant_source(path: &Path) -> Result<()> {
    let source = std::fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let ts = tokens(&source);
    for i in 0..ts.len().saturating_sub(1) {
        if ts[i].text == "fn" && ts[i + 1].text == "variants" {
            let start = (i..ts.len()).find(|j| ts[*j].text == "{").ok_or("release_300::variants has no body")?;
            let end = close(&ts, start).unwrap_or(ts.len() - 1);
            if sha256_hex(text(&ts[i..=end])) == VARIANT_SOURCE_SHA256 {
                return Ok(());
            }
            return Err("release_300::variants changed; audit the collector mirror before using its labels".into());
        }
    }
    Err("release_300::variants function was not found".into())
}

pub struct Collected {
    pub parser: Json,
    pub release: Json,
    pub inventory: Json,
}

pub fn collect(root: &Path) -> Result<Collected> {
    let release_path = root.join("crates/matchlab-server/tests/release_300.rs");
    verify_variant_source(&release_path)?;
    let parser = Reader::new(root).run()?;
    let mut release_reader = Reader::new(root);
    release_reader.file(&release_path)?;
    let mut release = release_reader.payload();
    if let Json::Obj(map) = &mut release {
        map.insert(
            "provenance".into(),
            Json::from("Explicit release_300.rs source assertions only; audited generator mirror, never output-derived labels."),
        );
    }
    let tests: Vec<Json> =
        parser.field("tests").as_arr().into_iter().chain(release.field("tests").as_arr()).flatten().cloned().collect();
    let loops: Vec<Json> = parser
        .field("unexpanded_loops")
        .as_arr()
        .into_iter()
        .chain(release.field("unexpanded_loops").as_arr())
        .flatten()
        .cloned()
        .collect();
    let number = |payload: &Json, key: &str| payload.field("summary").field(key).as_int().unwrap_or(0);
    let inventory = obj! {
        "provenance" => "Static test-function inventory; runtime trace is required to inventory every executed dynamic input.",
        "tests" => Json::Arr(tests.clone()),
        "summary" => obj! {
            "test_functions" => tests.len(),
            "parser_cases" => Json::Int(number(&parser, "unique_inputs")),
            "release_300_cases" => Json::Int(number(&release, "unique_inputs")),
            "source_assertions" => Json::Int(number(&parser, "scored_assertions") + number(&release, "scored_assertions")),
            "manual_unscored" => Json::Int(number(&parser, "manual_assertions") + number(&release, "manual_assertions")),
        },
        "unexpanded_loops" => loops,
    };
    Ok(Collected { parser, release, inventory })
}

// ---- runtime trace summary ----------------------------------------------

struct TraceInput {
    title: String,
    translate: bool,
    stages: BTreeSet<String>,
    threads: BTreeSet<String>,
    callers: BTreeSet<(String, i128)>,
    public: IndexMap<String, Json>,
    variants: BTreeSet<String>,
    occurrences: usize,
}

/// Keep runtime observations separate from source expectation labels.
pub fn summarize_trace(trace: &str, curated: &BTreeSet<String>) -> Vec<(&'static str, Json)> {
    const EMPTY_REVISION: i128 = 0xcbf2_9ce4_8422_2325;
    let mut counts: IndexMap<String, i128> = IndexMap::new();
    let mut bump = |key: String| *counts.entry(key).or_default() += 1;
    let mut inputs: BTreeMap<(String, bool), TraceInput> = BTreeMap::new();
    let mut errors = Vec::new();
    for (number, line) in split_lines(trace).iter().enumerate() {
        let row = match Json::parse(line) {
            Ok(row) => row,
            Err(error) => {
                errors.push(obj! {"line" => number + 1, "error" => error});
                continue;
            }
        };
        let text = |key: &str| row.field(key).as_str().unwrap_or_default().to_owned();
        bump("records".into());
        bump(format!("kind:{}", text("kind")));
        bump(format!("mode:{}", text("parser_mode")));
        let title = text("title");
        let translate = row.field("translate_languages").is_true();
        let item = inputs.entry((title.clone(), translate)).or_insert_with(|| TraceInput {
            title,
            translate,
            stages: BTreeSet::new(),
            threads: BTreeSet::new(),
            callers: BTreeSet::new(),
            public: IndexMap::new(),
            variants: BTreeSet::new(),
            occurrences: 0,
        });
        item.occurrences += 1;
        item.stages.insert(text("kind"));
        let thread = row.field("thread").as_str().filter(|name| !name.is_empty()).unwrap_or("<unnamed-worker>");
        item.threads.insert(thread.to_owned());
        item.callers.insert((text("caller_file"), row.field("caller_line").as_int().unwrap_or(0)));
        let observed = row.field("observed");
        if !observed.is_null() {
            let serialized = dumps(observed, Dump::COMPACT_UTF8.sorted());
            item.variants.insert(sha256_hex(&serialized));
            let mode = text("parser_mode");
            let revision = row.field("revision").as_int();
            let baseline = mode == "canonical" || (mode == "live" && matches!(revision, Some(0) | Some(EMPTY_REVISION)));
            if text("kind") == "parse_title" && baseline {
                item.public.insert(serialized, observed.clone());
            }
        }
        if row.field("error").truthy() {
            bump("parse_errors".into());
        }
    }
    let mut inventory = Vec::new();
    let mut parity = Vec::new();
    let mut unstable = 0;
    for (_, item) in inputs {
        if item.public.len() > 1 {
            unstable += 1;
        }
        let callers: Vec<Json> =
            item.callers.iter().map(|(file, line)| obj! {"file" => file.as_str(), "line" => Json::Int(*line)}).collect();
        let threads = Json::from_strs(item.threads.iter().cloned());
        inventory.push(obj! {
            "title" => item.title.as_str(),
            "translate_languages" => item.translate,
            "stages" => Json::from_strs(item.stages.iter().cloned()),
            "threads" => threads.clone(),
            "callers" => callers.clone(),
            "occurrences" => item.occurrences,
            "all_observation_variants" => item.variants.len(),
            "public_baseline_variants" => item.public.len(),
        });
        if item.public.len() == 1 && !item.translate && (1..=4096).contains(&item.title.len()) {
            let observed = item.public.values().next().cloned().unwrap_or(Json::Null);
            parity.push(obj! {
                "id" => format!("trace-{}", &sha256_hex(&item.title)[..16]),
                "endpoint" => "/parse",
                "request" => obj! {"title" => item.title.as_str()},
                "classification" => "local_remote_differential",
                "expected" => Json::Obj(Object::new()),
                "local_observed" => observed.clone(),
                "assertions" => Json::Arr(vec![obj! {"path" => Json::Arr(vec![]), "op" => "eq", "value" => observed}]),
                "expected_provenance" => "LOCAL OBSERVATION ONLY; version parity, never independent correctness.",
                "sources" => callers,
                "test_threads" => threads,
            });
        }
    }
    let stage_count = |stage: &str| {
        inventory.iter().filter(|item| item.field("stages").as_arr().is_some_and(|s| s.iter().any(|x| x.str_eq(stage)))).count()
    };
    let engine_only =
        inventory.iter().filter(|item| item.field("stages") == &Json::from_strs(["engine"])).count();
    let summary = obj! {
        "counts" => Json::Obj(counts.into_iter().map(|(key, value)| (key, Json::Int(value))).collect()),
        "unique_inputs_including_engine_components" => inventory.len(),
        "unique_public_inputs" => stage_count("parse_title"),
        "unique_batch_inputs" => stage_count("parse_many_input"),
        "unique_engine_only_inputs" => engine_only,
        "stable_public_false_translation_parity_cases" => parity.len(),
        "unstable_public_baseline_inputs" => Json::Int(unstable),
        "invalid_trace_lines" => errors,
    };
    let new_only: Vec<Json> = parity
        .iter()
        .filter(|row| !row.field("request").field("title").as_str().is_some_and(|title| curated.contains(title)))
        .cloned()
        .collect();
    vec![
        (
            "executed-title-inventory.json",
            obj! {"provenance" => "Executed inputs/observations, never independent expected labels.",
                  "summary" => summary.clone(), "inputs" => inventory},
        ),
        (
            "title-parity-corpus.json",
            obj! {"provenance" => "Stable local observations for version comparison only.", "cases" => parity},
        ),
        (
            "title-parity-new-only.json",
            obj! {"provenance" => "Stable local observations absent from parser golden corpus; comparison only.",
                  "cases" => new_only},
        ),
        ("trace-summary.json", summary),
    ]
}

// ---- optional trace instrumentation -------------------------------------

const TRACE_HELPER: &str = r#"
#[track_caller]
fn diagnostic_title_trace(
    kind:&str, raw_title:&str, translate_languages:bool,
    output:Option<&Map<String,Value>>, error:Option<&ParseError>,
){
    use std::io::Write;
    let Some(path)=std::env::var_os("MATCHLAB_DIAGNOSTIC_TITLE_TRACE") else{return;};
    static TRACE_LOCK:Mutex<()>=Mutex::new(());
    let Ok(_lock)=TRACE_LOCK.lock() else{return;};
    let thread=std::thread::current();
    let caller=std::panic::Location::caller();
    let mode=if CANONICAL_ONLY.with(Cell::get){"canonical"}
        else if REPLAY_ENGINE.with(|slot|slot.borrow().is_some()){"replay"}
        else {"live"};
    let row=json!({"kind":kind,"title":raw_title,
        "translate_languages":translate_languages,"thread":thread.name(),
        "pid":std::process::id(),"caller_file":caller.file(),
        "caller_line":caller.line(),"parser_mode":mode,
        "revision":handler_patch_revision(),"observed":output,
        "error":error.map(ToString::to_string)});
    let Ok(mut bytes)=serde_json::to_vec(&row)else{return;};
    bytes.push(b'\n');
    if let Ok(mut file)=std::fs::OpenOptions::new().create(true).append(true).open(path){
        let _=file.write_all(&bytes);
    }
}
"#;

/// Emit reviewable instrumentation; never modify the selected checkout.
pub fn trace_patch(root: &Path) -> Result<String> {
    let relative = "crates/matchlab-parser/src/lib.rs";
    let original =
        std::fs::read_to_string(root.join(relative)).map_err(|error| format!("{relative}: {error}"))?;
    if original.contains("fn diagnostic_title_trace(") {
        return Err("Selected source is already instrumented".into());
    }
    let signature = "pub fn parse_title(\n    raw_title: &str,\n    translate_languages: bool,\n) -> Result<Map<String, Value>, ParseError> {";
    let wrapper = format!(
        "#[track_caller]\n{signature}\n    let output=parse_title_untraced(raw_title,translate_languages);\n    \
         diagnostic_title_trace(\"parse_title\",raw_title,translate_languages,\n        \
         output.as_ref().ok(),output.as_ref().err());\n    output\n}}\n{}",
        signature.replace("pub fn parse_title(", "fn parse_title_untraced(")
    );
    let replacements = [
        (signature.to_owned(), wrapper),
        (
            "        result.insert(\"title\".to_owned(), Value::String(clean_title(title_slice)));".to_owned(),
            "        result.insert(\"title\".to_owned(), Value::String(clean_title(title_slice)));\n        \
             diagnostic_title_trace(\"engine\",raw_title,translate_languages,Some(&result),None);"
                .to_owned(),
        ),
        ("pub fn parse_many<'a, I>(".to_owned(), "#[track_caller]\npub fn parse_many<'a, I>(".to_owned()),
        (
            "    let titles: Vec<_> = titles.into_iter().collect();".to_owned(),
            "    let titles: Vec<_> = titles.into_iter().collect();\n    for title in &titles {\n        \
             diagnostic_title_trace(\"parse_many_input\",title,translate_languages,None,None);\n    }"
                .to_owned(),
        ),
    ];
    let mut changed = original.clone();
    for (old, new) in &replacements {
        if changed.matches(old.as_str()).count() != 1 {
            return Err("Trace anchor changed; audit the temporary instrumentation before applying".into());
        }
        changed = changed.replace(old.as_str(), new);
    }
    changed.push_str(TRACE_HELPER);
    Ok(crate::diff::unified(&original, &changed, &format!("a/{relative}"), &format!("b/{relative}"), 3))
}

pub fn run(root: &Path, output_dir: &Path, trace: Option<&Path>, patch: Option<&Path>) -> Result<Json> {
    let collected = collect(root)?;
    let mut outputs = vec![
        ("parser-title-expectations.json", collected.parser.clone()),
        ("release-300-parser-expectations.json", collected.release),
        ("source-test-inventory.json", collected.inventory.clone()),
    ];
    if let Some(trace) = trace {
        let curated: BTreeSet<String> = collected
            .parser
            .field("cases")
            .as_arr()
            .into_iter()
            .flatten()
            .filter_map(|row| row.field("title").as_str().map(str::to_owned))
            .collect();
        let text = std::fs::read_to_string(trace).map_err(|error| format!("{}: {error}", trace.display()))?;
        outputs.extend(summarize_trace(&text, &curated));
    }
    std::fs::create_dir_all(output_dir).map_err(|error| error.to_string())?;
    if let Some(patch) = patch {
        if let Some(parent) = patch.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        std::fs::write(patch, trace_patch(root)?).map_err(|error| error.to_string())?;
    }
    for (name, value) in outputs {
        std::fs::write(output_dir.join(name), dumps(&value, Dump::PRETTY_UTF8) + "\n").map_err(|error| error.to_string())?;
    }
    Ok(collected.inventory.field("summary").clone())
}
