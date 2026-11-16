//! `matchlab-community <command>`: one subcommand per workflow step.
use crate::json::{dumps, Dump, Json};
use crate::rule_intake::Catalog;
use crate::util::{
    git_ancestor, isoformat, read_json, read_json_or, read_text, split_lines, valid_sha, write_json, write_lines, write_text,
    Result,
};
use crate::{apply, backlog, cleanup, decisions, evidence, github, issue_intake, learning, regex_plan, review, review_diff};
use crate::{rule_intake, state, workflow};
use chrono::Utc;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

const USAGE: &str = "usage: matchlab-community <command> [options]

commands:
  issue-intake  --out FILE
  state         load|save ledger|backlog|decisions --path FILE [--phase prepared|final|ledger]
  learning      --comments F --verified F --sha SHA --seen F --seen-output F
                --review-output F --model-output F
  rule-intake   --comments F --seen F --sha SHA --out F --seen-out F
  backlog       prepare|finalize --state F --out F [--candidate-file F --seen F
                --curated F --extended F --daily F --accepted F --decisions F
                --handlers F --fixtures F --parser-src DIR]
  evidence      --comments F --candidates F --sha SHA --out F
  regex-plan    --candidates F --handlers F --out F
  review        INCOMING EXISTING OUTPUT [AUDIT]
  apply         --before F --after F --audit F --fixtures F --handlers F
                --promoted F --summary F
  review-diff   OLD_RULES NEW_RULES OLD_PROPOSALS NEW_PROPOSALS GITHUB_ENV
                [HANDLERS_BEFORE HANDLERS_AFTER FIXTURES_BEFORE FIXTURES_AFTER]
  pr-report     IMPACT PROMOTION
  accepted      --rules-before F --rules-after F --extended-before F
                --extended-after F --out F [--ledger F]
  decisions     --existing F --audit F --backlog F --handlers F --fixtures F
                [--parser-src DIR] --out F
  cleanup       [--apply]";

/// Minimal `--key value` / `--key=value` / positional argument parser.
struct Args {
    options: BTreeMap<String, String>,
    flags: BTreeSet<String>,
    positional: Vec<String>,
}

impl Args {
    fn parse(raw: &[String], flags: &[&str]) -> Result<Args> {
        let mut args = Args { options: BTreeMap::new(), flags: BTreeSet::new(), positional: Vec::new() };
        let mut iter = raw.iter();
        while let Some(item) = iter.next() {
            let Some(name) = item.strip_prefix("--") else {
                args.positional.push(item.clone());
                continue;
            };
            if flags.contains(&name) {
                args.flags.insert(name.to_owned());
                continue;
            }
            let (key, value) = match name.split_once('=') {
                Some((key, value)) => (key.to_owned(), value.to_owned()),
                None => (name.to_owned(), iter.next().cloned().ok_or_else(|| format!("--{name} needs a value"))?),
            };
            args.options.insert(key, value);
        }
        Ok(args)
    }

    fn take(&mut self, key: &str) -> Option<String> {
        self.options.remove(key)
    }

    fn path(&mut self, key: &str) -> Option<PathBuf> {
        self.take(key).map(PathBuf::from)
    }

    fn need(&mut self, key: &str) -> Result<String> {
        self.take(key).ok_or_else(|| format!("--{key} is required"))
    }

    fn need_path(&mut self, key: &str) -> Result<PathBuf> {
        self.need(key).map(PathBuf::from)
    }

    fn done(self, max_positional: usize) -> Result<()> {
        if let Some(key) = self.options.keys().next() {
            return Err(format!("unknown option --{key}"));
        }
        if self.positional.len() > max_positional {
            return Err(format!("unexpected argument {}", self.positional[max_positional]));
        }
        Ok(())
    }
}

pub fn main(raw: Vec<String>) -> Result<()> {
    let Some((command, rest)) = raw.split_first() else { return Err(USAGE.into()) };
    match command.as_str() {
        "issue-intake" => issue_intake_cmd(Args::parse(rest, &[])?),
        "state" => state_cmd(Args::parse(rest, &[])?),
        "learning" => learning_cmd(Args::parse(rest, &[])?),
        "rule-intake" => rule_intake_cmd(Args::parse(rest, &[])?),
        "backlog" => backlog_cmd(Args::parse(rest, &[])?),
        "evidence" => evidence_cmd(Args::parse(rest, &[])?),
        "regex-plan" => regex_plan_cmd(Args::parse(rest, &[])?),
        "review" => review_cmd(Args::parse(rest, &[])?),
        "apply" => apply_cmd(Args::parse(rest, &[])?),
        "review-diff" => review_diff_cmd(Args::parse(rest, &[])?),
        "pr-report" => pr_report_cmd(Args::parse(rest, &[])?),
        "accepted" => accepted_cmd(Args::parse(rest, &[])?),
        "decisions" => decisions_cmd(Args::parse(rest, &[])?),
        "cleanup" => cleanup_cmd(Args::parse(rest, &["apply"])?),
        "-h" | "--help" | "help" => {
            println!("{USAGE}");
            Ok(())
        }
        other => Err(format!("unknown command {other}\n{USAGE}")),
    }
}

fn issue_intake_cmd(mut args: Args) -> Result<()> {
    let out = args.need_path("out")?;
    args.done(0)?;
    let mut api = github::Rest::from_env("matchlab-community-intake")?;
    let rows = issue_intake::collect(&mut api, Utc::now(), true)?;
    let value = Json::Arr(rows.into_iter().map(Json::Arr).collect());
    write_json(out, &value, Dump::DEFAULT.utf8())
}

fn state_cmd(mut args: Args) -> Result<()> {
    let path = args.need_path("path")?;
    let phase = args.take("phase").unwrap_or_else(|| "ledger".into());
    let positional = std::mem::take(&mut args.positional);
    args.done(0)?;
    let [command, kind] = positional.as_slice() else { return Err("usage: state load|save KIND --path FILE".into()) };
    if !state::KINDS.contains(&kind.as_str()) || !state::PHASES.contains(&phase.as_str()) {
        return Err("invalid state kind or phase".into());
    }
    let mut api = github::Rest::from_env("matchlab-community-state")?;
    match command.as_str() {
        "load" => state::load(&mut api, kind, &path),
        "save" => {
            let run_id = std::env::var("GITHUB_RUN_ID").unwrap_or_default();
            let attempt = std::env::var("GITHUB_RUN_ATTEMPT").unwrap_or_else(|_| "1".into());
            state::save(&mut api, kind, &path, &phase, &run_id, &attempt)
        }
        other => Err(format!("unknown state command {other}")),
    }
}

fn optional_text(path: &Path) -> Result<Option<String>> {
    if path.exists() { read_text(path).map(Some) } else { Ok(None) }
}

fn learning_cmd(mut args: Args) -> Result<()> {
    let comments = args.need_path("comments")?;
    let verified = args.need_path("verified")?;
    let sha = args.need("sha")?;
    let seen_path = args.need_path("seen")?;
    let seen_output = args.need_path("seen-output")?;
    let review_output = args.need_path("review-output")?;
    let model_output = args.need_path("model-output")?;
    args.done(0)?;
    if !valid_sha(&sha) {
        return Err("sha must be a full Git commit id".into());
    }
    let comments = read_json(&comments)?;
    let seen = learning::read_ledger(optional_text(&seen_path)?.as_deref())?;
    let (reviewed, seen_new) = learning::intake(&comments, &sha, Utc::now(), &seen, &git_ancestor, &learning::replay_current)?;
    if let Some(parent) = review_output.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let mut lines: Vec<String> = reviewed.iter().map(|row| dumps(row, Dump::DEFAULT.sorted().utf8())).collect();
    if !lines.is_empty() {
        lines.push(String::new());
    }
    write_text(&review_output, &lines.join("\n"))?;
    write_lines(&seen_output, &seen_new)?;
    let labels = learning::read_verified(optional_text(&verified)?.as_deref(), &sha)?;
    let model = learning::train(&labels, &sha);
    let ready = model.is_some();
    let model = model.unwrap_or_else(|| {
        crate::obj! {
            "mode" => "inactive",
            "reason" => "Insufficient current-source independently verified labels or holdout validation failed",
        }
    });
    write_json(&model_output, &model, Dump::PRETTY.sorted())?;
    println!(
        "new unresolved candidate fingerprints: {}; verified labels: {}; shadow model: {}",
        reviewed.len(),
        labels.len(),
        if ready { "ready" } else { "inactive" }
    );
    Ok(())
}

fn rule_intake_cmd(mut args: Args) -> Result<()> {
    let comments = args.need_path("comments")?;
    let seen = args.need_path("seen")?;
    let sha = args.need("sha")?;
    let out = args.need_path("out")?;
    let seen_out = args.need_path("seen-out")?;
    args.done(0)?;
    if !valid_sha(&sha) {
        return Err("invalid full SHA".into());
    }
    let raw = read_json(&comments)?;
    if raw.as_arr().is_none() {
        return Err("invalid GitHub response".into());
    }
    let seen: BTreeSet<String> = split_lines(&read_text(&seen)?).into_iter().collect();
    let catalog = Catalog::load_default();
    let rules = rule_intake::collect(&raw, &sha, &seen, Utc::now(), &git_ancestor, &catalog)?;
    write_json(&out, &Json::Arr(rules.clone()), Dump::PRETTY_UTF8)?;
    write_lines(&seen_out, &seen)?;
    println!(
        "{} new, schema-validated community regex proposals; independent main branch checks pending",
        rules.len()
    );
    Ok(())
}

fn backlog_cmd(mut args: Args) -> Result<()> {
    let state_path = args.need_path("state")?;
    let out = args.need_path("out")?;
    let candidate_file = args.path("candidate-file");
    let seen = args.path("seen");
    let curated = args.path("curated");
    let extended = args.path("extended");
    let daily = args.path("daily");
    let accepted = args.path("accepted");
    let decisions_path = args.path("decisions");
    let handlers = args.path("handlers");
    let fixtures = args.path("fixtures");
    let parser_src = args.path("parser-src");
    let positional = std::mem::take(&mut args.positional);
    args.done(0)?;
    let old = read_json_or(&state_path, crate::obj! {"pending" => Json::Arr(Vec::new())})?;
    let missing = |name: &str| format!("--{name} is required");
    match positional.as_slice() {
        [phase] if phase == "prepare" => {
            let empty = || Json::Arr(Vec::new());
            let rules = match &curated { Some(path) => read_json_or(path, empty())?, None => empty() };
            let extended = match &extended { Some(path) => read_json_or(path, empty())?, None => empty() };
            let seen: BTreeSet<String> =
                split_lines(&read_text(seen.as_ref().ok_or_else(|| missing("seen"))?)?).into_iter().collect();
            let incoming = match &candidate_file { Some(path) => read_json_or(path, empty())?, None => empty() };
            let new = backlog::merge(&old, &incoming, &seen, &backlog::curated_fingerprints(&rules, &extended))?;
            write_json(&out, &new, Dump::PRETTY_UTF8)?;
            let pending = new.field("pending").as_arr().cloned().unwrap_or_default();
            let mut eligible: Vec<Json> = pending.iter().take(backlog::MAX_DAILY).cloned().collect();
            let mut deferred = 0;
            if let Some(decisions_path) = &decisions_path {
                let (Some(handlers), Some(fixtures)) = (&handlers, &fixtures) else {
                    return Err("decisions require immutable handlers and fixtures".into());
                };
                let decisions_state = read_json_or(decisions_path, crate::obj! {"decisions" => Json::Obj(Default::default())})?;
                let signature = decisions::signature(handlers, fixtures, parser_src.as_deref())?;
                (eligible, deferred) =
                    decisions::due(new.field("pending"), &decisions_state, &signature, backlog::MAX_DAILY)?;
            }
            write_json(daily.as_ref().ok_or_else(|| missing("daily"))?, &Json::Arr(eligible.clone()), Dump::PRETTY_UTF8)?;
            println!(
                "community_backlog_pending={} review_due={} cached_decisions={deferred}",
                pending.len(),
                eligible.len()
            );
            Ok(())
        }
        [phase] if phase == "finalize" => {
            let (Some(daily), Some(accepted)) = (&daily, &accepted) else {
                return Err("daily/accepted inputs required".into());
            };
            let daily = read_json_or(daily, Json::Arr(Vec::new()))?;
            let attempted: Vec<Option<String>> =
                daily.as_arr().into_iter().flatten().map(|row| backlog::fp_of(row).map(str::to_owned)).collect();
            let accepted = backlog::parse_accepted(&read_json_or(accepted, Json::Arr(Vec::new()))?)?;
            let out_value = backlog::finalize(&old, &attempted, &accepted)?;
            write_json(&out, &out_value, Dump::PRETTY_UTF8)?;
            println!("community_backlog_pending={}", out_value.field("pending").as_arr().map_or(0, Vec::len));
            Ok(())
        }
        _ => Err("usage: backlog prepare|finalize ...".into()),
    }
}

fn evidence_cmd(mut args: Args) -> Result<()> {
    let comments = read_json(args.need_path("comments")?)?;
    let candidates = read_json(args.need_path("candidates")?)?;
    let sha = args.need("sha")?;
    let out = args.need_path("out")?;
    args.done(0)?;
    if comments.as_arr().is_none() || candidates.as_arr().is_none() {
        return Err("Invalid issue intake or candidate format".into());
    }
    let catalog = Catalog::load_default();
    let rules = |comment: &Json, sha: &str| rule_intake::parse_batch(comment, sha, &git_ancestor, &catalog);
    let reports = |comment: &Json, sha: &str| learning::validate_comment(comment, sha, &git_ancestor);
    let result = evidence::gather(&comments, &candidates, &sha, Utc::now(), &rules, &reports);
    write_json(&out, &result, Dump::DEFAULT.utf8())?;
    let selected: usize = result.as_obj().into_iter().flatten().map(|(_, titles)| titles.as_arr().map_or(0, Vec::len)).sum();
    println!("community_actual_titles_selected={selected}");
    Ok(())
}

fn regex_plan_cmd(mut args: Args) -> Result<()> {
    let candidates = read_json(args.need_path("candidates")?)?;
    let handlers_path = args.need_path("handlers")?;
    let out = args.need_path("out")?;
    args.done(0)?;
    let handlers = read_json(&handlers_path)?.field("handlers").as_arr().cloned().ok_or("handler catalogue has no handlers")?;
    let plans = regex_plan::build(&candidates, &handlers, &Catalog::load_default())?;
    write_json(&out, &plans, Dump::PRETTY_UTF8)?;
    let planned = plans
        .as_obj()
        .into_iter()
        .flatten()
        .filter(|(_, plan)| plan.field("source_changes").truthy())
        .count();
    println!("community_replay_planned={planned} total={}", plans.as_obj().map_or(0, |plans| plans.len()));
    Ok(())
}

fn review_cmd(mut args: Args) -> Result<()> {
    let positional = std::mem::take(&mut args.positional);
    args.done(0)?;
    if !(3..=4).contains(&positional.len()) {
        return Err("usage: review incoming.json existing.json output.json [audit.json]".into());
    }
    let incoming = read_json(&positional[0])?;
    let existing = read_json(&positional[1])?;
    let audits = positional.get(3).map(read_json).transpose()?;
    let (accepted, changed) = review::screen(&incoming, &existing, audits.as_ref(), &git_ancestor, &Catalog::load_default())?;
    write_json(&positional[2], &accepted, Dump::PRETTY_UTF8)?;
    println!("community_extended_regex={}", if changed { "human_review_pr" } else { "none" });
    Ok(())
}

fn apply_cmd(mut args: Args) -> Result<()> {
    let before = read_json(args.need_path("before")?)?;
    let after = read_json(args.need_path("after")?)?;
    let audits = read_json(args.need_path("audit")?)?;
    let fixtures = read_json(args.need_path("fixtures")?)?;
    let handlers = args.need_path("handlers")?;
    let promoted_path = args.need_path("promoted")?;
    let summary_path = args.need_path("summary")?;
    args.done(0)?;
    let existing = read_text(&handlers)?;
    let promoted = read_json(&promoted_path)?;
    let catalog = Catalog::load(&handlers);
    let (replacement, tests, summary) = apply::apply(&before, &after, &audits, &fixtures, &existing, &promoted, &catalog)?;
    if summary.is_some() {
        write_text(&handlers, &replacement)?;
        write_json(&promoted_path, &tests, Dump::PRETTY_UTF8)?;
    }
    let ready = summary.is_some();
    write_json(&summary_path, &summary.unwrap_or(Json::Null), Dump::PRETTY_UTF8)?;
    println!("community_auto_source_diff={}", if ready { "review_required" } else { "none" });
    Ok(())
}

fn review_diff_cmd(mut args: Args) -> Result<()> {
    let paths: Vec<PathBuf> = std::mem::take(&mut args.positional).into_iter().map(PathBuf::from).collect();
    args.done(0)?;
    if paths.len() != 5 && paths.len() != 9 {
        return Err("usage: review-diff old_rules new_rules old_proposals new_proposals github_env \
                    [handlers_before handlers_after fixtures_before fixtures_after]"
            .into());
    }
    let mut changed = review_diff::check(&paths[0], &paths[1], &paths[2], &paths[3])?;
    if paths.len() == 9 {
        // Preserve review-only suggestions in the durable backlog and daily
        // audit; an unproven JSON proposal must not create a no-op code PR.
        changed = changed && review_diff::source_proven(&paths[5], &paths[6], &paths[7], &paths[8])?;
    }
    use std::io::Write;
    let mut env = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths[4])
        .map_err(|error| format!("{}: {error}", paths[4].display()))?;
    writeln!(env, "COMMUNITY_RULE_CHANGED={changed}").map_err(|error| error.to_string())?;
    println!("community_native_review={}", if changed { "source_patch_verified" } else { "no_proven_source_patch" });
    Ok(())
}

fn pr_report_cmd(mut args: Args) -> Result<()> {
    let positional = std::mem::take(&mut args.positional);
    args.done(0)?;
    let [impact, promotion] = positional.as_slice() else { return Err("usage: pr-report IMPACT PROMOTION".into()) };
    print!("{}", workflow::pr_report(&read_json(impact)?, &read_json(promotion)?));
    Ok(())
}

fn accepted_cmd(mut args: Args) -> Result<()> {
    let rules_before = args.need_path("rules-before")?;
    let rules_after = args.need_path("rules-after")?;
    let extended_before = args.need_path("extended-before")?;
    let extended_after = args.need_path("extended-after")?;
    let out = args.path("out");
    let ledger = args.path("ledger");
    args.done(0)?;
    let created = std::env::var("COMMUNITY_PR_CREATED").is_ok_and(|value| value == "true");
    let accepted = if created {
        workflow::accepted_fingerprints(
            &read_json(rules_before)?,
            &read_json(rules_after)?,
            &read_json(extended_before)?,
            &read_json(extended_after)?,
        )?
    } else {
        Vec::new()
    };
    if let Some(ledger) = ledger {
        // Dedup only rules actually placed into a review PR.
        let seen = split_lines(&read_text(&ledger)?);
        write_lines(&ledger, &workflow::ledger_with(seen, &accepted))?;
    }
    if let Some(out) = out {
        write_text(&out, &dumps(&Json::from_strs(accepted), Dump::DEFAULT))?;
    }
    Ok(())
}

fn decisions_cmd(mut args: Args) -> Result<()> {
    let state_value = read_json(args.need_path("existing")?)?;
    let audit = read_json(args.need_path("audit")?)?;
    let pending = read_json(args.need_path("backlog")?)?.field("pending").clone();
    let handlers = args.need_path("handlers")?;
    let fixtures = args.need_path("fixtures")?;
    let parser_src = args.path("parser-src");
    let out = args.need_path("out")?;
    args.done(0)?;
    let signature = decisions::signature(&handlers, &fixtures, parser_src.as_deref())?;
    let updated = decisions::update(&state_value, &audit, &pending, &signature, &isoformat(Utc::now()))?;
    write_json(&out, &updated, Dump::PRETTY.sorted())?;
    println!("community_decisions_saved={}", updated.field("decisions").as_obj().map_or(0, |data| data.len()));
    Ok(())
}

fn cleanup_cmd(args: Args) -> Result<()> {
    let apply = args.flags.contains("apply");
    args.done(0)?;
    let mut api = github::Rest::from_env("matchlab-community-cleanup")?;
    cleanup::run(&mut api, Utc::now(), apply).map(|_| ())
}
