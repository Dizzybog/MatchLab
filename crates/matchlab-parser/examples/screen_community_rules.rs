//! GitHub Actions-only reviewer for proposed public regex rules.
//! Never executes untrusted Issue text. It reads strict JSON, admits at most
//! ONE canonical, narrowly scoped marker per invocation and emits a reviewed
//! PR candidate (NOT a merged production rule).
use regex::RegexBuilder;
use serde_json::{json,Value};
use std::{env,fs};

const BAD:[&str;28]=[
    "season","episode","series","part","year","movie","sample","bluray",
    "webrip","webdl","hdtv","hdr","codec","audio","x264","x265",
    "h264","h265","hevc","hdr10","dovi","ep","eps","new","the",
    "eng","sub","dub"
];
fn canonical(marker:&str)->Option<String>{
    if !(3..=9).contains(&marker.len()) ||
       !marker.bytes().all(|b|b.is_ascii_lowercase()) ||
       BAD.contains(&marker){return None;}
    Some(format!(
        r"(?i)(?:^|[ ._-]){}[ ._-]*([1-9][0-9]{{0,4}})(?:$|[ ._-])",marker
    ))
}
fn technical_suffix(input:&str)->bool{
    let suffix=input.trim_matches(|c:char|matches!(c,' '|'.'|'_'|'-'));
    suffix.is_empty()||suffix.split(|c:char|matches!(c,' '|'.'|'_'|'-'))
        .filter(|s|!s.is_empty())
        .all(|word|matches!(word.to_ascii_lowercase().as_str(),
            "1080p"|"2160p"|"720p"|"480p"|"x264"|"x265"|"h264"|
            "h265"|"hevc"|"av1"|"aac"|"ac3"|"web"|"dl"|"webrip"|
            "hdtv"|"bluray"|"mkv"|"mp4"|"avi"|"remux"|"10bit"))
}
fn episode(re:&regex::Regex,title:&str)->Option<i64>{
    if title.len()>240||title.contains(['/', '\\','@','\n','\r']){return None;}
    let caps=re.captures(title)?;
    let group=caps.get(1)?;
    let full=caps.get(0)?;
    if re.captures_iter(title).take(2).count()!=1{return None;}
    if !technical_suffix(&title[full.end()..]){return None;}
    group.as_str().parse::<i64>().ok().filter(|n|(1..=99_999).contains(n))
}
fn already_parsed(title:&str,n:i64)->bool{
    let Ok(parsed)=matchlab_parser::parse_title(title,false)else{return false;};
    let val=Value::Object(parsed);
    val["episodes"].as_array().is_some_and(|es|
        es.len()==1&&es[0].as_i64()==Some(n))
      ||val["anime_episode_candidate"]["episode"].as_i64()==Some(n)
      ||val["absolute_episode_candidate"].as_i64()==Some(n)
}
fn safe_title(title:&str)->bool{
    let lower=title.to_ascii_lowercase();
    (1..=180).contains(&title.len()) &&
        !title.contains(['/', '\\','@','\0','\r','\n']) &&
        !title.chars().any(char::is_control) &&
        !["http:","https:","magnet:","token=","password=","secret=",
          "apikey="].iter().any(|part|lower.contains(part))
}
fn screen(item:&Value,accepted:&[Value])->Option<Value>{
    let marker=item["marker"].as_str()?;
    if accepted.iter().any(|x|x["marker"]==marker){return None;}
    let regex=canonical(marker)?;
    if item["regex"]!=regex{return None;}
    let score=(item["evidence_count"].as_i64()?,
        item["distinct_imdb"].as_i64()?,
        item["distinct_episodes"].as_i64()?);
    if score.0<12||score.1<4||score.2<5{return None;}
    let re=RegexBuilder::new(&regex).size_limit(16*1024).build().ok()?;
    let negatives=[
       "The Movie 2024","Bleach S16","Show S01E03","Title 1080p WEB DL",
       "Movie 1917","Doctor Who 2005","Naruto x265 1080p",
       "Episode 300 Explained","The Series EPIS 300 Documentary",
       "Show EPIS300 trailer","Show EPIS300 CAM", "Film EPIS 300 sample",
       "Movie EPIS 2024 WEB-DL", "Show S03E10 WEB-DL",
       "The Episode Guide EPIS 300 Review"
    ];
    if negatives.iter().any(|bad|episode(&re,bad).is_some()){return None;}
    let examples=item["examples"].as_array()?;
    if !(8..=12).contains(&examples.len()){return None;}
    let mut seen=std::collections::HashSet::new();
    let mut series=std::collections::HashSet::new();
    let mut numbers=std::collections::HashSet::new();
    let mut unsolved=0usize;
    for sample in examples {
        let title=sample["title"].as_str()?;
        let abs=sample["absolute_episode"].as_i64()?;
        let imdb=sample["imdb_id"].as_str()?;
        if !safe_title(title) || !(9..=10).contains(&imdb.len()) ||
            !imdb.starts_with("tt") || !imdb[2..].bytes().all(|b|b.is_ascii_digit()) ||
            episode(&re,title)!=Some(abs){return None;}
        for key in ["season","episode","anidb_episode"]{
            if !sample[key].as_i64().is_some_and(|n|(1..=99_999).contains(&n)){
                return None;
            }
        }
        if !seen.insert((imdb.to_owned(),abs)){return None;}
        series.insert(imdb);numbers.insert(abs);
        if !already_parsed(title,abs){unsolved+=1;}
    }
    if series.len()<3||numbers.len()<5||unsolved<6 {return None;}
    Some(json!({
        "marker":marker,
        "regex":regex,
        "evidence_count":score.0,
        "distinct_imdb":score.1,
        "distinct_episodes":score.2,
        "source_sha":item["source_build"],
        "review":"requires_independent_metadata_confirmation_before_merge",
        "examples":examples
    }))
}
fn main()->Result<(),Box<dyn std::error::Error>>{
    let args=env::args().collect::<Vec<_>>();
    if args.len()!=4 {
        return Err("usage: screen_community_rules proposals.json current.json output.json".into());
    }
    let incoming:Vec<Value>=serde_json::from_slice(&fs::read(&args[1])?)?;
    let mut existing:Vec<Value>=serde_json::from_slice(&fs::read(&args[2])?)?;
    if existing.len()>32{return Err("community rule set exceeds 32-entry hard cap".into());}
    let mut changed=false;
    for candidate in incoming.iter().take(24){
        if existing.len()>=32{break;}
        if let Some(verified)=screen(candidate,&existing){
            existing.push(verified);
            changed=true;
            break; // hard limit: at most one bot PR rule per daily run
        }
    }
    existing.sort_by(|a,b|a["marker"].as_str().cmp(&b["marker"].as_str()));
    fs::write(&args[3],serde_json::to_vec_pretty(&existing)?)?;
    println!("community_regex_candidate={}",if changed{"accepted_for_review"}else{"none"});
    Ok(())
}
#[cfg(test)]
mod tests{
    use super::*;
    #[test]
    fn unsafe_markers_cannot_become_regex(){
        assert!(canonical("epis").is_some());
        for bad in ["h264","episode","x265","xxx.*","aa","123","foo\n"]{
            assert!(canonical(bad).is_none(),"{bad}");
        }
    }
    #[test]
    fn source_parser_is_required_to_still_miss_the_sample(){
        let re=RegexBuilder::new(&canonical("epis").unwrap()).build().unwrap();
        assert_eq!(episode(&re,"Bleach.EPIS.300.1080p.WEB-DL"),Some(300));
        assert!(episode(&re,"Bleach.EPIS300.trailer").is_none());
        assert!(already_parsed("Show.S01E03",3));
    }
}
