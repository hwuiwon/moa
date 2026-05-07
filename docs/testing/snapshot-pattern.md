# Snapshot Testing Pattern

## When To Use A Snapshot Test

Use snapshots when the exact rendered output is the contract: provider wire-format request bodies, byte-stable prompt prefixes, rendered UI strings, and public error message formatting. Do not snapshot internal struct layouts, incidental debug output, or data that is easier to assert with targeted semantic checks.

## How To Add A New Snapshot

Keep fixture inputs fixed, small, and named with constants. Route through the same formatter or provider request builder used in production, then snapshot with a stable name in the form `<area>__<scenario>`.

```rust
const SYSTEM_PROMPT: &str = "You are MOA.";

#[test]
fn provider_request_body__minimal_request_serializes_with_stable_byte_layout() {
    let body = build_provider_request_body(SYSTEM_PROMPT)
        .expect("request body should build");

    insta::assert_json_snapshot!("provider_request_body__minimal_request", body, {
        ".metadata.request_id" => "[redacted]",
        ".timestamp" => "[redacted]"
    });
}
```

Use redactions only for genuinely nondeterministic fields such as IDs, timestamps, or provider-generated cache names. If the test can avoid nondeterminism by using fixed inputs, prefer that over redacting.

## Reviewing Snapshot Diffs

Run the failing test locally, then inspect pending updates with `cargo insta review`. A good diff has an intentional code change next to an expected wire-format or rendering change. A suspicious diff is only key reordering, whitespace churn, a changed cache marker location, or an unexpected new ID/timestamp; fix the determinism issue instead of accepting that snapshot.
