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
