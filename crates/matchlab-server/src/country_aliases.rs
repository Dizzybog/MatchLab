//! MatchLab country-qualified TV-series alias expansion.
//! Only an explicit trailing country marker is expanded; language tags are not countries.
use crate::imdb_matcher::clean_title;
use std::{collections::{HashMap,HashSet},sync::OnceLock};

const COUNTRIES:&[(&str,&[&str])]=&[
("US",&["us","usa","united states","america"]),
("UK",&["uk","gb","britain","united kingdom"]),
("NO",&["no","norway","norge","norsk"]),
("SE",&["se","sweden","sverige"]),
("DK",&["dk","denmark","danmark"]),
("FI",&["fi","finland","suomi"]),
("DE",&["de","germany","deutschland"]),
("FR",&["fr","france"]),
("ES",&["es","spain","espana","españa"]),
("IT",&["it","italy","italia"]),
("CA",&["ca","canada"]),
("AU",&["au","australia"]),
("NZ",&["nz","new zealand"]),
("JP",&["jp","japan"]),
("KR",&["kr","south korea","korea"]),
("IN",&["in","india"]),
("BR",&["br","brazil","brasil"]),
("MX",&["mx","mexico","méxico"]),
("PL",&["pl","poland","polska"]),
("NL",&["nl","netherlands","holland"]),
];
fn is_stop(token:&str)->bool{
    let t=token.to_ascii_lowercase();
    if matches!(t.as_str(),"4k"|"uhd"|"bluray"|"blu-ray"|"bdrip"|"brrip"|"webrip"|
        "hdrip"|"dvdrip"|"x264"|"x265"|"h264"|"h265"|"hevc"|"av1"|
        "nf"|"netflix"|"amzn"|"amazon"|"2160p"|"1080p"|"720p"|"576p"|"480p"){return true;}
    if t.len()==4&&t.starts_with("20")&&t.bytes().all(|c|c.is_ascii_digit()){return true;}
    if t.len()==4&&t.starts_with("19")&&t.bytes().all(|c|c.is_ascii_digit()){return true;}
    if t.starts_with('s')&&t.len()>1&&t.as_bytes()[1].is_ascii_digit(){return true;}
    if t.ends_with('p')&&t[..t.len()-1].bytes().all(|c|c.is_ascii_digit()){return true;}
    if t.len()>=3&&t.contains('x')&&t.split_once('x').is_some_and(|(a,b)|
        !a.is_empty()&&!b.is_empty()&&a.bytes().all(|c|c.is_ascii_digit())
        &&b.bytes().all(|c|c.is_ascii_digit())){return true;}
    false
}
/// A country marker explicitly present at the end of the release title,
/// before technical tags. This is independent of the host's locale.
pub fn qualified_with_base(raw:&str)->Option<(&'static str,String,Vec<String>)>{
    let normalized=raw.replace(['.','_','-']," ");
    let words:Vec<_>=normalized.split_whitespace().collect();
    let stop=words.iter().position(|w|is_stop(w)).unwrap_or(words.len());
    let title=words[..stop].join(" ");
    let title=title.trim();
    if title.is_empty(){return None;}
    let lower=title.to_lowercase();
    for (code,aliases) in COUNTRIES {
        let mut ordered=aliases.to_vec();
        ordered.sort_by_key(|s|std::cmp::Reverse(s.len()));
        for alias in ordered {
            let marker=format!(" {alias}");
            if !lower.ends_with(&marker){continue;}
            let prefix=&title[..title.len()-marker.len()];
            if prefix.trim().is_empty(){continue;}
            let mut seen=HashSet::new();
            let mut out=Vec::new();
            for variant in std::iter::once(*code).chain(aliases.iter().copied()){
                let key=clean_title(&format!("{prefix} {variant}"));
                if !key.is_empty()&&seen.insert(key.clone()){out.push(key);}
            }
            return Some((*code,clean_title(prefix),out));
        }
    }
    None
}
/// An independently verified regional-series identity that can disambiguate
/// two *different* IMDb series with the same unqualified display title.
/// IMDb title.akas 'region' describes localized display titles, not origin.
pub fn regional_series_id(base_title:&str,country:&str)->Option<&'static str>{
    // Nested maps let us query borrowed &str keys without allocations.
    // The registry is built once and contains only verified exceptions.
    type Registry=HashMap<&'static str,HashMap<&'static str,&'static str>>;
    static REGISTRY:OnceLock<Registry>=OnceLock::new();
    REGISTRY.get_or_init(||{
        let mut entries=Registry::new();
        for line in include_str!("../data/regional_series.tsv").lines()
            .filter(|line|!line.is_empty()&&!line.starts_with('#')){
            let mut fields=line.split('\t');
            let (Some(base),Some(code),Some(id),None)=
                (fields.next(),fields.next(),fields.next(),fields.next())
                else{continue};
            if id.starts_with("tt")&&!id[2..].is_empty()
                &&id[2..].bytes().all(|c|c.is_ascii_digit())
                &&!base.is_empty()&&!code.is_empty(){
                entries.entry(base).or_default().insert(code,id);
            }
        }
        entries
    }).get(base_title).and_then(|by_country|by_country.get(country)).copied()
}
#[cfg(test)]
pub fn qualified(raw:&str)->Option<(&'static str,Vec<String>)>{
    qualified_with_base(raw).map(|(code,_,aliases)|(code,aliases))
}
#[cfg(test)]
pub fn candidates(raw:&str)->Vec<String>{
    qualified(raw).map(|(_,aliases)|aliases).unwrap_or_default()
}
#[cfg(test)]
mod tests{
    use super::*;
    #[test]fn country_edition(){let c=candidates("The Voice US S03E01 1080p");
        assert!(c.contains(&"thevoiceusa".to_string()));assert!(c.contains(&"thevoiceunitedstates".to_string()));}
    #[test]fn curated_regional_series_id_requires_exact_base_and_country(){
        let (country,base,_)=qualified_with_base("Ex.on.the.Beach.US.S01").unwrap();
        assert_eq!((country,base.as_str()),("US","exonthebeach"));
        assert_eq!(regional_series_id(&base,country),Some("tt8285922"));
        assert_eq!(regional_series_id("exonthebeach","UK"),Some("tt3522052"));
        assert_eq!(regional_series_id("thevoice","US"),None);
        assert_eq!(regional_series_id("exonthebeach","NO"),None);
        assert_eq!(qualified_with_base("Ex.on.the.Beach.S01"),None);
    }
    #[test]fn explicit_region_only_when_title_has_country_suffix(){
        assert_eq!(qualified("The Voice Norge S01E01").unwrap().0,"NO");
        assert_eq!(qualified("The Voice UK S01E01").unwrap().0,"UK");
        assert_eq!(qualified("Example.2020.1080p"),None);
        assert_eq!(qualified("Example.MULTI.1080p"),None);
        assert_eq!(qualified("Norway.S01E01"),None,
            "a country alone is not an explicit title qualifier");
    }
    #[test]fn ignores_language_tags(){assert!(candidates("The Voice MULTI S03E01").is_empty());}
    #[test]fn nordic(){assert!(candidates("The Voice Norge S01E01").contains(&"thevoiceno".to_string()));}
}
