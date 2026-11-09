//! Canonical public IDs shared by independent IMDb/AniDB matchers and
//! episode conversion. This module has no metadata or database dependency.

/// Public ASIDs must have a numeric AniDB ID and episode in 1..=99_999.
pub(crate) fn canonical_asid(anidb_id:&str,episode:i64)->Option<String>{
    if !(1..=99_999).contains(&episode)
        || anidb_id.is_empty()
        || !anidb_id.bytes().all(|byte|byte.is_ascii_digit()){
        return None;
    }
    let id=anidb_id.parse::<u64>().ok()?;
    if !(1..=99_999).contains(&id){return None;}
    Some(format!("{anidb_id}:{episode}"))
}

pub(crate) fn valid_asid(asid:&str)->bool{
    let Some((id,ep))=asid.split_once(':')else{return false};
    let Some(episode)=ep.parse::<i64>().ok()else{return false};
    canonical_asid(id,episode).as_deref()==Some(asid)
}

#[cfg(test)]
mod tests{
    use super::*;
    #[test]
    fn canonical_bounds_are_independent_of_metadata(){
        assert_eq!(canonical_asid("2369",320).as_deref(),Some("2369:320"));
        for invalid in [("2369",0),("2369",100_000),("0",1),
            ("100000",1),("abc",2)]{
            assert!(canonical_asid(invalid.0,invalid.1).is_none());
        }
        assert!(valid_asid("2369:320"));
        assert!(!valid_asid("2369:000320"));
    }
}
