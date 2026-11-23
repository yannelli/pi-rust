//! Run `sh scripts/acp-schema-oracle.sh` to check the fixtures against the published 0.13.7 crate.
//! Pass `--write` to regenerate `schema_golden.json` after changing the cases.

mod cases {
    use crate::acp::schema::*;
    include!("schema_outbound.rs");
    include!("schema_inbound.rs");
}

#[test]
fn schema_matches_the_recorded_pinned_json() {
    let golden: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(include_str!("schema_golden.json")).unwrap();
    let cases: Vec<_> = cases::outbound().into_iter().chain(cases::inbound()).collect();
    let names: std::collections::HashSet<_> = cases.iter().map(|(name, _)| *name).collect();
    assert_eq!(names.len(), cases.len(), "case names are distinct");
    assert_eq!(cases.len(), golden.len(), "every recorded case runs");
    for (name, actual) in cases {
        assert_eq!(Some(actual.as_str()), golden[name].as_str(), "{name}");
    }
}
