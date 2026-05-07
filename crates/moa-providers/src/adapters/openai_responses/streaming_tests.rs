//! OpenAI Responses streaming unit tests.

mod ignorable_error_tests {
    use async_openai::error::OpenAIError;

    use super::super::is_ignorable_openai_stream_error;

    #[test]
    fn web_search_call_output_item_is_ignorable() {
        // Build a JSONDeserialize error by deliberately failing to
        // deserialize a payload with the web_search_call shape.
        let payload =
            r#"{"type":"response.output_item.added","item":{"type":"web_search_call","id":"x"}}"#;
        let serde_err: serde_json::Error =
            serde_json::from_str::<i32>(payload).expect_err("must fail");
        let err = OpenAIError::JSONDeserialize(serde_err, payload.to_string());
        assert!(is_ignorable_openai_stream_error(&err));
    }

    #[test]
    fn allow_listed_field_in_deserialize_content_is_ignorable() {
        // Real-world shape: the serde error alone doesn't mention the
        // field, but the chunk content does — we match on the content
        // as a second heuristic.
        let payload = r#"{"compatibility": {"foo": "bar"}}"#;
        // Fabricate a cheap serde error for the outer wrapper.
        let serde_err = serde_json::from_str::<i32>(payload).expect_err("must fail");
        let err = OpenAIError::JSONDeserialize(serde_err, payload.to_string());
        assert!(
            is_ignorable_openai_stream_error(&err),
            "compatibility-field chunks must be ignorable"
        );
    }

    #[test]
    fn invalid_argument_with_allow_listed_field_is_ignorable() {
        // Mirrors the exact error the user hit: async-openai's
        // path-aware string surfaces as InvalidArgument.
        let msg =
            "compatibility: invalid type: map, expected a string at line 4 column 3".to_string();
        let err = OpenAIError::InvalidArgument(msg);
        assert!(is_ignorable_openai_stream_error(&err));
    }

    #[test]
    fn rate_limit_update_event_is_ignorable() {
        let payload = r#"{"type":"response.rate_limits.updated","rate_limits":{"remaining_requests":"14999"}}"#;
        let serde_err = serde_json::from_str::<i32>(payload).expect_err("must fail");
        let err = OpenAIError::JSONDeserialize(serde_err, payload.to_string());
        assert!(is_ignorable_openai_stream_error(&err));
    }

    #[test]
    fn unrelated_deserialize_error_is_not_ignorable() {
        let payload = r#"{"foo":"bar"}"#;
        let serde_err = serde_json::from_str::<i32>(payload).expect_err("must fail");
        let err = OpenAIError::JSONDeserialize(serde_err, payload.to_string());
        assert!(!is_ignorable_openai_stream_error(&err));
    }
}
