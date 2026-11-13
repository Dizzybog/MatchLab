//! Deterministic IMDb title matcher (independent of AniDB identity selection).
//! Collect all safe exact/prefix/suffix candidates before ranking them by
//! release-title evidence. Season/year/region/votes remain deterministic;
//! only indexed Tantivy alias queries discover candidate titles.
use rusqlite::{params, Connection};
use std::collections::{BTreeMap, HashMap, HashSet};
use serde::Serialize;

pub use crate::title_normalization::clean_title;

#[derive(Clone,Debug)]
struct Aka {
    imdb_id:String, title:String, clean:String, year:i32, category:String,
    region:Option<String>, ordering:i32, media_type:String,
}
impl From<crate::tantivy_search::TitleAlias> for Aka {
    fn from(a:crate::tantivy_search::TitleAlias)->Self{
        Self{imdb_id:a.imdb_id,title:a.title,clean:a.clean,year:a.year,
            category:a.category,region:a.region,ordering:a.ordering,
            media_type:a.media_type}
    }
}
fn index_error(error:String)->rusqlite::Error{
    rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(error)))
}
#[derive(Clone,Debug,Serialize)]
pub struct Match {
    pub imdb_id:String,pub title:String,pub year:i32,pub category:String,
    pub matched_title:String,pub anime_ids:BTreeMap<String,String>,pub media_type:String,
}
fn is_series(t:&str)->bool {t=="tvSeries" || t=="tvMiniSeries"}
fn allowed(t:&str,series:bool,year:Option<i32>)->bool {
    if series {is_series(t)} else {
        matches!(t,"movie"|"tvMovie"|"short"|"tvShort"|"tvSpecial") || (year.is_none() && is_series(t))
    }
}
fn eligible(db:&Connection,r:&Aka,season:i64)->rusqlite::Result<bool>{
    if !is_series(&r.media_type)||season<=0{return Ok(true);}
    let max:Option<i64>=db.query_row("SELECT max_season FROM seasons WHERE imdb_id=?1",
        [&r.imdb_id],|row|row.get(0)).optional()?;
    if max.unwrap_or(0)>=std::cmp::max(1,season-1){return Ok(true);}
    Ok(db.query_row("SELECT 1 FROM anime_series_map l JOIN episode_map m ON m.anidb_id=l.anidb_id
        WHERE l.imdb_id=?1 AND (m.tvdb_season=?2 OR l.trakt_season=?2) LIMIT 1",
        params![r.imdb_id,season],|row|row.get::<_,i32>(0)).optional()?.is_some())
}
use rusqlite::OptionalExtension;
fn filter_eligible(db:&Connection,rows:Vec<Aka>,series:bool,year:Option<i32>,season:i64)
    ->rusqlite::Result<Vec<Aka>>{
    let rows=rows.into_iter()
        .filter(|row|allowed(&row.media_type,series,year))
        .collect::<Vec<_>>();
    if season<=0 {return Ok(rows);}
    // IMDb title discovery often returns several aliases per work. Query
    // season eligibility in bounded ID batches rather than one SQL statement
    // for every candidate. Neither season bounds nor AniDB fallback criteria
    // are relaxed; order and duplicate aliases are preserved.
    let ids=rows.iter().filter(|row|is_series(&row.media_type))
        .map(|row|row.imdb_id.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter().collect::<Vec<_>>();
    if ids.is_empty(){return Ok(rows);}
    // Most ordinary exact matches involve one or two IMDb identities.
    // Avoid building dynamic SQL and collecting a full ID set for the
    // small-candidate path; retain the original memoized checks instead.
    if ids.len()<=4 {
        let mut memo=std::collections::HashMap::new();
        let mut result=Vec::with_capacity(rows.len());
        for row in rows {
            if !is_series(&row.media_type){result.push(row);continue;}
            let yes=if let Some(&yes)=memo.get(&row.imdb_id){yes}else{
                let yes=eligible(db,&row,season)?;
                memo.insert(row.imdb_id.clone(),yes);
                yes
            };
            if yes{result.push(row);}
        }
        return Ok(result);
    }
    let threshold=std::cmp::max(1,season-1);
    let mut eligible_ids=std::collections::HashSet::new();
    for batch in ids.chunks(256){
        let markers=vec!["?";batch.len()].join(",");
        let sql=format!("SELECT imdb_id,max_season FROM seasons
            WHERE imdb_id IN ({markers})");
        let mut stmt=db.prepare(&sql)?;
        for row in stmt.query_map(rusqlite::params_from_iter(batch.iter()),
            |r|Ok((r.get::<_,String>(0)?,r.get::<_,Option<i64>>(1)?)))? {
            let (id,max)=row?;
            if max.unwrap_or(0)>=threshold{eligible_ids.insert(id);}
        }
    }
    let remaining=ids.iter().filter(|id|!eligible_ids.contains(*id))
        .collect::<Vec<_>>();
    for batch in remaining.chunks(256){
        let markers=vec!["?";batch.len()].join(",");
        let sql=format!("SELECT DISTINCT l.imdb_id
            FROM anime_series_map l JOIN episode_map m ON m.anidb_id=l.anidb_id
            WHERE l.imdb_id IN ({markers})
              AND (m.tvdb_season=? OR l.trakt_season=?)");
        let mut args=batch.iter().map(|id|*id as &dyn rusqlite::ToSql)
            .collect::<Vec<_>>();
        args.push(&season);
        args.push(&season);
        let mut stmt=db.prepare(&sql)?;
        for id in stmt.query_map(rusqlite::params_from_iter(args),
            |r|r.get::<_,String>(0))? {
            eligible_ids.insert(id?);
        }
    }
    Ok(rows.into_iter().filter(|row|
        !is_series(&row.media_type)||eligible_ids.contains(&row.imdb_id)).collect())
}
fn filter(db:&Connection,rows:Vec<Aka>,release:&str,series:bool,year:Option<i32>,season:i64)
    ->rusqlite::Result<Vec<Aka>>{
    filter_eligible(db,rows.into_iter()
        .filter(|row|release.contains(&row.clean)).collect(),series,year,season)
}
fn tantivy_aliases(db:&Connection,key:&str,suffix:bool)->rusqlite::Result<Vec<Aka>>{
    crate::tantivy_search::title_aliases(db,key,suffix)
        .map(|rows|rows.into_iter().map(Aka::from).collect()).map_err(index_error)
}
fn exact(db:&Connection,key:&str)->rusqlite::Result<Vec<Aka>>{
    crate::tantivy_search::title_aliases_exact(db,key)
        .map(|rows|rows.into_iter().map(Aka::from).collect()).map_err(index_error)
}
/// Prefer exact or complete-word edge alignment over raw alias length.
#[derive(Clone,Copy,Debug,PartialEq,Eq,PartialOrd,Ord)]
struct TitleFit{
    tier:u8,               // full-name fit, ordinary partial, fallback
    longest_first:i64,    // among full-release fits, longest matched words win
    unmatched_bp:u32,     // relative unmatched share (basis points)
    edge:u8,              // prefix before suffix only when coverage is equal
}
impl TitleFit{
    fn exact()->Self{Self{tier:0,unmatched_bp:0,edge:0,longest_first:0}}
    fn fallback(length:usize)->Self{
        Self{tier:2,unmatched_bp:0,edge:0,longest_first:-(length as i64)}
    }
}
/// Preserve non-Latin title words for ranking. Legacy index keys fold to ASCII:
/// "BLEACH 千年血戦篇" and "Bleach" both have the same old key.
fn semantic_words(raw:&str)->Vec<String>{
    use unicode_normalization::{UnicodeNormalization,char::is_combining_mark};
    let folded=raw.to_lowercase()
        .replace('æ',"ae").replace('ø',"o").replace('œ',"oe")
        .replace('ß',"ss").replace("'", "");
    let folded=folded.nfkd().filter(|c|!is_combining_mark(*c))
        .collect::<String>();
    folded.split(|c:char|!c.is_alphanumeric())
        .filter(|word|!word.is_empty() && *word!="and")
        .map(str::to_owned).collect()
}
/// Check whole-word containment in *both* directions; score the proportion
/// of the longer title not covered by the shorter title.
fn semantic_extension(short:&[String],long:&[String])->Option<TitleFit>{
    if short.is_empty()||short.len()>=long.len(){return None;}
    let prefix=long.starts_with(short);
    let suffix=long.ends_with(short);
    if !prefix&&!suffix{return None;}
    let matched=short.iter().map(|s|s.chars().count()).sum::<usize>();
    let total=long.iter().map(|s|s.chars().count()).sum::<usize>();
    if total==0{return None;}
    Some(TitleFit{tier:1,
        unmatched_bp:((total-matched)*10_000/total) as u32,
        edge:if prefix{0}else{1},longest_first:-(matched as i64)})
}
fn title_fit(key:&str,words:&[String],release_words:&[String],row:&Aka,
    semantic_release:&[String],cached_alias:Option<&[String]>)->Option<TitleFit>{
    if row.clean==key{
        // An identical ASCII index key is not necessarily an identical
        // Unicode title. Prefer complete identity to partial prefix/suffix.
        let computed;
        let full=match cached_alias{
            Some(words)=>words,
            None=>{computed=semantic_words(&row.title);&computed}
        };
        if let Some(partial)=semantic_extension(semantic_release,full)
            .or_else(||semantic_extension(full,semantic_release)){
            return Some(partial);
        }
        return Some(TitleFit::exact());
    }
    let short=key.len().min(row.clean.len());
    let long=key.len().max(row.clean.len());
    if short<4||short.saturating_mul(2)<long||long-short>16{return None;}
    let unmatched_bp=((long-short) as u64*10_000/long as u64) as u32;
    // The parser can treat an immediately trailing number as technical
    // metadata even when it is part of an IMDb title: Televisionseries1.
    // Reattach ONLY a complete ASCII numeric suffix already present in the
    // source release (checked by filter). Never accept a partial word.
    if row.clean.strip_prefix(key).is_some_and(|tail|
        !tail.is_empty() && tail.bytes().all(|b|b.is_ascii_digit())){
        return Some(TitleFit{tier:1,unmatched_bp,edge:0,
            longest_first:-(short as i64)});
    }
    let alias_words=crate::title_normalization::clean_words(&row.title);
    if words.is_empty()||alias_words.is_empty(){return None;}
    // The parser sometimes consumes part of a *real* title as release
    // metadata: Televisionseries1 -> Television. Restore a longer alias
    // only when its FULL normalized words occur in the original release.
    // A character-only prefix (Dune vs Duneland) is never sufficient.
    if row.clean.starts_with(key)
        && release_words.windows(alias_words.len())
            .any(|window|window==alias_words.as_slice()){
        return Some(TitleFit{tier:1,unmatched_bp,edge:0,
            longest_first:-(short as i64)});
    }
    let prefix=words.starts_with(&alias_words)||alias_words.starts_with(words);
    let suffix=words.ends_with(&alias_words)||alias_words.ends_with(words);
    if !prefix&&!suffix{return None;}
    Some(TitleFit{tier:1,unmatched_bp,edge:if prefix{0}else{1},
        longest_first:-(short as i64)})
}

/// Parser-independent title boundary shared by prefix and suffix
/// recovery. Technical metadata, an explicit production year or TV
/// coordinates delimit the meaningful words in the original release.
fn release_metadata_boundary(word:&str)->bool{
    let season=word.strip_prefix('s').is_some_and(|suffix|{
        let (s,e)=suffix.split_once('e').unwrap_or((suffix,""));
        !s.is_empty()&&s.len()<=3&&s.bytes().all(|b|b.is_ascii_digit())
            &&(e.is_empty()||e.bytes().all(|b|b.is_ascii_digit()))
    });
    let episode=word.strip_prefix("ep").or_else(||word.strip_prefix('e'))
        .is_some_and(|digits|!digits.is_empty()&&digits.len()<=5
            &&digits.bytes().all(|b|b.is_ascii_digit()));
    let quality=matches!(word,"web"|"webdl"|"webrip"|"bluray"|"brrip"
        |"dvdrip"|"hdtv"|"x264"|"x265"|"h264"|"h265"|"hevc"
        |"av1"|"aac"|"ac3"|"dts"|"mkv"|"mp4"|"avi"|"proper"
        |"repack"|"hdr"|"remux"|"dubbed"|"subbed"|"multi"
        |"720p"|"1080p"|"2160p"|"480p"|"4k");
    let production_year=word.len()==4
        && word.bytes().all(|b|b.is_ascii_digit())
        && word.parse::<i32>().is_ok_and(|n|(1900..=2099).contains(&n));
    season||episode||quality||production_year
}
fn release_content_words(release_title:&str)->Vec<String>{
    crate::title_normalization::clean_words(release_title).into_iter()
        .take(32).take_while(|word|!release_metadata_boundary(word))
        .collect()
}
fn semantic_release_content_words(release_title:&str)->Vec<String>{
    semantic_words(release_title).into_iter()
        .take(32).take_while(|word|!release_metadata_boundary(word))
        .collect()
}
/// Recover longer complete/partial IMDb titles beginning at the source
/// title's first word, even when the parser stopped early.
fn longer_release_title_prefixes(parsed_title:&str,release_title:&str)->Vec<String>{
    let parsed=crate::title_normalization::clean_words(parsed_title);
    let release=release_content_words(release_title);
    if parsed.is_empty()||!release.starts_with(&parsed){return Vec::new();}
    let mut joined=parsed.concat();
    let mut keys=Vec::new();
    for word in release.iter().skip(parsed.len()){
        joined.push_str(word);
        keys.push(joined.clone());
    }
    keys
}
/// Candidate IMDb titles whose START is at a later complete word boundary
/// and whose matching span reaches the END of the source's content title.
/// Requiring two source words avoids treating an isolated generic tag/word
/// as sufficient suffix evidence. At most 14 trailing words are searched.
fn longer_release_title_suffix_keys(release_title:&str)->Vec<String>{
    // Preserve non-Latin word boundaries while generating compact ASCII
    // Tantivy keys, so "Scene BLEACH 千年血戦篇" can discover "BLEACH"
    // without treating the Japanese subtitle as an empty token.
    let content=semantic_release_content_words(release_title);
    if content.len()<3{return Vec::new();}
    let mut keys=Vec::new();
    let start=content.len().saturating_sub(14).max(1);
    for idx in start..content.len()-1{
        // The validator requires EVERY remaining content word to belong to
        // this candidate. Use that entire suffix as the indexed prefix term:
        // far fewer false discoveries than a generic first-two-word query.
        let key=clean_title(&content[idx..].join(" "));
        if key.len()>=4 && !keys.contains(&key){keys.push(key);}
    }
    keys
}

/// Compare the longest candidate IMDb title against the ORIGINAL release
/// title words, up to the parser-independent metadata boundary. A complete
/// title prefix is exact; a source prefix of a longer IMDb title is accepted
/// only if it covers >=50% and leaves <=16 unmatched characters.
/// A different word before the metadata boundary invalidates the candidate.
fn title_in_original_release_fit(
    release_words:&[String],parsed_words:&[String],max_title_words:usize,row:&Aka,
)->Option<TitleFit>{
    let title_words=semantic_words(&row.title);
    if title_words.len()<=parsed_words.len()||max_title_words<=parsed_words.len(){
        return None;
    }
    let content=&release_words[..max_title_words.min(release_words.len())];
    let full=content.starts_with(&title_words);
    let count=if full{title_words.len()}else if title_words.starts_with(content){
        content.len()
    }else{return None};
    let matched=title_words[..count].iter().map(|s|s.chars().count()).sum::<usize>();
    let total=title_words.iter().map(|s|s.chars().count()).sum::<usize>();
    if total==0{return None;}
    let unmatched=total-matched;
    if !full && (matched<4||matched.saturating_mul(2)<total||unmatched>16){
        return None;
    }
    Some(TitleFit{tier:0,longest_first:-(matched as i64),
        unmatched_bp:((unmatched as u64*10_000)/total as u64) as u32,
        edge:if full{0}else{1}})
}

/// Rank an existing eligible prefix candidate on the *same* raw-release
/// length scale as a recovered suffix, even if the parser retained more
/// non-title words. This is only a re-ranking of already discovered valid
/// rows, so it does not broaden the candidate set to unsafe generic words.
fn title_prefix_in_original_release_fit(
    content:&[String],row:&Aka,
)->Option<TitleFit>{
    let title=semantic_words(&row.title);
    if content.len()<2||title.len()<2{return None;}
    let full=content.starts_with(&title);
    let count=if full{title.len()}else if title.starts_with(content){
        content.len()
    }else{return None};
    let matched=title[..count].iter().map(|w|w.chars().count()).sum::<usize>();
    let total=title.iter().map(|w|w.chars().count()).sum::<usize>();
    if total==0{return None;}
    let unmatched=total-matched;
    if !full && (matched<4||matched.saturating_mul(2)<total||unmatched>16){
        return None;
    }
    Some(TitleFit{tier:0,longest_first:-(matched as i64),
        unmatched_bp:((unmatched as u64*10_000)/total as u64) as u32,
        edge:0})
}

/// Symmetric raw-release suffix evidence: a complete IMDb title must equal
/// the entire final title span, or the entire span must be a whole-word
/// prefix of the IMDb title (bounded partial match). Stray words between
/// the title and the metadata boundary invalidate the candidate.
fn title_suffix_in_original_release_fit(
    content:&[String],row:&Aka,
)->Option<TitleFit>{
    let title=semantic_words(&row.title);
    if content.len()<3||title.len()<2{return None;}
    let mut best=None;
    let start=content.len().saturating_sub(14).max(1);
    for idx in start..content.len()-1{
        let suffix=&content[idx..];
        if !title.starts_with(suffix){continue;}
        let matched=suffix.iter().map(|w|w.chars().count()).sum::<usize>();
        let total=title.iter().map(|w|w.chars().count()).sum::<usize>();
        if total==0{continue;}
        let unmatched=total-matched;
        let full=unmatched==0;
        if !full && (matched<4||matched.saturating_mul(2)<total||unmatched>16){
            continue;
        }
        let fit=TitleFit{tier:0,longest_first:-(matched as i64),
            unmatched_bp:((unmatched as u64*10_000)/total as u64) as u32,
            edge:1};
        if best.is_none_or(|old|fit<old){best=Some(fit);}
    }
    best
}

/// The *source* can itself be a suffix of a LONGER IMDb title:
/// "The Ed Gein Story" is the trailing four words of
/// "Monster: The Ed Gein Story". This is a different direction from
/// matching an IMDb title at the end of a longer torrent title.
/// Require ALL significant release words to be the candidate's final
/// complete words, with the same coverage guard as partial-prefix recovery.
fn release_is_imdb_title_suffix_fit(
    content:&[String],row:&Aka,
)->Option<TitleFit>{
    if content.len()<2{return None;}
    let candidate=semantic_words(&row.title);
    if candidate.len()<=content.len()||!candidate.ends_with(content){
        return None;
    }
    let matched=content.iter().map(|s|s.chars().count()).sum::<usize>();
    let total=candidate.iter().map(|s|s.chars().count()).sum::<usize>();
    if total==0{return None;}
    let unmatched=total-matched;
    if matched<4||matched.saturating_mul(2)<total||unmatched>16{
        return None;
    }
    Some(TitleFit{tier:0,longest_first:-(matched as i64),
        unmatched_bp:((unmatched as u64*10_000)/total as u64) as u32,
        edge:1})
}

/// Stronger original-release evidence suppresses an unrelated shorter anime
/// identity even if that anime has a mapped episode. This covers complete
/// full-title matches AND validated partial prefix matches.
pub fn has_longer_title_evidence_in_release(
    parsed_title:&str,raw_release:&str,matched_title:&str,
)->bool{
    let raw_release=release_component_for_title(raw_release,parsed_title);
    let keys=longer_release_title_prefixes(parsed_title,raw_release);
    let release=semantic_words(raw_release);
    let parsed=semantic_words(parsed_title);
    let content=semantic_release_content_words(raw_release);
    let candidate=Aka{
        imdb_id:String::new(),title:matched_title.into(),clean:String::new(),
        year:0,category:String::new(),region:None,ordering:0,
        media_type:String::new(),
    };
    (!keys.is_empty() && title_in_original_release_fit(
        &release,&parsed,content.len(),&candidate).is_some())
        ||title_suffix_in_original_release_fit(&content,&candidate).is_some()
        ||release_is_imdb_title_suffix_fit(&content,&candidate).is_some()
}

/// On a torrent hierarchy, title relevance belongs to ONE meaningful
/// path component, never the concatenation of folder names, episode
/// coordinates, and the file extension. The parser already extracts fields
/// per level and chooses the nearest meaningful title; use that title to
/// identify the matching original component for bidirectional IMDb search.
/// Prefer the closest aligned component. Keep a conservative descriptive
/// fallback only when the parser normalized a title differently.
fn release_component_for_title<'a>(raw:&'a str,title:&str)->&'a str{
    if !raw.bytes().any(|c|c==b'/'||c==b'\\'){return raw;}
    let parsed=semantic_words(title);
    let mut fallback=None;
    for component in raw.rsplit(&['/','\\'][..]).filter(|s|!s.is_empty()){
        let content=semantic_release_content_words(component);
        if content.is_empty(){continue;}
        if !parsed.is_empty()
            && (content.starts_with(&parsed)||content.ends_with(&parsed)){
            return component;
        }
        // Do not choose numeric intermediate folders or leaf-only episode /
        // technical components as the source of a recovered IMDb title.
        let meaningful=content.len()>=2
            && !matches!(content[0].as_str(),"season"|"seasons"
                |"episode"|"episodes"|"specials"|"extras"|"subtitles"
                |"library"|"archive"|"movies"|"tv");
        if meaningful&&fallback.is_none(){fallback=Some(component);}
    }
    fallback.unwrap_or(raw)
}

/// Title relevance is ranked by the caller. After that, use release-year
/// distance, region preference, IMDb votes, and the earliest production year.
/// When no release year was supplied, its distance is neutral for all works.
/// IMDb ID is only a final deterministic tie-breaker.
// The same identity can have many AKA rows. During *intra-identity*
// alias selection, IMDb ID never distinguishes aliases and cloning it for
// every comparison is needless CPU/allocation. Keep the owned-ID rank only
// for the final cross-identity deterministic tie-break.
fn rank_without_id(r:&Aka,year:Option<i32>,duplicate:bool,
    country:Option<&str>,votes:i64)->(i64,i32,i64,i32){
    let distance=year.map_or(0,|y|(i64::from(r.year)-i64::from(y)).abs());
    // Explicit country overrides the default US region only for duplicate
    // aliases. IMDb names its British region GB; torrent markers use UK.
    let preferred=country.map(|code|if code=="UK"{"GB"}else{code});
    let region=if !duplicate
        || r.region.as_deref()==Some(preferred.unwrap_or("US")){0}else{1};
    let earliest_year=if r.year>0{r.year}else{i32::MAX};
    (distance,region,-votes,earliest_year)
}
fn rank(r:&Aka,year:Option<i32>,duplicate:bool,country:Option<&str>,votes:i64)
    ->(i64,i32,i64,i32,String){
    let (distance,region,negative_votes,earliest_year)=
        rank_without_id(r,year,duplicate,country,votes);
    (distance,region,negative_votes,earliest_year,r.imdb_id.clone())
}
// For a tied IMDb shortlist, the scalar ranker used one ratings query,
// one anime_ids query and often one anime_series_map query per identity.
// All shortlisted IDs already have a verified lexical fit; read only these
// exact relational rows in bounded IN batches, never the title catalogue.
// Keep small shortlists on their existing prepared-statement-friendly path.
fn shortlist_metadata(
    db:&Connection,ids:&[String],
)->rusqlite::Result<HashMap<String,(i64,BTreeMap<String,String>)>>{
    let mut metadata=ids.iter().map(|id|(id.clone(),(0,BTreeMap::new())))
        .collect::<HashMap<_,_>>();
    for batch in ids.chunks(200){
        let markers=vec!["?";batch.len()].join(",");
        let params=||rusqlite::params_from_iter(batch.iter());
        // The historical votes lookup treated a missing/malformed rating
        // as zero, even if ratings import was unavailable. Preserve that
        // nonfatal behavior; anime ID queries still propagate SQL errors.
        let sql=format!("SELECT imdb_id,num_votes FROM imdb_ratings
            WHERE imdb_id IN ({markers})");
        if let Ok(mut stmt)=db.prepare(&sql) {
            if let Ok(rows)=stmt.query_map(params(),|row|
                Ok((row.get::<_,String>(0)?,row.get::<_,i64>(1)?))){
                for row in rows.flatten(){
                    if let Some(entry)=metadata.get_mut(&row.0){entry.0=row.1;}
                }
            }
        }
        let sql=format!("SELECT group_id,type,value FROM anime_ids
            WHERE group_id IN ({markers}) ORDER BY group_id,type,value");
        let mut stmt=db.prepare(&sql)?;
        for row in stmt.query_map(params(),|row|Ok((
            row.get::<_,String>(0)?,row.get::<_,String>(1)?,
            row.get::<_,String>(2)?)))?{
            let (id,kind,value)=row?;
            if let Some(entry)=metadata.get_mut(&id){
                entry.1.insert(kind,value);
            }
        }
        // A verified explicit AniDB ID makes the fallback query unnecessary.
        // This also preserves legacy snapshots which have anime_ids but no
        // anime_series_map table unless fallback discovery is actually needed.
        let missing=batch.iter().filter(|id|
            metadata.get(*id).is_some_and(|(_,ids)|!ids.contains_key("anidb")))
            .collect::<Vec<_>>();
        if missing.is_empty(){continue;}
        let markers=vec!["?";missing.len()].join(",");
        let sql=format!("SELECT DISTINCT imdb_id,anidb_id FROM anime_series_map
            WHERE imdb_id IN ({markers})");
        let mut stmt=db.prepare(&sql)?;
        let mut links=HashMap::<String,std::collections::BTreeSet<String>>::new();
        for row in stmt.query_map(rusqlite::params_from_iter(missing.iter().copied()),|row|
            Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?)))?{
            let (id,anidb)=row?;
            links.entry(id).or_default().insert(anidb);
        }
        for (id,linked) in links {
            if linked.len()==1 {
                if let Some(entry)=metadata.get_mut(&id){
                    entry.1.insert("anidb".into(),linked.into_iter().next().unwrap());
                }
            }
        }
    }
    Ok(metadata)
}

pub fn search(db:&Connection,title:&str,year:Option<i32>,series:bool,required_season:i64,
    release_title:&str,has_episode:bool,limit:usize)->rusqlite::Result<Vec<Match>>{
    search_inner(db,title,year,series,required_season,release_title,has_episode,limit,false)
        .map(|(hits,_)|hits)
}

/// Preserve the public API's final eligibility gate when an exact-only
/// shortlist is used. If all exact results fail a caller-specific gate (e.g.
/// year evidence), retry the complete indexed discovery rather than
/// suppressing a valid shorter, word-aligned candidate.
pub fn search_validated(
    db:&Connection,title:&str,year:Option<i32>,series:bool,required_season:i64,
    release_title:&str,has_episode:bool,limit:usize,
    validate:impl Fn(Vec<Match>)->Vec<Match>,
)->rusqlite::Result<Vec<Match>>{
    let (hits,exact_only)=search_inner(
        db,title,year,series,required_season,release_title,has_episode,limit,false)?;
    let filtered=validate(hits);
    if exact_only && filtered.is_empty(){
        let (fallback,_) = search_inner(
            db,title,year,series,required_season,release_title,has_episode,limit,true)?;
        return Ok(validate(fallback));
    }
    Ok(filtered)
}

fn search_inner(db:&Connection,title:&str,year:Option<i32>,series:bool,required_season:i64,
    release_title:&str,_has_episode:bool,limit:usize,force_full:bool,
)->rusqlite::Result<(Vec<Match>,bool)>{
    // The parser has already inherited season/episode/year across the
    // hierarchy. Only the lexical candidate discovery should operate on
    // the original component containing its chosen title, not on a joined
    // parent/child path that is not a real IMDb title.
    let release_title=release_component_for_title(release_title,title);
    let key=clean_title(title);
    let release=clean_title(release_title);
    if key.is_empty()||limit==0{return Ok((vec![],false));}
    // For a complete, verified source title of at least six normalized
    // characters, exact aliases form a safe first phase. A truncated parser
    // title or a country-qualified alias must still use all lexical paths.
    // Otherwise the ordinary exhaustive discovery and global ranking stay
    // authoritative, including valid partial and reversed-title matches.
    let words=crate::title_normalization::clean_words(title);
    let release_words=crate::title_normalization::clean_words(release_title);
    let mut rows=Vec::new();
    let mut ordinary_title_matches=HashSet::new();
    let mut country_title_matches=HashSet::new();
    let country_qualified=crate::country_aliases::qualified(release_title);
    let country=country_qualified.as_ref().map(|(code,_)|*code);
    let semantic_release=semantic_words(title);
    let semantic_full_release=semantic_words(release_title);
    let semantic_content=semantic_release_content_words(release_title);

    // Recognized, explicitly country-qualified TV aliases remain eligible,
    // but do not preempt more specific names on another search path.
    if series{
        if let Some((_,aliases))=&country_qualified{
            for name in aliases {
                for row in exact(db,name)?{
                    if is_series(&row.media_type){
                        country_title_matches.insert((row.imdb_id.clone(),
                            row.title.clone()));
                        rows.push(row);
                    }
                }
            }
        }
    }
    let mut discovered=exact(db,&key)?;
    // Presence in the index is NOT enough: the source's complete significant
    // words must equal the parsed title, and at least one exact alias must
    // have the same Unicode-preserving words (ASCII keys may collide).
    // This protects longer names such as Monster / The Ed Gein Story and
    // BLEACH with a Japanese subtitle from premature early exits.
    let exact_only=!force_full && key.len()>=6 && country_qualified.is_none()
        && !semantic_content.is_empty() && semantic_release==semantic_content
        && discovered.iter().any(|row|
            row.clean==key && semantic_words(&row.title)==semantic_content);
    if !exact_only && key.len()>=4{
        discovered.extend(tantivy_aliases(db,&key,false)?);
        if key.len()>=8{
            discovered.extend(tantivy_aliases(db,&key,true)?);
        }
        discovered.extend(crate::tantivy_search::title_aliases_inside(db,&key)
            .map_err(index_error)?.into_iter().map(Aka::from));
    }
    // Lexical discovery from a parser title is less trustworthy than the
    // original release, so keep the historical word-boundary and containment
    // checks. Only the raw-release validation below can admit longer names.
    discovered.retain(|row|release.contains(&row.clean)
        && title_fit(&key,&words,&release_words,row,&semantic_release,None).is_some());
    for row in discovered{
        ordinary_title_matches.insert((row.imdb_id.clone(),row.title.clone()));
        rows.push(row);
    }
    // The parser can truncate a real title on a metadata-like word.
    // Recover full IMDb titles and legitimate longer partial prefixes from
    // the ORIGINAL release, keeping only whole-word matches before technical
    // metadata, without SQL title scans or unbounded substring searches.
    let extended_keys=if exact_only{Vec::new()}else{
        longer_release_title_prefixes(title,release_title)
    };
    let mut extended_title_matches=HashSet::new();
    if !extended_keys.is_empty(){
        let max_title_words=semantic_content.len();
        let found=crate::tantivy_search::title_aliases_at_release_prefixes(
            db,&extended_keys).map_err(index_error)?;
        let matching=found.into_iter().map(Aka::from)
            .filter(|row|title_in_original_release_fit(
                &semantic_full_release,&semantic_release,max_title_words,row).is_some())
            .collect::<Vec<_>>();
        // The original release already proves lexical relevance; defer
        // all media/season SQL eligibility to the single shared final pass.
        for candidate in matching {
            extended_title_matches.insert((candidate.imdb_id.clone(),
                candidate.title.clone()));
            rows.push(candidate);
        }
    }
    // The same recovery strategy also applies to suffixes of the source's
    // content title, not only to the parser's already-truncated title.
    // Never search in technical metadata, require complete word edges, and
    // reject stray source-title words that are not covered by the candidate.
    // Always collect suffix evidence, even if an exact/prefix candidate
    // already exists. Every candidate must be compared in one global rank.
    let suffix_keys=if exact_only{Vec::new()}else{
        longer_release_title_suffix_keys(release_title)
    };
    let mut suffix_title_matches=HashSet::new();
    if !suffix_keys.is_empty(){
        let found=crate::tantivy_search::title_aliases_at_release_suffixes(
            db,&suffix_keys).map_err(index_error)?;
        let matching=found.into_iter().map(Aka::from)
            .filter(|row|title_suffix_in_original_release_fit(
                &semantic_content,row).is_some()).collect::<Vec<_>>();
        for candidate in matching {
            suffix_title_matches.insert((candidate.imdb_id.clone(),
                candidate.title.clone()));
            rows.push(candidate);
        }
    }
    // The opposite suffix direction must also be considered: the original
    // torrent content may omit the START of the IMDb title, e.g.
    // "The.Ed.Gein.Story.S01E01" vs "Monster: The Ed Gein Story".
    // Tantivy's existing reversed-title index finds suffix completions in
    // one targeted lookup. Ordinary filter(release.contains(alias)) cannot
    // be used here: by definition the IMDb name is longer than the source.
    let mut imdb_title_suffix_matches=HashSet::new();
    if !exact_only && semantic_content.len()>=2 {
        let suffix_key=clean_title(&semantic_content.join(" "));
        if suffix_key.len()>=6{
            let found=tantivy_aliases(db,&suffix_key,true)?;
            let matching=found.into_iter()
                .filter(|row|release_is_imdb_title_suffix_fit(
                    &semantic_content,row).is_some()).collect::<Vec<_>>();
            for candidate in matching {
                imdb_title_suffix_matches.insert((candidate.imdb_id.clone(),
                    candidate.title.clone()));
                rows.push(candidate);
            }
        }
    }
    // Apply media-type and season eligibility once, after lexical discovery
    // has exhausted EVERY path. A more specific suffix can no longer be
    // starved by a shorter prefix/alias.
    let eligible_rows=filter_eligible(db,rows,series,year,required_season)?;
    // An exact title can exist but be ineligible for the requested media
    // type/season. Avoid letting another alias with the same ASCII key
    // masquerade as a verified complete title; resume full discovery.
    if exact_only && !eligible_rows.iter().any(|row|
        row.clean==key && semantic_words(&row.title)==semantic_content){
        return search_inner(db,title,year,series,required_season,
            release_title,_has_episode,limit,true);
    }
    let mut rows=eligible_rows;
    // The AniDB-only title lookup remains a separate *fallback* when none
    // of the IMDb title search paths found any eligible identity.
    if rows.is_empty()&&series&&key.len()>=4{
        // IMDb's English primary title need not appear in the release at
        // all: e.g. Shinigami Bocchan to Kuro Maid is indexed under its
        // English title. The exact/longest AniDB alias provides identity;
        // requiring release.contains(imdb_alias) silently rejects it.
        // Search via the existing Tantivy anime index, not SQLite title scans.
        let anime_aliases=crate::tantivy_search::anime_aliases_for_title(db,title,4)
            .map_err(index_error)?;
        let best_len=anime_aliases.first().map(|alias|alias.clean_title.len())
            .unwrap_or(0);
        let ids=anime_aliases.into_iter()
            .filter(|alias|alias.clean_title.len()==best_len
                && key.starts_with(&alias.clean_title))
            .map(|alias|alias.imdb_id)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter().collect::<Vec<_>>();
        if !ids.is_empty(){
            let aliases=crate::tantivy_search::titles_by_ids(db,&ids).map_err(index_error)?;
            let mut eligibility=std::collections::HashMap::new();
            for row in aliases.into_iter().map(Aka::from) {
                if !is_series(&row.media_type){continue;}
                let yes=if let Some(&cached)=eligibility.get(&row.imdb_id){cached}
                    else {
                        let value=eligible(db,&row,required_season)?;
                        eligibility.insert(row.imdb_id.clone(),value);
                        value
                    };
                if yes{rows.push(row);}
            }
        }
        if rows.is_empty(){
            // Keep the original exact indexed anime-ID discovery path as
            // a conservative fallback, for legacy datasets and alias
            // shapes that the prefix lookup cannot represent.
            let ids=crate::tantivy_search::anime_candidates(db,&key)
                .map_err(index_error)?;
            if !ids.is_empty(){
                let aliases=crate::tantivy_search::titles_by_ids(db,&ids)
                    .map_err(index_error)?;
                rows=filter(db,aliases.into_iter().map(Aka::from).collect(),
                    &release,series,year,required_season)?;
            }
        }
    }
    if rows.is_empty(){return Ok((vec![],false));}
    // Keep all plausible identities until ranked. Unrelated word fragments
    // are rejected before season eligibility and expensive ID lookups.
    // Score every (IMDb title alias, source) independently and use its
    // strongest verifiable alignment. The discovery order is irrelevant.
    // Repeat appearances via exact/forward/reverse retrieval do not change
    // the winner, and aliases of the same IMDb work are compared as one work.
    let mut groups:BTreeMap<String,Vec<(Aka,TitleFit)>>=BTreeMap::new();
    let semantic_content_length=semantic_content.iter()
        .map(|word|word.chars().count()).sum::<usize>();
    for row in rows{
        // Reuse the Unicode tokenization for the candidate's title-fit
        // evidence and exact-source comparison in this scoring pass.
        let alias_semantic=semantic_words(&row.title);
        let alias_key=(row.imdb_id.clone(),row.title.clone());
        let mut evidence=Vec::new();
        if ordinary_title_matches.contains(&alias_key){
            if let Some(fit)=title_fit(&key,&words,&release_words,
                &row,&semantic_release,Some(&alias_semantic)){evidence.push(fit);}
        }
        if country_title_matches.contains(&alias_key){
            // AU and "Australia" are equivalent *complete* release title
            // qualifiers when an exact imported IMDb alias confirms the
            // expanded form. A full qualified title must outrank a shorter
            // unqualified prefix ("Lego Masters"), regardless of votes.
            //
            // TitleFit::exact() has longest_first=0, which inadvertently
            // ranks BELOW a shorter prefix with a negative matched length.
            // Score the length of the original qualified title instead,
            // keeping ordinary lexical fit/season gates and the later
            // regional vote tie-breakers unchanged.
            evidence.push(TitleFit{tier:0,longest_first:-(semantic_content_length as i64),
                unmatched_bp:0,edge:0});
        }
        if extended_title_matches.contains(&alias_key){
            if let Some(fit)=title_in_original_release_fit(
                &semantic_full_release,&semantic_release,
                semantic_content.len(),&row){evidence.push(fit);}
        }
        if suffix_title_matches.contains(&alias_key){
            if let Some(fit)=title_suffix_in_original_release_fit(
                &semantic_content,&row){evidence.push(fit);}
        }
        if imdb_title_suffix_matches.contains(&alias_key){
            if let Some(fit)=release_is_imdb_title_suffix_fit(
                &semantic_content,&row){evidence.push(fit);}
        }
        if let Some(fit)=title_prefix_in_original_release_fit(
            &semantic_content,&row){evidence.push(fit);}
        // Score the full source title consistently, including exact-match
        // rows that are also returned by a different discovery direction.
        if alias_semantic==semantic_content{
            evidence.push(TitleFit{tier:0,longest_first:-(semantic_content_length as i64),
                unmatched_bp:0,edge:0});
        }
        let fit=evidence.into_iter().min().unwrap_or(TitleFit::fallback(
            row.clean.len()));
        groups.entry(row.imdb_id.clone()).or_default().push((row,fit));
    }
    // A shorter AKA can otherwise tie a complete title, e.g. "Bleach" for
    // a later "BLEACH 千年血戦篇" work. Compare full title.basics names
    // through Tantivy only when distinct IDs share an exact alias. All
    // comparisons are Unicode-preserving and work in both directions.
    let exact_ids=groups.iter().filter_map(|(id,aliases)|
        aliases.iter().any(|(_,fit)|fit.tier==0).then_some(id.clone()))
        .collect::<Vec<_>>();
    if exact_ids.len()>1 {
        let mut full_titles=BTreeMap::<String,Vec<Vec<String>>>::new();
        for id in &exact_ids {
            let forms=groups.get(id).unwrap().iter()
                .filter(|(a,_)|a.ordering==0
                    && a.region.as_deref().unwrap_or("").is_empty())
                .map(|(a,_)|semantic_words(&a.title)).collect::<Vec<_>>();
            if !forms.is_empty(){full_titles.insert(id.clone(),forms);}
        }
        let missing=exact_ids.iter().filter(|id|!full_titles.contains_key(*id))
            .cloned().collect::<Vec<_>>();
        if !missing.is_empty(){
            let fetched=crate::tantivy_search::titles_by_ids(db,&missing)
                .map_err(index_error)?;
            for item in fetched {
                if item.ordering==0
                    && item.region.as_deref().unwrap_or("").is_empty(){
                    full_titles.entry(item.imdb_id).or_default()
                        .push(semantic_words(&item.title));
                }
            }
        }
        for id in exact_ids {
            let Some(forms)=full_titles.get(&id)else{continue};
            if forms.iter().any(|full|full==&semantic_release){continue;}
            // The parser can strip an AU/US country suffix and report only
            // "Lego Masters" even though the original release explicitly
            // reads "Lego Masters AU". Its canonical IMDb title may be the
            // fully equivalent "Lego Masters Australia". Do not demote that
            // verified country-qualified exact match as if it were a
            // shortened sequel title. Only exempt canonical forms that are
            // themselves *exact* expanded source-country aliases.
            let qualified_canonical=country_qualified.as_ref().is_some_and(|(_,keys)|
                groups.get(&id).is_some_and(|aliases|aliases.iter().any(|(a,_)|
                    country_title_matches.contains(&(a.imdb_id.clone(),a.title.clone()))))
                && forms.iter().any(|words|keys.contains(&clean_title(&words.join(" ")))));
            if qualified_canonical {continue;}
            // Penalize a demonstrably shortened name, but NOT unrelated
            // translated aliases (e.g. exact English vs Japanese main title).
            let extension=forms.iter()
                .filter_map(|full|semantic_extension(&semantic_release,full))
                .min();
            if let Some(extension)=extension {
                if let Some(aliases)=groups.get_mut(&id){
                    for (_,fit) in aliases {
                        if fit.tier==0{*fit=extension;}
                    }
                }
            }
        }
    }
    // Longest whole-word title evidence in the original torrent outranks
    // shorter matching names, even if the longer IMDb title only partially
    // fits the source before the episode/technical marker. Year distance,
    // region, votes and earliest candidate year apply only to equal title fit.
    // Candidates below the lexical rank at `limit` cannot be returned.
    // Retain the entire tied boundary for the remaining criteria.
    // This avoids most per-IMDb rating, anime-ID and duplicate-title lookups.
    let mut ranked_groups=groups.into_iter().map(|(id,aliases)|{
        let best_fit=aliases.iter().map(|(_,fit)|*fit).min().unwrap();
        (best_fit,id,aliases)
    }).collect::<Vec<_>>();
    ranked_groups.sort_by_key(|(fit,_,_)|*fit);
    if ranked_groups.len()>limit {
        let cutoff=ranked_groups[limit-1].0;
        ranked_groups.retain(|(fit,_,_)|*fit<=cutoff);
    }
    let alias_keys=ranked_groups.iter()
        .flat_map(|(_,_,aliases)|aliases.iter().map(|(row,_)|row.clean.clone()))
        .collect::<std::collections::BTreeSet<_>>();
    let duplicates=crate::tantivy_search::duplicate_titles(db,&alias_keys)
        .map_err(index_error)?;
    // With no release year, every eligible work has zero distance.
    // Region always precedes votes, including season/episode releases.
    // Title fit and verified episode eligibility still outrank both.
    let mut metadata=if ranked_groups.len()>4 {
        let ids=ranked_groups.iter().map(|(_,id,_)|id.clone()).collect::<Vec<_>>();
        Some(shortlist_metadata(db,&ids)?)
    }else{None};
    let mut output=Vec::new();
    for (_,id,aliases) in ranked_groups{
        let prefetched=metadata.as_mut().and_then(|rows|rows.remove(&id));
        let votes:i64=if let Some((votes,_))=prefetched.as_ref() {*votes}else{
            db.query_row("SELECT num_votes FROM imdb_ratings WHERE imdb_id=?1",[&id],|r|r.get(0))
                .optional().unwrap_or(None).unwrap_or(0)
        };
        let (best,fit)=aliases.into_iter().min_by_key(|(r,fit)|
            (*fit,rank_without_id(r,year,duplicates.contains(&r.clean),country,votes))
        ).unwrap();
        let ids=if let Some((_,ids))=prefetched{ids}else{
            let mut ids=BTreeMap::new();
            let mut stmt=db.prepare("SELECT type,value FROM anime_ids WHERE group_id=?1")?;
            for item in stmt.query_map([&id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?{
                let (k,v)=item?;ids.insert(k,v);
            }
            if !ids.contains_key("anidb"){
                let mut stmt=db.prepare("SELECT DISTINCT anidb_id FROM anime_series_map WHERE imdb_id=?1")?;
                let linked=stmt.query_map([&id],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
                if linked.len()==1{ids.insert("anidb".into(),linked[0].clone());}
            }
            ids
        };
        let score=(fit,rank(&best,year,duplicates.contains(&best.clean),country,votes));
        output.push((score,Match{imdb_id:id,title:best.title.clone(),year:best.year,
            category:best.category,matched_title:best.title,anime_ids:ids,media_type:best.media_type}));
    }
    output.sort_by(|a,b|a.0.cmp(&b.0));
    Ok((output.into_iter().take(limit).map(|(_,m)|m).collect(),exact_only))
}
/// A matching English IMDb alias can belong to an older OVA with too few
/// episodes, while the modern TV series is linked only by AniDB's English
/// alias (e.g. JoJo 1993 versus JoJo 2012 S02E37).
///
/// Keep normal title ranking untouched when an already-ranked candidate has
/// independently confirmed episode coverage. Only on an episode-evidence
/// miss consult Tantivy's *exact* anime aliases and promote a UNIQUE
/// verified series identity. No SQLite title scan, no guessed offsets, and
/// no additional index query on ordinary successful matches.
pub fn prefer_verified_episode_identity(
    db:&Connection,mut candidates:Vec<Match>,title:&str,season:i64,episode:i64,
)->rusqlite::Result<Vec<Match>>{
    if !(1..=9_999).contains(&season)||!(1..=99_999).contains(&episode){
        return Ok(candidates);
    }
    let proven=|id:&str|->rusqlite::Result<bool>{
        let max:Option<i64>=db.query_row(
            "SELECT max_episode FROM season_episodes WHERE imdb_id=?1 AND season=?2",
            params![id,season],|r|r.get(0)).optional()?;
        if max.is_some_and(|max|max>=episode){return Ok(true);}
        Ok(crate::episode_crosswalk::resolve_season(
            db,id,season,episode)?.is_some())
    };
    // A canonical IMDb episode is stronger than an unrelated exact title.
    // Preserve relative ranking among equally supported candidates.
    let mut support=Vec::with_capacity(candidates.len());
    for candidate in &candidates{support.push(proven(&candidate.imdb_id)?);}
    if support.iter().any(|supported|*supported){
        let mut confirmed=Vec::new();
        let mut unconfirmed=Vec::new();
        for (candidate,supported) in candidates.into_iter().zip(support) {
            if supported {confirmed.push(candidate)}else{unconfirmed.push(candidate)}
        }
        confirmed.extend(unconfirmed);
        return Ok(confirmed);
    }
    let key=clean_title(title);
    if key.len()<4{return Ok(candidates);}
    let exact_anime_ids=crate::tantivy_search::anime_aliases_for_title(db,title,4)
        .map_err(index_error)?.into_iter()
        .filter(|alias|alias.clean_title==key)
        .map(|alias|alias.imdb_id)
        .collect::<std::collections::BTreeSet<_>>();
    // Never choose between multiple independently plausible anime works
    // with identical aliases solely because of a database iteration order.
    let mut verified=Vec::new();
    for id in exact_anime_ids {
        if candidates.iter().any(|c|c.imdb_id==id) || !proven(&id)?{continue;}
        verified.push(id);
    }
    if verified.len()!=1{return Ok(candidates);}
    let id=verified.pop().unwrap();
    let aliases=crate::tantivy_search::titles_by_ids(db,&[id.clone()])
        .map_err(index_error)?;
    let Some(best)=aliases.into_iter()
        .filter(|alias|is_series(&alias.media_type))
        .min_by_key(|alias|(
            if alias.original{0}else{1},
            alias.ordering,
            alias.title.len()
        )) else{return Ok(candidates)};
    let mut anime_ids=BTreeMap::new();
    let mut stmt=db.prepare("SELECT type,value FROM anime_ids WHERE group_id=?1")?;
    for row in stmt.query_map([&id],|r|
        Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))? {
        let (kind,value)=row?;
        anime_ids.insert(kind,value);
    }
    if !anime_ids.contains_key("anidb"){
        let mut stmt=db.prepare(
            "SELECT DISTINCT anidb_id FROM anime_series_map WHERE imdb_id=?1")?;
        let linked=stmt.query_map([&id],|r|r.get::<_,String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if linked.len()==1{anime_ids.insert("anidb".into(),linked[0].clone());}
    }
    candidates.insert(0,Match{
        imdb_id:id,title:best.title.clone(),year:best.year,
        category:best.category,matched_title:best.title,
        anime_ids,media_type:best.media_type
    });
    Ok(candidates)
}
#[cfg(test)]
mod tests{

    /// Exacts must be fast without changing duplicate-title ranking, including
    /// two IMDb works that share the same complete title and have different years.
    #[test]
    fn exact_first_matches_exhaustive_ranking_for_complete_titles(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,
            year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE imdb_ratings(imdb_id TEXT,num_votes INTEGER);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT);
            INSERT INTO titles VALUES
                ('ttmatrix','The Matrix','thematrix',1999,'movie','US',1,0,'movie'),
                ('ttnew','The Matrix','thematrix',2010,'movie','US',1,0,'movie'),
                ('ttsequel','The Matrix Reloaded','thematrixreloaded',2003,'movie','US',1,0,'movie');
            INSERT INTO imdb_ratings VALUES('ttmatrix',10),('ttnew',100000),
                ('ttsequel',999999);").unwrap();
        let args=("The Matrix",Some(1999),false,0,"The.Matrix.1999.1080p.BluRay",false,20);
        let (fast,exact_only)=search_inner(&db,args.0,args.1,args.2,args.3,
            args.4,args.5,args.6,false).unwrap();
        assert!(exact_only,"complete six-character-plus IMDb alias uses the fast path");
        let (full,used_fast)=search_inner(&db,args.0,args.1,args.2,args.3,
            args.4,args.5,args.6,true).unwrap();
        assert!(!used_fast);
        assert_eq!(fast.iter().map(|item|&item.imdb_id).collect::<Vec<_>>(),
            full.iter().map(|item|&item.imdb_id).collect::<Vec<_>>());
        assert_eq!(fast[0].imdb_id,"ttmatrix");
    }

    #[test]
    fn five_character_title_keeps_all_lexical_search_paths(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,
            year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE imdb_ratings(imdb_id TEXT,num_votes INTEGER);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT);
            INSERT INTO titles VALUES('ttshort','DuneX','dunex',2020,'movie','US',1,0,'movie');")
            .unwrap();
        let (hits,exact_only)=search_inner(&db,"DuneX",Some(2020),false,0,
            "DuneX.2020.WEB-DL",false,20,false).unwrap();
        assert!(!exact_only);
        assert_eq!(hits[0].imdb_id,"ttshort");
    }

    #[test]
    fn rejected_exact_only_candidate_retries_full_search_after_api_gate(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,
            year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE imdb_ratings(imdb_id TEXT,num_votes INTEGER);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT);
            INSERT INTO titles VALUES
                ('ttold','Star Force','starforce',2010,'movie','US',1,0,'movie'),
                ('ttnew','Force','force',2025,'movie','US',1,0,'movie');")
            .unwrap();
        let (quick,exact_only)=search_inner(&db,"Star Force",Some(2025),false,0,
            "Star.Force.2025",false,20,false).unwrap();
        assert!(exact_only);
        assert_eq!(quick.len(),1);
        assert_eq!(quick[0].imdb_id,"ttold");
        // Mirrors the production /match year gate: rejected exact results
        // must never prevent a shorter, word-boundary-safe newer candidate.
        let valid=search_validated(&db,"Star Force",Some(2025),false,0,
            "Star.Force.2025",false,20,|hits|{
                hits.into_iter().filter(|hit|hit.year==2025).collect()
            }).unwrap();
        let full=search_inner(&db,"Star Force",Some(2025),false,0,
            "Star.Force.2025",false,20,true).unwrap().0;
        assert_eq!(valid.iter().map(|hit|&hit.imdb_id).collect::<Vec<_>>(),
            full.iter().filter(|hit|hit.year==2025)
                .map(|hit|&hit.imdb_id).collect::<Vec<_>>());
        assert_eq!(valid[0].imdb_id,"ttnew");
    }

    #[test]
    fn longer_original_title_disables_exact_hit_for_shorter_parser_title(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,
            year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE seasons(imdb_id TEXT,max_season INTEGER);
            CREATE TABLE imdb_ratings(imdb_id TEXT,num_votes INTEGER);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT);
            INSERT INTO titles VALUES
                ('ttshort','The Office','theoffice',2005,'series','US',1,0,'tvSeries'),
                ('ttlong','The Office US','theofficeus',2005,'series','US',1,0,'tvSeries');
            INSERT INTO seasons VALUES('ttshort',9),('ttlong',9);
            INSERT INTO imdb_ratings VALUES('ttshort',50000),('ttlong',1);")
            .unwrap();
        let (hits,exact_only)=search_inner(&db,"The Office",None,true,1,
            "The.Office.US.S01E01",true,20,false).unwrap();
        assert!(!exact_only,"the source contains an additional title word");
        assert_eq!(hits[0].imdb_id,"ttlong",
            "longer full-source identity must outrank a shorter exact parser hit");
    }

    #[test]
    fn incomplete_or_unicode_qualified_exact_title_never_shortcuts(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,
            year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE seasons(imdb_id TEXT,max_season INTEGER);
            CREATE TABLE imdb_ratings(imdb_id TEXT,num_votes INTEGER);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT);
            INSERT INTO titles VALUES
                ('ttshort','The Ed','theed',2025,'series','US',1,0,'tvSeries'),
                ('ttgein','Monster: The Ed Gein Story','monstertheedgeinstory',
                    2025,'series','US',0,0,'tvSeries'),
                ('ttbleach','Bleach','bleach',2004,'series','US',1,0,'tvSeries'),
                ('ttjapan','BLEACH 千年血戦篇','bleach',2022,'series','JP',1,0,'tvSeries');
            INSERT INTO seasons VALUES('ttshort',1),('ttgein',2),
                ('ttbleach',17),('ttjapan',4);").unwrap();
        let (short,shortcut)=search_inner(&db,"The Ed",None,true,1,
            "The.Ed.Gein.Story.S01E01",true,20,false).unwrap();
        assert!(!shortcut);
        assert_eq!(short[0].imdb_id,"ttgein",
            "longer IMDb suffix must beat an exact but truncated parser title");
        let (anime,shortcut)=search_inner(&db,"Bleach",None,true,1,
            "Bleach.千年血戦篇.S01E01",true,20,false).unwrap();
        assert!(!shortcut,"a truncated Unicode title cannot use exact-only");
        let full=search_inner(&db,"Bleach",None,true,1,
            "Bleach.千年血戦篇.S01E01",true,20,true).unwrap().0;
        assert_eq!(anime.iter().map(|hit|&hit.imdb_id).collect::<Vec<_>>(),
            full.iter().map(|hit|&hit.imdb_id).collect::<Vec<_>>());
        let (complete,exact_only)=search_inner(&db,"Bleach 千年血戦篇",None,
            true,1,"Bleach.千年血戦篇.S01E01",true,20,false).unwrap();
        assert!(exact_only);
        assert_eq!(complete[0].imdb_id,"ttjapan",
            "full Unicode identity is preferred over a colliding ASCII alias");
    }


    use super::*;
    #[test] fn votes_replace_alias_count_as_tiebreaker(){
        let db=Connection::open_in_memory().unwrap();
        db.create_scalar_function("reverse",1,
            rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC
                |rusqlite::functions::FunctionFlags::SQLITE_UTF8,
            |ctx|{let s:String=ctx.get(0)?;Ok(s.chars().rev().collect::<String>())}).unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,
            year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE imdb_ratings(imdb_id TEXT PRIMARY KEY,num_votes INTEGER);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT);
            INSERT INTO titles VALUES('ttlow','Example','example',2020,'movie','',1,0,'movie');
            INSERT INTO titles VALUES('tthigh','Example','example',2020,'movie','',1,0,'movie');
            INSERT INTO titles VALUES('ttlow','Example alternate','examplealternate',2020,'movie','',0,1,'movie');
            INSERT INTO imdb_ratings VALUES('ttlow',100);
            INSERT INTO imdb_ratings VALUES('tthigh',50000);").unwrap();
        let found=search(&db,"Example",Some(2020),false,0,"Example.2020",false,1).unwrap();
        assert_eq!(found.len(),1);
        assert_eq!(found[0].imdb_id,"tthigh",
            "lexical cutoff must retain all tied identities for vote ranking");
    }
    #[test]
    fn votes_precede_year_when_release_year_is_missing_even_with_episode_coordinates(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,
            year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE imdb_ratings(imdb_id TEXT PRIMARY KEY,num_votes INTEGER);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT,trakt_season INTEGER);
            CREATE TABLE episode_map(anidb_id TEXT,tvdb_season INTEGER);
            CREATE TABLE seasons(imdb_id TEXT,max_season INTEGER);
            INSERT INTO titles VALUES
                ('ttold','Example','example',1993,'series','US',1,0,'tvSeries'),
                ('ttnew','Example','example',2012,'series','US',1,0,'tvSeries');
            INSERT INTO seasons VALUES('ttold',5),('ttnew',5);
            INSERT INTO imdb_ratings VALUES('ttold',100),('ttnew',90000);")
            .unwrap();
        // A missing year must choose popularity regardless of whether the
        // parsed torrent has a season, an episode, both, or neither.
        for (season,has_episode,release) in [
            (0,false,"Example"),
            (0,true,"Example.E37"),
            (2,false,"Example.S02"),
            (2,true,"Example.S02E37"),
        ] {
            let found=search(&db,"Example",None,true,season,release,has_episode,1).unwrap();
            assert_eq!(found.len(),1,"release: {release}");
            assert_eq!(found[0].imdb_id,"ttnew","release: {release}");
        }
        // An explicit year is still authoritative ahead of popularity.
        let with_year=search(&db,"Example",Some(1993),true,2,
            "Example.1993.S02E37",true,1).unwrap();
        assert_eq!(with_year[0].imdb_id,"ttold");
        // Eligibility continues to outrank popularity even with no year.
        db.execute("UPDATE seasons SET max_season=1 WHERE imdb_id='ttnew'",[])
            .unwrap();
        let eligible=search(&db,"Example",None,true,4,
            "Example.S04E37",true,1).unwrap();
        assert_eq!(eligible[0].imdb_id,"ttold");
    }
    #[test]
    fn ranking_orders_year_region_votes_then_earliest_year(){
        let candidate=|id:&str,year:i32,region:&str|{
            Aka{imdb_id:id.into(),title:"Example".into(),clean:"example".into(),
                year,category:"movie".into(),region:Some(region.into()),
                ordering:0,media_type:"movie".into()}
        };
        let us_old=candidate("ttus",2018,"US");
        let no_exact=candidate("ttno",2020,"NO");
        let no_old=candidate("ttno",2018,"NO");
        // An explicit release year always beats popularity and region.
        assert!(rank(&no_exact,Some(2020),true,None,1)
            <rank(&us_old,Some(2020),true,None,1_000_000));
        // Region wins ahead of popularity even without a country marker.
        assert!(rank(&us_old,Some(2018),true,None,1)
            <rank(&no_old,Some(2018),true,None,1_000_000));
        assert!(rank(&us_old,None,true,None,1)
            <rank(&no_old,None,true,None,1_000_000));
        // Explicit country qualifier: prefer the requested region first.
        assert!(rank(&us_old,Some(2018),true,Some("US"),1)
            <rank(&no_old,Some(2018),true,Some("US"),1_000_000));
        assert!(rank(&no_old,None,true,Some("NO"),1)
            <rank(&us_old,None,true,Some("NO"),1_000_000));
        // British releases use IMDb's GB regional title code.
        let british=candidate("ttgb",2018,"GB");
        assert!(rank(&british,None,true,Some("UK"),1)
            <rank(&us_old,None,true,Some("UK"),1_000_000));
        // If region is not applicable, votes alone lead.
        assert!(rank(&no_old,Some(2018),false,None,2)
            <rank(&us_old,Some(2018),false,None,1));
        // A complete vote/region/year-distance tie favors the earliest year.
        assert!(rank(&us_old,None,true,None,100)
            <rank(&candidate("ttnew",2020,"US"),None,true,None,100));
        // The same applies when two years are equally close to the given year.
        assert!(rank(&us_old,Some(2019),true,None,100)
            <rank(&candidate("ttnew",2020,"US"),Some(2019),true,None,100));
        // An unknown metadata year is not preferred as the earliest known.
        assert!(rank(&us_old,None,true,None,100)
            <rank(&candidate("ttmissing",0,"US"),None,true,None,100));
        // IMDb ID makes exact ties deterministic, but is not a semantic score.
        assert!(rank(&candidate("ttaaa",2020,"US"),None,true,None,100)
            <rank(&candidate("ttbbb",2020,"US"),None,true,None,100));
    }

    #[test]
    fn region_precedes_votes_with_or_without_explicit_country(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,
            year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE imdb_ratings(imdb_id TEXT PRIMARY KEY,num_votes INTEGER);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT);
            INSERT INTO titles VALUES
                ('ttus','Example','example',2020,'movie','US',0,99,'movie'),
                ('ttno','Example','example',2020,'movie','NO',1,0,'tvMovie'),
                ('ttgb','Example','example',2020,'movie','GB',1,0,'movie');
            INSERT INTO imdb_ratings VALUES('ttus',10),('ttno',100000),('ttgb',1000);")
            .unwrap();
        for specified_year in [None,Some(2020)] {
            // Without a country qualifier, the existing US-first
            // preference beats vote count for otherwise equal title/year.
            let found=search(&db,"Example",specified_year,false,0,
                "Example.1080p",false,3).unwrap();
            assert_eq!(found.iter().map(|m|m.imdb_id.as_str())
                .collect::<Vec<_>>(),vec!["ttus","ttno","ttgb"]);
            // Country explicitly supplied: requested region wins before votes.
            let us=search(&db,"Example",specified_year,false,0,
                "Example.US.1080p",false,3).unwrap();
            assert_eq!(us[0].imdb_id,"ttus");
            let gb=search(&db,"Example",specified_year,false,0,
                "Example.UK.1080p",false,3).unwrap();
            assert_eq!(gb[0].imdb_id,"ttgb");
            let no=search(&db,"Example",specified_year,false,0,
                "Example.Norge.1080p",false,3).unwrap();
            assert_eq!(no[0].imdb_id,"ttno");
        }
    }

    #[test]
    fn earliest_year_breaks_complete_vote_region_and_year_distance_ties(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,
            year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE imdb_ratings(imdb_id TEXT PRIMARY KEY,num_votes INTEGER);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT);
            INSERT INTO titles VALUES
                ('ttold','Example','example',2018,'movie','US',1,4,'movie'),
                ('ttnew','Example','example',2020,'movie','US',0,0,'movie');
            INSERT INTO imdb_ratings VALUES('ttold',100),('ttnew',100);")
            .unwrap();
        let old=search(&db,"Example",None,false,0,"Example.1080p",false,2)
            .unwrap();
        assert_eq!(old[0].imdb_id,"ttold");
        // The release year 2019 is equidistant from 2018 and 2020.
        let equidistant=search(&db,"Example",Some(2019),false,0,
            "Example.2019",false,2).unwrap();
        assert_eq!(equidistant[0].imdb_id,"ttold");
        // More votes still outrank the earliest candidate year.
        db.execute("UPDATE imdb_ratings SET num_votes=101 WHERE imdb_id='ttnew'",[])
            .unwrap();
        let popular=search(&db,"Example",None,false,0,
            "Example.1080p",false,2).unwrap();
        assert_eq!(popular[0].imdb_id,"ttnew");
    }

    #[test]
    fn votes_break_equal_movie_type_and_title_alias_ties(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,
            year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE imdb_ratings(imdb_id TEXT PRIMARY KEY,num_votes INTEGER);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT);
            INSERT INTO titles VALUES
                ('ttmovie','Same','same',2020,'movie','US',1,0,'movie'),
                ('tttv','Same','same',2020,'movie','US',0,99,'tvMovie');
            INSERT INTO imdb_ratings VALUES('ttmovie',10),('tttv',10000);")
            .unwrap();
        let found=search(&db,"Same",Some(2020),false,0,
            "Same.2020",false,1).unwrap();
        assert_eq!(found[0].imdb_id,"tttv",
            "media type, original flag and alias order are not ranking factors");
    }
    #[test]
    fn bleach_s01_uses_canonical_title_not_shortened_sequel_aka(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,
            year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE imdb_ratings(imdb_id TEXT PRIMARY KEY,num_votes INTEGER);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT,
                trakt_season INTEGER);
            CREATE TABLE episode_map(anidb_id TEXT,tvdb_season INTEGER);
            CREATE TABLE seasons(imdb_id TEXT,max_season INTEGER);
            INSERT INTO titles VALUES
                ('tt0434665','Bleach','bleach',2004,'series','',0,0,'tvSeries'),
                ('tt14986406','BLEACH 千年血戦篇',
                    'bleach',2022,'series','',0,0,'tvSeries'),
                ('tt14986406','Bleach: Thousand-Year Blood War',
                    'bleachthousandyearbloodwar',2022,'series','US',0,17,'tvSeries'),
                ('tt14986406','Bleach','bleach',2022,'series','US',0,42,'tvSeries');
            INSERT INTO imdb_ratings VALUES('tt0434665',90000),('tt14986406',120000);
            INSERT INTO seasons VALUES('tt0434665',16),('tt14986406',4);")
            .unwrap();
        // IMDb's sequel has more votes, but 'Bleach' is only an abbreviated
        // AKA of the sequel and is the main title of the 2004 series.
        for release in ["bleach.s1","Bleach.S01","Bleach.S01E01"] {
            let hits=search(&db,"Bleach",None,true,1,release,
                release.contains('E'),1).unwrap();
            assert_eq!(hits[0].imdb_id,"tt0434665","release: {release}");
        }
        // A full sequel title still resolves to the sequel.
        let sequel=search(&db,"Bleach Thousand Year Blood War",None,true,1,
            "Bleach.Thousand.Year.Blood.War.S01",false,1).unwrap();
        assert_eq!(sequel[0].imdb_id,"tt14986406");
        // Title fit ALWAYS precedes year: a full canonical title wins over
        // the sequel's abbreviated AKA even with 2022 explicitly supplied.
        let year=search(&db,"Bleach",Some(2022),true,1,
            "Bleach.2022.S01",false,1).unwrap();
        assert_eq!(year[0].imdb_id,"tt0434665");
        // The full Japanese title contains additional significant Unicode
        // words and must NOT normalize into an exact plain "bleach" title.
        let original_japanese=search(&db,"BLEACH 千年血戦篇",None,true,1,
            "BLEACH 千年血戦篇.S01",false,1).unwrap();
        assert_eq!(original_japanese[0].imdb_id,"tt14986406");
        let japanese_with_year=search(&db,"BLEACH 千年血戦篇",Some(2022),
            true,1,"BLEACH 千年血戦篇.2022.S01",false,1).unwrap();
        assert_eq!(japanese_with_year[0].imdb_id,"tt14986406");
        assert_eq!(semantic_words("BLEACH 千年血戦篇"),
            vec!["bleach","千年血戦篇"]);
        assert!(semantic_extension(&semantic_words("Bleach"),
            &semantic_words("BLEACH 千年血戦篇")).is_some());
        // The older series still survives the 1-season import tolerance;
        // the newer series must not be selected for the older show's S17.
        let season=search(&db,"Bleach",None,true,17,
            "Bleach.S17E01",true,1).unwrap();
        assert_eq!(season[0].imdb_id,"tt0434665");
    }

    #[test]
    fn equal_primary_titles_still_choose_imdb_votes(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,
            year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE imdb_ratings(imdb_id TEXT PRIMARY KEY,num_votes INTEGER);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT);
            INSERT INTO titles VALUES
                ('tt1984','Dune','dune',1984,'movie','',0,0,'movie'),
                ('tt2021','Dune','dune',2021,'movie','',0,0,'movie');
            INSERT INTO imdb_ratings VALUES('tt1984',2000),('tt2021',200000);")
            .unwrap();
        let hits=search(&db,"Dune",None,false,0,
            "Dune.1080p",false,1).unwrap();
        assert_eq!(hits[0].imdb_id,"tt2021",
            "popularity must win when both candidates share the full title");
        let with_year=search(&db,"Dune",Some(1984),false,0,
            "Dune.1984.1080p",false,1).unwrap();
        assert_eq!(with_year[0].imdb_id,"tt1984",
            "year distance is used only AFTER equal full-title relevance");
    }
    #[test] fn normalization(){assert_eq!(clean_title("Pokémon and Ægir"),"pokemonaegir");}
    #[test] fn short_titles_require_exact_match_instead_of_broad_tantivy_prefix(){
        let db=Connection::open_in_memory().unwrap();
        db.create_scalar_function("reverse",1,
            rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC
                |rusqlite::functions::FunctionFlags::SQLITE_UTF8,
            |ctx|{let s:String=ctx.get(0)?;Ok(s.chars().rev().collect::<String>())}).unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,
            year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE imdb_ratings(imdb_id TEXT PRIMARY KEY,num_votes INTEGER);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT);
            CREATE TABLE seasons(imdb_id TEXT,max_season INTEGER);
            INSERT INTO titles VALUES('tt0000001','Up','up',2009,'movie','',1,0,'movie');
            INSERT INTO titles VALUES('tt0000002','Upgrade','upgrade',2018,'movie','',1,0,'movie');
            INSERT INTO titles VALUES('tt0000003','The 100','the100',2014,'series','',1,0,'tvSeries');
            INSERT INTO seasons VALUES('tt0000003',7);").unwrap();
        let up=search(&db,"Up",Some(2009),false,0,"Up.2009",false,5).unwrap();
        assert_eq!(up.len(),1);
        assert_eq!(up[0].imdb_id,"tt0000001");
        let partial=search(&db,"Th",None,true,1,"The100.s01",false,5).unwrap();
        assert!(partial.is_empty(),"broken short title must not trigger broad prefix matching");
        let full=search(&db,"The100",None,true,1,"The100.s01",false,5).unwrap();
        assert_eq!(full[0].imdb_id,"tt0000003");
    }
    #[test]
    fn lego_masters_au_country_alias_beats_unqualified_us_title(){
        // Real IMDb identity names: the Australian 2019 series uses the
        // long-form "Lego Masters Australia", while releases write "AU".
        // The US 2020 series is simply called "Lego Masters" and may have
        // more IMDb votes. An explicit country-qualified full alias must
        // not lose to the shorter unqualified prefix.
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,
                clean_title TEXT,year INTEGER,category TEXT,region TEXT,
                original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE seasons(imdb_id TEXT,max_season INTEGER);
            CREATE TABLE imdb_ratings(imdb_id TEXT PRIMARY KEY,num_votes INTEGER);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT);
            INSERT INTO titles VALUES
                ('tt9615014','Lego Masters','legomasters',2020,'series','',1,0,'tvSeries'),
                ('tt10285320','Lego Masters Australia','legomastersaustralia',2019,'series','',1,0,'tvSeries');
            INSERT INTO seasons VALUES('tt9615014',5),('tt10285320',8);
            INSERT INTO imdb_ratings VALUES('tt9615014',1000000),('tt10285320',1);")
            .unwrap();
        for release in [
            "LEGO Masters AU S03E09 HDTV x264-FQM",
            "Lego.Masters.Australia.S03E09.HDTV.x264-FQM",
        ]{
            let parsed=matchlab_parser::parse_title(release,false).unwrap();
            assert_eq!(parsed["seasons"],serde_json::json!([3]),"{parsed:?}");
            assert_eq!(parsed["episodes"],serde_json::json!([9]),"{parsed:?}");
            let title=parsed["title"].as_str().unwrap();
            let hits=search(&db,title,None,true,3,release,true,5).unwrap();
            assert_eq!(hits.first().map(|hit|hit.imdb_id.as_str()),
                Some("tt10285320"),
                "AU qualifier must select the matching complete IMDb title: {release}; parsed={title}; hits={hits:?}");
        }
        // The country marker is a useful identity signal, not an excuse
        // to discard the shorter US work when no qualifier was supplied.
        let unqualified=search(&db,"LEGO Masters",None,true,3,
            "LEGO Masters S03E09 HDTV x264-FQM",true,5).unwrap();
        assert_eq!(unqualified.first().map(|hit|hit.imdb_id.as_str()),
            Some("tt9615014"));
    }
    #[test] fn country_specific_series_beats_generic_alias() {
        let db=Connection::open_in_memory().unwrap();
        db.create_scalar_function("reverse",1,
            rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC|rusqlite::functions::FunctionFlags::SQLITE_UTF8,
            |ctx|{let s:String=ctx.get(0)?;Ok(s.chars().rev().collect::<String>())}).unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,
            year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE seasons(imdb_id TEXT,max_season INTEGER);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT,tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE episode_map(anidb_id TEXT,tvdb_season INTEGER);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anidb_titles(anidb_id TEXT,clean_title TEXT);
            INSERT INTO titles VALUES('tt100','The Voice US','thevoiceus',2011,'series','US',1,0,'tvSeries');
            INSERT INTO titles VALUES('tt200','The Voice','thevoice',2010,'series','US',1,0,'tvSeries');
            INSERT INTO seasons VALUES('tt100',10);
            INSERT INTO seasons VALUES('tt200',10);").unwrap();
        let found=search(&db,"The Voice US",None,true,3,"The.Voice.US.S03E02.1080p",true,10).unwrap();
        assert_eq!(found[0].imdb_id,"tt100");
    }
    #[test]
    fn original_release_longest_complete_title_beats_truncated_parser_title(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,
                year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE seasons(imdb_id TEXT,max_season INTEGER);
            CREATE TABLE imdb_ratings(imdb_id TEXT PRIMARY KEY,num_votes INTEGER);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            INSERT INTO titles VALUES
                ('tt0434706','Monster','monster',2004,'series','JP',1,0,'tvSeries'),
                ('tt13207736','Monster: The Ed Gein Story',
                    'monstertheedgeinstory',2025,'series','US',0,7,'tvSeries');
            INSERT INTO seasons VALUES ('tt0434706',1),('tt13207736',3);
            INSERT INTO imdb_ratings VALUES('tt0434706',200000),
                ('tt13207736',10);").unwrap();

        for (raw,year) in [
            ("Monster.the.ed.gein.story.s1.e1",None),
            ("Monster.the.ed.gein.story.2025.s1.e1",Some(2025)),
            ("Monster.The.Ed.Gein.Story.S01E01.1080p.WEB-DL",None),
            // The source omits "Story": this must still outrank "Monster"
            // because 4 complete title words match before the episode.
            ("Monster.the.ed.gein.s1.e1",None),
            ("Monster.the.ed.gein.2025.s1.e1",Some(2025)),
            ("Monster.The.Ed.Gein.S01E01.1080p.WEB-DL",None),
        ] {
            let parsed=matchlab_parser::parse_title(raw,false).unwrap();
            let parser_title=parsed["title"].as_str().unwrap();
            // The native extras handler recognizes "Ed" as anime ED,
            // but full raw-title evidence must still make the right title win.
            let hits=search(&db,parser_title,year,true,1,raw,true,5).unwrap();
            assert_eq!(hits[0].imdb_id,"tt13207736",
                "raw={raw:?} parsed={parser_title:?} hits={hits:?}");
        }

        // A complete shorter title ending at the same word boundary wins
        // against a title that has further unmatched words. Both still
        // outrank the unrelated bare "Monster" show.
        db.execute("INSERT INTO titles VALUES
            ('ttshort','Monster: The Ed Gein','monstertheedgein',2025,
             'series','US',0,0,'tvSeries')",[]).unwrap();
        db.execute("INSERT INTO seasons VALUES('ttshort',3)",[]).unwrap();
        db.execute("INSERT INTO imdb_ratings VALUES('ttshort',1)",[]).unwrap();
        let shorter=search(&db,"Monster the",None,true,1,
            "Monster.the.ed.gein.s1.e1",true,5).unwrap();
        assert_eq!(shorter[0].imdb_id,"ttshort",
            "a complete full-length prefix must beat an equally long partial IMDb name");

        // No complete matching title, no upgrade to a generic anime title.
        let wrong=search(&db,"Monster The Jeffrey Dahmer Story",None,true,1,
            "Monster.the.jeffrey.dahmer.story.s01.e01",true,10).unwrap();
        assert!(wrong.is_empty());

        // A word fragment is not a full title. "storyline" must not
        // authenticate "Monster The Ed Gein Story".
        let fragment=search(&db,"Monster the",None,true,1,
            "Monster.the.ed.gein.storyline.s1.e1",true,10).unwrap();
        assert!(!fragment.iter().any(|m|m.imdb_id=="tt13207736"));

        // Longer matching title wins despite a lower rating, unlike
        // works with identical full titles where region/votes decide.
        let source="Monster.the.ed.gein.story.s1.e1";
        let parsed=matchlab_parser::parse_title(source,false).unwrap();
        let chosen=search(&db,parsed["title"].as_str().unwrap(),None,true,1,
            source,true,1).unwrap();
        assert_eq!(chosen[0].imdb_id,"tt13207736");
    }

    #[test]
    fn original_release_partial_title_prefix_requires_real_word_boundary_and_coverage(){
        let row=Aka{imdb_id:"tt1".into(),title:"Monster: The Ed Gein Story".into(),
            clean:"monstertheedgeinstory".into(),year:2025,category:"series".into(),
            region:None,ordering:0,media_type:"tvSeries".into()};
        let parsed=semantic_words("Monster the");
        for original in [
            "Monster.the.ed.gein.s1.e1",
            "Monster.the.ed.gein.2025.s1.e1",
            "Monster.the.ed.gein.1080p.WEB-DL",
        ] {
            let keys=longer_release_title_prefixes("Monster the",original);
            assert_eq!(keys.last().map(String::as_str),Some("monstertheedgein"));
            let fit=title_in_original_release_fit(
                &semantic_words(original),&parsed,parsed.len()+keys.len(),&row)
                .expect("valid 4-word prefix of the longer IMDb title");
            assert_eq!(fit.tier,0);
            assert!(has_longer_title_evidence_in_release("Monster the",original,&row.title));
        }
        let fragment="Monster.the.ed.gein.storyline.s1.e1";
        let keys=longer_release_title_prefixes("Monster the",fragment);
        assert!(title_in_original_release_fit(&semantic_words(fragment),
            &parsed,parsed.len()+keys.len(),&row).is_none(),
            "an unrelated word after the prefix must not be ignored");

        let weak=Aka{title:"Monster the Ed Gein Story of Eighty Other Things".into(),
            ..row.clone()};
        let raw="Monster.the.ed.gein.s1.e1";
        let keys=longer_release_title_prefixes("Monster the",raw);
        assert!(title_in_original_release_fit(&semantic_words(raw),&parsed,
            parsed.len()+keys.len(),&weak).is_none(),
            "less than 50% or more than 16 unmatched characters is not a safe partial match");
        assert!(!has_longer_title_evidence_in_release(
            "Monster","Monster.S01E01",&row.title));
    }

    #[test]
    fn source_suffix_words_use_same_partial_title_coverage_as_source_prefix(){
        let candidate=Aka{
            imdb_id:"ttlong".into(),title:"Monster: The Ed Gein Story".into(),
            clean:"monstertheedgeinstory".into(),year:2025,
            category:"series".into(),region:Some("US".into()),
            ordering:0,media_type:"tvSeries".into()
        };
        for (raw,complete) in [
            ("Scene.Monster.the.Ed.Gein.S01E01",false),
            ("Scene.Monster.the.Ed.Gein.2025.S01E01",false),
            ("Scene.Monster.the.Ed.Gein.1080p.WEB-DL",false),
            ("Scene.Monster.the.Ed.Gein.Story.S01E01",true),
        ]{
            let content=semantic_release_content_words(raw);
            let fit=title_suffix_in_original_release_fit(&content,&candidate)
                .unwrap_or_else(||panic!("expected valid suffix in {raw}"));
            assert_eq!(fit.tier,0);
            assert_eq!(fit.edge,1,
                "suffix location is the edge tie-breaker; completeness is unmatched_bp");
            assert_eq!(fit.unmatched_bp==0,complete);
            let keys=longer_release_title_suffix_keys(raw);
            assert!(keys.iter().any(|key|key.starts_with("monstertheedgein")),
                "{keys:?}");
            assert!(has_longer_title_evidence_in_release(
                "Scene Monster the",raw,&candidate.title));
        }
        for raw in [
            "Scene.Monster.the.Ed.Gein.Storyline.S01E01",
            "Scene.Monster.the.Ed.Gein.Story.Extra.S01E01",
            "Scene.Monster.the.Jeffrey.Dahmer.Story.S01E01",
            "Scene.Monster.the.Ed.Gein.OtherTitle.S01E01",
        ] {
            assert!(title_suffix_in_original_release_fit(
                &semantic_release_content_words(raw),&candidate).is_none(),
                "unmatched source words may not be ignored: {raw}");
        }
        let too_long=Aka{title:"Monster The Ed Gein Story Of Many Entirely Different Episodes".into(),
            ..candidate.clone()};
        assert!(title_suffix_in_original_release_fit(
            &semantic_release_content_words("Scene.Monster.the.Ed.Gein.S01E01"),
            &too_long).is_none(),
            "require >=50% coverage and <=16 unmatched characters");
        assert!(longer_release_title_suffix_keys("Scene.Monster.S01E01").is_empty(),
            "an isolated generic title cannot justify new suffix discovery");
        assert!(!has_longer_title_evidence_in_release(
            "Scene Monster","Scene.Monster.S01E01",&candidate.title));
        assert!(longer_release_title_suffix_keys(
            "Scene.BLEACH.千年血戦篇.S01E01").contains(&"bleach".to_string()),
            "Unicode title words remain visible despite ASCII Tantivy keys");
    }

    #[test]
    fn suffix_recovery_keeps_non_latin_title_words_in_relevance(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,
                year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE seasons(imdb_id TEXT,max_season INTEGER);
            CREATE TABLE imdb_ratings(imdb_id TEXT PRIMARY KEY,num_votes INTEGER);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            INSERT INTO titles VALUES
                ('ttbleach','Bleach','bleach',2004,'series','US',1,0,'tvSeries'),
                ('ttjapan','BLEACH 千年血戦篇','bleach',2022,'series','JP',1,0,'tvSeries');
            INSERT INTO seasons VALUES('ttbleach',16),('ttjapan',4);
            INSERT INTO imdb_ratings VALUES('ttbleach',500000),('ttjapan',10);")
            .unwrap();
        let raw="Scene.BLEACH.千年血戦篇.S01E01";
        let hits=search(&db,"Scene BLEACH",None,true,1,raw,true,3).unwrap();
        assert_eq!(hits[0].imdb_id,"ttjapan",
            "the Japanese title makes the whole suffix specific: {hits:?}");
    }

    #[test]
    fn longest_suffix_in_original_release_beats_shorter_title_and_anime(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,
                year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE seasons(imdb_id TEXT,max_season INTEGER);
            CREATE TABLE imdb_ratings(imdb_id TEXT PRIMARY KEY,num_votes INTEGER);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            INSERT INTO titles VALUES
                ('tt2004','Monster','monster',2004,'series','JP',1,0,'tvSeries'),
                ('tt2025','Monster: The Ed Gein Story','monstertheedgeinstory',
                    2025,'series','US',0,7,'tvSeries'),
                ('ttshort','Monster The Ed Gein','monstertheedgein',
                    2025,'series','US',0,0,'tvSeries');
            INSERT INTO seasons VALUES('tt2004',1),('tt2025',3),('ttshort',3);
            INSERT INTO imdb_ratings VALUES('tt2004',500000),('tt2025',10),
                ('ttshort',1);")
            .unwrap();

        for raw in [
            "Scene.Monster.the.Ed.Gein.Story.S01E01",
            "Scene.Monster.the.Ed.Gein.Story.2025.S01E01",
        ]{
            let hits=search(&db,"Scene Monster the",None,true,1,raw,true,5).unwrap();
            assert_eq!(hits[0].imdb_id,"tt2025",
                "longest complete title in source suffix: {raw}: {hits:?}");
        }
        for raw in [
            "Scene.Monster.the.Ed.Gein.S01E01",
            "Scene.Monster.the.Ed.Gein.2025.S01E01",
            "Scene.Monster.the.Ed.Gein.1080p.WEB-DL",
        ]{
            let hits=search(&db,"Scene Monster the",None,true,1,raw,true,5).unwrap();
            assert_eq!(hits[0].imdb_id,"ttshort",
                "equal matched length: full title before partial: {raw}: {hits:?}");
        }
        // The original long candidate still matches when the full shorter
        // title isn't indexed, even when a more popular Monster exists.
        db.execute("DELETE FROM titles WHERE imdb_id='ttshort'",[]).unwrap();
        let hits=search(&db,"Scene Monster the",None,true,1,
            "Scene.Monster.the.Ed.Gein.S01E01",true,5).unwrap();
        assert_eq!(hits[0].imdb_id,"tt2025");

        // An exact full title is stronger than a shorter exact suffix:
        // rank by the length actually matched, not whether the word span
        // was recovered using the new suffix path.
        db.execute_batch("INSERT INTO titles VALUES
            ('ttoffice','The Office US','theofficeus',2005,'series','US',1,0,'tvSeries'),
            ('ttshortoffice','Office US','officeus',2010,'series','US',0,0,'tvSeries');
            INSERT INTO seasons VALUES('ttoffice',9),('ttshortoffice',9);
            INSERT INTO imdb_ratings VALUES('ttoffice',1),('ttshortoffice',900000);")
            .unwrap();
        let office=search(&db,"The Office US",None,true,1,
            "The.Office.US.S01E01",true,5).unwrap();
        assert_eq!(office[0].imdb_id,"ttoffice",
            "suffix discovery must not hijack a longer exact original title");
        let not_a_word=search(&db,"Scene Monster the",None,true,1,
            "Scene.Monster.the.Ed.Gein.Storyline.S01E01",true,5).unwrap();
        assert!(!not_a_word.iter().any(|m|m.imdb_id=="tt2025"),
            "a source word 'Storyline' cannot match a title word 'Story'");
    }

    #[test]
    fn original_torrent_is_suffix_of_longer_imdb_title_even_when_prefix_was_found(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,
                year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE seasons(imdb_id TEXT,max_season INTEGER);
            CREATE TABLE imdb_ratings(imdb_id TEXT PRIMARY KEY,num_votes INTEGER);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            INSERT INTO titles VALUES
                ('ttgein','Monster: The Ed Gein Story','monstertheedgeinstory',
                    2025,'series','US',0,9,'tvSeries'),
                ('ttshort','The Ed','theed',2025,'series','US',1,0,'tvSeries'),
                ('ttdahmer','Monster: The Jeffrey Dahmer Story',
                    'monsterthejeffreydahmerstory',2022,'series','US',0,1,'tvSeries');
            INSERT INTO seasons VALUES('ttgein',3),('ttshort',1),('ttdahmer',3);
            INSERT INTO imdb_ratings VALUES('ttgein',1),('ttshort',200000),
                ('ttdahmer',1000000);")
            .unwrap();
        for (raw,year) in [
            ("The.ed.gein.story.s01e01",None),
            ("The.ed.gein.story.2025.s01e01",Some(2025)),
            ("The.Ed.Gein.Story.S01E01.1080p.WEB-DL",None),
            ("Ed.Gein.Story.S01E01",None),
        ]{
            let parsed=matchlab_parser::parse_title(raw,false).unwrap();
            let parsed_title=parsed["title"].as_str().unwrap();
            let hits=search(&db,parsed_title,year,true,1,raw,true,5).unwrap();
            assert!(!hits.is_empty(),"no candidates: {raw}, parsed={parsed_title}");
            assert_eq!(hits[0].imdb_id,"ttgein",
                "longer IMDb-title suffix outranks shorter prefix and votes: {raw}, parser={parsed_title}: {hits:?}");
        }
        // The critical regression: an ALREADY FOUND exact/short prefix
        // must not prevent discovering the longer reversed-title suffix.
        // Verify both identities survive initial retrieval and are ranked
        // together rather than stopping on the first nonempty stage.
        let jointly_ranked=search(&db,"The Ed",None,true,1,
            "The.ed.gein.story.s01e01",true,10).unwrap();
        assert_eq!(jointly_ranked[0].imdb_id,"ttgein",
            "long reverse-title suffix must outrank an already found exact prefix");
        assert!(jointly_ranked.iter().any(|item|item.imdb_id=="ttshort"),
            "both prefix and suffix IMDb IDs must be gathered before ranking");
        // A complete matching IMDb title is stronger when the matched
        // source-word count ties a suffix of another IMDb identity.
        db.execute("INSERT INTO titles VALUES
            ('ttfull','The Ed Gein Story','theedgeinstory',2025,'series','US',1,0,'tvSeries')",
            []).unwrap();
        db.execute("INSERT INTO seasons VALUES('ttfull',1)",[]).unwrap();
        let full=search(&db,"The Ed Gein Story",None,true,1,
            "The.Ed.Gein.Story.S01E01",true,5).unwrap();
        assert_eq!(full[0].imdb_id,"ttfull",
            "complete exact title outranks a title missing its first word");

        for raw in [
            "The.Ed.Gein.Storyline.S01E01",
            "The.Ed.Gein.Other.S01E01",
            "Gein.Story.S01E01",
            "The.Ed.Gein.Story.Extra.S01E01",
        ]{
            let parsed=matchlab_parser::parse_title(raw,false).unwrap();
            let parsed_title=parsed["title"].as_str().unwrap();
            let hits=search(&db,parsed_title,None,true,1,raw,true,10).unwrap();
            assert!(!hits.iter().any(|h|h.imdb_id=="ttgein"),
                "full word, source coverage and metadata boundary must hold: {raw}: {hits:?}");
        }
    }

    #[test]
    fn imdb_title_suffix_has_same_coverage_guards_as_prefix(){
        let candidate=Aka{
            imdb_id:"ttgein".into(),title:"Monster: The Ed Gein Story".into(),
            clean:"monstertheedgeinstory".into(),year:2025,
            category:"series".into(),region:Some("US".into()),
            ordering:0,media_type:"tvSeries".into(),
        };
        for raw in ["The.Ed.Gein.Story.S01E01",
                    "The.Ed.Gein.Story.2025.S01E01",
                    "The.Ed.Gein.Story.1080p.WEB-DL"]{
            let content=semantic_release_content_words(raw);
            let fit=release_is_imdb_title_suffix_fit(&content,&candidate)
                .expect("full source title is an IMDb title suffix");
            assert_eq!(fit.tier,0);
            assert_eq!(fit.edge,1);
            assert!(fit.longest_first<0);
            assert!(has_longer_title_evidence_in_release("The",raw,&candidate.title));
        }
        for raw in ["The.Ed.Gein.Storyline.S01E01",
                    "Gein.Story.S01E01",
                    "Scene.The.Ed.Gein.Story.S01E01",
                    "The.Ed.Gein.Story.Extras.S01E01"]{
            assert!(release_is_imdb_title_suffix_fit(
                &semantic_release_content_words(raw),&candidate).is_none(),
                "a random substring, weak suffix or unmatched word is invalid: {raw}");
        }
    }

    #[test]
    fn monster_dahmer_release_never_becomes_monster_2004_anime(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,
                year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE seasons(imdb_id TEXT,max_season INTEGER);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT,
                tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE episode_map(anidb_id TEXT,tvdb_season INTEGER);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE imdb_ratings(imdb_id TEXT,num_votes INTEGER);
            CREATE TABLE anidb_titles(anidb_id TEXT,clean_title TEXT);
            INSERT INTO titles VALUES('tt0434706','Monster','monster',2004,
                'series','JP',1,0,'tvSeries');
            INSERT INTO seasons VALUES('tt0434706',1);
            INSERT INTO anidb_titles VALUES('100','monster');
            INSERT INTO anime_series_map VALUES('tt0434706','100','100',0);").unwrap();
        let wrong=search(&db,"Monster The Jeffrey Dahmer Story",None,true,1,
            "monster.the.jeffrey.dahmer.story.s01.e01",true,10).unwrap();
        assert!(wrong.is_empty(),
            "an unrelated long live-action title must not return the 2004 Monster anime");
        let exact=search(&db,"Monster",None,true,1,"Monster.S01E01",true,10)
            .unwrap();
        assert_eq!(exact.len(),1);
        assert_eq!(exact[0].imdb_id,"tt0434706",
            "the exact anime title should still match normally");
    }
    #[test]
    fn lexical_alignment_ranks_whole_word_prefix_over_equal_suffix(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,
            year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE imdb_ratings(imdb_id TEXT,num_votes INTEGER);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT);
            INSERT INTO titles VALUES
                ('tt1111111','Alpha Beta','alphabeta',2021,'movie','US',1,0,'movie'),
                ('tt2222222','Beta Gamma','betagamma',2021,'movie','US',1,0,'movie');
            INSERT INTO imdb_ratings VALUES('tt1111111',1),('tt2222222',90000);")
            .unwrap();
        let found=search(&db,"Alpha Beta Gamma",None,false,0,
            "Alpha.Beta.Gamma.2021.WEB-DL",false,10).unwrap();
        assert_eq!(found.len(),2);
        assert_eq!(found[0].imdb_id,"tt1111111",
            "equal-coverage whole-word prefix must beat suffix, not popularity");
    }
    #[test]
    fn title_number_mistaken_for_parser_suffix_is_preserved(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,
            year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE imdb_ratings(imdb_id TEXT,num_votes INTEGER);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT);
            CREATE TABLE seasons(imdb_id TEXT,max_season INTEGER);
            INSERT INTO titles VALUES
                ('tt0000123','Televisionseries1','televisionseries1',2020,
                 'series','US',1,0,'tvSeries');
            INSERT INTO seasons VALUES('tt0000123',10);").unwrap();
        for title in ["Televisionseries","Television"] {
            let hits=search(&db,title,None,true,1,
                "Televisionseries1.S01E02.1080p.WEB-DL",true,10).unwrap();
            assert_eq!(hits[0].imdb_id,"tt0000123","parser title: {title}");
        }
    }
    #[test]
    fn suffix_of_release_title_is_discovered_without_reverse_fallback(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,
            year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE imdb_ratings(imdb_id TEXT,num_votes INTEGER);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT);
            INSERT INTO titles VALUES
                ('tt1111111','Beyond Dune','beyonddune',2021,'movie','US',1,0,'movie'),
                ('tt2222222','Mystery','mystery',2021,'movie','US',1,0,'movie');")
            .unwrap();
        let found=search(&db,"Mystery Beyond Dune",None,false,0,
            "Mystery.Beyond.Dune.2021.1080p",false,10).unwrap();
        assert_eq!(found.len(),1);
        assert_eq!(found[0].imdb_id,"tt1111111");
    }
    #[test]
    fn word_fragments_and_short_generic_titles_are_not_identity(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE titles(imdb_id TEXT,title TEXT,clean_title TEXT,
            year INTEGER,category TEXT,region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE imdb_ratings(imdb_id TEXT,num_votes INTEGER);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT);
            INSERT INTO titles VALUES
                ('tt1111111','Dune','dune',2021,'movie','US',1,0,'movie'),
                ('tt2222222','Monster','monster',2004,'movie','US',1,0,'movie');")
            .unwrap();
        assert!(search(&db,"Duneland",None,false,0,"Duneland.2026",false,10)
            .unwrap().is_empty(),"a substring of a word cannot be an alias");
        assert!(search(&db,"Monster The Jeffrey Dahmer Story",None,false,0,
            "Monster.The.Jeffrey.Dahmer.Story",false,10).unwrap().is_empty(),
            "a short generic prefix cannot override an unrelated longer title");
    }

    #[test] fn imdb_exact_year_and_series_cap() {
        let db=Connection::open_in_memory().unwrap();
        db.create_scalar_function("reverse",1,
            rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC|rusqlite::functions::FunctionFlags::SQLITE_UTF8,
            |ctx|{let s:String=ctx.get(0)?;Ok(s.chars().rev().collect::<String>())}).unwrap();
        db.execute_batch("CREATE TABLE titles(
            imdb_id TEXT,title TEXT,clean_title TEXT,year INTEGER,category TEXT,
            region TEXT,original INTEGER,ordering INTEGER,type TEXT);
            CREATE TABLE seasons(imdb_id TEXT,max_season INTEGER);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT,tvdb_id TEXT,trakt_season INTEGER);
            CREATE TABLE episode_map(anidb_id TEXT,tvdb_season INTEGER);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            CREATE TABLE anidb_titles(anidb_id TEXT,clean_title TEXT);
            INSERT INTO titles VALUES('tt1','Dune','dune',1984,'movie','US',1,0,'movie');
            INSERT INTO titles VALUES('tt2','Dune','dune',2021,'movie','US',1,0,'movie');
            INSERT INTO titles VALUES('tt3','Breaking Bad','breakingbad',2008,'series','US',1,0,'tvSeries');
            INSERT INTO seasons VALUES('tt3',5);").unwrap();
        let movies=search(&db,"Dune",Some(2021),false,0,"Dune.2021.1080p",false,20).unwrap();
        assert_eq!(movies[0].imdb_id,"tt2");
        let no_year=search(&db,"Dune",None,false,0,"Dune.1080p",false,20).unwrap();
        assert!(!no_year.is_empty(),"equally plausible titles must still return a candidate");
        assert_eq!(no_year[0].imdb_id,"tt1","stable ranking chooses the earlier title without a release year");

        let series=search(&db,"Breaking Bad",None,true,4,"Breaking.Bad.S04E02",true,20).unwrap();
        assert_eq!(series[0].imdb_id,"tt3");
        let rejected=search(&db,"Breaking Bad",None,true,9,"Breaking.Bad.S09E02",true,20).unwrap();
        assert!(rejected.is_empty());
    }
}

#[cfg(test)]
mod batch_eligibility_tests {
    use super::*;

    #[test]
    fn batched_season_gating_matches_individual_sql_across_chunk_boundaries(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE seasons(imdb_id TEXT,max_season INTEGER);
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT,
                trakt_season INTEGER);
            CREATE TABLE episode_map(anidb_id TEXT,tvdb_season INTEGER);")
            .unwrap();
        let mut rows=Vec::new();
        for index in 0..310 {
            let id=format!("tt{index:07}");
            let max_season=if index%3==0{8}else{4};
            db.execute("INSERT INTO seasons VALUES (?1,?2)",
                params![&id,max_season]).unwrap();
            if index%7==0 {
                let anidb=format!("a{index}");
                db.execute("INSERT INTO anime_series_map VALUES(?1,?2,0)",
                    params![&id,&anidb]).unwrap();
                db.execute("INSERT INTO episode_map VALUES(?1,8)",
                    [&anidb]).unwrap();
            }
            let row=Aka{imdb_id:id,title:"Example".into(),clean:"example".into(),
                year:2020,category:"series".into(),region:None,
                ordering:0,media_type:"tvSeries".into()};
            rows.push(row.clone());
            // Multiple IMDb AKA rows must not cause duplicate eligibility SQL.
            if index%4==0 {rows.push(row);}
        }
        let expected=rows.iter()
            .filter(|row|eligible(&db,row,8).unwrap())
            .map(|row|row.imdb_id.clone()).collect::<Vec<_>>();
        let actual=filter_eligible(&db,rows,true,None,8).unwrap()
            .into_iter().map(|row|row.imdb_id).collect::<Vec<_>>();
        assert_eq!(actual,expected);
    }
}

#[cfg(test)]
mod shortlist_metadata_tests {
    use super::*;

    #[test]
    fn batched_ratings_and_anime_ids_match_scalar_enrichment_over_multiple_chunks(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE imdb_ratings(
                imdb_id TEXT PRIMARY KEY,num_votes INTEGER);
            CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT,
                PRIMARY KEY(group_id,type,value));
            CREATE TABLE anime_series_map(imdb_id TEXT,anidb_id TEXT);").unwrap();
        let mut ids=Vec::new();
        for n in 0..415 {
            let id=format!("tt{n:07}");
            ids.push(id.clone());
            if n%3==0 {
                db.execute("INSERT INTO imdb_ratings VALUES(?1,?2)",
                    params![&id,(n*137) as i64]).unwrap();
            }
            if n%4==0 {
                db.execute("INSERT INTO anime_ids VALUES(?1,'mal',?2)",
                    params![&id,format!("M{n}")]).unwrap();
            }
            if n%5==0 {
                db.execute("INSERT INTO anime_ids VALUES(?1,'anidb',?2)",
                    params![&id,format!("A{n:05}")]).unwrap();
                if n%25==0 {
                    db.execute("INSERT INTO anime_ids VALUES(?1,'anidb',?2)",
                        params![&id,format!("B{n:05}")]).unwrap();
                }
            }
            if n%7==0 {
                db.execute("INSERT INTO anime_series_map VALUES(?1,?2)",
                    params![&id,format!("fallback{n}")]).unwrap();
                if n%49==0 {
                    db.execute("INSERT INTO anime_series_map VALUES(?1,?2)",
                        params![&id,format!("other{n}")]).unwrap();
                }
            }
        }
        let batch=shortlist_metadata(&db,&ids).unwrap();
        assert_eq!(batch.len(),ids.len());
        for id in ids {
            let votes=db.query_row("SELECT num_votes FROM imdb_ratings WHERE imdb_id=?1",
                [&id],|r|r.get::<_,i64>(0)).optional().unwrap().unwrap_or(0);
            let mut expected=BTreeMap::new();
            let mut stmt=db.prepare("SELECT type,value FROM anime_ids WHERE group_id=?1")
                .unwrap();
            for row in stmt.query_map([&id],|r|
                Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))).unwrap(){
                let (kind,value)=row.unwrap();
                expected.insert(kind,value);
            }
            if !expected.contains_key("anidb") {
                let mut stmt=db.prepare(
                    "SELECT DISTINCT anidb_id FROM anime_series_map WHERE imdb_id=?1")
                    .unwrap();
                let linked=stmt.query_map([&id],|r|r.get::<_,String>(0)).unwrap()
                    .collect::<rusqlite::Result<Vec<_>>>().unwrap();
                if linked.len()==1 {
                    expected.insert("anidb".into(),linked[0].clone());
                }
            }
            assert_eq!(batch.get(&id),Some(&(votes,expected)),"{id}");
        }
    }

    #[test]
    fn absent_ratings_table_preserves_zero_vote_fallback(){
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE anime_ids(group_id TEXT,type TEXT,value TEXT);
            INSERT INTO anime_ids VALUES
             ('tt0000001','anidb','11'),('tt0000002','anidb','12'),
             ('tt0000003','anidb','13'),('tt0000004','anidb','14'),
             ('tt0000005','anidb','15');").unwrap();
        let ids=(1..=5).map(|n|format!("tt{n:07}")).collect::<Vec<_>>();
        let batch=shortlist_metadata(&db,&ids).unwrap();
        assert!(batch.values().all(|(votes,ids)|*votes==0 && ids.contains_key("anidb")));
    }
}
