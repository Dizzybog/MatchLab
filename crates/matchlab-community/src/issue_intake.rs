//! Collect community reports from *all* recently active official daily issues.
//!
//! GitHub's /issues API also returns pull requests; never use the first page as
//! an exhaustive issue list. Include today's comments and late comments to older
//! bot issues, and fail closed rather than silently truncate an oversized intake.
use crate::cleanup::daily_title_date;
use crate::github::{GitHub, BOT};
use crate::json::Json;
use crate::obj;
use crate::util::{timestamp, Result};
use chrono::{DateTime, Duration, Utc};

pub const WINDOW_DAYS: i64 = 60;
pub const MAX_ISSUES: usize = 2000;
pub const MAX_COMMENTS: usize = 5000;

pub fn official(row: &Json) -> bool {
    row.as_obj().is_some()
        && !row.field("pull_request").truthy()
        && row.field("user").field("login").str_eq(BOT)
        && row.field("title").as_str().is_some_and(daily_title_shape)
        && row.field("number").as_int().is_some()
}

/// `[community-learning] dddd-dd-dd` (shape only, like the original regex).
fn daily_title_shape(title: &str) -> bool {
    daily_title_date(title).is_some()
        || title.strip_prefix(crate::cleanup::ISSUE_PREFIX).is_some_and(|day| {
            day.len() == 10
                && day.bytes().enumerate().all(|(i, b)| if i == 4 || i == 7 { b == b'-' } else { b.is_ascii_digit() })
        })
}

pub fn collect(api: &mut dyn GitHub, now: DateTime<Utc>, create_daily: bool) -> Result<Vec<Vec<Json>>> {
    let cutoff = now - Duration::days(WINDOW_DAYS);
    let issues = api.pages("/issues?state=all&sort=updated&direction=desc")?;
    let today = format!("[community-learning] {}", now.date_naive().format("%Y-%m-%d"));
    if create_daily && !issues.iter().any(|row| official(row) && row.field("title").str_eq(&today)) {
        api.request(
            "POST",
            "/issues",
            Some(&obj! {
                "title" => today.as_str(),
                "body" => "One shared daily thread for opt-in MatchLab reports. \
                           Candidate batches are not ground truth.",
            }),
        )?;
    }
    let eligible: Vec<&Json> = issues
        .iter()
        .filter(|row| official(row) && timestamp(row.field("updated_at")).is_some_and(|stamp| stamp >= cutoff))
        .collect();
    if eligible.len() > MAX_ISSUES {
        return Err("Too many community issues; refusing partial intake".into());
    }
    let mut result = Vec::new();
    let mut count = 0;
    // Existing downstream consumers accept a list of page/list entries.
    for issue in &eligible {
        if !issue.field("comments").truthy() {
            continue;
        }
        let number = issue.field("number").as_int().unwrap_or_default();
        let comments = api.pages(&format!("/issues/{number}/comments"))?;
        let selected: Vec<Json> = comments
            .into_iter()
            .filter(|entry| {
                entry.as_obj().is_some()
                    && timestamp(entry.field("created_at"))
                        .is_some_and(|stamp| cutoff <= stamp && stamp <= now + Duration::minutes(5))
            })
            .collect();
        count += selected.len();
        if count > MAX_COMMENTS {
            return Err("Too many comments; refusing partial intake".into());
        }
        result.push(selected);
    }
    println!("community_issues_scanned={} comments={count}", eligible.len());
    Ok(result)
}
