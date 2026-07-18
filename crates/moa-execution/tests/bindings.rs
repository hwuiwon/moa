use std::collections::{BTreeMap, BTreeSet};

use moa_execution::bindings::{
    BindingContext, encode_map_key, extract_map_key, resolve_bindings, resolve_reference,
};
use serde_json::json;

#[test]
fn bindings_replace_whole_values_and_enforce_dependency_visibility() {
    // Pins: bindings resolve only whole-value references to run input or direct dependency output.
    let run_input = json!({ "query": "damaged order" });
    let outputs = BTreeMap::from([
        ("visible".to_string(), json!({ "items": [1, 2] })),
        ("hidden".to_string(), json!({ "secret": true })),
    ]);
    let dependencies = BTreeSet::from(["visible".to_string()]);
    let context = BindingContext {
        run_input: &run_input,
        node_outputs: &outputs,
        dependencies: &dependencies,
        item: None,
        item_key: None,
    };

    assert_eq!(
        resolve_bindings(
            &json!({
                "query": { "$ref": "$.input.query" },
                "items": { "$ref": "$.nodes.visible.output.items" }
            }),
            &context,
        )
        .expect("resolve visible values"),
        json!({ "query": "damaged order", "items": [1, 2] })
    );
    assert!(
        resolve_reference("$.nodes.hidden.output.secret", &context).is_err(),
        "non-dependency output must be inaccessible"
    );
    assert!(
        resolve_bindings(&json!({ "$ref": "$.input.query", "suffix": "x" }), &context).is_err(),
        "binding objects cannot interpolate or carry fallback fields"
    );
}

#[test]
fn map_variables_exist_only_inside_a_map_context() {
    // Pins: $item and $item_key resolve as complete values without expression evaluation.
    let run_input = json!({});
    let outputs = BTreeMap::new();
    let dependencies = BTreeSet::new();
    let item = json!({ "id": 42 });
    let context = BindingContext {
        run_input: &run_input,
        node_outputs: &outputs,
        dependencies: &dependencies,
        item: Some(&item),
        item_key: Some("number:42"),
    };
    assert_eq!(
        resolve_bindings(
            &json!({ "item": { "$item": true }, "key": { "$item_key": true } }),
            &context,
        )
        .expect("resolve map variables"),
        json!({ "item": { "id": 42 }, "key": "number:42" })
    );

    let outside = BindingContext {
        item: None,
        item_key: None,
        ..context
    };
    assert!(resolve_bindings(&json!({ "$item": true }), &outside).is_err());
}

#[test]
fn map_keys_are_type_prefixed_canonical_and_size_bounded() {
    // Pins: typed canonical encoding prevents cross-type and object-order collisions.
    assert_eq!(encode_map_key(&json!(null)).expect("null key"), "null:");
    assert_eq!(encode_map_key(&json!(true)).expect("bool key"), "bool:true");
    assert_eq!(encode_map_key(&json!(1)).expect("number key"), "number:1");
    assert_eq!(
        encode_map_key(&json!("1")).expect("string key"),
        "string:\"1\""
    );
    assert_eq!(
        encode_map_key(&json!({ "b": 2, "a": 1 })).expect("object key"),
        "object:{\"a\":1,\"b\":2}"
    );
    assert_eq!(
        extract_map_key(&json!({ "id": [1, 2] }), "/id").expect("array key"),
        "array:[1,2]"
    );
    assert!(
        encode_map_key(&json!("x".repeat(1_025))).is_err(),
        "encoded UTF-8 map key must not exceed 1,024 bytes"
    );
}
