# Graph Report - .  (2026-04-09)

## Corpus Check
- 22 files · ~11,801 words
- Verdict: corpus is large enough that graph structure adds value.

## Summary
- 199 nodes · 248 edges · 11 communities detected
- Extraction: 100% EXTRACTED · 0% INFERRED · 0% AMBIGUOUS
- Token cost: 0 input · 0 output

## God Nodes (most connected - your core abstractions)
1. `TursoSessionStore` - 13 edges
2. `Event` - 7 edges
3. `HandHandle` - 6 edges
4. `session_meta_from_row()` - 6 edges
5. `UserId` - 5 edges
6. `WorkspaceId` - 5 edges
7. `MemoryPath` - 5 edges
8. `session_summary_from_row()` - 5 edges
9. `event_record_from_row()` - 5 edges
10. `SessionId` - 4 edges

## Surprising Connections (you probably didn't know these)
- None detected - all connections are within the same source files.

## Communities

### Community 0 - "Community 0"
Cohesion: 0.04
Nodes (52): ActionButton, ApprovalDecision, ApprovalRequest, Attachment, ButtonStyle, ChannelRef, CompletionContent, CompletionStream (+44 more)

### Community 1 - "Community 1"
Cohesion: 0.07
Nodes (10): BrainId, CompletionRequest, EventRange, MemoryPath, MessageId, session_id_roundtrip(), SessionId, SessionMeta (+2 more)

### Community 2 - "Community 2"
Cohesion: 0.09
Nodes (14): CloudConfig, CloudFlyioConfig, CloudHandsConfig, CloudTemporalConfig, config_loads_from_file(), default_config_is_valid(), GatewayConfig, GeneralConfig (+6 more)

### Community 3 - "Community 3"
Cohesion: 0.16
Nodes (2): is_remote_url(), TursoSessionStore

### Community 4 - "Community 4"
Cohesion: 0.29
Nodes (9): event_record_from_row(), event_type_from_db(), optional_i64(), optional_text(), parse_timestamp(), platform_from_db(), session_meta_from_row(), session_status_from_db() (+1 more)

### Community 5 - "Community 5"
Cohesion: 0.18
Nodes (9): MoaError, BrainOrchestrator, ContextProcessor, CredentialVault, HandProvider, LLMProvider, MemoryStore, PlatformAdapter (+1 more)

### Community 6 - "Community 6"
Cohesion: 0.18
Nodes (1): Event

### Community 7 - "Community 7"
Cohesion: 0.2
Nodes (0): 

### Community 8 - "Community 8"
Cohesion: 0.29
Nodes (1): HandHandle

### Community 9 - "Community 9"
Cohesion: 0.5
Nodes (1): ContextMessage

### Community 10 - "Community 10"
Cohesion: 1.0
Nodes (0): 

## Knowledge Gaps
- **61 isolated node(s):** `Platform`, `SessionStatus`, `SandboxTier`, `RiskLevel`, `ApprovalDecision` (+56 more)
  These have ≤1 connection - possible missing edges or undocumented components.
- **Thin community `Community 10`** (2 nodes): `main.rs`, `main()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **Why does `HandHandle` connect `Community 8` to `Community 0`?**
  _High betweenness centrality (0.038) - this node is a cross-community bridge._
- **Why does `UserId` connect `Community 1` to `Community 0`?**
  _High betweenness centrality (0.029) - this node is a cross-community bridge._
- **What connects `Platform`, `SessionStatus`, `SandboxTier` to the rest of the system?**
  _61 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `Community 0` be split into smaller, more focused modules?**
  _Cohesion score 0.04 - nodes in this community are weakly interconnected._
- **Should `Community 1` be split into smaller, more focused modules?**
  _Cohesion score 0.07 - nodes in this community are weakly interconnected._
- **Should `Community 2` be split into smaller, more focused modules?**
  _Cohesion score 0.09 - nodes in this community are weakly interconnected._