use callboard_core::feed::*;
use serde_json::json;

fn parse(value: serde_json::Value) -> Result<Snapshot, ValidationError> {
    parse_submission(&serde_json::to_vec(&value).unwrap())
}

#[test]
fn feed_name_boundaries() {
    for name in ["a", "0", "gh.my_repo-1", &"a".repeat(64)] {
        assert!(validate_feed_name(name).is_ok(), "{name}");
    }
    for name in ["", "A", "-a", "a/b", "a b", "é", &"a".repeat(65)] {
        assert!(validate_feed_name(name).is_err(), "{name}");
    }
}

#[test]
fn clearing_requires_explicit_items() {
    for input in [
        "",
        " \n\t",
        "{}",
        "null",
        "{\"items\":null}",
        "[",
        "[] garbage",
    ] {
        assert!(parse_submission(input.as_bytes()).is_err(), "{input:?}");
    }
    for input in ["[]", "{\"items\":[]}"] {
        assert!(parse_submission(input.as_bytes()).unwrap().items.is_empty());
    }
}

#[test]
fn object_roundtrip_preserves_order_and_scalar_metadata() {
    let snapshot = parse(json!({
        "title": "Reviews", "source_url": "https://example.com", "stale_after": "1h",
        "items": [
            {"key": "z", "title": "First", "meta": {"author": "someone", "count": 2, "ready": true}},
            {"key": "a", "title": "Second", "url": "custom:shown-only", "tags": ["review"]}
        ]
    })).unwrap();
    assert_eq!(
        snapshot
            .items
            .iter()
            .map(|i| i.key.as_str())
            .collect::<Vec<_>>(),
        ["z", "a"]
    );
    assert_eq!(
        parse_submission(&serde_json::to_vec(&snapshot).unwrap()).unwrap(),
        snapshot
    );
    assert_eq!(parse(json!([])).unwrap().title, None);
}

#[test]
fn invalid_shapes_and_duplicate_keys_are_rejected() {
    for item in [json!({"key":"a"}), json!({"title":"Missing key"})] {
        assert!(parse(json!([item])).is_err());
    }
    for value in [json!(null), json!([]), json!({"nested": true})] {
        assert!(parse(json!([{"key":"a", "title":"A", "meta":{"bad":value}}])).is_err());
    }
    assert!(
        parse(json!([
            {"key":"a", "title":"One"}, {"key":"a", "title":"Two"}
        ]))
        .is_err()
    );
    assert!(parse(json!({"stale_after":"eventually", "items":[]})).is_err());
}

#[test]
fn title_characters_and_body_bytes_have_distinct_limits() {
    assert!(parse(json!([{"key":"a", "title":"é".repeat(500), "body":"é".repeat(8192)}])).is_ok());
    assert!(parse(json!([{"key":"a", "title":"é".repeat(501)}])).is_err());
    assert!(parse(json!([{"key":"a", "title":"A", "body":"é".repeat(8193)}])).is_err());
    assert!(parse(json!({"title":"a".repeat(501), "items":[]})).is_err());
}

#[test]
fn collection_limits_accept_boundary_and_reject_overflow() {
    for (size, accepted) in [(32, true), (33, false)] {
        let tags = vec!["tag"; size];
        let meta: serde_json::Map<String, serde_json::Value> =
            (0..size).map(|i| (i.to_string(), json!(true))).collect();
        assert_eq!(
            parse(json!([{"key":"a", "title":"A", "tags":tags}])).is_ok(),
            accepted
        );
        assert_eq!(
            parse(json!([{"key":"a", "title":"A", "meta":meta}])).is_ok(),
            accepted
        );
    }
    for (size, accepted) in [(1000, true), (1001, false)] {
        let items: Vec<_> = (0..size)
            .map(|i| json!({"key":i.to_string(), "title":"A"}))
            .collect();
        assert_eq!(parse(json!(items)).is_ok(), accepted);
    }
}

#[test]
fn wire_size_includes_whitespace() {
    let mut input = b"[]".to_vec();
    input.resize(MAX_SNAPSHOT_BYTES, b' ');
    assert!(parse_submission(&input).is_ok());
    input.push(b' ');
    assert!(parse_submission(&input).is_err());
}
