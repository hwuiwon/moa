//! Offline boundary checks for the `SessionStore` Restate facade.

const EDGE_ROUTES: &str = include_str!("../../../crates/moa-edge/src/routes.rs");

#[test]
fn session_store_mutators_are_not_public_edge_routes() {
    // Pins: public callers enter through typed edge routes, not raw session mutation handlers.
    assert!(
        EDGE_ROUTES.contains("/SessionStore/create_agent_session"),
        "control check: edge routes should include the configured-agent session admission route"
    );

    for handler in ["append_event", "update_status"] {
        let route = format!("/SessionStore/{handler}");
        assert!(
            !EDGE_ROUTES.contains(&route),
            "public edge routes must not expose {route} as a mutation endpoint"
        );
    }
}
