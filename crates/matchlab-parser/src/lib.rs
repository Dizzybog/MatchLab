use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use aho_corasick::{AhoCorasick, AhoCorasickBuilder};
use chrono::NaiveDate;
use fancy_regex::Regex;
use once_cell::sync::Lazy;
use pcre2::bytes::{Regex as PcreRegex, RegexBuilder as PcreRegexBuilder};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use thiserror::Error;

mod prefilter;

use prefilter::{Gate, Hits, Prefilter};

#[derive(Debug, Error)]
pub enum ParseError {
    #[error("regex error: {0}")]
    Regex(String),
    #[error("data error: {0}")]
    Data(String),
}

#[derive(Debug, Clone)]
pub struct MatchInfo {
    pub raw_match: String,
    pub match_index: usize,
}

#[derive(Debug)]
pub struct ParseContext {
    pub result: Map<String, Value>,
    pub working_title: String,
    pub end_of_title: usize,
    pub matched: HashMap<String, MatchInfo>,
}

#[derive(Debug, Clone)]
struct HandlerMatch {
    match_len: usize,
    match_index: usize,
    remove: bool,
    skip_from_title: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct HandlerTable {
    handlers: Vec<HandlerDefRaw>,
}

#[derive(Debug, Clone, Deserialize)]
struct HandlerDefRaw {
    name: String,
    kind: String,
    pattern: Option<String>,
    flags: Option<u32>,
    transform: String,
    #[serde(default)]
    function: Option<String>,
    options: HandlerOptionsRaw,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[allow(non_snake_case)]
struct HandlerOptionsRaw {
    skipIfAlreadyFound: Option<bool>,
    skipFromTitle: Option<bool>,
    skipIfFirst: Option<bool>,
    remove: Option<bool>,
}

#[derive(Debug, Clone, Default)]
struct HandlerOptions {
    skip_if_already_found: bool,
    skip_from_title: bool,
    skip_if_first: bool,
    remove: bool,
}

impl From<HandlerOptionsRaw> for HandlerOptions {
    fn from(v: HandlerOptionsRaw) -> Self {
        Self {
            skip_if_already_found: v.skipIfAlreadyFound.unwrap_or(true),
            skip_from_title: v.skipFromTitle.unwrap_or(false),
            skip_if_first: v.skipIfFirst.unwrap_or(false),
            remove: v.remove.unwrap_or(false),
        }
    }
}

#[derive(Debug, Clone)]
enum TransformSpec {
    None,
    Integer,
    FirstInteger,
    Boolean,
    Lowercase,
    Uppercase,
    Value(String),
    Date(Vec<String>),
    RangeFunc,
    RangeXOfYFunc,
    ArrayInteger,
    UniqConcatValue(String),
    HdrFormat,
    TransformResolution,
}

#[derive(Debug, Clone)]
enum RuntimeHandlerKind {
    Regex(PcreRegex),
    Function(String),
}

#[derive(Debug, Clone)]
struct RuntimeHandler {
    name: String,
    kind: RuntimeHandlerKind,
    transform: TransformSpec,
    options: HandlerOptions,
    gate: Gate,
    guard: MatchGuard,
}

#[derive(Debug, Clone, Copy, Default)]
enum MatchGuard {
    #[default]
    None,
    AmbiguousMetadata,
    SitePrefix,
    UnmarkedSeason,
    AnchoredEpisode,
}

struct ParserEngine {
    handlers: Vec<RuntimeHandler>,
    prefilter: Prefilter,
}

fn compile_regex(pattern: &str, ignore_case: bool) -> Result<PcreRegex, ParseError> {
    let normalized_pattern = normalize_pattern_for_pcre2(pattern);
    let mut builder = PcreRegexBuilder::new();
    builder.utf(true);
    builder.ucp(true);
    builder.caseless(ignore_case);
    builder.jit_if_available(true);
    builder.build(&normalized_pattern).map_err(|error| {
        ParseError::Regex(format!("Error compiling regex: {error}; pattern={pattern}"))
    })
}

fn normalize_pattern_for_pcre2(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && chars.peek().copied() == Some('u') {
            chars.next(); // consume 'u'
            let mut hex = String::with_capacity(4);
            for _ in 0..4 {
                if let Some(h) = chars.peek().copied() {
                    if h.is_ascii_hexdigit() {
                        hex.push(h);
                        chars.next();
                    } else {
                        break;
                    }
                }
            }
            if hex.len() == 4 {
                out.push_str("\\x{");
                out.push_str(&hex);
                out.push('}');
                continue;
            }
            out.push('\\');
            out.push('u');
            out.push_str(&hex);
            continue;
        }
        out.push(c);
    }
    out.replace(r"\[?\]?", r"(?:\[\]?)?")
        .replace(r"[\])?]?", r"(?:\]|\)|\?)?")
}

fn parse_quoted_arg(expr: &str, prefix: &str, suffix: &str) -> Option<String> {
    if !expr.starts_with(prefix) || !expr.ends_with(suffix) {
        return None;
    }
    let mut raw = expr[prefix.len()..expr.len() - suffix.len()]
        .trim()
        .to_owned();
    if raw.len() >= 2
        && ((raw.starts_with('\'') && raw.ends_with('\''))
            || (raw.starts_with('"') && raw.ends_with('"')))
    {
        raw = raw[1..raw.len() - 1].to_owned();
    }
    Some(raw)
}

fn parse_date_formats(expr: &str) -> Option<Vec<String>> {
    if !expr.starts_with("date(") || !expr.ends_with(')') {
        return None;
    }
    let inner = &expr[5..expr.len() - 1];
    let inner = inner.trim();
    if inner.starts_with('[') && inner.ends_with(']') {
        let inner = &inner[1..inner.len() - 1];
        let mut out = Vec::new();
        for part in inner.split(',') {
            let part = part.trim();
            let unquoted = if (part.starts_with('\'') && part.ends_with('\''))
                || (part.starts_with('"') && part.ends_with('"'))
            {
                part[1..part.len() - 1].to_owned()
            } else {
                part.to_owned()
            };
            if !unquoted.is_empty() {
                out.push(unquoted);
            }
        }
        return Some(out);
    }
    let fmt = parse_quoted_arg(expr, "date(", ")")?;
    Some(vec![fmt])
}

fn parse_transform(expr: &str) -> TransformSpec {
    let expr = expr.trim();
    match expr {
        "none" => TransformSpec::None,
        "integer" => TransformSpec::Integer,
        "first_integer" => TransformSpec::FirstInteger,
        "boolean" => TransformSpec::Boolean,
        "lowercase" => TransformSpec::Lowercase,
        "uppercase" => TransformSpec::Uppercase,
        "range_func" => TransformSpec::RangeFunc,
        "range_x_of_y_func" => TransformSpec::RangeXOfYFunc,
        "array(integer)" => TransformSpec::ArrayInteger,
        "transform_resolution" => TransformSpec::TransformResolution,
        _ => {
            if let Some(v) = parse_quoted_arg(expr, "value(", ")") {
                return TransformSpec::Value(v);
            }
            if let Some(v) = parse_quoted_arg(expr, "uniq_concat(value(", "))") {
                return TransformSpec::UniqConcatValue(v);
            }
            if let Some(v) = parse_date_formats(expr) {
                return TransformSpec::Date(v);
            }
            TransformSpec::None
        }
    }
}

impl ParserEngine {
    fn from_json(json_text: &str) -> Result<Self, ParseError> {
        let table: HandlerTable =
            serde_json::from_str(json_text).map_err(|e| ParseError::Data(e.to_string()))?;
        let mut handlers = Vec::with_capacity(table.handlers.len());
        let mut gate_specs = Vec::with_capacity(table.handlers.len());
        for mut raw in table.handlers {
            // Keep the generated upstream snapshot intact; these are deliberate
            // parser corrections applied before compiling both regexes and gates.
            let guard = match (raw.name.as_str(), raw.pattern.as_deref()) {
                ("hdr", _) | ("bit_depth", Some(r"\bhdr10\b")) => MatchGuard::AmbiguousMetadata,
                ("dubbed", Some(r"\bdual\b(?![ .-]*sub)")) => MatchGuard::AmbiguousMetadata,
                ("site", Some(pattern)) if pattern.contains("com|org|net") => {
                    MatchGuard::SitePrefix
                }
                ("seasons", Some(r"\b(\d{2})[ ._]\d{2}(?:.F)?\.\w{2,4}$")) => {
                    MatchGuard::UnmarkedSeason
                }
                // The upstream catch-all E123 handler can also match the
                // trailing "e100" in The100 or a hash like RE301085.
                // Require a token boundary, except after an Sxx season marker.
                ("episodes", Some(r"E(\d+)\b")) => MatchGuard::AnchoredEpisode,
                _ => MatchGuard::None,
            };
            let is_hdr_format =
                raw.name == "hdr" && raw.pattern.as_deref() == Some(r"\bHDR(?:10)?\b");
            if is_hdr_format {
                raw.pattern = Some(r"\b(?:HDR(?:10)?|HLG)\b".to_owned());
            }
            let options: HandlerOptions = raw.options.into();
            let transform = if is_hdr_format {
                TransformSpec::HdrFormat
            } else {
                parse_transform(&raw.transform)
            };
            let kind = match raw.kind.as_str() {
                "regex" => {
                    let pat = raw
                        .pattern
                        .ok_or_else(|| ParseError::Data("missing pattern".to_owned()))?;
                    let ignore_case = (raw.flags.unwrap_or(0) & 2) != 0;
                    let normalized = normalize_pattern_for_pcre2(&pat);
                    gate_specs.push(prefilter::derive(&normalized));
                    RuntimeHandlerKind::Regex(compile_regex(&pat, ignore_case)?)
                }
                _ => {
                    let function = raw.function.unwrap_or_default();
                    gate_specs.push(Vec::new());
                    RuntimeHandlerKind::Function(function)
                }
            };
            handlers.push(RuntimeHandler {
                name: raw.name,
                kind,
                transform,
                options,
                gate: Gate::default(),
                guard,
            });
        }
        let (prefilter, gates) = Prefilter::build(&gate_specs);
        for (handler, gate) in handlers.iter_mut().zip(gates) {
            handler.gate = gate;
        }
        Ok(Self {
            handlers,
            prefilter,
        })
    }

    fn parse(
        &self,
        raw_title: &str,
        translate_languages: bool,
    ) -> Result<Map<String, Value>, ParseError> {
        Ok(self.parse_context(raw_title, translate_languages)?.result)
    }

    fn parse_context(
        &self,
        raw_title: &str,
        translate_languages: bool,
    ) -> Result<ParseContext, ParseError> {
        self.parse_context_impl::<true>(raw_title, translate_languages)
    }

    fn parse_context_impl<const PREFILTER: bool>(
        &self,
        raw_title: &str,
        translate_languages: bool,
    ) -> Result<ParseContext, ParseError> {
        let mut title = normalize_underscores(raw_title);
        let mut result = Map::new();
        let mut matched: HashMap<String, MatchInfo> = HashMap::new();
        let mut end_of_title = title.len();
        let mut hits = Hits::default();
        if PREFILTER {
            self.prefilter.scan(&title, &mut hits);
        }

        for handler in &self.handlers {
            if PREFILTER && !handler.gate.hit(&hits) {
                continue;
            }
            let maybe_match = match &handler.kind {
                RuntimeHandlerKind::Regex(re) => self.apply_regex_handler(
                    handler,
                    re,
                    &title,
                    end_of_title,
                    &mut result,
                    &mut matched,
                )?,
                RuntimeHandlerKind::Function(func_name) => self.apply_function_handler(
                    handler,
                    func_name,
                    &title,
                    &mut result,
                    &mut matched,
                )?,
            };

            let Some(match_result) = maybe_match else {
                continue;
            };

            if match_result.remove {
                let start = match_result.match_index.min(title.len());
                let end = (match_result.match_index + match_result.match_len).min(title.len());
                if start <= end && title.is_char_boundary(start) && title.is_char_boundary(end) {
                    title.replace_range(start..end, "");
                    if PREFILTER {
                        self.prefilter.scan(&title, &mut hits);
                    }
                }
            }
            if !match_result.skip_from_title
                && match_result.match_index > 1
                && match_result.match_index < end_of_title
            {
                end_of_title = match_result.match_index;
            }
            if match_result.remove
                && match_result.skip_from_title
                && match_result.match_index < end_of_title
            {
                end_of_title = end_of_title.saturating_sub(match_result.match_len);
            }
        }

        result
            .entry("episodes".to_owned())
            .or_insert_with(|| json!([]));
        result
            .entry("seasons".to_owned())
            .or_insert_with(|| json!([]));
        result
            .entry("languages".to_owned())
            .or_insert_with(|| json!([]));

        post_process_result(raw_title, &mut result)?;

        if translate_languages && let Some(Value::Array(langs)) = result.get_mut("languages") {
            let mut translated = Vec::with_capacity(langs.len());
            for lang in langs.iter().filter_map(|v| v.as_str()) {
                if let Some(name) = LANGUAGES_TRANSLATION_TABLE.get(lang) {
                    translated.push(Value::String((*name).to_owned()));
                }
            }
            *langs = translated;
        }

        let end_of_title = end_of_title.min(title.len());
        let title_slice = if title.is_char_boundary(end_of_title) {
            &title[..end_of_title]
        } else {
            &title
        };
        result.insert("title".to_owned(), Value::String(clean_title(title_slice)));
        Ok(ParseContext {
            result,
            working_title: title,
            end_of_title,
            matched,
        })
    }

    fn apply_regex_handler(
        &self,
        handler: &RuntimeHandler,
        regex: &PcreRegex,
        title: &str,
        end_of_title: usize,
        result: &mut Map<String, Value>,
        matched: &mut HashMap<String, MatchInfo>,
    ) -> Result<Option<HandlerMatch>, ParseError> {
        if result.contains_key(&handler.name) && handler.options.skip_if_already_found {
            return Ok(None);
        }

        // PCRE2 pools match data for is_match; captures allocates its capture
        // vector, so only run it for the small set of handlers that match.
        if !regex
            .is_match(title.as_bytes())
            .map_err(|e| ParseError::Regex(e.to_string()))?
        {
            return Ok(None);
        }
        let captures = regex
            .captures(title.as_bytes())
            .map_err(|e| ParseError::Regex(e.to_string()))?;
        let Some(captures) = captures else {
            return Ok(None);
        };
        let Some(m0) = captures.get(0) else {
            return Ok(None);
        };
        // PCRE2 UTF mode returns offsets on UTF-8 boundaries. Borrow captures;
        // only episode/group context needs to retain an owned match.
        let raw_match = &title[m0.start()..m0.end()];
        let clean_match = captures
            .get(1)
            .map(|m| &title[m.start()..m.end()])
            .unwrap_or(raw_match);

        if handler.name == "year" && is_numeric_title(title.get(..end_of_title).unwrap_or(title)) {
            return Ok(None);
        }

        match handler.guard {
            MatchGuard::AmbiguousMetadata => {
                let bracketed = title[..m0.start()]
                    .rfind('[')
                    .is_some_and(|open| !title[open..m0.start()].contains(']'));
                if !bracketed {
                    let has_identity = ["year", "seasons", "episodes"]
                        .iter()
                        .any(|key| matched.contains_key(*key));
                    let episode_marker = if has_identity {
                        None
                    } else {
                        EXPLICIT_EPISODE_RE
                            .find(title)
                            .map_err(|e| ParseError::Regex(e.to_string()))?
                    };
                    let before_identity = (has_identity && m0.start() < end_of_title)
                        || episode_marker.is_some_and(|m| m0.start() < m.start());
                    let has_technical = ["resolution", "quality", "codec"]
                        .iter()
                        .any(|key| matched.contains_key(*key));
                    if before_identity
                        || (!has_identity && !has_technical && episode_marker.is_none())
                    {
                        return Ok(None);
                    }
                }
            }
            MatchGuard::SitePrefix => {
                let explicit_www = raw_match
                    .get(..3)
                    .is_some_and(|prefix| prefix.eq_ignore_ascii_case("www"));
                let standalone_domain = !matched.contains_key("year") && raw_match == title.trim();
                if !explicit_www
                    && !standalone_domain
                    && (m0.start() == 0 || raw_match.contains(char::is_whitespace))
                {
                    return Ok(None);
                }
            }
            MatchGuard::UnmarkedSeason => {
                if EXPLICIT_EPISODE_RE
                    .is_match(title)
                    .map_err(|e| ParseError::Regex(e.to_string()))?
                {
                    return Ok(None);
                }
            }
            MatchGuard::AnchoredEpisode => {
                // E01 and S01E01 are valid coordinates. The100, RE301085
                // and 3E121247 are *not* episode tokens: their E is embedded
                // inside an unrelated alphanumeric word/release code.
                let prefix=&title[..m0.start()];
                if prefix.as_bytes().last().is_some_and(u8::is_ascii_alphanumeric){
                    let token=prefix.rsplit(|c:char|!c.is_ascii_alphanumeric())
                        .next().unwrap_or_default();
                    let digits=token.strip_prefix('s').or_else(||token.strip_prefix('S'));
                    if !digits.is_some_and(|d|!d.is_empty()&&d.len()<=3
                        &&d.bytes().all(|b|b.is_ascii_digit())){
                        return Ok(None);
                    }
                }
            }
            MatchGuard::None => {}
        }
        if handler.name == "episodes" && !raw_match.chars().any(char::is_alphabetic) {
            let prefix = title[..m0.start()].trim_end_matches(|c: char| !c.is_alphanumeric());
            if prefix
                .rsplit(|c: char| !c.is_alphanumeric())
                .next()
                .is_some_and(|word| {
                    word.eq_ignore_ascii_case("movie") || word.eq_ignore_ascii_case("film")
                })
            {
                return Ok(None);
            }
        }
        let existing = result.get(&handler.name);
        let transformed = transform_value(&handler.transform, clean_match, existing)?;
        let Some(mut transformed) = transformed else {
            return Ok(None);
        };
        if let Value::String(s) = &mut transformed {
            *s = s.trim().to_owned();
        }

        let mut is_before_title = false;
        if title.starts_with('[')
            && let Some(caps) = BEFORE_TITLE_MATCH_REGEX
                .captures(title)
                .map_err(|e| ParseError::Regex(e.to_string()))?
            && let Some(g1) = caps.get(1)
        {
            is_before_title = g1.as_str().contains(raw_match);
        }

        let mut has_other_match = false;
        let mut all_before_others = true;
        for (key, info) in matched.iter() {
            if key == &handler.name {
                continue;
            }
            has_other_match = true;
            if m0.start() >= info.match_index {
                all_before_others = false;
                break;
            }
        }
        let is_skip_if_first =
            handler.options.skip_if_first && has_other_match && all_before_others;

        if is_skip_if_first {
            return Ok(None);
        }

        matched
            .entry(handler.name.clone())
            .or_insert_with(|| MatchInfo {
                raw_match: if matches!(handler.name.as_str(), "episodes" | "group") {
                    raw_match.to_owned()
                } else {
                    String::new()
                },
                match_index: m0.start(),
            });
        result.insert(handler.name.clone(), transformed);
        Ok(Some(HandlerMatch {
            match_len: raw_match.len(),
            match_index: m0.start(),
            remove: handler.options.remove,
            skip_from_title: is_before_title || handler.options.skip_from_title,
        }))
    }

    fn apply_function_handler(
        &self,
        _handler: &RuntimeHandler,
        function_name: &str,
        title: &str,
        result: &mut Map<String, Value>,
        matched: &mut HashMap<String, MatchInfo>,
    ) -> Result<Option<HandlerMatch>, ParseError> {
        match function_name {
            "is_adult_content" => {
                let already_adult = result
                    .get("adult")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if already_adult {
                    return Ok(None);
                }
                let is_adult = if title.is_ascii() {
                    ADULT_KEYWORDS.is_match(title)
                } else {
                    ADULT_KEYWORDS.is_match(&title.to_lowercase())
                };
                if is_adult {
                    result.insert("adult".to_owned(), Value::Bool(true));
                }
                Ok(None)
            }
            "handle_bit_depth" => {
                if let Some(Value::String(s)) = result.get_mut("bit_depth") {
                    *s = s.replace([' ', '-'], "");
                }
                Ok(None)
            }
            "handle_space_in_codec" => {
                if let Some(Value::String(s)) = result.get_mut("codec") {
                    *s = s.replace([' ', '.', '-'], "");
                }
                Ok(None)
            }
            "handle_volumes" => {
                let start_index = matched.get("year").map(|m| m.match_index).unwrap_or(0);
                let slice = title.get(start_index..).unwrap_or(title);
                if let Some(caps) = HANDLE_VOLUMES_RE
                    .captures(slice)
                    .map_err(|e| ParseError::Regex(e.to_string()))?
                    && let Some(m0) = caps.get(0)
                {
                    let v = caps
                        .get(1)
                        .and_then(|m| m.as_str().parse::<i64>().ok())
                        .unwrap_or(0);
                    matched.insert(
                        "volumes".to_owned(),
                        MatchInfo {
                            raw_match: m0.as_str().to_owned(),
                            match_index: start_index + m0.start(),
                        },
                    );
                    result.insert(
                        "volumes".to_owned(),
                        Value::Array(vec![Value::Number(v.into())]),
                    );
                    return Ok(Some(HandlerMatch {
                        match_len: m0.end() - m0.start(),
                        match_index: start_index + m0.start(),
                        remove: true,
                        skip_from_title: false,
                    }));
                }
                Ok(None)
            }
            "handle_episodes" => {
                if result.contains_key("episodes") {
                    return Ok(None);
                }

                let mut start_indexes = Vec::new();
                if let Some(m) = matched.get("year")
                    && m.match_index != 0
                {
                    start_indexes.push(m.match_index);
                }
                if let Some(m) = matched.get("seasons")
                    && m.match_index != 0
                {
                    start_indexes.push(m.match_index);
                }
                let mut end_indexes = Vec::new();
                for key in ["resolution", "quality", "codec", "audio", "container"] {
                    if let Some(m) = matched.get(key)
                        && m.match_index != 0
                    {
                        end_indexes.push(m.match_index);
                    }
                }
                let start_index = start_indexes.into_iter().min().unwrap_or(0);
                let end_index = end_indexes
                    .into_iter()
                    .chain(std::iter::once(title.len()))
                    .min()
                    .unwrap_or(title.len());
                let beginning_title = title.get(..end_index).unwrap_or(title);
                if is_numeric_title(beginning_title) {
                    return Ok(None);
                }
                let middle_title = title.get(start_index..end_index).unwrap_or(beginning_title);

                let beginning_caps = HANDLE_EPISODES_BEGINNING_RE
                    .captures(beginning_title)
                    .map_err(|e| ParseError::Regex(e.to_string()))?;
                let (caps, from_beginning) = if let Some(c) = beginning_caps {
                    (c, true)
                } else if let Some(c) = HANDLE_EPISODES_MIDDLE_RE
                    .captures(middle_title)
                    .map_err(|e| ParseError::Regex(e.to_string()))?
                {
                    (c, false)
                } else {
                    return Ok(None);
                };

                if let Some(g1) = caps.get(1) {
                    let m0 = caps.get(0);
                    if from_beginning && let Some(m0) = m0 {
                        let match_start = m0.start();
                        let prefix = if match_start <= beginning_title.len() {
                            &beginning_title[..match_start]
                        } else {
                            beginning_title
                        };
                        let prefix_norm = prefix
                            .trim_end_matches(|c: char| !c.is_alphanumeric())
                            .to_lowercase();
                        if prefix_norm.ends_with("movie") || prefix_norm.ends_with("film") {
                            return Ok(None);
                        }
                    }

                    let episode_numbers: Vec<Value> = DIGITS_RE
                        .find_iter(g1.as_str())
                        .filter_map(Result::ok)
                        .filter_map(|m| m.as_str().parse::<i64>().ok())
                        .filter(|n| !matches!(n, 480 | 720 | 1080))
                        .map(|n| Value::Number(n.into()))
                        .collect();
                    if !episode_numbers.is_empty() {
                        result.insert("episodes".to_owned(), Value::Array(episode_numbers));
                        if let Some(m0) = m0 {
                            let idx = title.find(m0.as_str()).unwrap_or(0);
                            return Ok(Some(HandlerMatch {
                                match_len: m0.end() - m0.start(),
                                match_index: idx,
                                remove: false,
                                skip_from_title: false,
                            }));
                        }
                    }
                }
                Ok(None)
            }
            "handle_anime_eps" => {
                if !ANIME_SPECIAL_RE
                    .is_match(title)
                    .map_err(|e| ParseError::Regex(e.to_string()))?
                {
                    return Ok(None);
                }
                if result.contains_key("episodes") {
                    return Ok(None);
                }
                if let Some(m) = ANIME_EP_RE
                    .find(title)
                    .map_err(|e| ParseError::Regex(e.to_string()))?
                    && let Ok(n) = m.as_str().parse::<i64>()
                {
                    result.insert(
                        "episodes".to_owned(),
                        Value::Array(vec![Value::Number(n.into())]),
                    );
                    return Ok(Some(HandlerMatch {
                        match_len: m.end() - m.start(),
                        match_index: m.start(),
                        remove: false,
                        skip_from_title: false,
                    }));
                }
                Ok(None)
            }
            "infer_language_based_on_naming" => {
                let has_pt_or_es = result
                    .get("languages")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(Value::as_str)
                            .any(|lang| lang == "pt" || lang == "es")
                    })
                    .unwrap_or(false);
                if !has_pt_or_es {
                    let mut should_add = false;
                    if let Some(ep_match) = matched.get("episodes") {
                        should_add = INFER_PT_EP_RE
                            .is_match(&ep_match.raw_match)
                            .map_err(|e| ParseError::Regex(e.to_string()))?;
                    }
                    if !should_add {
                        should_add = INFER_PT_TITLE_RE
                            .is_match(title)
                            .map_err(|e| ParseError::Regex(e.to_string()))?;
                    }
                    if should_add {
                        let mut langs: Vec<Value> = result
                            .get("languages")
                            .and_then(Value::as_array)
                            .cloned()
                            .unwrap_or_default();
                        langs.push(Value::String("pt".to_owned()));
                        result.insert("languages".to_owned(), Value::Array(langs));
                    }
                }
                Ok(None)
            }
            "handle_group" => {
                if let Some(group_match) = matched.get("group")
                    && group_match.raw_match.starts_with('[')
                    && group_match.raw_match.ends_with(']')
                {
                    let end_index = group_match.match_index + group_match.raw_match.len();
                    let overlap = matched
                        .iter()
                        .any(|(k, m)| k != "group" && m.match_index < end_index);
                    if overlap {
                        result.remove("group");
                    }
                }
                Ok(None)
            }
            "handle_group_exclusion" => {
                let remove = matches!(
                    result.get("group").and_then(Value::as_str),
                    Some("-") | Some("")
                );
                if remove {
                    result.remove("group");
                }
                Ok(None)
            }
            _ => Ok(None),
        }
    }
}

fn transform_value(
    spec: &TransformSpec,
    input: &str,
    existing: Option<&Value>,
) -> Result<Option<Value>, ParseError> {
    let v = match spec {
        TransformSpec::None => Some(Value::String(input.to_owned())),
        TransformSpec::Integer => integer_transform(input).map(|n| Value::Number(n.into())),
        TransformSpec::FirstInteger => {
            first_integer_transform(input).map(|n| Value::Number(n.into()))
        }
        TransformSpec::Boolean => Some(Value::Bool(true)),
        TransformSpec::Lowercase => Some(Value::String(input.to_lowercase())),
        TransformSpec::Uppercase => Some(Value::String(input.to_uppercase())),
        TransformSpec::Value(val) => {
            if val.contains("$1") {
                Some(Value::String(val.replace("$1", input)))
            } else {
                Some(Value::String(val.clone()))
            }
        }
        TransformSpec::RangeFunc => expand_number_range(input).map(|numbers| json!(numbers)),
        TransformSpec::RangeXOfYFunc => range_x_of_y_func(input).map(Value::Array),
        TransformSpec::ArrayInteger => {
            let item = integer_transform(input)
                .map(|n| Value::Number(n.into()))
                .unwrap_or(Value::Null);
            Some(Value::Array(vec![item]))
        }
        TransformSpec::UniqConcatValue(_) | TransformSpec::HdrFormat => {
            let v = match spec {
                TransformSpec::UniqConcatValue(value) => value.as_str(),
                _ if input.eq_ignore_ascii_case("HDR10") => "HDR10",
                _ if input.eq_ignore_ascii_case("HLG") => "HLG",
                _ => "HDR",
            };
            let mut existing_vec = existing
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let exists = existing_vec.iter().any(|e| e.as_str() == Some(v));
            if !exists {
                existing_vec.push(Value::String(v.to_owned()));
            }
            Some(Value::Array(existing_vec))
        }
        TransformSpec::TransformResolution => Some(Value::String(transform_resolution(input))),
        TransformSpec::Date(formats) => parse_date_with_formats(input, formats).map(Value::String),
    };
    Ok(v)
}

fn integer_transform(input: &str) -> Option<i64> {
    let digits: String = input.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse::<i64>().ok()
}

fn first_integer_transform(input: &str) -> Option<i64> {
    DIGITS_RE
        .find(input)
        .ok()
        .flatten()
        .and_then(|m| m.as_str().parse::<i64>().ok())
}

const MAX_EXPANDED_RANGE_ITEMS: i64 = 10_000;

fn is_numeric_title(title: &str) -> bool {
    let bare = title.trim_matches(|c: char| c.is_whitespace() || c == '.');
    // Keep a standalone four-digit identity; a leading zero is episode evidence.
    bare.len() == 4 && !bare.starts_with('0') && bare.bytes().all(|byte| byte.is_ascii_digit())
}

/// Expand marked numeric ranges and preserve explicit lists, with a bounded result.
pub fn expand_number_range(input: &str) -> Option<Vec<i64>> {
    let mut matches = DIGITS_RE.find_iter(input);
    let first = matches.next()?.ok()?;
    let mut previous = first.as_str().parse::<i64>().ok()?;
    let mut previous_end = first.end();
    let mut numbers = vec![previous];
    let mut unmarked_list = false;
    for next in matches {
        let next = next.ok()?;
        let number = next.as_str().parse::<i64>().ok()?;
        let separator = &input[previous_end..next.start()];
        let is_range = separator.contains(['-', '~', ':', 'à'])
            || separator.split(|c: char| !c.is_alphabetic()).any(|word| {
                ["a", "ao", "to"]
                    .iter()
                    .any(|marker| word.eq_ignore_ascii_case(marker))
            });
        if is_range {
            let length = number.checked_sub(previous)?;
            if length <= 0 || length > MAX_EXPANDED_RANGE_ITEMS - numbers.len() as i64 {
                return None;
            }
            numbers.extend(previous + 1..=number);
        } else {
            let is_list = separator.contains(['&', '+', ',', '/'])
                || ["s", "e", "ep", "x", "х", "and"].iter().any(|marker| {
                    separator
                        .trim_matches([' ', '.'])
                        .eq_ignore_ascii_case(marker)
                });
            let unmarked = separator.chars().all(|c| c.is_whitespace() || c == '.');
            if (!is_list && !unmarked) || numbers.len() >= MAX_EXPANDED_RANGE_ITEMS as usize {
                return None;
            }
            unmarked_list |= unmarked;
            numbers.push(number);
        }
        previous = number;
        previous_end = next.end();
    }
    // Two unmarked numbers can be a season/episode pair ("Season 11 01").
    // Preserve only unambiguous contiguous lists with at least three entries.
    if unmarked_list
        && (numbers.len() < 3
            || !numbers
                .windows(2)
                .all(|pair| pair[0].checked_add(1) == Some(pair[1])))
    {
        return None;
    }
    if numbers.len() > 1 {
        let mut seen = HashSet::with_capacity(numbers.len());
        numbers.retain(|n| seen.insert(*n));
    }
    Some(numbers)
}

fn site_tld_lang_code(tld: &str) -> Option<&'static str> {
    match tld {
        "nl" => Some("nl"),
        "fi" => Some("fi"),
        "se" => Some("sv"),
        "tel" => Some("te"),
        _ => None,
    }
}

fn post_process_result(raw_title: &str, result: &mut Map<String, Value>) -> Result<(), ParseError> {
    if let Some(Value::String(q)) = result.get("quality")
        && q == "REMUX"
        && BLURAY_HINT_RE
            .is_match(raw_title)
            .map_err(|e| ParseError::Regex(e.to_string()))?
    {
        result.insert(
            "quality".to_owned(),
            Value::String("BluRay REMUX".to_owned()),
        );
    }

    if MOVIE_EP_PREFIX_RE
        .is_match(raw_title)
        .map_err(|e| ParseError::Regex(e.to_string()))?
    {
        result.insert("episodes".to_owned(), Value::Array(vec![]));
    }

    let mut langs: Vec<String> = result
        .get("languages")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default();
    if langs.is_empty() {
        return Ok(());
    }

    if let Some(site) = result.get("site").and_then(Value::as_str) {
        let tld_raw = site
            .rsplit(['.', ' '])
            .next()
            .unwrap_or_default()
            .to_lowercase();
        let tld: String = tld_raw
            .chars()
            .filter(|c| c.is_ascii_alphabetic())
            .collect();
        if let Some(code) = site_tld_lang_code(&tld) {
            langs.retain(|l| l != code);
        }
    }

    // If `site` is not extracted, domain TLDs can still produce false language positives.
    for cap in RAW_SITE_TLD_RE
        .captures_iter(raw_title)
        .filter_map(Result::ok)
    {
        let Some(m) = cap.get(1) else { continue };
        let tld = m.as_str().to_lowercase();
        if let Some(code) = site_tld_lang_code(&tld) {
            langs.retain(|l| l != code);
        }
    }

    if langs.iter().any(|l| l == "de") {
        let has_de_hint = GERMAN_HINT_RE
            .is_match(raw_title)
            .map_err(|e| ParseError::Regex(e.to_string()))?;
        let has_de_code = DE_CODE_RE
            .is_match(raw_title)
            .map_err(|e| ParseError::Regex(e.to_string()))?;
        let code_count = UPPER_LANG_CODE_RE
            .find_iter(raw_title)
            .filter_map(Result::ok)
            .count();
        let keep_from_code_list = has_de_code && code_count >= 3;
        if !has_de_hint && !keep_from_code_list {
            langs.retain(|l| l != "de");
        }
    }

    if SCI_FI_RE
        .is_match(raw_title)
        .map_err(|e| ParseError::Regex(e.to_string()))?
    {
        langs.retain(|l| l != "fi");
    }

    // preserve insertion order while deduping
    let mut seen = HashSet::new();
    let mut deduped = Vec::new();
    for lang in langs {
        if seen.insert(lang.clone()) {
            deduped.push(Value::String(lang));
        }
    }
    result.insert("languages".to_owned(), Value::Array(deduped));
    Ok(())
}

fn range_x_of_y_func(input: &str) -> Option<Vec<Value>> {
    let numbers: Vec<i64> = DIGITS_RE
        .find_iter(input)
        .filter_map(Result::ok)
        .filter_map(|m| m.as_str().parse::<i64>().ok())
        .collect();
    if numbers.len() != 1 {
        return None;
    }
    if !(1..=MAX_EXPANDED_RANGE_ITEMS).contains(&numbers[0]) {
        return None;
    }
    Some((1..=numbers[0]).map(|n| Value::Number(n.into())).collect())
}

fn transform_resolution(input: &str) -> String {
    let lower = input.to_lowercase();
    if lower.contains("2160") || lower.contains("4k") {
        "2160p".to_owned()
    } else if lower.contains("1440") || lower.contains("2k") {
        "1440p".to_owned()
    } else if lower.contains("1080") {
        "1080p".to_owned()
    } else if lower.contains("720") {
        "720p".to_owned()
    } else if lower.contains("480") {
        "480p".to_owned()
    } else if lower.contains("360") {
        "360p".to_owned()
    } else {
        "240p".to_owned()
    }
}

fn parse_date_with_formats(input: &str, formats: &[String]) -> Option<String> {
    let sanitized = NON_WORD_RE.replace_all(input, " ").trim().to_owned();
    if sanitized.is_empty() {
        return None;
    }
    let normalized = convert_months(&sanitized);
    for fmt in formats {
        if let Some(date) = parse_date(&normalized, fmt) {
            return Some(date.format("%Y-%m-%d").to_string());
        }
    }
    None
}

fn parse_date(input: &str, fmt: &str) -> Option<NaiveDate> {
    let tokens: Vec<&str> = input.split_whitespace().collect();
    match fmt {
        "YYYYMMDD" => {
            let digits: String = input.chars().filter(|c| c.is_ascii_digit()).collect();
            if digits.len() != 8 {
                return None;
            }
            let y = digits[0..4].parse::<i32>().ok()?;
            let m = digits[4..6].parse::<u32>().ok()?;
            let d = digits[6..8].parse::<u32>().ok()?;
            NaiveDate::from_ymd_opt(y, m, d)
        }
        "YYYY MM DD" => {
            if tokens.len() < 3 {
                return None;
            }
            let y = tokens[0].parse::<i32>().ok()?;
            let m = tokens[1].parse::<u32>().ok()?;
            let d = tokens[2].parse::<u32>().ok()?;
            NaiveDate::from_ymd_opt(y, m, d)
        }
        "DD MM YYYY" => {
            if tokens.len() < 3 {
                return None;
            }
            let d = tokens[0].parse::<u32>().ok()?;
            let m = tokens[1].parse::<u32>().ok()?;
            let y = tokens[2].parse::<i32>().ok()?;
            NaiveDate::from_ymd_opt(y, m, d)
        }
        "MM DD YY" => {
            if tokens.len() < 3 {
                return None;
            }
            let m = tokens[0].parse::<u32>().ok()?;
            let d = tokens[1].parse::<u32>().ok()?;
            let y = parse_year_2(tokens[2])?;
            NaiveDate::from_ymd_opt(y, m, d)
        }
        "YY MM DD" => {
            if tokens.len() < 3 {
                return None;
            }
            let y = parse_year_2(tokens[0])?;
            let m = tokens[1].parse::<u32>().ok()?;
            let d = tokens[2].parse::<u32>().ok()?;
            NaiveDate::from_ymd_opt(y, m, d)
        }
        "DD MM YY" => {
            if tokens.len() < 3 {
                return None;
            }
            let d = tokens[0].parse::<u32>().ok()?;
            let m = tokens[1].parse::<u32>().ok()?;
            let y = parse_year_2(tokens[2])?;
            NaiveDate::from_ymd_opt(y, m, d)
        }
        "DD MMM YY" => {
            if tokens.len() < 3 {
                return None;
            }
            let d = parse_day(tokens[0])?;
            let m = month_to_num(tokens[1])?;
            let y = parse_year_2(tokens[2])?;
            NaiveDate::from_ymd_opt(y, m, d)
        }
        "DD MMM YYYY" | "Do MMM YYYY" | "Do MMMM YYYY" => {
            if tokens.len() < 3 {
                return None;
            }
            let d = parse_day(tokens[0])?;
            let m = month_to_num(tokens[1])?;
            let y = tokens[2].parse::<i32>().ok()?;
            NaiveDate::from_ymd_opt(y, m, d)
        }
        _ => None,
    }
}

fn parse_day(token: &str) -> Option<u32> {
    let cleaned = token.trim_end_matches(['s', 't', 'n', 'd', 'r', 'h']);
    cleaned.parse::<u32>().ok()
}

fn parse_year_2(token: &str) -> Option<i32> {
    let y = token.parse::<i32>().ok()?;
    if y < 100 {
        Some(if y <= 69 { 2000 + y } else { 1900 + y })
    } else {
        Some(y)
    }
}

fn month_to_num(token: &str) -> Option<u32> {
    let t = token.to_lowercase();
    if t.starts_with("jan") {
        Some(1)
    } else if t.starts_with("feb") {
        Some(2)
    } else if t.starts_with("mar") {
        Some(3)
    } else if t.starts_with("apr") {
        Some(4)
    } else if t.starts_with("may") {
        Some(5)
    } else if t.starts_with("jun") {
        Some(6)
    } else if t.starts_with("jul") {
        Some(7)
    } else if t.starts_with("aug") {
        Some(8)
    } else if t.starts_with("sep") {
        Some(9)
    } else if t.starts_with("oct") {
        Some(10)
    } else if t.starts_with("nov") {
        Some(11)
    } else if t.starts_with("dec") {
        Some(12)
    } else {
        None
    }
}

fn convert_months(input: &str) -> String {
    let mut out = input.to_owned();
    for (re, replacement) in MONTH_REGEXES.iter() {
        out = re.replace_all(&out, *replacement).to_string();
    }
    out
}

fn replace_all_in_place(text: &mut String, regex: &Regex, replacement: &str) {
    if let Cow::Owned(replaced) = regex.replace_all(text, replacement) {
        *text = replaced;
    }
}

fn replace_in_place(text: &mut String, regex: &Regex, replacement: &str) {
    if let Cow::Owned(replaced) = regex.replace(text, replacement) {
        *text = replaced;
    }
}

fn normalize_underscores(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut in_run = false;
    for character in input.chars() {
        if character == '_' {
            if !in_run {
                output.push(' ');
                in_run = true;
            }
        } else {
            output.push(character);
            in_run = false;
        }
    }
    output
}

fn clean_title(raw_title: &str) -> String {
    clean_title_impl::<true>(raw_title)
}

fn clean_title_impl<const GUARDED: bool>(raw_title: &str) -> String {
    let mut cleaned = raw_title.replace('_', " ");
    if !GUARDED || contains_ascii_fold(&cleaned, "movie") {
        replace_all_in_place(&mut cleaned, &MOVIE_REGEX, "");
    }
    if !GUARDED || !starts_ascii_word(&cleaned) || last_byte_in(&cleaned, b" -:/\\[|{(#$&^") {
        replace_all_in_place(&mut cleaned, &NOT_ALLOWED_SYMBOLS_AT_START_AND_END, "");
    }
    if !GUARDED || cleaned.ends_with(')') {
        replace_all_in_place(&mut cleaned, &RUSSIAN_CAST_REGEX, "");
    }
    if !GUARDED || cleaned.starts_with(['[', '【', '★']) {
        replace_in_place(&mut cleaned, &STAR_REGEX_1, "$1");
    }
    if !GUARDED || cleaned.ends_with([']', '】', '★']) {
        replace_in_place(&mut cleaned, &STAR_REGEX_2, "$1");
    }
    if !GUARDED || (!cleaned.is_ascii() && cleaned.contains(['/', '|'])) {
        replace_all_in_place(&mut cleaned, &ALT_TITLES_REGEX, "");
    }
    if !GUARDED
        || cleaned.ends_with(']')
        || (!starts_ascii_word(&cleaned) && !cleaned.starts_with('#'))
    {
        replace_all_in_place(
            &mut cleaned,
            &REMAINING_NOT_ALLOWED_SYMBOLS_AT_START_AND_END,
            "",
        );
    }
    if !GUARDED || cleaned.contains(['(', '[', '{']) {
        replace_all_in_place(&mut cleaned, &EMPTY_BRACKETS_REGEX, "");
    }
    if !GUARDED
        || cleaned
            .as_bytes()
            .get(cleaned.len().saturating_sub(3)..)
            .is_some_and(|suffix| suffix.eq_ignore_ascii_case(b"mp3"))
    {
        replace_all_in_place(&mut cleaned, &MP3_REGEX, "");
    }
    if !GUARDED || cleaned.contains(['(', '[', '{']) {
        replace_all_in_place(&mut cleaned, &PARANTHESES_WITHOUT_CONTENT, "");
    }
    if !GUARDED || cleaned.contains(['-', '+', '_', '{', '}', '[', ']']) {
        replace_all_in_place(&mut cleaned, &SPECIAL_CHAR_SPACING, "");
    }

    for (open, close) in [("{", "}"), ("[", "]"), ("(", ")")] {
        if cleaned.matches(open).count() != cleaned.matches(close).count() {
            cleaned = cleaned.replace(open, "").replace(close, "");
        }
    }

    if !cleaned.contains(' ') && cleaned.contains('.') {
        cleaned = cleaned.replace('.', " ");
    }

    cleaned = normalize_mixed_script_title(cleaned);
    if !has_latin(&cleaned)
        && raw_title.contains('/')
        && has_latin(raw_title)
        && has_non_english(raw_title)
    {
        let recovered = recover_latin_from_slash(raw_title);
        if !recovered.is_empty() {
            cleaned = recovered;
        }
    }

    if !GUARDED || last_byte_in(&cleaned, b" -:./\\") {
        replace_all_in_place(&mut cleaned, &REDUNDANT_SYMBOLS_AT_END, "");
    }
    if !GUARDED || has_collapsible_whitespace(&cleaned) {
        replace_all_in_place(&mut cleaned, &SPACING_REGEX, " ");
    }
    cleaned.trim().to_owned()
}

fn starts_ascii_word(input: &str) -> bool {
    input
        .as_bytes()
        .first()
        .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
}

fn last_byte_in(input: &str, bytes: &[u8]) -> bool {
    input
        .as_bytes()
        .last()
        .is_some_and(|last| bytes.contains(last))
}

fn contains_ascii_fold(input: &str, needle: &str) -> bool {
    input
        .as_bytes()
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

fn has_collapsible_whitespace(input: &str) -> bool {
    let mut previous_was_whitespace = false;
    for byte in input.bytes() {
        let whitespace = matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0c);
        if whitespace && (previous_was_whitespace || byte != b' ') {
            return true;
        }
        previous_was_whitespace = whitespace;
    }
    false
}

fn normalize_mixed_script_title(mut title: String) -> String {
    if !has_latin(&title) || !has_non_english(&title) {
        return title;
    }

    // Prefer the Latin alternative when title is in the form "Non-Latin (Latin)".
    if let Some(caps) = LATIN_IN_PARENS_RE.captures(&title).ok().flatten()
        && let Some(inside) = caps.get(1).map(|m| m.as_str())
        && has_latin(inside)
        && !has_non_english(inside)
    {
        let before = title.split('(').next().unwrap_or_default();
        if has_non_english(before) && !has_latin(before) {
            title = inside.to_owned();
        }
    }

    // For mixed slash titles like "Русский / English", keep Latin segments.
    if title.contains('/') {
        let latin_parts: Vec<String> = title
            .split('/')
            .map(str::trim)
            .filter(|p| has_latin(p))
            .map(ToOwned::to_owned)
            .collect();
        if !latin_parts.is_empty() {
            title = latin_parts.join(" / ");
        } else if let Some(last) = title.split('/').map(str::trim).rfind(|p| !p.is_empty()) {
            title = last.to_owned();
        }
    }

    // Drop parenthetical metadata that still contains non-English text after selecting Latin title parts.
    title = strip_non_english_parens(&title);

    // Strip leading non-Latin prefix when title later contains Latin.
    if let Some((idx, _)) = title.char_indices().find(|(_, c)| c.is_ascii_alphabetic()) {
        let prefix = &title[..idx];
        if has_non_english(prefix)
            && !has_latin(prefix)
            && prefix.chars().last().is_some_and(|c| !c.is_alphanumeric())
        {
            title = title[idx..]
                .trim_start_matches(|c: char| c.is_whitespace() || "-_[](){}./".contains(c))
                .to_owned();
        }
    }

    if !title.contains(' ') && title.contains('.') {
        title = title.replace('.', " ");
    }
    title
}

fn has_latin(s: &str) -> bool {
    let mut run = 0usize;
    for c in s.chars() {
        if c.is_ascii_alphabetic() {
            run += 1;
            if run >= 2 {
                return true;
            }
        } else {
            run = 0;
        }
    }
    false
}

fn is_non_english_char(c: char) -> bool {
    matches!(c as u32, 0x3040..=0x30ff | 0x3400..=0x4dbf | 0x4e00..=0x9fff | 0xf900..=0xfaff | 0xff66..=0xff9f | 0x0400..=0x04ff | 0x0600..=0x06ff | 0x0750..=0x077f | 0x0c80..=0x0cff | 0x0d00..=0x0d7f | 0x0e00..=0x0e7f)
}

fn has_non_english(s: &str) -> bool {
    s.chars().any(is_non_english_char)
}

fn recover_latin_from_slash(raw: &str) -> String {
    let after_first = raw
        .split_once('/')
        .map(|(_, rest)| rest.trim())
        .unwrap_or("");
    if after_first.is_empty() || !has_latin(after_first) {
        return String::new();
    }

    let mut candidate = strip_non_english_parens(after_first);
    candidate = PARENS_RE.replace_all(&candidate, "").to_string();
    let latin_parts: Vec<String> = candidate
        .split('/')
        .map(str::trim)
        .filter(|p| has_latin(p))
        .map(|p| {
            p.trim_matches(|c: char| "-_[](){}., ".contains(c))
                .to_owned()
        })
        .filter(|p| !p.is_empty())
        .collect();

    if latin_parts.is_empty() {
        return String::new();
    }

    let joined = latin_parts.join(" / ");
    let joined = if !joined.contains(' ') && joined.contains('.') {
        joined.replace('.', " ")
    } else {
        joined
    };
    SPACING_REGEX.replace_all(&joined, " ").trim().to_owned()
}

fn strip_non_english_parens(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let rest = &input[i..];
        if let Some(open_rel) = rest.find('(') {
            let open = i + open_rel;
            out.push_str(&input[i..open]);
            if let Some(close_rel) = input[open..].find(')') {
                let close = open + close_rel;
                let inner = &input[open + 1..close];
                if !(has_non_english(inner) && has_latin(input)) {
                    out.push_str(&input[open..=close]);
                }
                i = close + 1;
            } else {
                out.push_str(&input[open..]);
                break;
            }
        } else {
            out.push_str(rest);
            break;
        }
    }
    out
}

/// Explicit absolute anime episode range, e.g. E768-E1064 or EP768-EP1064.
/// Keep the endpoints rather than allocating hundreds of episode numbers.
fn absolute_episode_range(raw: &str) -> Option<(i64,i64)> {
    static RANGE: Lazy<Regex> = Lazy::new(|| Regex::new(
        r"(?i)(?:^|[^a-z0-9])(?:e|ep|episode)[ ._-]*([0-9]{1,5})[ ]*[-–][ ]*(?:e|ep|episode)?[ ._-]*([0-9]{1,5})(?:$|[^a-z0-9])"
    ).expect("valid absolute episode range regex"));
    let caps=RANGE.captures(raw).ok()??;
    let start=caps.get(1)?.as_str().parse::<i64>().ok()?;
    let end=caps.get(2)?.as_str().parse::<i64>().ok()?;
    if start==0||end<start||end-start>9999{return None;}
    Some((start,end))
}

/// A plain 01-12 can also be part of a title, a season, a volume or a year.
/// Expose only a candidate to the metadata matcher; it becomes an absolute
/// anime episode interval only after an exact AniDB-linked title is verified.
/// Never expand the whole interval into an episodes array.
const MAX_NEW_EPISODE_RANGE: i64 = 1_000;
static UNMARKED_ANIME_RANGE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(
    r"(?i)(?:^|[\s._(\[-])([0-9]{1,5})\s*[-~–—～]\s*([0-9]{1,5})(?=$|[^a-z0-9])"
).expect("valid unmarked anime range regex"));

static SEASON_EPISODE_RANGE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(
    r"(?i)(?:^|[^a-z0-9])s([0-9]{1,3})[ ._-]*e([0-9]{1,4})\s*[-~–—～]\s*(?:s([0-9]{1,3})[ ._-]*)?e?([0-9]{1,4})(?=$|[^a-z0-9])"
).expect("valid explicit season episode range regex"));

fn explicit_season_episode_range(raw: &str) -> Result<Option<(i64,i64,i64)>,ParseError> {
    for candidate in SEASON_EPISODE_RANGE_RE.captures_iter(raw) {
        let caps=candidate.map_err(|e|ParseError::Regex(e.to_string()))?;
        let nums=[1,2,4].map(|i|caps.get(i).and_then(|m|m.as_str().parse::<i64>().ok()));
        let (Some(season),Some(start),Some(end))=(nums[0],nums[1],nums[2]) else{continue;};
        let next_season=caps.get(3).and_then(|m|m.as_str().parse::<i64>().ok());
        if next_season.is_some_and(|s|s!=season){continue;}
        if (1..=120).contains(&season) && start>0 && end>=start
            && end-start < MAX_NEW_EPISODE_RANGE {
            return Ok(Some((season,start,end)));
        }
    }
    Ok(None)
}

/// Return a conservative interpretation of a numeric anime batch in a raw
/// release title. No metadata is available here, so this is intentionally not
/// written into the canonical episode_start/end fields.
fn unmarked_anime_episode_candidate(
    raw:&str,translate_languages:bool,
)->Result<Option<Value>,ParseError>{
    for candidate in UNMARKED_ANIME_RANGE_RE.captures_iter(raw){
        let caps=candidate.map_err(|e|ParseError::Regex(e.to_string()))?;
        let (Some(start_token),Some(end_token))=(caps.get(1),caps.get(2))else{continue;};
        let (Ok(start),Ok(end))=(start_token.as_str().parse::<i64>(),
            end_token.as_str().parse::<i64>()) else{continue;};
        if start==0 || end<=start || end-start>=MAX_NEW_EPISODE_RANGE {continue;}
        // Release years and technical dimensions are not anime episodes.
        if (1900..=2100).contains(&start)|| (1900..=2100).contains(&end)
            || [480,720,1080,1440,2160,4320].contains(&start)
            || [480,720,1080,1440,2160,4320].contains(&end) {continue;}
        let prefix=raw[..caps.get(0).expect("range match").start()]
            .trim_end_matches(|c:char|c.is_whitespace()||matches!(c,'.'|'_'|'-'|'('|'['));
        if prefix.is_empty(){continue;}
        let final_word=prefix.split(|c:char|!c.is_ascii_alphanumeric())
            .filter(|w|!w.is_empty()).last().unwrap_or_default();
        if ["season","seasons","saison","sezon","staffel","vol","volume",
            "volumes","part","parts","cour","disc","disk","cd","chapter","chapters",
            "chap"].iter().any(|word|final_word.eq_ignore_ascii_case(word)){
            continue;
        }
        // Require the candidate to stand at the end or precede ordinary
        // release metadata, not prose belonging to the anime title.
        let suffix=raw[end_token.end()..].trim_start_matches(|c:char|
            c.is_whitespace()||matches!(c,')'|']'|'.'|'_'|'-'));
        let suffix_lower=suffix.to_ascii_lowercase();
        if !suffix.is_empty()
            && !suffix.starts_with('[') && !suffix.starts_with('(')
            && !["batch","complete","pack","collection","ova","special","1080p",
                "720p","2160p","480p","web","bd","bdrip","bluray","mkv","mp4"]
                .iter().any(|tag|suffix_lower.starts_with(tag))
            && !suffix.starts_with('+') {continue;}
        let candidate_title=ENGINE.parse(prefix,translate_languages)?
            .get("title").and_then(Value::as_str).unwrap_or(prefix).trim().to_owned();
        if candidate_title.is_empty(){continue;}
        return Ok(Some(json!({
            "title":candidate_title, "start":start, "end":end,
            "numbering":"absolute","verified":false
        })));
    }
    Ok(None)
}

// Some common release conventions are more specific than the generic upstream
// handlers. Apply these corrections to the leaf *after* normal parsing so we
// never change the handler table or the fast regex prefilter.
static SEASON_WORD_COORDS_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:^|[ ._-])(?:seasons?|staffel|saison|seizoen|temporada|sezon|seson)[ ._-]+(\d{1,2})\b")
        .expect("valid explicit season word regex")
});
static SEASON_WORD_IN_TITLE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)[ ._-]+(?:seasons?|staffel|saison|seizoen|temporada|sezon|seson)(?:[ ._-]*\d{1,2})?")
        .expect("valid season word cleanup regex")
});
static X_COORDS_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:^|[ ._-])([1-9]|[1-3]\d|40)[xх](\d{1,3}(?:[xх]\d{1,3})*)(?=$|[^a-z0-9])")
        .expect("valid x notation regex")
});
static X_COORDS_IN_TITLE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)[ ._-]+\d{1,2}[xх]\d{1,3}(?:[xх]\d{1,3})*$")
        .expect("valid x notation title suffix regex")
});
static S_E_COORDS_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:^|[ ._-])s([1-9]\d{0,2})[ ._-]*e(\d{1,4})(?=$|[^0-9])")
        .expect("valid explicit SxxExxx regex")
});

fn patch_missing_coordinates(raw: &str, result: &mut Map<String, Value>) -> Result<(), ParseError> {
    // A word-based season number is different from an anime *episode* named
    // "Final Season - 05". Only accept the spaced-dash spelling when "Season"
    // is itself a separate release marker ("Show - Season - 04").
    if let Some(caps) = SEASON_WORD_COORDS_RE
        .captures(raw).map_err(|e| ParseError::Regex(e.to_string()))?
    {
        let matched = caps.get(0).expect("season match");
        let marker = matched.as_str().to_ascii_lowercase();
        let before = raw[..matched.start()].trim_end();
        let spaced_dash = marker.contains("season - ") || marker.contains("season -.");
        if !spaced_dash || before.ends_with('-') {
            if let Some(season) = caps.get(1).and_then(|m| m.as_str().parse::<i64>().ok())
                && (1..=99).contains(&season)
            {
                if result.get("seasons").and_then(Value::as_array).is_some_and(Vec::is_empty) {
                    result.insert("seasons".into(), json!([season]));
                }
                // Upstream sometimes classifies the season number as an episode.
                // Preserve an independent explicit episode such as S02E04.
                if spaced_dash && before.ends_with('-')
                    && !EXPLICIT_EPISODE_RE.is_match(raw).unwrap_or(false)
                    && result.get("episodes") == Some(&json!([season]))
                {
                    result.insert("episodes".into(), json!([]));
                }
                if let Some(Value::String(title)) = result.get_mut("title") {
                    let cleaned = SEASON_WORD_IN_TITLE_RE.replace_all(title, " ");
                    let cleaned = cleaned.trim_matches(&[' ', '-', '.', '_'][..]);
                    if !cleaned.is_empty() {
                        *title = cleaned.to_owned();
                    }
                }
            }
        }
    }

    // Uppercase Spanish 1X124 and chained 22X17X18 often bypass the upstream
    // case-sensitive season handler. A 4-digit width such as 1920x1080 must
    // never be interpreted as season/episode notation.
    if let Some(caps) = X_COORDS_RE
        .captures(raw).map_err(|e| ParseError::Regex(e.to_string()))?
    {
        let matched = caps.get(0).expect("x notation match");
        if raw[..matched.start()].chars().filter(|c| c.is_alphabetic()).count() >= 3 {
            let season = caps.get(1).and_then(|m| m.as_str().parse::<i64>().ok());
            let episodes: Vec<i64> = caps.get(2).into_iter()
                .flat_map(|m| m.as_str().split(|c| matches!(c, 'x' | 'X' | 'х' | 'Х')))
                .filter_map(|s| s.parse::<i64>().ok())
                .collect();
            if let Some(season) = season
                && !episodes.is_empty() && episodes.len() <= 16
                && episodes.iter().all(|&n| (1..=999).contains(&n))
            {
                if result.get("seasons").and_then(Value::as_array).is_some_and(Vec::is_empty) {
                    result.insert("seasons".into(), json!([season]));
                }
                // Explicit x-notation is more reliable than a number in the
                // show's name (for example "911 Nashville 1X5").
                result.insert("episodes".into(), json!(episodes));
                if let Some(Value::String(title)) = result.get_mut("title") {
                    let cleaned = X_COORDS_IN_TITLE_RE.replace(title, "");
                    let cleaned = cleaned.trim_matches(&[' ', '-', '.', '_'][..]);
                    if !cleaned.is_empty() {
                        *title = cleaned.to_owned();
                    }
                }
            }
        }
    }

    // Defensively restore a missing season in unambiguous S51E12 notation;
    // existing ranges and more specific parsed episode lists are untouched.
    if let Some(caps) = S_E_COORDS_RE
        .captures(raw).map_err(|e| ParseError::Regex(e.to_string()))?
    {
        let season = caps.get(1).and_then(|m| m.as_str().parse::<i64>().ok());
        let episode = caps.get(2).and_then(|m| m.as_str().parse::<i64>().ok());
        if let (Some(season), Some(episode)) = (season, episode)
            && (1..=120).contains(&season) && (1..=9999).contains(&episode)
        {
            if result.get("seasons").and_then(Value::as_array).is_some_and(Vec::is_empty) {
                result.insert("seasons".into(), json!([season]));
            }
            if result.get("episodes").and_then(Value::as_array).is_some_and(Vec::is_empty) {
                result.insert("episodes".into(), json!([episode]));
            }
        }
    }
    Ok(())
}

// Explicit CJK episode marker, e.g. "[第41集]". It can denote an
// absolute series episode while the file supplies S02E41. Preserve it as a
// *candidate*, never as a verified season/episode coordinate: only the matcher
// can resolve it through a trusted AniDB/TVDB crosswalk.
// Explicit Japanese/Chinese/Korean episode words are *absolute* episode
// hints, not an instruction to rewrite a season/episode coordinate. The
// database matcher can only convert them through a verified anime crosswalk.
static EAST_ASIAN_EPISODE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?:第\s*([1-9][0-9]{0,4})\s*[集話话]|제\s*([1-9][0-9]{0,4})\s*[화회])")
        .expect("valid East Asian episode marker")
});

fn marked_absolute_episode_candidate(raw: &str) -> Result<Option<i64>, ParseError> {
    let Some(caps) = EAST_ASIAN_EPISODE_RE
        .captures(raw).map_err(|e| ParseError::Regex(e.to_string()))? else {
        return Ok(None);
    };
    Ok(caps.get(1).or_else(|| caps.get(2))
        .and_then(|value| value.as_str().parse::<i64>().ok()))
}

// Rakun-style fansub episode markers are explicit only in a release context.
// A bare "Room #5" or "Movie #05" must remain a title, not a TV episode.
static FAN_SUB_HASH_EPISODE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:^|[\s._-])#([0-9]{1,4})(?:v[1-9][0-9]?)?(?=$|[^a-z0-9])")
        .expect("valid fansub hash episode marker")
});

fn fansub_hash_episode(raw: &str, result: &Map<String, Value>)
    -> Result<Option<(i64, usize)>, ParseError>
{
    if S_E_COORDS_RE.is_match(raw).map_err(|e| ParseError::Regex(e.to_string()))?
        || SEASON_EPISODE_RANGE_RE.is_match(raw).map_err(|e| ParseError::Regex(e.to_string()))?
    {
        return Ok(None);
    }
    for caps in FAN_SUB_HASH_EPISODE_RE.captures_iter(raw) {
        let caps = caps.map_err(|e| ParseError::Regex(e.to_string()))?;
        let (Some(marker), Some(number)) = (caps.get(0), caps.get(1)) else {
            continue;
        };
        let Some(episode) = number.as_str().parse::<i64>().ok() else {
            continue;
        };
        if !(1..=9999).contains(&episode) {
            continue;
        }
        let prefix = raw[..marker.start()].trim_end();
        let last_word = prefix.split(|c: char| !c.is_alphabetic())
            .filter(|word| !word.is_empty()).next_back().unwrap_or_default();
        if ["movie", "film", "part", "volume", "vol", "chapter", "season"]
            .iter().any(|word| last_word.eq_ignore_ascii_case(word))
        {
            continue;
        }
        let looks_like_fansub = raw.trim_start().starts_with(['[', '【'])
            || prefix.ends_with('-');
        let has_title = result.get("title").and_then(Value::as_str)
            .is_some_and(|title| title.chars().filter(|c| c.is_alphabetic()).count() >= 3);
        if looks_like_fansub && has_title {
            return Ok(Some((episode, marker.start())));
        }
    }
    Ok(None)
}

// A labelled release range is stronger than an unmarked "01-12" candidate.
// Preserve the two endpoints without allocating a possibly huge episode list.
// A season marker, when present, determines season-relative numbering.
static LABELED_EPISODE_RANGE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)(?:^|[^a-z0-9])(?:e(?:p(?:s|isodes?)?)?|epis[oó]dios?)\s*[.:#_-]?\s*([0-9]{1,4})\s*[-~–—～&]\s*(?:e(?:p(?:s|isodes?)?)?|epis[oó]dios?)?\s*[.:#_-]?\s*([0-9]{1,4})(?=$|[^a-z0-9])"
    ).expect("valid labelled episode range")
});

fn labeled_episode_range(raw: &str) -> Result<Option<(i64, i64, usize)>, ParseError> {
    for caps in LABELED_EPISODE_RANGE_RE.captures_iter(raw) {
        let caps = caps.map_err(|e| ParseError::Regex(e.to_string()))?;
        let (Some(marker), Some(first), Some(last)) =
            (caps.get(0), caps.get(1), caps.get(2)) else { continue; };
        let (Ok(start), Ok(end)) = (first.as_str().parse::<i64>(),
            last.as_str().parse::<i64>()) else { continue; };
        if start > 0 && end >= start && end - start < MAX_NEW_EPISODE_RANGE {
            return Ok(Some((start, end, marker.start())));
        }
    }
    Ok(None)
}

fn set_title_from_release_prefix(
    raw: &str, marker_start: usize, translate_languages: bool,
    result: &mut Map<String, Value>
) -> Result<(), ParseError> {
    let prefix = raw[..marker_start]
        .trim_end_matches(|c: char| c.is_whitespace() || matches!(c, '.' | '_' | '-'));
    if prefix.is_empty() {
        return Ok(());
    }
    let clean = ENGINE.parse(prefix, translate_languages)?;
    if let Some(title) = clean.get("title").and_then(Value::as_str)
        && title.chars().filter(|c| c.is_alphabetic()).count() >= 3
    {
        result.insert("title".into(), json!(title));
    }
    Ok(())
}

// Parse each directory entry independently. The path-level merge below
// never concatenates release names (which could confuse episode coordinates).
fn parse_path_component(raw: &str, translate_languages: bool) -> Result<Map<String,Value>,ParseError> {
    let mut result=ENGINE.parse(raw,translate_languages)?;
    if let Some(episode)=marked_absolute_episode_candidate(raw)? {
        result.insert("absolute_episode_candidate".into(), json!(episode));
    }
    if let Some((start,end))=absolute_episode_range(raw){
        result.insert("episode_start".into(),json!(start));
        result.insert("episode_end".into(),json!(end));
        result.insert("episode_count".into(),json!(end-start+1));
        result.insert("episode_range_numbering".into(),json!("absolute"));
    }
    patch_missing_coordinates(raw,&mut result)?;
    if let Some((season,start,end))=explicit_season_episode_range(raw)?{
        result.insert("seasons".into(),json!([season]));
        result.insert("episodes".into(),json!([start]));
        result.insert("episode_start".into(),json!(start));
        result.insert("episode_end".into(),json!(end));
        result.insert("episode_count".into(),json!(end-start+1));
        result.insert("episode_range_numbering".into(),json!("season"));
    } else if !result.contains_key("episode_start") {
        if let Some((start, end, marker_start)) = labeled_episode_range(raw)? {
            // "Episodio 93~94" / "Ep01~Ep07" has explicit episode syntax.
            // A plain 01-07 still uses metadata-verified anime candidates.
            let has_season = result.get("seasons").and_then(Value::as_array)
                .is_some_and(|seasons| !seasons.is_empty());
            result.insert("episodes".into(), json!([start]));
            result.insert("episode_start".into(), json!(start));
            result.insert("episode_end".into(), json!(end));
            result.insert("episode_count".into(), json!(end - start + 1));
            result.insert("episode_range_numbering".into(),
                json!(if has_season { "season" } else { "absolute" }));
            set_title_from_release_prefix(raw, marker_start, translate_languages, &mut result)?;
        } else if let Some(candidate)=unmarked_anime_episode_candidate(raw,translate_languages)? {
            result.insert("anime_episode_range_candidate".into(), candidate);
        }
    }
    // "#05" is an explicit fansub episode marker, not a CRC tag or a
    // standalone movie/part number. Do not override a parsed SxxExx episode.
    if !result.contains_key("episode_start")
        && let Some((episode, marker_start)) = fansub_hash_episode(raw, &result)?
    {
        let existing = result.get("episodes").and_then(Value::as_array);
        if existing.is_none_or(|values| values.is_empty() ||
            (values.len() == 1 && values[0].as_i64() == Some(episode)))
        {
            result.insert("episodes".into(), json!([episode]));
            set_title_from_release_prefix(raw, marker_start, translate_languages, &mut result)?;
        }
    }
    Ok(result)
}

fn meaningful_path_value(v:&Value)->bool{
    match v {
        Value::Null=>false,
        Value::String(s)=>!s.trim().is_empty(),
        Value::Array(a)=>!a.is_empty(),
        Value::Object(o)=>!o.is_empty(),
        _=>true,
    }
}

fn path_has_coordinates(layer:&Map<String,Value>, field:&str)->bool{
    layer.get(field).is_some_and(meaningful_path_value)
}

fn merge_path_metadata(merged:&mut Map<String,Value>, layer:&Map<String,Value>){
    let new_episodes=path_has_coordinates(layer,"episodes")
        ||path_has_coordinates(layer,"episode_start")
        ||path_has_coordinates(layer,"anime_episode_range_candidate")
        ||path_has_coordinates(layer,"absolute_episode_candidate");
    let new_season=path_has_coordinates(layer,"seasons");
    if new_episodes||new_season {
        // A deeper episode overrides a parent batch's range. A parent's
        // East Asian absolute hint is retained ONLY when it agrees with the
        // child coordinate: e.g. 第41集/Show.S02E41 can be crosswalk-resolved,
        // but 第41集/Show.S02E05 cannot claim an absolute episode 41.
        let keep_parent_absolute = merged.get("absolute_episode_candidate")
            .and_then(Value::as_i64).is_some_and(|candidate|
                layer.get("episodes").and_then(Value::as_array)
                    .is_some_and(|eps| eps.len() == 1
                        && eps[0].as_i64() == Some(candidate)));
        for key in ["episode_start","episode_end","episode_count",
            "episode_range_numbering","anime_episode_range_candidate"] {
            merged.remove(key);
        }
        if !keep_parent_absolute {
            merged.remove("absolute_episode_candidate");
        }
        merged.remove("episodes");
    }
    // An empty value from the generic parser means "not detected", not an
    // instruction to erase metadata that was detected in an ancestor.
    for (key,value) in layer {
        if key!="title"&&meaningful_path_value(value){
            merged.insert(key.clone(),value.clone());
        }
    }
}

fn generic_path_title(title:&str)->bool{
    let lowercase=title.trim().to_lowercase();
    // A leaf like "S6E1-8 ..." can be reduced by the upstream parser to
    // just "S". It is a coordinate marker, not a stronger title than its
    // descriptive parent directory.
    if lowercase.len()==1 && lowercase.bytes().all(|b|b.is_ascii_alphabetic()){
        return true;
    }
    lowercase.is_empty()||[
        "episode","ep","season","seasons","season 1","series",
        "shows","tv","tv shows","movies","films","media","videos",
        "downloads","download","archive","library","complete",
        "extras","specials","subtitles"
    ].contains(&lowercase.as_str())
}

/// Parse a hierarchical torrent path from ancestor to leaf. Any explicit,
/// non-empty metadata value on a deeper level overrides the same field from
/// an ancestor (year, season, episode, quality, resolution, codec, etc.).
/// Title is NOT merged: use the leaf's meaningful series title, otherwise
/// inherit the nearest meaningful parent title.
pub fn parse_title(
    raw_title: &str,
    translate_languages: bool,
) -> Result<Map<String, Value>, ParseError> {
    let components:Vec<&str>=raw_title.split(&['/','\\'][..])
        .filter(|part|!part.is_empty() && *part!="." && *part!="..").collect();
    let mut merged=Map::new();
    let mut fallback_parent_title:Option<String>=None;
    // Preserve title candidates separately. The parser cannot determine if
    // a title is anime; the matcher may use a verified AniDB *part* alias in
    // a subfolder, without changing the ordinary series/movie title.
    let mut parent_title_candidates:Vec<String>=Vec::new();
    let mut leaf_title=String::new();
    let mut leaf_title_generic=false;
    for (index,component) in components.iter().enumerate(){
        let layer=parse_path_component(component,translate_languages)?;
        if let Some(title)=layer.get("title").and_then(Value::as_str){
            let title=title.trim();
            if index+1==components.len(){
                // A standalone title is preserved even if it happens to be
                // named "Season" or "Episode". Only use the parent when one
                // exists and the leaf title is empty or generic.
                leaf_title=title.to_owned();
                leaf_title_generic=generic_path_title(title);
            }else if !generic_path_title(title){
                fallback_parent_title=Some(title.to_owned());
                parent_title_candidates.push(title.to_owned());
            }
        }
        merge_path_metadata(&mut merged,&layer);
    }
    let title=if !leaf_title.is_empty()&&!leaf_title_generic{
        leaf_title
    }else{
        fallback_parent_title.unwrap_or(leaf_title)
    };
    if !title.is_empty(){merged.insert("title".into(),Value::String(title));}
    if !parent_title_candidates.is_empty() {
        merged.insert("path_title_candidates".into(),json!(parent_title_candidates));
    }
    // The upstream parser exposes empty season/episode arrays even when
    // nothing was recognized. Keep this established /parse API behavior,
    // but never let an empty layer override populated ancestor metadata.
    merged.entry("seasons").or_insert_with(||json!([]));
    merged.entry("episodes").or_insert_with(||json!([]));
    Ok(merged)
}

pub fn parse_title_context(
    raw_title: &str,
    translate_languages: bool,
) -> Result<ParseContext, ParseError> {
    ENGINE.parse_context(raw_title, translate_languages)
}

pub fn parse_many<'a, I>(
    titles: I,
    translate_languages: bool,
) -> Result<Vec<Map<String, Value>>, ParseError>
where
    I: IntoIterator<Item = &'a str>,
{
    const MIN_TITLES_PER_WORKER: usize = 16;

    let titles: Vec<_> = titles.into_iter().collect();
    let available_workers = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1);
    let worker_count = available_workers.min(titles.len().div_ceil(MIN_TITLES_PER_WORKER));
    if worker_count <= 1 {
        return titles
            .into_iter()
            .map(|title| parse_title(title, translate_languages))
            .collect();
    }

    let chunk_size = titles.len().div_ceil(worker_count);
    let mut output: Vec<Option<Map<String, Value>>> = vec![None; titles.len()];
    std::thread::scope(|scope| {
        let handles: Vec<_> = titles
            .chunks(chunk_size)
            .zip(output.chunks_mut(chunk_size))
            .map(|(input, output)| {
                scope.spawn(move || {
                    for (title, slot) in input.iter().zip(output) {
                        *slot = Some(parse_title(title, translate_languages)?);
                    }
                    Ok::<_, ParseError>(())
                })
            })
            .collect();
        for handle in handles {
            handle
                .join()
                .map_err(|_| ParseError::Data("batch parser worker panicked".to_owned()))??;
        }
        Ok::<_, ParseError>(())
    })?;
    Ok(output
        .into_iter()
        .map(|item| item.expect("successful worker filled every output slot"))
        .collect())
}

pub fn clean_title_native(raw_title: &str) -> String {
    clean_title(raw_title)
}

pub fn translate_langs_codes(langs: &[String]) -> Vec<String> {
    langs
        .iter()
        .filter_map(|lang| {
            LANGUAGES_TRANSLATION_TABLE
                .get(lang.as_str())
                .map(|value| (*value).to_owned())
        })
        .collect()
}

pub fn languages_translation_table() -> Vec<(String, String)> {
    LANGUAGES_TRANSLATION_TABLE
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

static HANDLERS_JSON: &str = include_str!("generated/handlers.json");
static ENGINE: Lazy<ParserEngine> =
    Lazy::new(|| ParserEngine::from_json(HANDLERS_JSON).expect("valid generated handler table"));

static BEFORE_TITLE_MATCH_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^\[([^\[\]]+)\]").expect("valid regex"));
static DIGITS_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\d+").expect("valid regex"));
static EXPLICIT_EPISODE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:[st]\d{1,3}(?:[ ._-]*e\d{1,4})?|e(?:p(?:isode)?)?[ ._-]*\d{1,4})\b")
        .expect("valid regex")
});
static NON_WORD_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\W+").expect("valid regex"));
static LATIN_IN_PARENS_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\(([^()]*)\)").expect("valid regex"));
static PARENS_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\([^()]*\)").expect("valid regex"));
static HANDLE_VOLUMES_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\bvol(?:ume)?[. -]*(\d{1,2})").expect("valid regex"));
static HANDLE_EPISODES_BEGINNING_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)(?:[ .]+-[ .]+|[\[(][ .]*)(\d{1,4})(?:a|b|v\d|\.\d)?(?:\W|$)(?!movie|film|\d+)",
    )
    .expect("valid regex")
});
static HANDLE_EPISODES_MIDDLE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)^(?:[\[(\-][ .]?)?(\d{1,4})(?:a|b|v\d)?(?:\W|$)(?!movie|film)(?!\[(480|720|1080)\])",
    )
    .expect("valid regex")
});
static ANIME_SPECIAL_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"One.*?Piece|Bleach|Naruto").expect("valid regex"));
static ANIME_EP_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b\d{1,4}\b").expect("valid regex"));
static INFER_PT_EP_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)capitulo|ao").expect("valid regex"));
static INFER_PT_TITLE_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)dublado").expect("valid regex"));

const NON_ENGLISH_CHARS: &str = "\u{3040}-\u{30ff}\u{3400}-\u{4dbf}\u{4e00}-\u{9fff}\u{f900}-\u{faff}\u{ff66}-\u{ff9f}\u{0400}-\u{04ff}\u{0600}-\u{06ff}\u{0750}-\u{077f}\u{0c80}-\u{0cff}\u{0d00}-\u{0d7f}\u{0e00}-\u{0e7f}";
static RUSSIAN_CAST_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\([^)]*[\u0400-\u04ff][^)]*\)$|/.*\(.*\)$").expect("valid regex"));
static ALT_TITLES_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(&format!(
        r"[^/|(]*[{NON_ENGLISH_CHARS}][^/|]*[/|]|[/|][^/|(]*[{NON_ENGLISH_CHARS}][^/|]*"
    ))
    .expect("valid regex")
});
static NOT_ALLOWED_SYMBOLS_AT_START_AND_END: Lazy<Regex> = Lazy::new(|| {
    Regex::new(&format!(
        r"^[^\w{NON_ENGLISH_CHARS}#\[【★]+|[ \-:/\\\[\|{{(#$&^]+$"
    ))
    .expect("valid regex")
});
static REMAINING_NOT_ALLOWED_SYMBOLS_AT_START_AND_END: Lazy<Regex> =
    Lazy::new(|| Regex::new(&format!(r"^[^\w{NON_ENGLISH_CHARS}#]+|]$")).expect("valid regex"));
static REDUNDANT_SYMBOLS_AT_END: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"[ \-:./\\]+$").expect("valid regex"));
static EMPTY_BRACKETS_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\(\s*\)|\[\s*\]|\{\s*\}").expect("valid regex"));
static PARANTHESES_WITHOUT_CONTENT: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\(\W*\)|\[\W*\]|\{\W*\}").expect("valid regex"));
static MOVIE_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)(?:\[|\()movie(?:\)|\])").expect("valid regex"));
static STAR_REGEX_1: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^(?:\[|【|★).*(?:\]|】|★)[ .]?(.+)").expect("valid regex"));
static STAR_REGEX_2: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(.+)[ .]?(?:\[|【|★).*(?:\]|】|★)$").expect("valid regex"));
static MP3_REGEX: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\bmp3$").expect("valid regex"));
static SPACING_REGEX: Lazy<Regex> = Lazy::new(|| Regex::new(r"\s+").expect("valid regex"));
static SPECIAL_CHAR_SPACING: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"[\-\+\_\{\}\[\]]\W{2,}").expect("valid regex"));
static BLURAY_HINT_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\bblu[ .-]*ray\b").expect("valid regex"));
static MOVIE_EP_PREFIX_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\b(?:movie|film)\s*-\s*\d+\s*-").expect("valid regex"));
static SCI_FI_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\bsci[ .-]?fi\b").expect("valid regex"));
static GERMAN_HINT_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:GER|DEU|german|alem[aã]o|deutsch)\b|(?<=\.)de(?=\.)")
        .expect("valid regex")
});
static RAW_SITE_TLD_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\bwww\.[a-z0-9_-]+\.(nl|fi|se|tel)\b").expect("valid regex"));
static DE_CODE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\bDE\b").expect("valid regex"));
static UPPER_LANG_CODE_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\b[A-Z]{2,3}\b").expect("valid regex"));

const MONTH_MAPPING: &[(&str, &str)] = &[
    (r"\bJanu\b", "Jan"),
    (r"\bFebr\b", "Feb"),
    (r"\bMarc\b", "Mar"),
    (r"\bApri\b", "Apr"),
    (r"\bMay\b", "May"),
    (r"\bJune\b", "Jun"),
    (r"\bJuly\b", "Jul"),
    (r"\bAugu\b", "Aug"),
    (r"\bSept\b", "Sep"),
    (r"\bOcto\b", "Oct"),
    (r"\bNove\b", "Nov"),
    (r"\bDece\b", "Dec"),
];
static MONTH_REGEXES: Lazy<Vec<(Regex, &'static str)>> = Lazy::new(|| {
    MONTH_MAPPING
        .iter()
        .map(|(pat, replacement)| {
            (
                Regex::new(&format!("(?i){pat}")).expect("valid month regex"),
                *replacement,
            )
        })
        .collect()
});

static LANGUAGES_TRANSLATION_TABLE: Lazy<HashMap<&'static str, &'static str>> = Lazy::new(|| {
    HashMap::from([
        ("en", "English"),
        ("ja", "Japanese"),
        ("zh", "Chinese"),
        ("ru", "Russian"),
        ("ar", "Arabic"),
        ("pt", "Portuguese"),
        ("es", "Spanish"),
        ("fr", "French"),
        ("de", "German"),
        ("it", "Italian"),
        ("ko", "Korean"),
        ("hi", "Hindi"),
        ("bn", "Bengali"),
        ("pa", "Punjabi"),
        ("mr", "Marathi"),
        ("gu", "Gujarati"),
        ("ta", "Tamil"),
        ("te", "Telugu"),
        ("kn", "Kannada"),
        ("ml", "Malayalam"),
        ("th", "Thai"),
        ("vi", "Vietnamese"),
        ("id", "Indonesian"),
        ("tr", "Turkish"),
        ("he", "Hebrew"),
        ("fa", "Persian"),
        ("uk", "Ukrainian"),
        ("el", "Greek"),
        ("lt", "Lithuanian"),
        ("lv", "Latvian"),
        ("et", "Estonian"),
        ("pl", "Polish"),
        ("cs", "Czech"),
        ("sk", "Slovak"),
        ("hu", "Hungarian"),
        ("ro", "Romanian"),
        ("bg", "Bulgarian"),
        ("sr", "Serbian"),
        ("hr", "Croatian"),
        ("sl", "Slovenian"),
        ("nl", "Dutch"),
        ("da", "Danish"),
        ("fi", "Finnish"),
        ("sv", "Swedish"),
        ("no", "Norwegian"),
        ("ms", "Malay"),
        ("la", "Latino"),
    ])
});

fn adult_keywords() -> impl Iterator<Item = &'static str> {
    include_str!("../data/combined-keywords.txt")
        .lines()
        .map(str::trim)
        .filter(|keyword| !keyword.is_empty())
}

static ADULT_KEYWORDS: Lazy<AhoCorasick> = Lazy::new(|| {
    AhoCorasickBuilder::new()
        .ascii_case_insensitive(true)
        .build(adult_keywords())
        .expect("valid adult keyword automaton")
});

#[cfg(test)]
mod tests {
    use super::*;

    const EQUIVALENCE_TITLES: &[&str] = &[
        "Deadpool 2016 1080p BluRay x264 DTS-JYK",
        "The.Walking.Dead.S05E03.720p.WEB-DL.x264-ASAP[ettv]",
        "Game.of.Thrones.S01-S08.COMPLETE.1080p.WEB-DL.DDP5.1.H.264-GoT",
        "[SubsPlease] Sousou no Frieren - 28 (1080p) [F1FF71EB].mkv",
        "Oppenheimer.2023.IMAX.2160p.UHD.BluRay.REMUX.DV.HDR10.HEVC.TrueHD.7.1.Atmos-FGT",
        "Spider-Man.No.Way.Home.2021.2160p.WEB-DL.DDP5.1.Atmos.HDR.HEVC-EVO[TGx]",
        "One Piece - 1071 (1080p) [Multi-Subs] [Dual-Audio] [x264]",
        "www.Torrenting.com - Anatomy Of A Fall (2023) 1080p WEBRip x265 10bit AAC",
        "Mission.Impossible.Pentalogy.1996-2015.1080p.BluRay.x264.AAC.5.1",
        "Мстители: Война бесконечности / Avengers: Infinity War (2018) BDRip 1080p",
        "(Hi10)_Re_Zero_Shin_Henshuu-ban_-_02v2_(720p)_(DDY)_(72006E34).mkv",
        "Movie remaſtered uncenſored 720p",
        "Kelvin K sign 1080p WEB-DL",
        "Show.S02E04.2024.06.12.GERMAN.DL.2160p.WEB.H265.DV.HDR.DDP5.1",
        "Title.2024.1080p.BluRay.REMUX.DTS-HD.MA.7.1.TrueHD.Atmos.10bit",
        "Title S01E01-E08 COMPLETE MULTi DUBBED SUBBED 1080p NF WEB-DL",
        "Wonder.Woman.1984.2020.3D.1080p.BluRay.x264-SURCODE[rarbg]",
        "Movie.vietnamese.1080p.WEB-DL",
        "Movie.norwegian.1080p.BluRay",
        "004 - Male Unbonding - [DVD].avi",
        " not to be present for ",
        "",
    ];

    #[test]
    fn prefilter_is_equivalent_to_full_handler_scan() {
        for title in EQUIVALENCE_TITLES {
            let filtered = ENGINE
                .parse_context_impl::<true>(title, false)
                .expect("filtered parse");
            let exhaustive = ENGINE
                .parse_context_impl::<false>(title, false)
                .expect("exhaustive parse");
            assert_eq!(filtered.result, exhaustive.result, "{title:?}");
            assert_eq!(
                filtered.working_title, exhaustive.working_title,
                "{title:?}"
            );
            assert_eq!(filtered.end_of_title, exhaustive.end_of_title, "{title:?}");
        }

        let fragments = [
            "Movie",
            "S01E02",
            "1080p",
            "BluRay",
            "WEB-DL",
            "REMUX",
            "x265",
            "10bit",
            "DTS-HD",
            "Atmos",
            "MULTi",
            "sample",
            "trailer",
            "COMPLETE",
            "www.site.com",
            "XXX",
            "[Group]",
            "Мстители",
            "日本語",
            "remaſtered",
            "K",
            "2024",
            ".mkv",
            "-",
            "_",
            " ",
        ];
        let separators = [".", " ", "-", "_", "/", "[", "]"];
        let mut state = 0x9e37_79b9_u32;
        for _ in 0..256 {
            let mut title = String::new();
            for _ in 0..2 + state as usize % 7 {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                title.push_str(fragments[state as usize % fragments.len()]);
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                title.push_str(separators[state as usize % separators.len()]);
            }
            let filtered = ENGINE
                .parse_context_impl::<true>(&title, false)
                .expect("filtered parse");
            let exhaustive = ENGINE
                .parse_context_impl::<false>(&title, false)
                .expect("exhaustive parse");
            assert_eq!(filtered.result, exhaustive.result, "{title:?}");
            assert_eq!(
                filtered.working_title, exhaustive.working_title,
                "{title:?}"
            );
            assert_eq!(filtered.end_of_title, exhaustive.end_of_title, "{title:?}");
        }
    }

    #[test]
    fn clean_title_guards_are_equivalent() {
        for title in EQUIVALENCE_TITLES {
            assert_eq!(
                clean_title_impl::<true>(title),
                clean_title_impl::<false>(title),
                "{title:?}"
            );
        }
    }

    #[test]
    fn embedded_e_release_codes_do_not_invent_enormous_episodes(){
        let potion=parse_title(
            "Potion danomi de Ikinobimasu 04 480p Multiple Subtitle 3E121247 Erai raws TGx",false).unwrap();
        assert!(!potion["episodes"].as_array().unwrap().contains(&json!(121247)));
        assert!(potion["title"].as_str().unwrap().to_lowercase().contains("potion"));
        let diary=parse_title("survive - Scumbag sage's diary (2020) [RE301085]",false).unwrap();
        assert!(diary["episodes"].as_array().unwrap().is_empty());
        assert!(diary["title"].as_str().unwrap().to_lowercase().contains("survive"));
        let the100=parse_title("The100.s01",false).unwrap();
        assert!(the100["title"].as_str().unwrap().to_lowercase().contains("the100"),
            "title should not be truncated to Th: {the100:?}");
        assert_eq!(the100["seasons"],json!([1]));
        assert!(the100["episodes"].as_array().unwrap().is_empty());
        // Do not break legitimately embedded season-episode coordinates.
        let explicit=parse_title("Show.S01E06.1080p",false).unwrap();
        assert_eq!(explicit["seasons"],json!([1]));
        assert_eq!(explicit["episodes"],json!([6]));
        let bare=parse_title("Show.E06.1080p",false).unwrap();
        assert_eq!(bare["episodes"],json!([6]));
    }

    #[test]
    fn weak_season_leaf_inherits_descriptive_parent_title(){
        let raw="Hercules: The Legendary Journeys / S6E1-8 (8) (Bruce Campbell, Garth Maxwell, Charles Siebert) [1999, 3 DVD9 + DVD5 (Custom)] MVO + Original";
        let result=parse_title(raw,false).unwrap();
        assert!(result["title"].as_str().unwrap().to_lowercase().contains("hercules"),
            "coordinate-only leaf should not override its descriptive parent: {result:?}");
        assert_eq!(result["seasons"],json!([6]));
    }

    #[test]
    fn matching_parent_folder_supplies_missing_series_year() {
        let full="Doctor.Who.2005.S01.REPACK.1080p.AMZN.WEB-DL.DDP2.0.H.264-KHEZU/Doctor.Who.S01E06.Dalek.1080p.AMZN.WEB-DL.DDP2.0.H.264-KHEZU.mkv";
        let leaf=parse_title(full.rsplit('/').next().unwrap(),false).unwrap();
        assert!(leaf.get("year").is_none(),"episode file alone has no year");
        let parsed=parse_title(full,false).unwrap();
        assert_eq!(parsed["year"],json!(2005));
        assert_eq!(parsed["seasons"],json!([1]));
        assert_eq!(parsed["episodes"],json!([6]));
        assert_eq!(parsed["title"],leaf["title"],"parent must not replace episode title");
    }

    #[test]
    fn hierarchy_inherits_year_at_each_level_with_deepest_value_winning() {
        for (input,year) in [
            ("Doctor.Who.1963.S01/Doctor.Who.2005.S01E06.mkv",2005),
            ("Doctor.Who.2005/Doctor.Who.S01E06.mkv",2005),
            ("Archive.2024/Doctor.Who.S01E06.mkv",2024),
            ("Doctor.Whom.2024/Doctor.Who.S01E06.mkv",2024),
            ("Doctor.Who.2005/Season.1/Doctor.Who.S01E06.mkv",2005),
            ("Doctor.Who.1963/Doctor.Who.2005/Doctor.Who.S01E06.mkv",2005),
            ("Doctor.Who.2005/Doctor.Who.2006.S01E06.mkv",2006),
            ("Doctor.Who.2005/Doctor.Who.2006/Doctor.Who.2007.S01E06.mkv",2007),
        ] {
            let parsed=parse_title(input,false).unwrap();
            assert_eq!(parsed["year"],json!(year),"{input}");
            assert_eq!(parsed["seasons"],json!([1]),"{input}");
            assert_eq!(parsed["episodes"],json!([6]),"{input}");
        }
        // Non-title metadata inherits through movie paths as well.
        let movie=parse_title("Movie.Name.2005.2160p/Movie.Name.mkv",false).unwrap();
        assert_eq!(movie["year"],2005);
    }

    #[test]
    fn hierarchical_metadata_merges_season_episode_and_quality_independently() {
        let folder="The.Show.2005.S01.2160p.WEB-DL";
        let subfolder="Season.2.720p";
        let episode="The.Show.E06.1080p.mkv";
        let parsed=parse_title(&format!("{folder}/{subfolder}/{episode}"),false).unwrap();
        let leaf=parse_title(episode,false).unwrap();
        let parent=parse_title(folder,false).unwrap();
        assert_eq!(parsed["year"],2005);
        assert_eq!(parsed["seasons"],json!([2]));
        assert_eq!(parsed["episodes"],json!([6]));
        assert_eq!(parsed["title"],leaf["title"]);
        assert!(leaf.get("resolution").is_some(),"leaf must have resolution");
        assert_eq!(parsed["resolution"],leaf["resolution"]);
        if let Some(quality)=parent.get("quality"){
            assert_eq!(parsed["quality"],*quality);
        }

        let inherited=parse_title("The.Show.2005.S02/E06.mkv",false).unwrap();
        assert_eq!(inherited["year"],2005);
        assert_eq!(inherited["seasons"],json!([2]));
        assert_eq!(inherited["episodes"],json!([6]));
        assert!(!inherited["title"].as_str().unwrap_or("").is_empty());
    }

    #[test]
    fn path_title_candidates_preserve_subfolder_anime_part_names() {
        let parsed=parse_title(
            "Sword Art Online/Sword Art Online Alicization/             Sword.Art.Online.S03E01.1080p.mkv",false).unwrap();
        assert_eq!(parsed["path_title_candidates"],
            json!(["Sword Art Online","Sword Art Online Alicization"]));
        // Parser does not decide which is anime: regular titles still use
        // the leaf (or nearest meaningful parent for a bare episode file).
        let leaf=parse_title("Sword.Art.Online.S03E01.1080p.mkv",false).unwrap();
        assert_eq!(parsed["title"],leaf["title"]);
        assert_eq!(parsed["seasons"],json!([3]));
        assert_eq!(parsed["episodes"],json!([1]));
    }

    #[test]
    fn child_episode_and_season_clear_parent_episode_ranges() {
        let single=parse_title("One.Piece.E768-E1064/One.Piece.S02E05.mkv",false).unwrap();
        assert_eq!(single["seasons"],json!([2]));
        assert_eq!(single["episodes"],json!([5]));
        assert!(single.get("episode_start").is_none());
        assert!(single.get("episode_end").is_none());
        assert!(single.get("episode_count").is_none());

        let season=parse_title("The.Show.S01E06/The.Show.S02",false).unwrap();
        assert_eq!(season["seasons"],json!([2]));
        assert!(season.get("episodes").is_none_or(|v|v.as_array().is_some_and(Vec::is_empty)));
    }

    #[test]
    fn hierarchy_supports_windows_paths_and_deeper_overrides() {
        let parsed=parse_title("Doctor.Who.2005.S01\\Season.2\\Doctor.Who.2006.S03E06.mkv",false).unwrap();
        assert_eq!(parsed["year"],2006);
        assert_eq!(parsed["seasons"],json!([3]));
        assert_eq!(parsed["episodes"],json!([6]));
    }

    #[test]
    fn folder_season_pack_does_not_override_leaf_episode() {
        let parsed=parse_title("Supernatural S01-S12/S02E01",false).unwrap();
        assert_eq!(parsed["seasons"],json!([2]));
        assert_eq!(parsed["episodes"],json!([1]));
        assert!(parsed["title"].as_str().unwrap_or("").to_lowercase().contains("supernatural"));
    }

    #[test]
    fn absolute_anime_episode_ranges_keep_both_endpoints() {
        let p=parse_title("One.piece.E768-E1064",false).unwrap();
        assert_eq!(p["episode_start"],768);
        assert_eq!(p["episode_end"],1064);
        assert_eq!(p["episode_count"],297);
        assert!(absolute_episode_range("One.Piece.E1064-E768").is_none());
        assert!(absolute_episode_range("One.Piece.E0-E1064").is_none());
    }

    #[test]
    fn parallel_batch_preserves_order_and_results() {
        let titles: Vec<_> = EQUIVALENCE_TITLES
            .iter()
            .copied()
            .cycle()
            .take(64)
            .collect();
        let expected: Vec<_> = titles
            .iter()
            .map(|title| parse_title(title, false).expect("scalar parse"))
            .collect();
        assert_eq!(
            parse_many(titles.iter().copied(), false).expect("batch parse"),
            expected
        );
    }

    #[test]
    fn prefilter_covers_most_regex_handlers() {
        let regex_handlers = ENGINE
            .handlers
            .iter()
            .filter(|handler| matches!(handler.kind, RuntimeHandlerKind::Regex(_)))
            .count();
        let gated_handlers = ENGINE
            .handlers
            .iter()
            .filter(|handler| handler.gate.is_active())
            .count();
        assert_eq!(regex_handlers, 423);
        assert!(
            gated_handlers >= 300,
            "only {gated_handlers} of {regex_handlers} regex handlers are gated"
        );
    }
}
