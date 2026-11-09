//! MatchLab country-qualified TV-series alias expansion.
//! Only an explicit trailing country marker is expanded; language tags are not countries.
use crate::imdb_matcher::clean_title;
use std::collections::HashSet;

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
pub fn candidates(raw:&str)->Vec<String>{
    let normalized=raw.replace(['.','_','-']," ");
    let words:Vec<_>=normalized.split_whitespace().collect();
    let stop=words.iter().position(|w|is_stop(w)).unwrap_or(words.len());
    let title=words[..stop].join(" ");
    let title=title.trim();
    if title.is_empty(){return vec![];}
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
            return out;
        }
    }
    vec![]
}
#[cfg(test)]
mod tests{
    use super::*;
    #[test]fn country_edition(){let c=candidates("The Voice US S03E01 1080p");
        assert!(c.contains(&"thevoiceusa".to_string()));assert!(c.contains(&"thevoiceunitedstates".to_string()));}
    #[test]fn ignores_language_tags(){assert!(candidates("The Voice MULTI S03E01").is_empty());}
    #[test]fn nordic(){assert!(candidates("The Voice Norge S01E01").contains(&"thevoiceno".to_string()));}
}
