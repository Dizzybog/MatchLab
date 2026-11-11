//! Shared title normalization. Independent IMDb and AniDB matchers reuse
//! the same normalization contract without depending on each other.
use unicode_normalization::UnicodeNormalization;

fn ascii_fold(input:&str)->String{
    let lower=input.to_lowercase()
        .replace('æ',"ae").replace('ø',"o").replace('œ',"oe")
        .replace('ß',"ss");
    lower.nfkd().filter(|c|c.is_ascii()).collect()
}

pub fn clean_title(input:&str)->String{
    let ascii=ascii_fold(input);
    ascii.split(|c:char|!c.is_ascii_alphanumeric())
        .filter(|word|!word.is_empty() && *word!="and")
        .collect()
}

/// Retain normalized word boundaries for ranking partial title matches.
/// A joined character prefix alone would confuse "Dune" and "Duneland".
/// Only the shortlist ranker uses this; Tantivy's compact keys stay unchanged.
pub fn clean_words(input:&str)->Vec<String>{
    let ascii=ascii_fold(input);
    ascii.split(|c:char|!c.is_ascii_alphanumeric())
        .filter(|word|!word.is_empty() && *word!="and")
        .map(str::to_owned).collect()
}

#[cfg(test)]
mod tests{
    use super::*;
    #[test]
    fn normalizes_both_matcher_domains_identically(){
        assert_eq!(clean_title("Bleach 320"),"bleach320");
        assert_eq!(clean_title("JoJo's Bizarre Adventure"),"jojosbizarreadventure");
        assert_eq!(clean_words("The Lord of the Rings"),vec!["the","lord","of","the","rings"]);
        assert_eq!(clean_words("Duneland"),vec!["duneland"]);
        assert_eq!(clean_words("Dune"),vec!["dune"]);
        assert_eq!(clean_words("Pokémon and Ægir"),vec!["pokemon","aegir"]);
        assert_eq!(clean_words("Pokémon and Ægir").concat(),clean_title("Pokémon and Ægir"));
    }
}
