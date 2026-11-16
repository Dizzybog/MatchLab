//! Versioned, crash-safe GitHub Release storage for Community Learning.
//!
//! A Release is a container, not proof that an asset was uploaded. Every save
//! creates a *new* asset; old snapshots remain recoverable until confirmation.
//! No zero-byte files are uploaded, and malformed existing state fails closed.
use crate::github::GitHub;
use crate::json::{dumps, Dump, Json};
use crate::util::{is_hex64, split_lines, Result};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub const HEADER: &str = "# matchlab-community-ledger-v1";
pub const KEEP: usize = 6;
pub const MAX_LEDGER: usize = 100_000;
pub const MAX_PENDING: usize = 20_000;
pub const KINDS: &[&str] = &["ledger", "backlog", "decisions"];
pub const PHASES: &[&str] = &["prepared", "final", "ledger"];

/// (release tag, asset prefix, extension).
pub fn spec(kind: &str) -> Result<(&'static str, &'static str, &'static str)> {
    Ok(match kind {
        "ledger" => ("community-learning-ledger", "fingerprints", "txt"),
        "backlog" => ("community-learning-backlog", "backlog", "json"),
        "decisions" => ("community-learning-decisions", "decisions", "json"),
        _ => return Err(format!("unknown community state kind {kind}")),
    })
}

fn compact(value: &Json) -> Vec<u8> {
    (dumps(value, Dump::COMPACT_UTF8.sorted()) + "\n").into_bytes()
}

/// Validate the full snapshot before any write or a later cleanup.
pub fn normalize(kind: &str, raw: &[u8]) -> Result<Vec<u8>> {
    if kind == "ledger" {
        let text = std::str::from_utf8(raw).map_err(|_| "Invalid fingerprint encoding".to_owned())?;
        let mut lines = split_lines(text);
        if lines.first().is_some_and(|line| line == HEADER) {
            lines.remove(0);
        }
        if lines.iter().any(|line| !is_hex64(line)) {
            return Err("Invalid fingerprint record; refusing to reset ledger".into());
        }
        if lines.iter().collect::<HashSet<_>>().len() != lines.len() || lines.len() > MAX_LEDGER {
            return Err("Duplicate or oversized fingerprint ledger".into());
        }
        lines.sort();
        let mut text = lines.join("\n");
        if !lines.is_empty() {
            text.push('\n');
        }
        return Ok(text.into_bytes());
    }
    let data = Json::parse_bytes(raw).map_err(|_| "Invalid community state JSON".to_owned())?;
    if kind == "decisions" {
        crate::decisions::validate(&data)?;
        return Ok(compact(&data));
    }
    if !data.has_exact_keys(&["pending"]) {
        return Err("Invalid backlog schema".into());
    }
    let Some(pending) = data.field("pending").as_arr().filter(|items| items.len() <= MAX_PENDING) else {
        return Err("Invalid or oversized backlog".into());
    };
    let mut seen = HashSet::new();
    for entry in pending {
        let Some(fingerprint) = entry.field("fingerprint").as_str().filter(|fp| entry.as_obj().is_some() && is_hex64(fp)) else {
            return Err("Invalid pending candidate".into());
        };
        if !seen.insert(fingerprint) {
            return Err("Duplicate backlog fingerprint".into());
        }
    }
    Ok(compact(&data))
}

pub fn stored_bytes(kind: &str, normalized: Vec<u8>) -> Vec<u8> {
    if kind == "ledger" && normalized.is_empty() { format!("{HEADER}\n").into_bytes() } else { normalized }
}

pub fn asset_name(kind: &str, name: &str) -> bool {
    let Ok((_, prefix, ext)) = spec(kind) else { return false };
    if name == format!("{prefix}.{ext}") {
        return true;
    }
    let Some(rest) = name.strip_prefix(prefix).and_then(|rest| rest.strip_prefix('-')) else { return false };
    let Some(rest) = rest.strip_suffix(ext).and_then(|rest| rest.strip_suffix('.')) else { return false };
    let parts: Vec<&str> = rest.split('-').collect();
    parts.len() == 3
        && parts[..2].iter().all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
        && PHASES.contains(&parts[2])
}

fn asset_id(asset: &Json) -> Result<i128> {
    match asset.field("id") {
        Json::Int(id) => Ok(*id),
        Json::Float(id) => Ok(id.trunc() as i128),
        Json::Str(id) => id.trim().parse().map_err(|_| "Invalid Release asset id".to_owned()),
        _ => Err("Invalid Release asset id".into()),
    }
}

pub fn snapshots(kind: &str, release: &Json) -> Result<Vec<Json>> {
    let assets = match release.get("assets") {
        None => return Ok(Vec::new()),
        Some(Json::Arr(items)) => items,
        Some(_) => return Err("Invalid Release assets".into()),
    };
    let mut chosen: Vec<(i128, Json)> = Vec::new();
    for asset in assets {
        if asset.field("name").as_str().is_some_and(|name| asset_name(kind, name)) {
            chosen.push((asset_id(asset)?, asset.clone()));
        }
    }
    // Asset IDs are unique and monotonically assigned by GitHub. This also
    // orders consecutive prepared/final uploads within the same second.
    chosen.sort_by_key(|item| std::cmp::Reverse(item.0));
    Ok(chosen.into_iter().map(|(_, asset)| asset).collect())
}

/// A temporary directory removed on drop.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new() -> Result<TempDir> {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "matchlab-community-{}-{nanos}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        Ok(TempDir(path))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn release(api: &mut dyn GitHub, kind: &str) -> Result<Json> {
    let (tag, _, _) = spec(kind)?;
    api.send("GET", &format!("/releases/tags/{tag}"), None, true)
}

fn size_of(asset: &Json) -> Option<i128> {
    asset.field("size").as_int()
}

pub fn load(api: &mut dyn GitHub, kind: &str, path: &Path) -> Result<()> {
    let (tag, _, _) = spec(kind)?;
    let current = release(api, kind)?;
    let available = if current.truthy() { snapshots(kind, &current)? } else { Vec::new() };
    let raw = if let Some(asset) = available.first() {
        if !size_of(asset).is_some_and(|size| size > 0) {
            return Err("Latest community state asset is empty; refusing silent reset".into());
        }
        let name = asset.field("name").as_str().unwrap_or_default().to_owned();
        let temp = TempDir::new()?;
        let args = ["release", "download", tag, "--repo", api.repo(), "--pattern", &name, "--dir"];
        let mut args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
        args.push(temp.path().display().to_string());
        api.gh(&args)?;
        let source = temp.path().join(&name);
        if !source.is_file() {
            return Err("GitHub reported asset but download did not produce it".into());
        }
        let raw = std::fs::read(&source).map_err(|error| error.to_string())?;
        println!("community_{kind}=restored {name}");
        raw
    } else {
        // The initial Release may have been created before a failed
        // first upload; this is not evidence that dedup ever existed.
        println!("community_{kind}=bootstrap_empty (no Release asset)");
        match kind {
            "ledger" => Vec::new(),
            "decisions" => b"{\"decisions\":{}}\n".to_vec(),
            _ => b"{\"pending\":[]}\n".to_vec(),
        }
    };
    let clean = normalize(kind, &raw)?;
    if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    std::fs::write(path, clean).map_err(|error| format!("{}: {error}", path.display()))
}

pub fn save(api: &mut dyn GitHub, kind: &str, path: &Path, phase: &str, run_id: &str, attempt: &str) -> Result<()> {
    let numeric = |text: &str| !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
    if !numeric(run_id) || !numeric(attempt) {
        return Err("Numeric GitHub run ID and attempt required".into());
    }
    if !PHASES.contains(&phase) {
        return Err(format!("invalid phase {phase}"));
    }
    // Missing input is a pipeline defect, never a request to reset state.
    let raw = std::fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let clean = normalize(kind, &raw)?;
    let (tag, prefix, ext) = spec(kind)?;
    if !release(api, kind)?.truthy() {
        let args = [
            "release",
            "create",
            tag,
            "--repo",
            api.repo(),
            "--title",
            &format!("Community learning persistent {kind}"),
            "--notes",
            "Versioned machine-owned review state; no active rules.",
            "--latest=false",
        ];
        let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
        api.gh(&args)?;
    }
    let filename = format!("{prefix}-{run_id}-{attempt}-{phase}.{ext}");
    {
        let temp = TempDir::new()?;
        let source = temp.path().join(&filename);
        std::fs::write(&source, stored_bytes(kind, clean)).map_err(|error| error.to_string())?;
        let args = vec![
            "release".to_owned(),
            "upload".to_owned(),
            tag.to_owned(),
            source.display().to_string(),
            "--repo".to_owned(),
            api.repo().to_owned(),
        ];
        api.gh(&args)?;
    }
    // Verify before pruning. A failed upload must never remove the last
    // recoverable state, including the original legacy asset.
    let mut verified = None;
    let mut latest = Json::Null;
    for attempt in 0..3 {
        latest = release(api, kind)?;
        let assets = if latest.truthy() { snapshots(kind, &latest)? } else { Vec::new() };
        if let Some(found) =
            assets.into_iter().find(|asset| asset.field("name").str_eq(&filename) && size_of(asset).is_some_and(|size| size > 0))
        {
            verified = Some(found);
            break;
        }
        if attempt < 2 {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }
    let Some(verified) = verified else { return Err("Uploaded community state was not verified by GitHub".into()) };
    let assets = snapshots(kind, &latest)?;
    println!("community_{kind}=persisted {filename} size={}", size_of(&verified).unwrap_or_default());
    // Cleanup is deliberately best-effort, after durable confirmation.
    for old in assets.iter().skip(KEEP) {
        if old.field("id") == verified.field("id") {
            continue;
        }
        let route = format!("/releases/assets/{}", dumps(old.field("id"), Dump::DEFAULT));
        if let Err(error) = api.request("DELETE", &route, None) {
            println!("Warning: retained old {kind} snapshot: {error}");
        }
    }
    Ok(())
}
