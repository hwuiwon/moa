# Graph Report - .  (2026-04-09)

## Corpus Check
- 26 files · ~14,595 words
- Verdict: corpus is large enough that graph structure adds value.

## Summary
- 263 nodes · 353 edges · 16 communities detected
- Extraction: 100% EXTRACTED · 0% INFERRED · 0% AMBIGUOUS
- Token cost: 0 input · 0 output

## God Nodes (most connected - your core abstractions)
1. `TursoSessionStore` - 13 edges
2. `AnthropicProvider` - 9 edges
3. `AnthropicStreamState` - 9 edges
4. `CompletionStream` - 8 edges
5. `Event` - 7 edges
6. `HandHandle` - 6 edges
7. `session_meta_from_row()` - 6 edges
8. `consume_sse_events()` - 6 edges
9. `UserId` - 5 edges
10. `WorkspaceId` - 5 edges

## Surprising Connections (you probably didn't know these)
- None detected - all connections are within the same source files.

## Communities

### Community 0 - "Community 0"
Cohesion: 0.04
Nodes (52): ActionButton, ApprovalDecision, ApprovalRequest, Attachment, ButtonStyle, ChannelRef, CompletionContent, CompletionResponse (+44 more)

### Community 1 - "Community 1"
Cohesion: 0.08
Nodes (25): anthropic_message(), AnthropicProvider, AnthropicStreamState, BlockAccumulator, build_request_body(), canonical_model_id(), capabilities_for_model(), completion_request_serializes_to_anthropic_format() (+17 more)

### Community 2 - "Community 2"
Cohesion: 0.08
Nodes (15): CloudConfig, CloudFlyioConfig, CloudHandsConfig, CloudTemporalConfig, config_loads_from_file(), default_config_is_valid(), GatewayConfig, GeneralConfig (+7 more)

### Community 3 - "Community 3"
Cohesion: 0.08
Nodes (8): BrainId, CompletionRequest, MemoryPath, MessageId, session_id_roundtrip(), SessionId, UserId, WorkspaceId

### Community 4 - "Community 4"
Cohesion: 0.16
Nodes (2): is_remote_url(), TursoSessionStore

### Community 5 - "Community 5"
Cohesion: 0.29
Nodes (9): event_record_from_row(), event_type_from_db(), optional_i64(), optional_text(), parse_timestamp(), platform_from_db(), session_meta_from_row(), session_status_from_db() (+1 more)

### Community 6 - "Community 6"
Cohesion: 0.17
Nodes (3): EventRange, HandHandle, SessionMeta

### Community 7 - "Community 7"
Cohesion: 0.18
Nodes (1): Event

### Community 8 - "Community 8"
Cohesion: 0.2
Nodes (0): 

### Community 9 - "Community 9"
Cohesion: 0.22
Nodes (8): BrainOrchestrator, ContextProcessor, CredentialVault, HandProvider, LLMProvider, MemoryStore, PlatformAdapter, SessionStore

### Community 10 - "Community 10"
Cohesion: 0.32
Nodes (1): CompletionStream

### Community 11 - "Community 11"
Cohesion: 0.48
Nodes (5): build_http_client(), response_text(), retries_on_rate_limit(), retry_delay(), send_with_retry()

### Community 12 - "Community 12"
Cohesion: 0.5
Nodes (1): ContextMessage

### Community 13 - "Community 13"
Cohesion: 0.67
Nodes (0): 

### Community 14 - "Community 14"
Cohesion: 1.0
Nodes (0): 

### Community 15 - "Community 15"
Cohesion: 1.0
Nodes (0): 

## Knowledge Gaps
- **74 isolated node(s):** `Platform`, `SessionStatus`, `SandboxTier`, `RiskLevel`, `ApprovalDecision` (+69 more)
  These have ≤1 connection - possible missing edges or undocumented components.
- **Thin community `Community 14`** (2 nodes): `main.rs`, `main()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Community 15`** (2 nodes): `anthropic_live.rs`, `anthropic_live_completion_returns_expected_answer()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **Why does `CompletionStream` connect `Community 10` to `Community 0`?**
  _High betweenness centrality (0.031) - this node is a cross-community bridge._
- **Why does `HandHandle` connect `Community 6` to `Community 0`?**
  _High betweenness centrality (0.023) - this node is a cross-community bridge._
- **What connects `Platform`, `SessionStatus`, `SandboxTier` to the rest of the system?**
  _74 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `Community 0` be split into smaller, more focused modules?**
  _Cohesion score 0.04 - nodes in this community are weakly interconnected._
- **Should `Community 1` be split into smaller, more focused modules?**
  _Cohesion score 0.08 - nodes in this community are weakly interconnected._
- **Should `Community 2` be split into smaller, more focused modules?**
  _Cohesion score 0.08 - nodes in this community are weakly interconnected._
- **Should `Community 3` be split into smaller, more focused modules?**
  _Cohesion score 0.08 - nodes in this community are weakly interconnected._