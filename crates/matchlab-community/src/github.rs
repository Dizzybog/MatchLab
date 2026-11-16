//! Minimal GitHub REST client for the trusted Community Learning Action.
use crate::json::{dumps, Dump, Json};
use crate::util::Result;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const BOT: &str = "github-actions[bot]";

/// `owner/repo` with GitHub's permitted characters.
pub fn valid_repo(repo: &str) -> bool {
    let Some((owner, name)) = repo.split_once('/') else { return false };
    let part = |text: &str| !text.is_empty() && text.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'));
    part(owner) && part(name)
}

/// Python `urllib.parse.quote(text, safe)`.
pub fn quote(text: &str, safe: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"_.-~".contains(&byte) || safe.as_bytes().contains(&byte) {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

pub trait GitHub {
    fn repo(&self) -> &str;

    /// One REST call. An empty body is `Json::Null`; with `missing_ok`,
    /// HTTP 404 is also `Json::Null`.
    fn send(&mut self, method: &str, route: &str, payload: Option<&Json>, missing_ok: bool) -> Result<Json>;

    fn request(&mut self, method: &str, route: &str, payload: Option<&Json>) -> Result<Json> {
        self.send(method, route, payload, false)
    }

    /// Every page of a collection; refuses a silently truncated listing.
    fn pages(&mut self, path: &str) -> Result<Vec<Json>> {
        let mut rows = Vec::new();
        let separator = if path.contains('?') { '&' } else { '?' };
        for page in 1..=100 {
            let part = self.request("GET", &format!("{path}{separator}per_page=100&page={page}"), None)?;
            let Json::Arr(items) = part else { return Err("GitHub returned an unexpected collection".into()) };
            let done = items.len() < 100;
            rows.extend(items);
            if done {
                return Ok(rows);
            }
        }
        Err("GitHub pagination cap reached; refusing partial cleanup".into())
    }

    /// Run the GitHub CLI (release asset transfer).
    fn gh(&mut self, args: &[String]) -> Result<()>;
}

pub struct Rest {
    repo: String,
    token: String,
    client: reqwest::blocking::Client,
    user_agent: &'static str,
}

impl Rest {
    /// From `GH_REPO`/`GH_TOKEN`.
    pub fn from_env(user_agent: &'static str) -> Result<Rest> {
        let repo = std::env::var("GH_REPO").unwrap_or_default();
        let token = std::env::var("GH_TOKEN").unwrap_or_default();
        if !valid_repo(&repo) || token.is_empty() {
            return Err("GH_REPO=owner/repo and GH_TOKEN are required".into());
        }
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|error| error.to_string())?;
        Ok(Rest { repo, token, client, user_agent })
    }
}

impl GitHub for Rest {
    fn repo(&self) -> &str {
        &self.repo
    }

    fn send(&mut self, method: &str, route: &str, payload: Option<&Json>, missing_ok: bool) -> Result<Json> {
        let url = format!("https://api.github.com/repos/{}{route}", self.repo);
        let verb = reqwest::Method::from_bytes(method.as_bytes()).map_err(|error| error.to_string())?;
        let mut request = self
            .client
            .request(verb, url)
            .header("Authorization", format!("Bearer {}", self.token))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("User-Agent", self.user_agent);
        if let Some(payload) = payload {
            request = request.header("Content-Type", "application/json").body(dumps(payload, Dump::DEFAULT));
        }
        let response = request.send().map_err(|error| format!("GitHub {method} {route} failed: {error}"))?;
        let status = response.status();
        if status.as_u16() == 404 && missing_ok {
            return Ok(Json::Null);
        }
        if !status.is_success() {
            return Err(format!("GitHub {method} {route} returned HTTP {}", status.as_u16()));
        }
        let raw = response.bytes().map_err(|error| error.to_string())?;
        if raw.is_empty() {
            return Ok(Json::Null);
        }
        Json::parse_bytes(&raw).map_err(|error| format!("GitHub {method} {route}: invalid JSON: {error}"))
    }

    fn gh(&mut self, args: &[String]) -> Result<()> {
        let mut child = Command::new("gh")
            .args(args)
            .env("GH_TOKEN", &self.token)
            .env("GH_REPO", &self.repo)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("gh: {error}"))?;
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
                if status.success() {
                    return Ok(());
                }
                let mut detail = String::new();
                if let Some(mut stderr) = child.stderr.take() {
                    let _ = std::io::Read::read_to_string(&mut stderr, &mut detail);
                }
                return Err(format!("gh {} failed: {}", args.first().map(String::as_str).unwrap_or(""), detail.trim()));
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err("gh timed out".into());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
