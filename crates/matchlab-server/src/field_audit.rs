//! Conservative field-level parser audit. Independent raw-name clues suggest
//! fields that *might* be missing; none is a training label or an auto-fix.
//! Detection is O(filename length), bounded, and makes no database queries.
use serde_json::{json,Value};

fn leaf(raw:&str)->&str{
    raw.rsplit(&['/','\\'][..]).next().unwrap_or(raw)
}
fn tokens(raw:&str)->Vec<String>{
    leaf(raw).split(|c:char|!c.is_ascii_alphanumeric())
        .filter(|s|!s.is_empty()).take(80)
        .map(str::to_ascii_lowercase).collect()
}
fn has(tokens:&[String],choices:&[&str])->Option<String>{
    tokens.iter().find(|t|choices.contains(&t.as_str())).cloned()
}
fn pair(tokens:&[String],first:&str,second:&str)->bool{
    tokens.windows(2).any(|p|p[0]==first&&p[1]==second)
}
fn present(parsed:&Value,field:&str)->bool{
    match &parsed[field]{
        Value::Null=>false,
        Value::String(s)=>!s.trim().is_empty(),
        Value::Array(v)=>!v.is_empty(),
        Value::Object(v)=>!v.is_empty(),
        Value::Bool(v)=>*v,
        _=>true,
    }
}
fn missing(out:&mut Vec<Value>,parsed:&Value,field:&'static str,
    evidence:&str,confidence:&'static str,
){
    if !present(parsed,field){
        // evidence is always a short allowlisted technical token, never a
        // user-derived arbitrary substring, secret or filename.
        out.push(json!({
            "code":format!("field_missing_{field}"),
            "field":field,"confidence":confidence,
            "signal":evidence,"severity":"review",
            "message":format!("Filename has a plausible {field} marker that was not parsed")
        }));
    }
}
fn explicit_season(tokens:&[String])->bool{
    tokens.iter().any(|t|{
        let Some(n)=t.strip_prefix('s')else{return false};
        !n.is_empty()&&n.len()<=3&&n.bytes().all(|c|c.is_ascii_digit())
        && n.parse::<u16>().is_ok_and(|v|(1..=120).contains(&v))
    })||tokens.iter().any(|t|{
        let Some(rest)=t.strip_prefix('s')else{return false};
        let Some((season,episode))=rest.split_once('e')else{return false};
        !season.is_empty()&&season.len()<=3&&!episode.is_empty()
            &&episode.len()<=4
            &&season.bytes().all(|c|c.is_ascii_digit())
            &&episode.bytes().all(|c|c.is_ascii_digit())
    })
}
fn explicit_episode(tokens:&[String])->bool{
    tokens.iter().any(|t|{
        let Some(ep)=t.strip_prefix('e')else{return false};
        !ep.is_empty()&&ep.len()<=4&&ep.bytes().all(|c|c.is_ascii_digit())
    })||tokens.iter().any(|t|{
        let Some(rest)=t.strip_prefix('s')else{return false};
        let Some((_season,ep))=rest.split_once('e')else{return false};
        !ep.is_empty()&&ep.len()<=4&&ep.bytes().all(|c|c.is_ascii_digit())
    })
}
pub(crate) fn supported_pixel_height(token:&str)->bool{
    let Some(prefix)=token.strip_suffix('p')else{return false;};
    let Ok(n)=prefix.parse::<u16>()else{return false;};
    matches!(n,240|360|480|540|576|600|640|720|800|864|900|960
        |1024|1080|1200|1440|1600|1800|2160|2880|3200|4320)
}
fn audio_channels(raw:&str)->Option<&'static str>{
    let bytes=leaf(raw).as_bytes();
    for (n,signal) in [("5.1","5.1"),("7.1","7.1"),("2.0","2.0")] {
        let needle=n.as_bytes();
        for (i,window) in bytes.windows(3).enumerate(){
            if window==needle
                &&(i==0||!bytes[i-1].is_ascii_digit())
                &&(i+3==bytes.len()||!bytes[i+3].is_ascii_digit()){
                return Some(signal);
            }
        }
    }
    None
}
/// A parser/matcher discrepancy is only an *observation*. In particular,
/// the filename year may be an episode's air year or a remaster year rather
/// than the IMDb premiere year. Never automatically turn these into labels.
fn candidate_num(value:&Value)->Option<i64>{
    value.as_i64().filter(|v|*v>0)
}
fn includes_number(value:&Value,number:i64)->bool{
    value.as_array().is_some_and(|items|
        items.iter().any(|v|v.as_i64()==Some(number)))
}
fn verified_anime_remap(selected:&Value)->bool{
    selected["anime"].is_object()
        || selected["episode_range"]["complete"]==true
}
fn compare_parsed_to_selected(parsed:&Value,selected:&Value)->Vec<Value>{
    let mut issues=Vec::new();
    if !selected.is_object(){return issues;}
    let is_series=selected["category"]=="series";
    let verified_remap=verified_anime_remap(selected);
    if is_series {
        // Packs have a starting and ending coordinate: the first parsed
        // number need only occur within the selected interval, not be equal
        // to every representative number.
        for (field,selected_field) in [("seasons","season"),("episodes","episode")] {
            let Some(parsed_values)=parsed[field].as_array().filter(|v|!v.is_empty())else{continue};
            if parsed_values.len()>16 || !parsed_values.iter()
                .all(|v|v.as_i64().is_some_and(|n|(1..=99_999).contains(&n))){
                continue;
            }
            let actual=selected[selected_field].as_i64();
            if actual.is_none() {
                issues.push(json!({
                    "code":format!("field_unresolved_{selected_field}"),
                    "field":selected_field,"confidence":"review","severity":"review",
                    "signal":"parsed_has_coordinate",
                    "message":"Parser has an episode/season coordinate but selected match has none"
                }));
            }else if !verified_remap && parsed_values.len()==1
                && let Some(n)=actual
                && !includes_number(&parsed[field],n)
            {
                issues.push(json!({
                    "code":format!("field_mismatch_{selected_field}"),
                    "field":selected_field,"confidence":"review","severity":"review",
                    "signal":"parsed_and_selected_disagree",
                    "message":"Parser and matcher selected different episode/season coordinates"
                }));
            }
        }
    }
    // IMDb 'year' normally means a title's first release; a torrent year
    // might describe its air date or a later edition. Suppress TV series
    // and never treat this observation alone as proof.
    if !is_series
        && let (Some(parsed_year),Some(selected_year))=(
            candidate_num(&parsed["year"]),candidate_num(&selected["year"]))
        && (1880..=2100).contains(&parsed_year)
        && (1880..=2100).contains(&selected_year)
        && parsed_year!=selected_year
    {
        issues.push(json!({
            "code":"field_mismatch_year","field":"year",
            "confidence":"review","severity":"review",
            "signal":"release_year_differs_from_imdb_start_year",
            "message":"Parsed release year differs from IMDb title start year; this may be legitimate"
        }));
    }
    issues
}

/// These are suspicious extraction *spans*, not trusted corrections.
/// Record them as review evidence; an ML score must not turn them directly
/// into a field value or activate a source regex.
fn extraction_review(raw:&str,parsed:&Value)->Vec<Value>{
    let name=leaf(raw);
    let release=tokens(name);
    let mut issues=Vec::new();
    let structural=|token:&str|{
        let b=token.as_bytes();
        let explicit_sxe=b.first()==Some(&b's')
            && token.split_once('e').is_some_and(|(s,e)|
                s.len()>=2&&s[1..].bytes().all(|c|c.is_ascii_digit())
                &&!e.is_empty()&&e.bytes().all(|c|c.is_ascii_digit()));
        let numeric_pair=token.split_once('x').is_some_and(|(l,r)|
            !l.is_empty()&&!r.is_empty()&&l.len()<=3&&r.len()<=5
            &&l.bytes().all(|c|c.is_ascii_digit())
            &&r.bytes().all(|c|c.is_ascii_digit()));
        explicit_sxe||numeric_pair||supported_pixel_height(token)
            ||matches!(token,"x264"|"x265"|"h264"|"h265"|"hevc"|
                "webdl"|"webrip"|"bluray"|"bdrip"|"hdrip"|"hdtv")
    };
    if let Some(split)=release.iter().position(|t|structural(t))
        &&split>0
        &&let Some(parsed_title)=parsed["title"].as_str()
        &&!parsed_title.is_empty()
    {
        let extracted=tokens(parsed_title);
        let expected_prefix=&release[..split];
        let aligned=extracted.iter().zip(expected_prefix)
            .all(|(a,b)|a==b);
        if aligned && extracted.len()>expected_prefix.len()
            &&extracted.iter().any(|t|structural(t)){
            issues.push(json!({
                "code":"field_overextracted_title","field":"title",
                "confidence":"review","signal":"release_marker_in_title",
                "severity":"review",
                "message":"Parsed title may contain release metadata beyond its title boundary"
            }));
        } else if aligned && extracted.len()+1<expected_prefix.len() {
            issues.push(json!({
                "code":"field_underextracted_title","field":"title",
                "confidence":"review","signal":"unparsed_title_prefix",
                "severity":"review",
                "message":"Parsed title may omit multiple words before the first release marker"
            }));
        }
    }
    // A clear SxxExx-Eyy range is evidence that extracting only one
    // episode may be incomplete; it is *not* a replacement coordinate.
    static RANGE:std::sync::OnceLock<regex::Regex>=std::sync::OnceLock::new();
    let range=RANGE.get_or_init(||regex::Regex::new(
        r"(?i)(?:^|[._ -])s[0-9]{1,3}[._ -]*e([0-9]{1,4})[._ -]*(?:-|to|through)[._ -]*(?:e(?:p)?)?[._ -]*([0-9]{1,4})(?:$|[._ -])"
    ).expect("bounded episode-range evidence pattern"));
    if name.len()<=240 {
        if let Some(caps)=range.captures(name)
            &&let (Some(a),Some(b))=(caps.get(1),caps.get(2))
            &&let (Ok(start),Ok(end))=(
                a.as_str().parse::<i64>(),b.as_str().parse::<i64>())
            &&start>0&&end>=start&&end-start<=100
            &&let Some(episodes)=parsed["episodes"].as_array()
            &&!episodes.is_empty()
            &&(!episodes.iter().any(|v|v.as_i64()==Some(start))
                ||!episodes.iter().any(|v|v.as_i64()==Some(end)))
        {
            issues.push(json!({
                "code":"field_underextracted_episodes","field":"episodes",
                "confidence":"review","signal":"explicit_episode_range_incomplete",
                "severity":"review",
                "message":"An explicit episode range may have been parsed incompletely"
            }));
        }
    }
    // Exactly one explicit SxxExx marker is a useful negative probe for
    // extracting multiple episode values from the same release.
    static SINGLE:std::sync::OnceLock<regex::Regex>=std::sync::OnceLock::new();
    let single=SINGLE.get_or_init(||regex::Regex::new(
        r"(?i)(?:^|[._ -])s[0-9]{1,3}[._ -]*e([0-9]{1,5})(?:$|[._ -])"
    ).expect("bounded explicit episode evidence pattern"));
    if name.len()<=240
        &&!range.is_match(name)
        &&single.captures_iter(name).take(2).count()==1
        &&let Some(episodes)=parsed["episodes"].as_array()
        &&episodes.len()>1
    {
        issues.push(json!({
            "code":"field_overextracted_episodes","field":"episodes",
            "confidence":"review","signal":"single_episode_multiple_values",
            "severity":"review",
            "message":"An explicit single episode marker produced several episode values"
        }));
    }
    issues
}
/// A parsed field can also be wrong while nonempty. Only compare
/// unambiguous, independently readable raw markers, and never turn these
/// comparisons into corrected values.
fn explicit_marker_disagreements(raw:&str,parsed:&Value)->Vec<Value>{
    let t=tokens(raw);
    let mut out=Vec::new();
    let unique=|values:Vec<String>|{
        let mut values=values;
        values.sort();
        values.dedup();
        (values.len()==1).then(||values.into_iter().next()).flatten()
    };
    let resolutions=t.iter().filter(|word|supported_pixel_height(word))
        .cloned().collect::<Vec<_>>();
    if let Some(marker)=unique(resolutions)
        &&let Some(value)=parsed["resolution"].as_str()
        &&!value.is_empty()&&!value.eq_ignore_ascii_case(&marker)
    {
        out.push(json!({"code":"field_mismatch_resolution",
            "field":"resolution","confidence":"review",
            "signal":"explicit_resolution_disagrees","severity":"review",
            "message":"Parsed resolution differs from a single explicit release marker"}));
    }
    let codecs=t.iter().filter_map(|v|match v.as_str(){
        "x264"|"h264"|"avc"=>Some("avc".to_owned()),
        "x265"|"h265"|"hevc"=>Some("hevc".to_owned()),
        "av1"=>Some("av1".to_owned()),_=>None
    }).collect::<Vec<_>>();
    if let Some(marker)=unique(codecs)
        &&let Some(value)=parsed["codec"].as_str()
        &&!value.is_empty()&&!value.eq_ignore_ascii_case(&marker)
    {
        out.push(json!({"code":"field_mismatch_codec",
            "field":"codec","confidence":"review",
            "signal":"explicit_codec_disagrees","severity":"review",
            "message":"Parsed codec differs from a single explicit release marker"}));
    }
    let mut quality=t.iter().filter_map(|v|match v.as_str(){
        "webdl"=>Some("webdl".to_owned()),
        "webrip"=>Some("webrip".to_owned()),
        "bluray"=>Some("bluray".to_owned()),
        "hdtv"=>Some("hdtv".to_owned()),_=>None
    }).collect::<Vec<_>>();
    if pair(&t,"web","dl"){quality.push("webdl".to_owned());}
    if pair(&t,"blu","ray"){quality.push("bluray".to_owned());}
    if let Some(marker)=unique(quality)
        &&let Some(value)=parsed["quality"].as_str()
        &&!value.is_empty()
        &&value.chars().filter(char::is_ascii_alphanumeric)
            .collect::<String>().to_ascii_lowercase()!=marker
    {
        out.push(json!({"code":"field_mismatch_quality",
            "field":"quality","confidence":"review",
            "signal":"explicit_quality_disagrees","severity":"review",
            "message":"Parsed release quality differs from a single explicit marker"}));
    }
    out
}
/// Cross-field attribution is a fourth kind of parser mistake. The
/// same unambiguous SxxExx / NxM / Season N Episode M grammar used by the TV
/// learner identifies *possible* swapped coordinates independently of the
/// canonical parser output. It never decides a new value.
fn misassigned_coordinates(raw:&str,parsed:&Value)->Vec<Value>{
    let Some((season,episode))=crate::tv_learning::explicit_coordinates(raw)
        else{return Vec::new();};
    // Packs, ranges and ambiguous multiple markers were rejected by the
    // source grammar. A single present value in both fields is necessary to
    // attribute the error to a field rather than to missing extraction.
    let (Some(seasons),Some(episodes))=(
        parsed["seasons"].as_array(),parsed["episodes"].as_array())
        else{return Vec::new();};
    if seasons.len()!=1||episodes.len()!=1||season==episode{
        return Vec::new();
    }
    let (Some(got_season),Some(got_episode))=(
        seasons[0].as_i64(),episodes[0].as_i64())
        else{return Vec::new();};
    let swapped=got_season==episode && got_episode==season;
    let season_from_episode=got_season==episode && got_season!=season;
    let episode_from_season=got_episode==season && got_episode!=episode;
    if swapped{
        return vec![json!({
            "code":"field_misassigned_season_episode",
            "field":"seasons","affected_fields":["seasons","episodes"],
            "confidence":"review","signal":"explicit_season_episode_swapped",
            "severity":"review",
            "message":"Parser may have swapped season and episode fields"
        })];
    }
    let mut out=Vec::new();
    if season_from_episode {
        out.push(json!({
            "code":"field_misassigned_season",
            "field":"seasons","affected_fields":["seasons"],
            "confidence":"review","signal":"episode_marker_used_as_season",
            "severity":"review",
            "message":"The episode marker may have been assigned to seasons"
        }));
    }
    if episode_from_season {
        out.push(json!({
            "code":"field_misassigned_episode",
            "field":"episodes","affected_fields":["episodes"],
            "confidence":"review","signal":"season_marker_used_as_episode",
            "severity":"review",
            "message":"The season marker may have been assigned to episodes"
        }));
    }
    // Completely incorrect nonempty coordinates are also worth review.
    // The raw convention is independent evidence, not a writable label.
    if got_season!=season && !season_from_episode {
        out.push(json!({
            "code":"field_incorrect_season","field":"seasons",
            "confidence":"review","signal":"explicit_season_disagrees",
            "severity":"review",
            "message":"Parser season disagrees with an unambiguous season marker"
        }));
    }
    if got_episode!=episode && !episode_from_season {
        out.push(json!({
            "code":"field_incorrect_episode","field":"episodes",
            "confidence":"review","signal":"explicit_episode_disagrees",
            "severity":"review",
            "message":"Parser episode disagrees with an unambiguous episode marker"
        }));
    }
    out
}
/// Skip suspicious canonical outputs when training the advisory online model.
/// Review evidence is retained independently; a misparse must not become
/// a positive training label simply because the parser emitted it.
pub fn canonical_for_training(parsed:&Value,flags:&[Value])->Value{
    let mut copy=parsed.clone();
    let Some(map)=copy.as_object_mut()else{return copy;};
    for flag in flags {
        let code=flag["code"].as_str().unwrap_or("");
        if !(code.starts_with("field_overextracted_")
            ||code.starts_with("field_underextracted_")
            ||code.starts_with("field_mismatch_")
            ||code.starts_with("field_incorrect_")
            ||code.starts_with("field_misassigned_")){continue;}
        if code=="field_misassigned_season_episode" {
            map.remove("seasons");
            map.remove("episodes");
            continue;
        }
        if let Some(field)=flag["field"].as_str(){
            if field=="season"{map.remove("seasons");}
            else if field=="episode"{map.remove("episodes");}
            else{map.remove(field);}
        }
    }
    copy
}
pub fn audit(raw:&str,parsed:&Value,selected:&Value)->Vec<Value>{
    // Requests can contain full relative paths. Only inspect the basename;
    // never emit arbitrary user-provided tokens as the diagnostic signal.
    if raw.len()>4096{return Vec::new();}
    let name=leaf(raw);
    let t=tokens(name);
    if t.is_empty(){return Vec::new();}
    let mut out=Vec::new();

    // Title: no reliable token alone proves a *correct* title, but a wholly
    // missing parsed title in a plausible release warrants review.
    if !present(parsed,"title")
        && t.iter().any(|s|s.len()>=4&&s.bytes().any(|b|b.is_ascii_alphabetic())){
        missing(&mut out,parsed,"title","nonempty_release_name","review");
    }
    let resolution=has(&t,&["4320p","2160p","1080p","720p","576p","480p","4k","8k"])
        .or_else(||t.iter().find(|token|supported_pixel_height(token)).cloned());
    if let Some(ref v)=resolution{
        missing(&mut out,parsed,"resolution",v,"high");
    }
    if let Some(v)=has(&t,&["x264","h264","avc","x265","h265","hevc","av1","vp9"]){
        missing(&mut out,parsed,"codec",&v,"high");
    }
    let quality=has(&t,&["webrip","webdl","bluray","bdrip","brrip","hdtv",
        "dvdrip","hddvd","web-dl"]);
    let quality=quality.or_else(||pair(&t,"web","dl").then(||"web_dl".into()))
        .or_else(||pair(&t,"blu","ray").then(||"blu_ray".into()));
    if let Some(v)=quality.as_deref(){
        missing(&mut out,parsed,"quality",v,"high");
    }
    if let Some(v)=has(&t,&["aac","ac3","eac3","ddp","dts","flac","truehd","opus"]){
        missing(&mut out,parsed,"audio",&v,"high");
    }
    if let Some(v)=has(&t,&["10bit","12bit","8bit","10bits","12bits"]){
        missing(&mut out,parsed,"bit_depth",&v,"high");
    } else if pair(&t,"10","bit") || pair(&t,"12","bit"){
        missing(&mut out,parsed,"bit_depth","bit_depth_marker","high");
    }
    if let Some(v)=has(&t,&["hdr10","hdr10plus","dovi","dolbyvision"]){
        missing(&mut out,parsed,"hdr",&v,"high");
    } else if pair(&t,"dolby","vision"){
        missing(&mut out,parsed,"hdr","dolby_vision","high");
    }
    if let Some(v)=audio_channels(name){
        missing(&mut out,parsed,"channels",v,"review");
    }
    if let Some(v)=has(&t,&["norwegian","norsk","swedish","svenska",
        "danish","dansk","french","german","italian","japanese"]){
        // Named languages can also appear in a movie title: review only.
        missing(&mut out,parsed,"languages",&v,"review");
    }
    if let Some(v)=has(&t,&["subbed","hardsubbed"]){
        missing(&mut out,parsed,"subbed",&v,"review");
    }
    if let Some(v)=has(&t,&["dubbed"]){
        missing(&mut out,parsed,"dubbed",&v,"review");
    }
    if explicit_season(&t){
        missing(&mut out,parsed,"seasons","explicit_season_marker","high");
    }
    if explicit_episode(&t){
        missing(&mut out,parsed,"episodes","explicit_episode_marker","high");
    }
    if let Some(ext)=name.rsplit_once('.').map(|(_,ext)|ext.to_ascii_lowercase())
        && ["mkv","mp4","avi","m4v","webm","m2ts"].contains(&ext.as_str()){
        missing(&mut out,parsed,"extension",&ext,"high");
    }
    // False positives are common for standalone 4-digit numbers (e.g. 1917,
    // 1984 or anime titles). Require a technical release marker as context,
    // and never use this as an automatic correction.
    if !present(parsed,"year") && (present(parsed,"resolution")||
        present(parsed,"codec")||quality.is_some()){
        if let Some(year)=t.iter().find(|s|s.len()==4&&s.bytes().all(|b|b.is_ascii_digit())
            &&s.parse::<i32>().is_ok_and(|v|(1900..=2035).contains(&v))){
            missing(&mut out,parsed,"year",year,"review");
        }
    }
    // Selection is not part of parsing, but missing IMDb/AniDB coordinates
    // require a separate verified metadata check. Never infer ASID from a
    // number in the title alone.
    if selected.is_object()
        && selected["category"]=="series"
        && selected["episode"].is_null()
        && parsed["anime_episode_candidate"]["episode"].as_i64().is_some()
    {
        out.push(json!({"code":"field_missing_anime_episode",
            "field":"anime_episode","confidence":"review","signal":"absolute_episode_hint",
            "severity":"review",
            "message":"Parsed absolute anime episode candidate has no verified selected anime coordinate"}));
    }
    out.extend(extraction_review(name,parsed));
    out.extend(explicit_marker_disagreements(name,parsed));
    out.extend(misassigned_coordinates(name,parsed));
    out.extend(compare_parsed_to_selected(parsed,selected));
    out
}
#[cfg(test)]
mod tests{
    use super::*;
    fn codes(name:&str,parsed:Value,selected:Value)->Vec<String>{
        audit(name,&parsed,&selected).into_iter()
            .filter_map(|a|a["code"].as_str().map(str::to_owned)).collect()
    }
    #[test]
    fn safely_detects_new_pixel_height_tags_but_not_arbitrary_numbers(){
        for p in ["1440p","900p","360p","2880p"]{
            let name=format!("Film.Name.{p}.WEB-DL");
            let parsed=json!({"title":"Film Name"});
            assert!(codes(&name,parsed,Value::Null)
                .contains(&"field_missing_resolution".to_owned()),"{p}");
        }
        for p in ["9999p","1999p","1234p","23p","p1440"]{
            assert!(!supported_pixel_height(p),"{p}");
        }
    }
    #[test]
    fn reports_independent_field_evidence_only_when_missing(){
        let c=codes("Show.S02E07.1080p.WEB-DL.x265.10bit.AAC.5.1.mkv",
            json!({"title":"Show","seasons":[2],"episodes":[7]}),Value::Null);
        for field in ["resolution","quality","codec","bit_depth","audio",
            "channels","extension"] {
            assert!(c.contains(&format!("field_missing_{field}")),"{field}: {c:?}");
        }
        assert!(!c.contains(&"field_missing_seasons".into()));
        assert!(!c.contains(&"field_missing_episodes".into()));
    }
    #[test]
    fn normal_titles_without_markers_do_not_generate_missing_fields(){
        let c=codes("Bleach",json!({"title":"Bleach"}),Value::Null);
        assert!(c.is_empty(),"{c:?}");
        let c=codes("1984",json!({"title":"1984"}),Value::Null);
        assert!(!c.contains(&"field_missing_year".into()));
        let c=codes("German Connection",json!({"title":"German Connection",
            "languages":["de"]}),Value::Null);
        assert!(c.is_empty(),"{c:?}");
    }
    #[test]
    fn missing_matcher_coordinates_are_distinct_from_missing_parser_fields(){
        let flags=codes("Example.S02E07",json!({"title":"Example",
            "seasons":[2],"episodes":[7]}),json!({
                "category":"series","imdb_id":"tt1234567",
                "season":null,"episode":null}));
        assert!(flags.contains(&"field_unresolved_season".into()),"{flags:?}");
        assert!(flags.contains(&"field_unresolved_episode".into()),"{flags:?}");
        assert!(!flags.contains(&"field_missing_seasons".into()));
        assert!(!flags.contains(&"field_missing_episodes".into()));
    }
    #[test]
    fn year_mismatches_are_review_signals_not_certain_errors(){
        let flags=codes("Example.2024.WEB-DL",json!({
            "title":"Example","year":2024,"quality":"WEB-DL"
        }),json!({"category":"movie","year":2020,"imdb_id":"tt1234567"}));
        assert!(flags.contains(&"field_mismatch_year".into()),"{flags:?}");
        let flags=codes("Example.S02E07.2024",json!({
            "title":"Example","year":2024,"seasons":[2],"episodes":[7]
        }),json!({"category":"series","year":2020,"season":2,"episode":7}));
        assert!(!flags.contains(&"field_mismatch_year".into()),"{flags:?}");
    }
    #[test]
    fn verified_anime_episode_remaps_must_not_generate_false_disagreements(){
        let flags=codes("Bleach.300",json!({"title":"Bleach","episodes":[300]}),
            json!({"category":"series","season":14,"episode":1,
                "anime":{"anidb_id":"100","episode":300}}));
        assert!(!flags.contains(&"field_mismatch_episode".into()),"{flags:?}");
        let flags=codes("Show.S02E07",json!({"title":"Show","seasons":[2],
            "episodes":[7]}),json!({"category":"series","season":2,
                "episode":8}));
        assert!(flags.contains(&"field_mismatch_episode".into()),"{flags:?}");
        assert!(!flags.contains(&"field_unresolved_episode".into()));
    }
    #[test]
    fn learns_from_over_and_underextracted_title_spans_without_guessing(){
        let too_long=codes("Jojo.Stone.Ocean.S02E07.1080p",
            json!({"title":"Jojo Stone Ocean S02E07","seasons":[2],
                "episodes":[7]}),Value::Null);
        assert!(too_long.contains(&"field_overextracted_title".to_owned()));
        let too_short=codes("Jojo.Stone.Ocean.S02E07.1080p",
            json!({"title":"Jojo"}),Value::Null);
        assert!(too_short.contains(&"field_underextracted_title".to_owned()));
        let normal=codes("Jojo.Stone.Ocean.S02E07.1080p",
            json!({"title":"Jojo Stone Ocean","seasons":[2],
                "episodes":[7]}),Value::Null);
        assert!(!normal.iter().any(|v|v.contains("extracted_title")));
    }
    #[test]
    fn detects_incomplete_ranges_and_overfull_episode_arrays(){
        let incomplete=codes("Show.S02E07-E08.1080p",
            json!({"title":"Show","episodes":[7]}),Value::Null);
        assert!(incomplete.contains(
            &"field_underextracted_episodes".to_owned()));
        let complete=codes("Show.S02E07-E08.1080p",
            json!({"title":"Show","episodes":[7,8]}),Value::Null);
        assert!(!complete.contains(
            &"field_underextracted_episodes".to_owned()));
        let excessive=codes("Show.S02E07.1080p",
            json!({"title":"Show","episodes":[7,8]}),Value::Null);
        assert!(excessive.contains(
            &"field_overextracted_episodes".to_owned()));
    }
    #[test]
    fn nonempty_but_wrong_technical_fields_become_review_evidence(){
        let flags=codes("Movie.1080p.x265.WEB-DL",
            json!({"title":"Movie","resolution":"720p",
                "codec":"avc","quality":"WEBRip"}),Value::Null);
        for field in ["resolution","codec","quality"] {
            assert!(flags.contains(&format!("field_mismatch_{field}")));
        }
        let correct=codes("Movie.1080p.x265.WEB-DL",
            json!({"title":"Movie","resolution":"1080p",
                "codec":"hevc","quality":"WEB-DL"}),Value::Null);
        assert!(!correct.iter().any(|v|v.starts_with("field_mismatch_")));
        let ambiguous=codes("Movie.1080p.720p.x265.x264",
            json!({"title":"Movie","resolution":"480p",
                "codec":"mpeg"}),Value::Null);
        assert!(!ambiguous.iter().any(|v|v=="field_mismatch_resolution"
            ||v=="field_mismatch_codec"));
    }
    #[test]
    fn detects_swapped_season_episode_without_writing_parser_output(){
        for raw in ["Series.S02E07.1080p",
            "Series.02x07.1080p",
            "Series.Season.2.Episode.7.1080p"]{
            let parsed=json!({"title":"Series","seasons":[7],
                "episodes":[2]});
            let original=parsed.clone();
            let evidence=audit(raw,&parsed,&Value::Null);
            assert!(evidence.iter().any(|f|
                f["code"]=="field_misassigned_season_episode"),"{raw}");
            assert_eq!(parsed,original,"auditing never changes fields");
            let sanitized=canonical_for_training(&parsed,&evidence);
            assert!(sanitized.get("seasons").is_none());
            assert!(sanitized.get("episodes").is_none());
        }
    }
    #[test]
    fn detects_partial_cross_field_assignment_but_rejects_ambiguous_packs(){
        let bad_season=codes("Series.S02E07.1080p",
            json!({"seasons":[7],"episodes":[7]}),Value::Null);
        assert!(bad_season.contains(&"field_misassigned_season".into()));
        let bad_episode=codes("Series.S02E07.1080p",
            json!({"seasons":[2],"episodes":[2]}),Value::Null);
        assert!(bad_episode.contains(&"field_misassigned_episode".into()));
        let correct=codes("Series.S02E07.1080p",
            json!({"seasons":[2],"episodes":[7]}),Value::Null);
        assert!(!correct.iter().any(|s|s.starts_with("field_misassigned_")));
        for raw in ["Series.S02E07-E08.1080p",
            "Series.S02E07.S03E08.1080p",
            "Series.02x07-08.1080p"]{
            let issues=codes(raw,json!({"seasons":[7],"episodes":[2]}),Value::Null);
            assert!(!issues.iter().any(|s|s.starts_with("field_misassigned_")),
                "ambiguous ranges/packs cannot prove cross-field assignment");
        }
    }
    #[test]
    fn rejects_other_incorrect_explicit_coordinates_without_guessing(){
        let issues=codes("Series.S02E07.1080p",
            json!({"title":"Series","seasons":[9],"episodes":[99]}),
            Value::Null);
        assert!(issues.contains(&"field_incorrect_season".into()));
        assert!(issues.contains(&"field_incorrect_episode".into()));
        let release=json!({"title":"Series","seasons":[9],"episodes":[99]});
        let diagnostic=audit("Series.S02E07.1080p",&release,&Value::Null);
        let clean=canonical_for_training(&release,&diagnostic);
        assert!(clean.get("seasons").is_none());
        assert!(clean.get("episodes").is_none());
    }
    #[test]
    fn does_not_use_suspicious_field_values_as_training_truth(){
        let parsed=json!({"title":"Show S02E07","episodes":[7,8],
            "codec":"avc","year":2024});
        let issues=vec![json!({"code":"field_overextracted_title",
            "field":"title"}),json!({"code":"field_mismatch_episode",
            "field":"episode"})];
        let safe=canonical_for_training(&parsed,&issues);
        assert!(safe.get("title").is_none());
        assert!(safe.get("episodes").is_none());
        assert_eq!(safe["codec"],"avc");
        assert_eq!(parsed["episodes"],json!([7,8]));
    }
    #[test]
    fn season_only_and_absolute_anime_stay_separate(){
        let c=codes("Bleach.S16",json!({"title":"Bleach","seasons":[16]}),
            json!({"category":"series","season":16,"episode":1,
                "anime_range_diagnostics":{"complete":false}}));
        assert!(c.is_empty(),"{c:?}");
        let c=codes("Bleach_300",json!({"title":"Bleach",
            "anime_episode_candidate":{"title":"Bleach","episode":300}}),
            json!({"category":"series","episode":null}));
        assert!(c.contains(&"field_missing_anime_episode".into()));
    }
}
