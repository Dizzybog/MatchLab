//! Trusted community regex promotion regression tests.
//! Only the GitHub Action can append examples from the maintainer-owned
//! fixture catalogue *after* the Rust impact auditor certifies the source
//! diff. No Issue title becomes a label by posting a comment.
use matchlab_parser::parse_title_canonical;
use serde_json::{Map, Value};

#[test]
fn independently_verified_community_source_regexes_keep_their_labels(){
    let fixtures:Vec<Value>=serde_json::from_str(
        include_str!("../../../learning/regex_promoted_tests.json")
    ).expect("approved community regression JSON");
    assert!(fixtures.len()<=1000,"unbounded approved regex fixtures");
    for fixture in fixtures{
        let title=fixture["title"].as_str()
            .expect("approved fixture title");
        assert!(!title.is_empty()&&title.len()<=200);
        let expected=fixture["expected"].as_object()
            .expect("independently verified labels");
        assert!(!Map::is_empty(expected));
        let parsed=parse_title_canonical(title,false)
            .unwrap_or_else(|err|panic!("{title}: {err}"));
        for (field,truth) in expected {
            assert_eq!(parsed.get(field),Some(truth),
                "approved community regex changed a trusted field: title={title}, field={field}");
        }
    }
}
