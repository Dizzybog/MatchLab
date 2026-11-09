//! Shared title normalization. Independent IMDb and AniDB matchers reuse
//! the same normalization contract without depending on each other.
use unicode_normalization::UnicodeNormalization;

pub fn clean_title(input:&str)->String{
    let lower=input.to_lowercase()
        .replace('æ',"ae").replace('ø',"o").replace('œ',"oe")
        .replace('ß',"ss");
    let ascii:String=lower.nfkd().filter(|c|c.is_ascii()).collect();
    ascii.split(|c:char|!c.is_ascii_alphanumeric())
        .filter(|word|!word.is_empty() && *word!="and")
        .collect()
}

#[cfg(test)]
mod tests{
    use super::*;
    #[test]
    fn normalizes_both_matcher_domains_identically(){
        assert_eq!(clean_title("Bleach 320"),"bleach320");
        assert_eq!(clean_title("JoJo's Bizarre Adventure"),"jojosbizarreadventure");
    }
}
