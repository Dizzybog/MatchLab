//! `matchlab-diagnostics audit-api|benchmark-startup|collect-title-tests`.
use matchlab_community::json::{dumps, Dump, Json};
use matchlab_community::util::Result;
use matchlab_diagnostics::{audit_api, benchmark, title_tests};
use std::collections::BTreeMap;
use std::path::PathBuf;

const USAGE: &str = "usage: matchlab-diagnostics <command> [options]

commands:
  audit-api CORPUS --base-url URL --output FILE [--endpoint /match|/parse]
            [--transport curl|native] [--delay S] [--timeout S]
            [--max-response-bytes N] [--rate-limit-retries N]
            [--max-retry-after S] [--quiet]
  benchmark-startup --output-dir DIR [--binary PATH] [--sizes N ...]
            [--restarts N] [--samples N] [--first-request parse|match]
            [--timeout S] [--poll-seconds S] [--missing-observe-seconds S]
            [--fixtures-only]
  collect-title-tests --output-dir DIR [--root DIR] [--trace FILE]
            [--trace-patch FILE]";

struct Args {
    options: BTreeMap<String, Vec<String>>,
    flags: Vec<String>,
    positional: Vec<String>,
}

impl Args {
    fn parse(raw: &[String], flags: &[&str], multi: &[&str]) -> Result<Args> {
        let mut args = Args { options: BTreeMap::new(), flags: Vec::new(), positional: Vec::new() };
        let mut index = 0;
        while index < raw.len() {
            let item = &raw[index];
            index += 1;
            let Some(name) = item.strip_prefix("--") else {
                args.positional.push(item.clone());
                continue;
            };
            if flags.contains(&name) {
                args.flags.push(name.to_owned());
                continue;
            }
            if let Some((key, value)) = name.split_once('=') {
                args.options.entry(key.to_owned()).or_default().push(value.to_owned());
                continue;
            }
            let values = args.options.entry(name.to_owned()).or_default();
            if multi.contains(&name) {
                while index < raw.len() && !raw[index].starts_with("--") {
                    values.push(raw[index].clone());
                    index += 1;
                }
            } else {
                values.push(raw.get(index).cloned().ok_or_else(|| format!("--{name} needs a value"))?);
                index += 1;
            }
        }
        Ok(args)
    }

    fn take(&mut self, key: &str) -> Option<String> {
        self.options.remove(key).and_then(|mut values| values.pop())
    }

    fn need(&mut self, key: &str) -> Result<String> {
        self.take(key).ok_or_else(|| format!("--{key} is required"))
    }

    fn number<T: std::str::FromStr>(&mut self, key: &str, default: T) -> Result<T> {
        match self.take(key) {
            Some(text) => text.parse().map_err(|_| format!("--{key}: invalid number {text}")),
            None => Ok(default),
        }
    }

    fn flag(&self, key: &str) -> bool {
        self.flags.iter().any(|flag| flag == key)
    }

    fn done(&self, positional: usize) -> Result<()> {
        if let Some(key) = self.options.keys().next() {
            return Err(format!("unknown option --{key}"));
        }
        if self.positional.len() != positional {
            return Err(format!("expected {positional} positional argument(s)"));
        }
        Ok(())
    }
}

fn repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap_or_else(|_| PathBuf::from("."))
}

fn audit(raw: &[String]) -> Result<i32> {
    let mut args = Args::parse(raw, &["quiet"], &[])?;
    let base_url = args.need("base-url")?;
    let output = PathBuf::from(args.need("output")?);
    let endpoint = args.take("endpoint").unwrap_or_else(|| "/match".into());
    let transport_name = args.take("transport").unwrap_or_else(|| "curl".into());
    let settings = audit_api::Settings {
        endpoint,
        delay: args.number("delay", 0.15)?,
        timeout: args.number("timeout", 20.0)?,
        max_bytes: args.number("max-response-bytes", 2 * 1024 * 1024)?,
        rate_limit_retries: args.number("rate-limit-retries", 1)?,
        max_retry_after: args.number("max-retry-after", 60.0)?,
    };
    args.done(1)?;
    if !audit_api::ENDPOINTS.contains(&settings.endpoint.as_str()) {
        return Err("--endpoint must be /match or /parse".into());
    }
    if !audit_api::valid_base_url(&base_url) {
        return Err("base URL must be HTTP(S), without credentials, query or fragment".into());
    }
    if ![settings.delay, settings.timeout, settings.max_retry_after].iter().all(|value| value.is_finite())
        || settings.delay < 0.0
        || settings.timeout <= 0.0
        || settings.max_retry_after < 0.0
        || settings.max_bytes == 0
    {
        return Err("invalid delay, timeout, byte limit or retry configuration".into());
    }
    let corpus = PathBuf::from(&args.positional[0]);
    let text = std::fs::read_to_string(&corpus).map_err(|error| format!("{}: {error}", corpus.display()))?;
    let cases = audit_api::validate_cases(&audit_api::strict_json(&text)?)?;
    let mut transport: Box<dyn audit_api::Transport> = match transport_name.as_str() {
        "curl" => Box::new(audit_api::Curl::new()?),
        "native" | "urllib" => Box::new(audit_api::Native::new()?),
        other => return Err(format!("unknown transport {other}")),
    };
    let quiet = args.flag("quiet");
    let mut progress = |index: usize, total: usize, row: &Json| {
        if !quiet {
            eprintln!(
                "{index}/{total} {} {} HTTP={} {:.1}ms",
                row.field("id").as_str().unwrap_or_default(),
                row.field("outcome").as_str().unwrap_or_default(),
                dumps(row.field("status"), Dump::DEFAULT),
                row.field("elapsed_ms").as_number().unwrap_or(0.0)
            );
        }
    };
    let mut sleep = |seconds: f64| std::thread::sleep(std::time::Duration::from_secs_f64(seconds.max(0.0)));
    let mut report = audit_api::run_audit(&cases, &base_url, &settings, transport.as_mut(), &mut sleep, &mut progress);
    if let Json::Obj(map) = &mut report
        && let Some(Json::Obj(metadata)) = map.get_mut("metadata") {
            metadata.insert("corpus".into(), Json::from(corpus.display().to_string()));
        }
    if let Some(parent) = output.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let temporary = output.with_file_name(format!("{}.tmp", output.file_name().unwrap_or_default().to_string_lossy()));
    std::fs::write(&temporary, dumps(&report, Dump::PRETTY) + "\n").map_err(|error| error.to_string())?;
    std::fs::rename(&temporary, &output).map_err(|error| error.to_string())?;
    println!("{}", dumps(report.field("summary"), Dump::DEFAULT.sorted()));
    Ok(0)
}

fn startup(raw: &[String]) -> Result<i32> {
    let mut args = Args::parse(raw, &["fixtures-only"], &["sizes"])?;
    let root = repository_root();
    let binary = args.take("binary").map_or_else(|| root.join("target/release/matchlab-server"), PathBuf::from);
    let output_dir = PathBuf::from(args.need("output-dir")?);
    let sizes: Vec<i64> = match args.options.remove("sizes") {
        Some(values) => values.iter().map(|value| value.parse().map_err(|_| format!("invalid size {value}"))).collect::<Result<_>>()?,
        None => vec![100, 10_000, 100_000],
    };
    let options = benchmark::Options {
        root: root.clone(),
        binary: binary.canonicalize().unwrap_or(binary),
        samples: args.number("samples", 10)?,
        first_request: args.take("first-request").unwrap_or_else(|| "parse".into()),
        timeout: args.number("timeout", 120.0)?,
        poll_seconds: args.number("poll-seconds", 0.01)?,
        missing_observe_seconds: args.number("missing-observe-seconds", 1.0)?,
    };
    let plan = benchmark::Plan { output_dir, sizes, restarts: args.number("restarts", 5)?, fixtures_only: args.flag("fixtures-only") };
    args.done(0)?;
    if !matches!(options.first_request.as_str(), "parse" | "match") {
        return Err("--first-request must be parse or match".into());
    }
    if plan.sizes.iter().any(|size| *size < 1)
        || plan.restarts < 1
        || options.samples < 1
        || [options.timeout, options.poll_seconds, options.missing_observe_seconds].iter().any(|value| *value <= 0.0)
    {
        return Err("sizes, restarts, samples and timeouts must be positive".into());
    }
    if !plan.fixtures_only && !options.binary.is_file() {
        return Err(format!("build the release binary first: {}", options.binary.display()));
    }
    let (destination, failed) = benchmark::run(&options, &plan)?;
    println!("{}", destination.display());
    Ok(i32::from(failed))
}

fn titles(raw: &[String]) -> Result<i32> {
    let mut args = Args::parse(raw, &[], &[])?;
    let root = args.take("root").map_or_else(repository_root, PathBuf::from);
    let output_dir = PathBuf::from(args.need("output-dir")?);
    let trace = args.take("trace").map(PathBuf::from);
    let patch = args.take("trace-patch").map(PathBuf::from);
    args.done(0)?;
    let summary = title_tests::run(&root, &output_dir, trace.as_deref(), patch.as_deref())?;
    println!("{}", dumps(&summary, Dump::DEFAULT));
    Ok(0)
}

fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let result = match raw.split_first() {
        Some((command, rest)) => match command.as_str() {
            "audit-api" => audit(rest),
            "benchmark-startup" => startup(rest),
            "collect-title-tests" => titles(rest),
            "-h" | "--help" | "help" => {
                println!("{USAGE}");
                Ok(0)
            }
            other => Err(format!("unknown command {other}\n{USAGE}")),
        },
        None => Err(USAGE.into()),
    };
    match result {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("error: {error}");
            std::process::exit(2);
        }
    }
}
