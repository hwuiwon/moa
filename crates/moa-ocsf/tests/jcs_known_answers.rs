//! Known-answer tests for JCS canonicalization.

use moa_ocsf::jcs::{JcsError, canonicalize};
use serde_json::json;

#[test]
fn jcs_known_answers() {
    // Pins: signing bytes do not depend on insertion order.
    let value = json!({ "z": 1, "a": 2, "m": 3 });

    let canonical = canonicalize(&value).expect("canonicalize object");

    assert_eq!(canonical, br#"{"a":2,"m":3,"z":1}"#);
}

#[test]
fn nested_objects_are_sorted_recursively() {
    // Pins: nested OCSF metadata is canonicalized before signing.
    let value = json!({ "outer": { "b": 1, "a": 2 }, "inner": [3, { "d": 4, "c": 5 }] });

    let canonical = canonicalize(&value).expect("canonicalize nested");

    assert_eq!(
        canonical,
        br#"{"inner":[3,{"c":5,"d":4}],"outer":{"a":2,"b":1}}"#
    );
}

#[test]
fn strings_use_minimal_json_escapes() {
    // Pins: canonical strings preserve Unicode while escaping control bytes.
    let value = json!({ "s": "€$\u{000f}\nA'B\"\\\\\"/" });

    let canonical = canonicalize(&value).expect("canonicalize string");

    assert_eq!(
        std::str::from_utf8(&canonical).expect("utf8"),
        "{\"s\":\"€$\\u000f\\nA'B\\\"\\\\\\\\\\\"/\"}"
    );
}

#[test]
fn utf16_key_order_matches_rfc_8785_sorting_rule() {
    // Pins: non-ASCII property ordering follows UTF-16 code units, not UTF-8.
    let value = json!({
        "\u{20ac}": "Euro Sign",
        "\r": "Carriage Return",
        "\u{1f600}": "Emoji: Grinning Face",
        "\u{0080}": "Control",
        "\u{00f6}": "Latin Small Letter O With Diaeresis",
        "1": "One"
    });

    let canonical = canonicalize(&value).expect("canonicalize unicode keys");
    let text = std::str::from_utf8(&canonical).expect("utf8");

    let positions = [
        text.find("Carriage Return").expect("carriage key"),
        text.find("One").expect("one key"),
        text.find("Control").expect("control key"),
        text.find("Latin Small").expect("latin key"),
        text.find("Euro Sign").expect("euro key"),
        text.find("Emoji").expect("emoji key"),
    ];
    assert_eq!(positions, {
        let mut sorted = positions;
        sorted.sort_unstable();
        sorted
    });
}

#[test]
fn rfc_8785_appendix_b_integer_sample_is_exact() {
    // Pins: RFC 8785 Appendix B maps IEEE 754 4340000000000000 to this integer.
    let value = json!({ "n": 9007199254740992_u64 });

    let canonical = canonicalize(&value).expect("canonicalize appendix b integer");

    assert_eq!(canonical, br#"{"n":9007199254740992}"#);
}

#[test]
fn floats_are_rejected_for_moa_event_payloads() {
    // Pins: unsupported number canonicalization cannot silently sign events.
    let value = json!({ "n": 4.5 });

    let error = canonicalize(&value).expect_err("floats are outside current scope");

    assert!(matches!(error, JcsError::FloatUnsupported));
}
