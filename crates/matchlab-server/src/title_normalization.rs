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

/// Count the title's original words before Tantivy's compact ASCII key
/// removes word boundaries. Dots, dashes, underscores and spaces separate
/// words; apostrophes/backticks within names do not (JoJo's == Jojos: one word).
/// Unlike clean_words, common connective words such as "and" still count.
/// An ampersand counts as that same connective word ("Locke & Key").
/// No allocations and no Tantivy index schema change are required.
pub fn title_word_count(input:&str)->usize{
    let mut count=0;
    let mut in_word=false;
    for ch in input.chars(){
        if ch.is_alphanumeric(){
            if !in_word{count+=1;}
            in_word=true;
        }else if ch=='&'{
            count+=1;
            in_word=false;
        }else if ch!='\'' && ch!='’' && ch!='`'
            && !unicode_normalization::char::is_combining_mark(ch){
            in_word=false;
        }
    }
    count
}

/// Retain normalized word boundaries for ranking partial title matches.
/// A joined character prefix alone would confuse "Dune" and "Duneland".
/// Only the shortlist ranker uses this; Tantivy's compact keys stay unchanged.
pub fn clean_words(input:&str)->Vec<String>{
    // AniDB uses ASCII backticks as apostrophes in official English names.
    // Possessives must agree on words as well as the existing compact key.
    let ascii=ascii_fold(input).replace(['\'','`'],"");
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
        assert_eq!(clean_words("JoJo's Bizarre Adventure"),
            vec!["jojos","bizarre","adventure"]);
        assert_eq!(clean_words("Pokémon and Ægir").concat(),clean_title("Pokémon and Ægir"));
    }
    #[test]
    fn title_word_count_preserves_separators_and_apostrophes(){
        assert_eq!(title_word_count("Supernatural"),1);
        assert_eq!(title_word_count("Super-Natural"),2);
        assert_eq!(title_word_count("Super.Natural"),2);
        assert_eq!(title_word_count("Super_Natural"),2);
        assert_eq!(title_word_count("Super  Natural"),2);
        assert_eq!(title_word_count("JoJo's Bizarre Adventure"),3);
        assert_eq!(title_word_count("JoJo’s Bizarre Adventure"),3);
        assert_eq!(title_word_count("JoJo`s Bizarre Adventure"),3);
        for title in ["JoJo's Bizarre Adventure","JoJo’s Bizarre Adventure",
            "JoJo`s Bizarre Adventure","JoJos Bizarre Adventure"]{
            assert_eq!(clean_words(title),vec!["jojos","bizarre","adventure"]);
            assert_eq!(clean_title(title),"jojosbizarreadventure");
        }
        assert_eq!(title_word_count("The Lord of the Rings"),5);
        assert_eq!(title_word_count("Rock and Roll"),3);
        assert_eq!(title_word_count("Rock & Roll"),3);
        assert_eq!(title_word_count("Rock&Roll"),3);
        assert_eq!(title_word_count("Rock Roll"),2);
        assert_eq!(title_word_count("BLEACH 千年血戦篇"),2);
        assert_eq!(title_word_count("Amélie"),1);
        assert_eq!(title_word_count("Ame\u{301}lie"),1);
        assert_eq!(title_word_count("Pokémon"),title_word_count("Poke\u{301}mon"));
        assert_eq!(title_word_count("..."),0);
    }
}
