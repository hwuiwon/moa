# Recording Long-Conversation Transcripts

Recorded long-conversation scenarios replay provider calls deterministically in PR CI. Each transcript is JSONL:

1. The first line is metadata:
   `{"version":1,"scenario":"<name>","recorded_at":"<iso8601>","provider":"anthropic","model":"claude-sonnet-4"}`
2. Each following line is one provider call, not necessarily one user turn.
3. When the brain calls a tool and then asks the provider to continue the same turn, record another line with the same `user.text`. The smoke runner treats adjacent duplicate `user.text` records as provider continuations inside one user turn.

To re-record a scenario:

1. Stand up a local Postgres test database with AGE and pgvector.
2. Export the relevant live provider key locally. Do not commit secrets or shell history containing secrets.
3. Run the scenario with `MOA_RECORD_TRANSCRIPT=1` through the hosted Eval API or a dedicated ignored integration test.
4. Capture the provider stream with the recording wrapper and save it as `<scenario>/transcript.jsonl`.
5. Validate it with `moa_core::transcript::Transcript::read_jsonl`.
6. Run `cargo test -p moa-eval --test long_conversation_smoke_eval --locked -- --ignored`.

Recorded fixtures should preserve tool-call argument JSON and usage counters exactly. Redact request IDs, timestamps inside response bodies, UUIDs generated during a live run, and any credentials before committing.
