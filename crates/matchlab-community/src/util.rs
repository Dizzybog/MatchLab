//! Small shared helpers: digests, commit IDs, timestamps and file output.
use crate::json::{dumps, Dump, Json};
use chrono::{DateTime, NaiveDateTime, Timelike, Utc};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub type Result<T> = std::result::Result<T, String>;

/// A check "is `source` an ancestor of (or equal to) `target`".
pub type Ancestor<'a> = &'a dyn Fn(&str, &str) -> bool;

pub fn sha256_hex(data: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(data.as_ref()))
}

pub fn is_hex(value: &str, length: usize) -> bool {
    value.len() == length && value.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

pub fn is_hex64(value: &str) -> bool {
    is_hex(value, 64)
}

pub fn valid_sha(value: &str) -> bool {
    is_hex(value, 40)
}

pub fn json_valid_sha(value: &Json) -> bool {
    value.as_str().is_some_and(valid_sha)
}

/// `git merge-base --is-ancestor`, bounded to three seconds.
pub fn git_ancestor(source: &str, target: &str) -> bool {
    if !valid_sha(source) || !valid_sha(target) {
        return false;
    }
    let child = Command::new("git")
        .args(["merge-base", "--is-ancestor", source, target])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else { return false };
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

/// Python `datetime.fromisoformat` for timezone-aware timestamps.
pub fn aware_timestamp(value: &Json) -> Option<DateTime<Utc>> {
    let text = value.as_str()?;
    DateTime::parse_from_rfc3339(text).ok().map(|stamp| stamp.with_timezone(&Utc))
}

/// Like `aware_timestamp`, but a naive timestamp is read as UTC
/// (`astimezone` on a UTC runner).
pub fn timestamp(value: &Json) -> Option<DateTime<Utc>> {
    aware_timestamp(value).or_else(|| {
        let text = value.as_str()?;
        ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f"]
            .iter()
            .find_map(|format| NaiveDateTime::parse_from_str(text, format).ok())
            .map(|naive| naive.and_utc())
    })
}

/// Python `datetime.isoformat()` of an aware UTC timestamp.
pub fn isoformat(stamp: DateTime<Utc>) -> String {
    if stamp.nanosecond() / 1000 == 0 {
        stamp.format("%Y-%m-%dT%H:%M:%S+00:00").to_string()
    } else {
        stamp.format("%Y-%m-%dT%H:%M:%S%.6f+00:00").to_string()
    }
}

pub fn read_text(path: impl AsRef<Path>) -> Result<String> {
    let path = path.as_ref();
    std::fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))
}

pub fn read_json(path: impl AsRef<Path>) -> Result<Json> {
    let path = path.as_ref();
    let raw = std::fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
    Json::parse_bytes(&raw).map_err(|error| format!("{}: invalid JSON: {error}", path.display()))
}

/// Read JSON, or return `default` when the file does not exist.
pub fn read_json_or(path: impl AsRef<Path>, default: Json) -> Result<Json> {
    if path.as_ref().exists() { read_json(path) } else { Ok(default) }
}

pub fn write_text(path: impl AsRef<Path>, text: &str) -> Result<()> {
    let path = path.as_ref();
    std::fs::write(path, text).map_err(|error| format!("{}: {error}", path.display()))
}

pub fn write_json(path: impl AsRef<Path>, value: &Json, options: Dump) -> Result<()> {
    write_text(path, &(dumps(value, options) + "\n"))
}

/// Sorted newline-terminated fingerprint ledger.
pub fn write_lines<'a>(path: impl AsRef<Path>, lines: impl IntoIterator<Item = &'a String>) -> Result<()> {
    let mut sorted: Vec<&String> = lines.into_iter().collect();
    sorted.sort();
    sorted.dedup();
    let mut text = sorted.iter().map(|line| line.as_str()).collect::<Vec<_>>().join("\n");
    if !sorted.is_empty() {
        text.push('\n');
    }
    write_text(path, &text)
}

/// Python `str.splitlines()` for ledger text.
pub fn split_lines(text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                lines.push(std::mem::take(&mut current));
            }
            '\n' | '\u{b}' | '\u{c}' | '\u{1c}' | '\u{1d}' | '\u{1e}' | '\u{85}' | '\u{2028}' | '\u{2029}' => {
                lines.push(std::mem::take(&mut current));
            }
            ch => current.push(ch),
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// Python `re.escape` for the ASCII alphabet used in technical markers.
pub fn py_escape(text: &str) -> String {
    let mut out = String::new();
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || !ch.is_ascii() {
            out.push(ch);
        } else {
            if "()[]{}?*+-|^$\\.&~# \t\n\r\u{b}\u{c}".contains(ch) {
                out.push('\\');
            }
            out.push(ch);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_and_lines() {
        let stamp = aware_timestamp(&Json::from("2026-11-11T12:00:00Z")).unwrap();
        assert_eq!(isoformat(stamp), "2026-11-11T12:00:00+00:00");
        assert!(aware_timestamp(&Json::from("2026-11-11T12:00:00")).is_none());
        assert!(timestamp(&Json::from("2026-11-11T12:00:00")).is_some());
        assert_eq!(split_lines("a\nb\r\nc\n"), ["a", "b", "c"]);
        assert_eq!(split_lines(""), Vec::<String>::new());
        assert_eq!(split_lines("\n"), [""]);
    }
}
