# Intentional Adversarial Fixture

This scenario intentionally includes prompt-injection text inside `transcript.jsonl`. The malicious text is a read-only fixture used to verify that MOA treats tool output as untrusted, blocks the attempted behavior change, and never executes the requested exfiltration command. The fixture contains no real credentials, secrets, or PII.
