//! Scripted-provider fixture defaults for orchestrator service tests.

pub(super) fn default_script() -> Vec<u8> {
    br#"{"default":{"completion":{"content":"ok","duration_ms":1,"input_tokens":64,"cached_input_tokens":0,"cache_write_input_tokens":0,"tool_calls":[]}}}"#.to_vec()
}
