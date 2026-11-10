//! Trusted GitHub Actions regex impact auditor.
//!
//! Incoming Issue proposals are UNTRUSTED, even when they contain evidence
//! counts. The GitHub intake validates their schema and source ancestry first.
//! This executable independently reconstructs a bounded source patch using
//! the exact bundled handler, reparses public test titles with the entire
//! original and proposed Rust parsers, and checks maintainer-owned labels.
//! It never activates a rule or edits source code.
use matchlab_parser::{existing_regex_rules, parse_title_canonical,
    replay_handler_rules, verify_live_regression_corpus, HandlerPatch};
use serde_json::{json, Map, Value};
use std::{collections::HashSet, env, fs};

const MAX_CANDIDATES:usize=24;
const MAX_FIXTURES:usize=100;
const NEGATIVES:&[&str]=&[
    "Movie.1917.2019.2160p.BluRay",
    "Breaking.Bad.S01E01.1080p.WEB-DL.x264",
    "Bleach.S17E01.1080p.WEB-DL",
    "One.Piece.Episode.1067.1080p",
    "The100.2014.S01E01.1080p",
    "Example.Movie.2025.Ultimate.Edition.1080p.WEB-DL",
    "The.Simpsons.S10E05.720p.WEB.x264",
    "Example.Show.S00E18.1080p.WEB-DL",
    "Example.Show.S01E02.S02E01.WEB-DL",
    "Example.Movie.2024.1920x1080",
    "Example.Show.S01E01-E04.1080p.WEB-DL",
    "Princess.Connect.Re.Dive.S2.(01-12).1080p"
];

fn technical_pattern(field:&str,signal:&str)->Option<String>{
    if !matches!(field,"resolution"|"codec"|"bit_depth"){return None;}
    if !(2..=24).contains(&signal.len()) ||
        !signal.bytes().all(|b|b.is_ascii_lowercase()||b.is_ascii_digit()
            ||b==b'.'||b==b'_'){return None;}
    let tokens=signal.split(&['.','_'][..]).filter(|x|!x.is_empty())
        .collect::<Vec<_>>();
    if !(1..=4).contains(&tokens.len()){return None;}
    Some(format!(r"(?i)(?:^|[ ._-])(?:{})(?:$|[ ._-])",
        tokens.into_iter().map(regex::escape)
            .collect::<Vec<_>>().join("[ ._-]*")))
}

fn anchored_patch(item:&Value, catalog:&[Value])->Result<HandlerPatch,String>{
    if item["kind"]!="learned_source_regex_patch" {
        return Err("source handler identity not supplied; human review only".into());
    }
    let index=item["handler_index"].as_u64()
        .and_then(|i|usize::try_from(i).ok())
        .ok_or("missing source handler index")?;
    let source=catalog.iter().find(|v|v["index"].as_u64()==Some(index as u64))
        .ok_or("source handler not in original catalogue")?;
    let field=item["field"].as_str().ok_or("missing field")?;
    let original=item["original_regex"].as_str().ok_or("missing old regex")?;
    let replacement=item["proposed_regex"].as_str().ok_or("missing new regex")?;
    let signal=item["signal"].as_str().ok_or("missing release token")?;
    let extra=technical_pattern(field,signal)
        .ok_or("unsupported or unsafe technical marker")?;
    if source["kind"]!="regex"||source["field"]!=field
        ||source["pattern"]!=original||source["transform"]!=item["transform"] {
        return Err("source regex/field/transform mismatch (possibly superseded)".into());
    }
    // Only the exact, independently reconstructed OR of the original
    // catalogue regex and one bounded marker may reach PCRE compilation.
    let expected=format!("(?:{original})|(?:{extra})");
    if replacement!=expected || replacement.len()>512 {
        return Err("proposal is not a safe extension of its original regex".into());
    }
    Ok(HandlerPatch{index,original_pattern:original.to_owned(),
        replacement_pattern:expected})
}

fn fixture_title(fixture:&Value)->Result<&str,String>{
    let title=fixture["title"].as_str().ok_or("fixture lacks title")?;
    if title.is_empty()||title.len()>200||title.contains(['/','\\','\n','\r','\0'])
        ||title.chars().any(char::is_control) {
        return Err("fixture contains unsafe title".into());
    }
    if !fixture["expected"].is_object() ||
        fixture["expected"].as_object().is_some_and(Map::is_empty) {
        return Err("fixture needs independent field expectations".into());
    }
    Ok(title)
}
fn validate_fixtures(fixtures:&[Value])->Result<(),String>{
    if fixtures.len()>MAX_FIXTURES {return Err("too many trusted fixtures".into());}
    let mut seen=HashSet::new();
    for row in fixtures{
        let title=fixture_title(row)?;
        if !seen.insert(title){return Err("duplicate trusted fixture title".into());}
    }
    Ok(())
}
fn audit_one(item:&Value,fixtures:&[Value],catalog:&[Value])->Value{
    let fingerprint=item["fingerprint"].as_str().unwrap_or("");
    let kind=item["kind"].as_str().unwrap_or("unknown");
    let mut report=json!({
        "fingerprint":fingerprint,
        "kind":kind,
        "field":item["field"],
        "status":"review_only",
        "promotion_eligible":false,
        "verified_improvements":0,
        "synthetic_changes":0,
        "regressions":0,
        "source_handler_index":item["handler_index"],
        "reason":"requires independently verified field labels or a supported source handler"
    });
    if kind!="learned_source_regex_patch"{return report;}
    let patch=match anchored_patch(item,catalog){
        Ok(p)=>p,
        Err(reason)=>{
            report["status"]=json!("invalid_source");
            report["reason"]=json!(reason);
            return report;
        }
    };
    if let Err(error)=verify_live_regression_corpus(&[patch.clone()],&[],&[]){
        report["status"]=json!("regression");
        report["reason"]=json!(format!("protected full-parser corpus changed: {error}"));
        return report;
    }
    let field=item["field"].as_str().unwrap_or("");
    let signal=item["signal"].as_str().unwrap_or("");
    let synthetic=vec![
        format!("Example.Show.S01E02.{signal}.WEB-DL"),
        format!("Another.Film.2025.{signal}.WEB-DL"),
        format!("Different.Show.S02E10.1080p.{signal}.mkv"),
        format!("Classic.Movie.1999.{signal}.BluRay"),
    ];
    let mut titles=fixtures.iter().filter_map(|f|f["title"].as_str())
        .map(str::to_owned).collect::<Vec<_>>();
    let fixture_count=titles.len();
    titles.extend(NEGATIVES.iter().map(|s|(*s).to_owned()));
    titles.extend(synthetic.iter().cloned());
    if titles.len()>128{
        report["status"]=json!("invalid_source");
        report["reason"]=json!("audit corpus too large");
        return report;
    }
    let baseline=titles.iter().map(|title|parse_title_canonical(title,false))
        .collect::<Result<Vec<_>,_>>();
    let result=replay_handler_rules(&[patch],&[],&[],&titles);
    let (Ok(before),Ok(after))=(baseline,result)else{
        report["status"]=json!("invalid_source");
        report["reason"]=json!("candidate failed safe full-parser compilation or replay");
        return report;
    };
    let mut improvements=0usize;
    let mut synthetic_changes=0usize;
    let mut regressions=0usize;
    let mut changes=0usize;
    for (i,(a,b)) in before.iter().zip(after.iter()).enumerate(){
        if a==b{continue;}
        changes+=1;
        if i>=fixture_count+NEGATIVES.len(){
            if a.get(field)!=b.get(field){synthetic_changes+=1;}
            // Synthetic cases are not independent truth and never authorize
            // writing parser source by themselves.
            continue;
        }
        if i>=fixture_count{
            // Protected public negative controls must stay identical in
            // every field, regardless of whether a proposed token matches.
            regressions+=1;
            continue;
        }
        let expected=fixtures[i]["expected"].as_object()
            .expect("trusted fixture validated");
        let keys=a.keys().chain(b.keys()).collect::<HashSet<_>>();
        for key in keys{
            if a.get(key)==b.get(key){continue;}
            let verified=expected.get(key.as_str());
            if key==field {
                if verified==b.get(key){
                    if verified!=a.get(key){improvements+=1;}
                }else if verified.is_some(){
                    regressions+=1;
                }
                // An unlabeled improvement is unproven, NOT a failure:
                // it simply cannot authorize an automatic source patch.
            }else if verified!=b.get(key){
                regressions+=1;
            }
        }
        for (key,label) in expected{
            if a.get(key)==Some(label) && b.get(key)!=Some(label){
                regressions+=1;
            }
        }
    }
    report["verified_improvements"]=json!(improvements);
    report["synthetic_changes"]=json!(synthetic_changes);
    report["regressions"]=json!(regressions);
    report["parser_output_changes"]=json!(changes);
    let (status,reason)=if regressions>0{
        ("regression","candidate changes unrelated or previously verified fields")
    }else if changes==0{
        ("no_effect","existing parser already returns identical results for all review titles")
    }else if improvements>=2{
        ("ready_for_source_diff","two or more independently labeled fixtures improve with zero regressions")
    }else{
        ("needs_independent_evidence","parser changes on titles, but not enough trusted positive labels")
    };
    report["status"]=json!(status);
    report["reason"]=json!(reason);
    report["promotion_eligible"]=json!(status=="ready_for_source_diff");
    report
}

fn main()->Result<(),Box<dyn std::error::Error>>{
    let args=env::args().collect::<Vec<_>>();
    if args.len()!=4{
        return Err("usage: audit_community_regex daily.json trusted-fixtures.json audit.json".into());
    }
    let proposals:Vec<Value>=serde_json::from_slice(&fs::read(&args[1])?)?;
    let fixtures:Vec<Value>=serde_json::from_slice(&fs::read(&args[2])?)?;
    validate_fixtures(&fixtures)?;
    let catalog=existing_regex_rules();
    let reports=proposals.iter().take(MAX_CANDIDATES)
        .map(|item|audit_one(item,&fixtures,&catalog))
        .collect::<Vec<_>>();
    let eligible=reports.iter().filter(|r|r["promotion_eligible"]==true).count();
    fs::write(&args[3],format!("{}\n",serde_json::to_string_pretty(&reports)?))?;
    println!("community_regex_audited={} source_diff_eligible={}",
        reports.len(),eligible);
    Ok(())
}
#[cfg(test)]
mod tests{
    use super::*;
    #[test]
    fn reject_original_source_modification_and_arbitrary_regex(){
        let source=existing_regex_rules().into_iter()
            .find(|r|r["field"]=="codec" &&r["pattern"]==r"\b[hx][\. \-]?264\b")
            .expect("original h264 source handler");
        let index=source["index"].as_u64().unwrap();
        let original=source["pattern"].as_str().unwrap();
        let item=json!({
            "kind":"learned_source_regex_patch","fingerprint":"a".repeat(64),
            "handler_index":index,"field":"codec","signal":"h264",
            "original_regex":original,"transform":source["transform"],
            "proposed_regex":format!("(?:{original})|(?:{})",
                technical_pattern("codec","h264").unwrap())
        });
        let catalogue=existing_regex_rules();
        assert!(anchored_patch(&item,&catalogue).is_ok());
        assert!(anchored_patch(&json!({"original_regex":"wrong",..}),&catalogue).is_err());
    }
    #[test]
    fn no_evidence_never_authorizes_source_change(){
        let source=existing_regex_rules().into_iter()
            .find(|r|r["field"]=="codec" &&r["pattern"]==r"\b[hx][\. \-]?264\b")
            .unwrap();
        let original=source["pattern"].as_str().unwrap();
        let item=json!({
            "kind":"learned_source_regex_patch","fingerprint":"b".repeat(64),
            "handler_index":source["index"],"field":"codec","signal":"h264",
            "original_regex":original,"transform":source["transform"],
            "proposed_regex":format!("(?:{original})|(?:{})",
                technical_pattern("codec","h264").unwrap())
        });
        let res=audit_one(&item,&[],&existing_regex_rules());
        assert_eq!(res["promotion_eligible"],false);
        assert_ne!(res["status"],"ready_for_source_diff");
    }
    #[test]
    fn unsupported_field_cannot_write_source(){
        assert!(technical_pattern("seasons","s02").is_none());
        assert!(technical_pattern("codec","foo.*").is_none());
    }
}