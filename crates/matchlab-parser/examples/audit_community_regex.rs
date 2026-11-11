//! Trusted GitHub Actions regex impact auditor.
//!
//! Incoming Issue proposals are UNTRUSTED, even when they contain evidence
//! counts. The GitHub intake validates their schema and source ancestry first.
//! This executable independently reconstructs a bounded source patch using
//! the exact bundled handler, reparses public test titles with the entire
//! original and proposed Rust parsers, and checks maintainer-owned labels.
//! It never activates a rule or edits source code.
use matchlab_parser::{existing_regex_rules, parse_title_canonical,
    replay_handler_rules, verify_live_regression_corpus, HandlerAddition,
    HandlerCorrection, HandlerPatch};
use serde_json::{json, Map, Value};
use std::{collections::{HashMap, HashSet}, env, fs};

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
fn audit_one(item:&Value,fixtures:&[Value],catalog:&[Value],
    community_titles:&[String])->Value{
    let fingerprint=item["fingerprint"].as_str().unwrap_or("");
    let kind=item["kind"].as_str().unwrap_or("unknown");
    let mut report=json!({
        "fingerprint":fingerprint,
        "kind":kind,
        "field":item["field"],
        "status":"review_only",
        "promotion_eligible":false,
        "verified_improvements":0,
        "verified_fixture_indexes":[],
        "synthetic_changes":0,
        "community_title_cases":community_titles.len(),
        "community_title_changes":0,
        "community_field_changes":0,
        "regressions":0,
        "source_handler_index":item["handler_index"],
        "reason":"requires independently verified field labels or a supported source handler"
    });
    // For field and TV proposals that have no unambiguous native handler
    // mapping, still quantify present coverage. Never infer labels from ML
    // output or auto-patch unrelated handler transforms/options.
    if kind=="learned_field_regex"{
        let field=item["field"].as_str().unwrap_or_default();
        let signal=item["signal"].as_str().unwrap_or_default();
        if (2..=24).contains(&signal.len()) && signal.bytes().all(|byte|
            byte.is_ascii_lowercase()||byte.is_ascii_digit()||byte==b'.'||byte==b'_')
            && !field.is_empty() && !item["canonical"].is_null()
        {
            let samples=[
                format!("Example.Show.S01E02.{signal}.WEB-DL"),
                format!("Another.Movie.2024.{signal}.BluRay"),
                format!("Different.Show.S02E04.1080p.{signal}.mkv"),
            ];
            let matched=samples.iter().filter(|title|
                parse_title_canonical(title,false).ok().is_some_and(|parsed|
                    parsed.get(field)==Some(&item["canonical"])))
                .count();
            report["native_marker_hits"]=json!(matched);
            report["native_marker_cases"]=json!(samples.len());
            // These are real torrent names submitted with the same batch.
            // The candidate's own labels are still NOT ground truth.
            let community_hits=community_titles.iter().filter(|title|
                parse_title_canonical(title,false).ok().is_some_and(|parsed|
                    parsed.get(field)==Some(&item["canonical"])))
                .count();
            report["community_native_hits"]=json!(community_hits);
            report["reason"]=json!(if matched==samples.len(){
                "canonical parser already resolves all checked marker forms; source novelty unproven"
            }else{
                "not all field variants are resolved; needs exact handler identity and trusted labels"
            });
        }
        return report;
    }
    if kind=="learned_tv_regex"{
        let patterns:&[(&str,i64,i64)]=match item["family"].as_str(){
            Some("sxe")=>&[("Example.Show.S01E02",1,2),("Example.Show.s02e18",2,18)],
            Some("x")=>&[("Example.Show.1x02",1,2),("Example.Show.2x18",2,18)],
            Some("words")=>&[("Example.Show.Season.1.Episode.2",1,2),
                ("Example.Show.Season.2.Episode.18",2,18)],
            _=>&[]
        };
        if !patterns.is_empty(){
            let matched=patterns.iter().filter(|(title,season,episode)|
                parse_title_canonical(title,false).ok().is_some_and(|parsed|
                    parsed["seasons"]==json!([season])&&parsed["episodes"]==json!([episode])))
                .count();
            report["native_marker_hits"]=json!(matched);
            report["native_marker_cases"]=json!(patterns.len());
            report["reason"]=json!(if matched==patterns.len(){
                "canonical parser already resolves tested TV coordinates; independent novelty needed"
            }else{"TV format has gaps; mapping and full-field regression tests required"});
        }
        return report;
    }
    if kind=="learned_general_regex_review" {
        let same=item["handler_index"].as_u64().and_then(|index|
            catalog.iter().find(|r|r["index"].as_u64()==Some(index))
        ).is_some_and(|row|row["field"]==item["field"]&&row["pattern"]==item["regex"]);
        if same{
            report["status"]=json!("no_effect");
            report["reason"]=json!("identical regex already exists on the same native handler");
        }
        return report;
    }
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
    let negative_end=titles.len();
    titles.extend(community_titles.iter().cloned());
    let community_end=titles.len();
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
    let mut improved_fixture_indexes=Vec::new();
    let mut distinct_title_identities=HashSet::new();
    let mut synthetic_changes=0usize;
    let mut community_changes=0usize;
    let mut community_field_changes=0usize;
    let mut regressions=0usize;
    let mut changes=0usize;
    for (i,(a,b)) in before.iter().zip(after.iter()).enumerate(){
        if a==b{continue;}
        changes+=1;
        if i>=community_end{
            if a.get(field)!=b.get(field){synthetic_changes+=1;}
            // Synthetic cases never authorize a source patch.
            continue;
        }
        if i>=negative_end{
            community_changes+=1;
            if a.get(field)!=b.get(field){community_field_changes+=1;}
            // For a field-specific source patch, changing another parsed
            // field in an unverified real torrent is a safety regression.
            let keys=a.keys().chain(b.keys()).collect::<HashSet<_>>();
            if keys.iter().any(|key|key.as_str()!=field &&
                a.get(key.as_str())!=b.get(key.as_str())){
                regressions+=1;
            }
            continue;
        }
        if i>=fixture_count{
            // Negative controls must remain identical in every field.
            regressions+=1;
            continue;
        }
        let expected=fixtures[i]["expected"].as_object()
            .expect("trusted fixture validated");
        let keys=a.keys().chain(b.keys()).collect::<HashSet<_>>();
        for key in keys{
            if a.get(key)==b.get(key){continue;}
            let verified=expected.get(key.as_str());
            if key.as_str()==field {
                if verified==b.get(key){
                    if verified!=a.get(key){
                        improvements+=1;
                        improved_fixture_indexes.push(i);
                        if let Some(title)=b.get("title").and_then(Value::as_str){
                            distinct_title_identities.insert(title.to_ascii_lowercase());
                        }
                    }
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
    report["verified_fixture_indexes"]=json!(improved_fixture_indexes);
    report["distinct_verified_titles"]=json!(distinct_title_identities.len());
    report["synthetic_changes"]=json!(synthetic_changes);
    report["community_title_changes"]=json!(community_changes);
    report["community_field_changes"]=json!(community_field_changes);
    report["regressions"]=json!(regressions);
    report["parser_output_changes"]=json!(changes);
    let (status,reason)=if regressions>0{
        ("regression","candidate changes unrelated or previously verified fields")
    }else if changes==0{
        ("no_effect","existing parser already returns identical results for all review titles")
    }else if improvements>=2 && distinct_title_identities.len()>=2{
        ("ready_for_source_diff","two or more independently labeled series identities improve with zero regressions")
    }else{
        ("needs_independent_evidence","parser changes on titles, but not enough trusted positive labels")
    };
    report["status"]=json!(status);
    report["reason"]=json!(reason);
    report["promotion_eligible"]=json!(status=="ready_for_source_diff");
    report
}

/// Only rule plans reconstructed by the trusted Python schema/ancestry
/// validator enter this path. This Rust boundary still checks exact source
/// identity, source transform, operation and bounded safe compilation before
/// running the full native parser.
fn planned_rules(
    plan:&Value,catalog:&[Value]
)->Result<(Vec<HandlerPatch>,Vec<HandlerAddition>,Vec<HandlerCorrection>,
    HashSet<String>),String>{
    let changes=plan["source_changes"].as_array()
        .ok_or("missing native source change plan")?;
    if changes.is_empty()||changes.len()>4{
        return Err("empty or excessive source changes".into());
    }
    let mut patches=Vec::new();
    let mut additions=Vec::new();
    let mut corrections=Vec::new();
    let mut fields=HashSet::new();
    let mut seen=HashSet::new();
    for change in changes{
        let index=change["handler_index"].as_u64()
            .and_then(|v|usize::try_from(v).ok())
            .ok_or("invalid source handler index")?;
        let source=catalog.iter().find(|r|
            r["index"].as_u64()==Some(index as u64))
            .ok_or("missing source handler")?;
        let field=change["field"].as_str().ok_or("missing field")?;
        let original=change["original_pattern"].as_str()
            .ok_or("missing original pattern")?;
        let pattern=change["pattern"].as_str()
            .ok_or("missing proposed pattern")?;
        let transform=change["transform"].as_str()
            .ok_or("missing trusted transform")?;
        let action=change["operation"].as_str().ok_or("missing operation")?;
        if source["kind"]!="regex"||source["field"]!=field ||
            source["pattern"]!=original||source["transform"]!=transform{
            return Err("native source identity diverged".into());
        }
        if !(4..=512).contains(&pattern.len()) ||
            !seen.insert((index,action.to_owned(),pattern.to_owned())){
            return Err("unsafe or duplicate native source change".into());
        }
        // The linear-time Rust regex engine restricts untrusted patterns
        // before PCRE2 is permitted to compile the replay engine.
        let safe=regex::RegexBuilder::new(pattern)
            .size_limit(256*1024).build()
            .map_err(|_|"candidate regex is not in the safe linear-time subset")?;
        if safe.is_match(""){
            return Err("zero-width candidate pattern is forbidden".into());
        }
        fields.insert(field.to_owned());
        match action{
            "patch"=>{
                patches.push(HandlerPatch{index,
                    original_pattern:original.to_owned(),
                    replacement_pattern:pattern.to_owned()});
            }
            "add"=>{
                additions.push(HandlerAddition{
                    source_index:index,pattern:pattern.to_owned()});
            }
            "correct"=>{
                if !matches!(field,"seasons"|"episodes") {
                    return Err("unsafe corrective target".into());
                }
                let is_range=transform=="range_func";
                corrections.push(HandlerCorrection{
                    source_index:index,pattern:pattern.to_owned(),is_range});
            }
            _=>return Err("unsupported native source operation".into()),
        }
    }
    Ok((patches,additions,corrections,fields))
}

/// A common before/after decision engine for all supported community regex
/// families. Real torrent names are untrusted test INPUTS, never truth labels.
/// Only independently maintained expected fixtures may authorize a source PR.
fn audit_planned(
    item:&Value,plan:&Value,fixtures:&[Value],catalog:&[Value],
    community_titles:&[String]
)->Value{
    let fp=item["fingerprint"].as_str().unwrap_or("");
    let kind=item["kind"].as_str().unwrap_or("unknown");
    let mut report=json!({
        "fingerprint":fp,"kind":kind,"field":item["field"],
        "source_handler_index":item["handler_index"],
        "status":"review_only","promotion_eligible":false,
        "reason":plan["reason"],
        "source_changes":[],
        "verified_improvements":0,"distinct_verified_titles":0,
        "verified_fixture_indexes":[],
        "verified_fields":[],"regressions":0,"parser_output_changes":0,
        "community_title_cases":community_titles.len(),
        "community_title_changes":0,"community_field_changes":0,
        "synthetic_changes":0
    });
    if plan["source_changes"].as_array().is_some_and(Vec::is_empty){
        report["status"]=json!(if plan["reason"]=="already_supported"{
            "no_effect"
        }else{"review_only"});
        return report;
    }
    let (patches,additions,corrections,fields)=match planned_rules(plan,catalog){
        Ok(parts)=>parts,
        Err(reason)=>{
            report["status"]=json!("invalid_source");
            report["reason"]=json!(reason);
            return report;
        }
    };
    report["source_changes"]=plan["source_changes"].clone();
    if let Err(error)=verify_live_regression_corpus(&patches,&additions,&corrections){
        report["status"]=json!("regression");
        report["regressions"]=json!(1);
        report["reason"]=json!(format!("protected parser regression: {error}"));
        return report;
    }
    let mut titles=fixtures.iter()
        .map(|row|row["title"].as_str().unwrap_or_default().to_owned())
        .collect::<Vec<_>>();
    let fixture_end=titles.len();
    titles.extend(NEGATIVES.iter().map(|s|(*s).to_owned()));
    let negatives_end=titles.len();
    titles.extend(community_titles.iter().cloned());
    let community_end=titles.len();
    // Synthetic marker cases are replay diagnostics only, never labels.
    if let Some(marker)=item["signal"].as_str(){
        if (2..=24).contains(&marker.len()) &&
            marker.bytes().all(|b|b.is_ascii_lowercase()||
                b.is_ascii_digit()||b==b'.'||b==b'_'){
            titles.push(format!("Synthetic.Show.S01E02.{marker}.WEB-DL"));
            titles.push(format!("Synthetic.Movie.2025.{marker}.mkv"));
        }
    }
    if titles.len()>128{
        report["status"]=json!("invalid_source");
        report["reason"]=json!("bounded comparison corpus exceeded");
        return report;
    }
    let before=titles.iter().map(|title|parse_title_canonical(title,false))
        .collect::<Result<Vec<_>,_>>();
    let after=replay_handler_rules(&patches,&additions,&corrections,&titles);
    let (Ok(before),Ok(after))=(before,after)else{
        report["status"]=json!("invalid_source");
        report["reason"]=json!("safe native parser replay could not compile");
        return report;
    };
    let mut gains=0usize;
    let mut regressions=0usize;
    let mut changed=0usize;
    let mut actual_changes=0usize;
    let mut actual_target_changes=0usize;
    let mut synthetic_changes=0usize;
    let mut fixture_indexes=HashSet::new();
    let mut identities=HashSet::new();
    let mut verified_fields=HashSet::new();
    for (i,(before,after)) in before.iter().zip(after.iter()).enumerate(){
        if before==after{continue;}
        changed+=1;
        let keys=before.keys().chain(after.keys())
            .map(String::as_str).collect::<HashSet<_>>();
        if i>=community_end{
            synthetic_changes+=1;
            continue;
        }
        if i>=negatives_end{
            actual_changes+=1;
            if fields.iter().any(|field|before.get(field)!=after.get(field)){
                actual_target_changes+=1;
            }
            if keys.iter().any(|key|!fields.contains(*key) &&
                before.get(*key)!=after.get(*key)){
                regressions+=1;
            }
            continue;
        }
        if i>=fixture_end{
            // All protected release names are full-output regression oracles.
            regressions+=1;
            continue;
        }
        let expected=fixtures[i]["expected"].as_object()
            .expect("validated expected labels");
        let mut fixture_improved=false;
        for key in keys{
            if before.get(key)==after.get(key){continue;}
            if let Some(label)=expected.get(key){
                if after.get(key)!=Some(label){
                    regressions+=1;
                }else if fields.contains(key) && before.get(key)!=Some(label){
                    gains+=1;
                    fixture_improved=true;
                    verified_fields.insert(key.to_owned());
                }
            }else if !fields.contains(key){
                // A proposed field regex must not silently damage title,
                // year, metadata or other unlabeled parsed fields.
                regressions+=1;
            }
        }
        if fixture_improved{
            fixture_indexes.insert(i);
            let identity=after.get("title").and_then(Value::as_str)
                .or_else(||before.get("title").and_then(Value::as_str))
                .unwrap_or(titles[i].as_str()).to_lowercase();
            identities.insert(identity);
        }
    }
    let mut idx=fixture_indexes.into_iter().collect::<Vec<_>>();
    idx.sort_unstable();
    let mut fields_verified=verified_fields.iter().cloned().collect::<Vec<_>>();
    fields_verified.sort();
    report["source_handler_index"]=json!(
        plan["source_changes"][0]["handler_index"]);
    report["field"]=json!(fields.iter().cloned().collect::<Vec<_>>());
    report["verified_improvements"]=json!(gains);
    report["verified_fixture_indexes"]=json!(idx);
    report["distinct_verified_titles"]=json!(identities.len());
    report["verified_fields"]=json!(fields_verified);
    report["parser_output_changes"]=json!(changed);
    report["community_title_changes"]=json!(actual_changes);
    report["community_field_changes"]=json!(actual_target_changes);
    report["synthetic_changes"]=json!(synthetic_changes);
    report["regressions"]=json!(regressions);
    let proven=gains>=2 && identities.len()>=2 &&
        fields.iter().all(|field|verified_fields.contains(field));
    let (status,reason)=if regressions>0{
        ("regression","full parser replay changed protected or unrelated fields")
    }else if changed==0{
        ("no_effect","candidate produced exactly the existing parser output")
    }else if proven{
        ("ready_for_source_diff",
         "independent fixtures prove all modified fields; zero regressions")
    }else{
        ("needs_independent_evidence",
         "native replay ran but independent labels do not prove a safe gain")
    };
    report["status"]=json!(status);
    report["reason"]=json!(reason);
    report["promotion_eligible"]=json!(status=="ready_for_source_diff");
    report
}

fn main()->Result<(),Box<dyn std::error::Error>>{
    let args=env::args().collect::<Vec<_>>();
    if !(4..=6).contains(&args.len()){
        return Err("usage: audit_community_regex daily.json trusted-fixtures.json audit.json [community-titles.json] [plans.json]".into());
    }
    let proposals:Vec<Value>=serde_json::from_slice(&fs::read(&args[1])?)?;
    let fixtures:Vec<Value>=serde_json::from_slice(&fs::read(&args[2])?)?;
    validate_fixtures(&fixtures)?;
    let supplied:HashMap<String,Vec<String>>=if args.len()>=5{
        serde_json::from_slice(&fs::read(&args[4])?)?
    }else{HashMap::new()};
    if supplied.len()>MAX_CANDIDATES || supplied.values().any(|titles|
        titles.len()>12 || titles.iter().any(|title|
            title.is_empty() || title.len()>200 ||
            title.contains(['/', '\\', '\n', '\r', '\0']) ||
            title.chars().any(char::is_control))){
        return Err("untrusted community title corpus exceeds bounds".into());
    }
    let plans:HashMap<String,Value>=if args.len()==6{
        serde_json::from_slice(&fs::read(&args[5])?)?
    }else{HashMap::new()};
    if args.len()==6 && (plans.len()!=proposals.len() ||
        plans.len()>MAX_CANDIDATES){
        return Err("community plans do not cover the complete bounded batch".into());
    }
    let catalog=existing_regex_rules();
    let reports=proposals.iter().take(MAX_CANDIDATES)
        .map(|item|{
            let fp=item["fingerprint"].as_str().unwrap_or("");
            let cases=supplied.get(fp).map(Vec::as_slice).unwrap_or(&[]);
            if args.len()==6{
                if let Some(plan)=plans.get(fp){
                    audit_planned(item,plan,&fixtures,&catalog,cases)
                }else{
                    json!({"fingerprint":fp,"status":"invalid_source",
                        "promotion_eligible":false,"reason":"missing plan"})
                }
            }else{
                audit_one(item,&fixtures,&catalog,cases)
            }
        })
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
    fn all_native_plan_modes_replay_only_exact_existing_handler_sources(){
        let catalogue=existing_regex_rules();
        let source=catalogue.iter().find(|row|
            row["field"]=="audio" && row["transform"]=="uniq_concat(value('OPUS'))")
            .expect("native OPUS source");
        let index=source["index"].as_u64().unwrap();
        let pattern=r"(?i)(?:^|[ ._-])opus(?:$|[ ._-])";
        let plan=json!({"source_changes":[{
            "operation":"add","handler_index":index,"field":"audio",
            "original_pattern":source["pattern"],"pattern":pattern,
            "transform":source["transform"]
        }]});
        let (patches,adds,corrections,fields)=
            planned_rules(&plan,&catalogue).expect("safe native addition");
        assert!(patches.is_empty()&&corrections.is_empty());
        assert_eq!(adds.len(),1);
        assert!(fields.contains("audio"));
        let bad=json!({"source_changes":[{
            "operation":"add","handler_index":index,"field":"year",
            "original_pattern":source["pattern"],"pattern":pattern,
            "transform":source["transform"]
        }]});
        assert!(planned_rules(&bad,&catalogue).is_err());
        let malicious=json!({"source_changes":[{
            "operation":"add","handler_index":index,"field":"audio",
            "original_pattern":source["pattern"],
            "pattern":"(?s)(.*)+","transform":source["transform"]
        }]});
        assert!(planned_rules(&malicious,&catalogue).is_err());
    }

    #[test]
    fn actual_torrent_names_never_count_as_independent_labels(){
        let catalogue=existing_regex_rules();
        let source=catalogue.iter().find(|row|
            row["field"]=="audio" && row["transform"]=="uniq_concat(value('OPUS'))")
            .unwrap();
        let plan=json!({"source_changes":[{
            "operation":"add","handler_index":source["index"],"field":"audio",
            "original_pattern":source["pattern"],
            "pattern":r"(?i)(?:^|[ ._-])opus(?:$|[ ._-])",
            "transform":source["transform"]
        }]});
        let candidate=json!({"fingerprint":"a".repeat(64),
            "kind":"learned_field_regex","field":"audio","signal":"opus"});
        let titles=vec!["Example.Show.S01E01.Opus.mkv".to_owned()];
        let result=audit_planned(&candidate,&plan,&[],&catalogue,&titles);
        assert_ne!(result["status"],"ready_for_source_diff");
        assert_eq!(result["promotion_eligible"],false);
        assert_eq!(result["verified_improvements"],0);
        assert_eq!(result["community_title_cases"],1);
    }
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
        let mut forged=item.clone();
        forged["original_regex"]=json!("wrong");
        assert!(anchored_patch(&forged,&catalogue).is_err());
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
        let res=audit_one(&item,&[],&existing_regex_rules(),&[]);
        assert_eq!(res["promotion_eligible"],false);
        assert_ne!(res["status"],"ready_for_source_diff");
    }
    #[test]
    fn unsupported_field_cannot_write_source(){
        assert!(technical_pattern("seasons","s02").is_none());
        assert!(technical_pattern("codec","foo.*").is_none());
    }
}