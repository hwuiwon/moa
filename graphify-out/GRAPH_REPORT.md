# Graph Report - .  (2026-04-09)

## Corpus Check
- Corpus is ~7,713 words - fits in a single context window. You may not need a graph.

## Summary
- 184 nodes · 204 edges · 18 communities detected
- Extraction: 97% EXTRACTED · 3% INFERRED · 0% AMBIGUOUS · INFERRED: 6 edges (avg confidence: 0.73)
- Token cost: 0 input · 0 output

## God Nodes (most connected - your core abstractions)
1. `docs/ Specification Directory` - 12 edges
2. `Event` - 7 edges
3. `HandHandle` - 6 edges
4. `UserId` - 5 edges
5. `WorkspaceId` - 5 edges
6. `MemoryPath` - 5 edges
7. `MOA Platform` - 5 edges
8. `SessionId` - 4 edges
9. `BrainId` - 4 edges
10. `MessageId` - 4 edges

## Surprising Connections (you probably didn't know these)
- None detected - all connections are within the same source files.

## Hyperedges (group relationships)
- **MOA Rust Quality Gates** — agentsmd_rule_clippy_fmt, agentsmd_rule_no_unwrap, agentsmd_rule_doc_comments, agentsmd_rule_tests_layout [INFERRED 0.80]
- **MOA Error Handling Stack** — agentsmd_rule_thiserror_anyhow, agentsmd_rule_no_unwrap, agentsmd_convention_errors [INFERRED 0.85]
- **MOA Async/Observability Runtime** — agentsmd_rule_tokio, agentsmd_rule_tracing, agentsmd_moa_platform [INFERRED 0.75]

## Communities

### Community 0 - "Core Domain Types"
Cohesion: 0.04
Nodes (52): ActionButton, ApprovalDecision, ApprovalRequest, Attachment, ButtonStyle, ChannelRef, CompletionContent, CompletionStream (+44 more)

### Community 1 - "Config & Cloud Settings"
Cohesion: 0.09
Nodes (14): CloudConfig, CloudFlyioConfig, CloudHandsConfig, CloudTemporalConfig, config_loads_from_file(), default_config_is_valid(), GatewayConfig, GeneralConfig (+6 more)

### Community 2 - "Sessions & Memory Paths"
Cohesion: 0.11
Nodes (6): CompletionRequest, MemoryPath, session_id_roundtrip(), SessionId, UserId, WorkspaceId

### Community 3 - "Specification Docs"
Cohesion: 0.11
Nodes (18): docs/00-direction.md — Product Identity, docs/01-architecture-overview.md — Architecture Overview, docs/02-brain-orchestration.md — Brain Orchestration, docs/03-communication-layer.md — Communication Layer, docs/04-memory-architecture.md — Memory Architecture, docs/05-session-event-log.md — Session Event Log, docs/06-hands-and-mcp.md — Hands and MCP, docs/07-context-pipeline.md — Context Pipeline (+10 more)

### Community 4 - "Core Traits & Errors"
Cohesion: 0.18
Nodes (9): MoaError, BrainOrchestrator, ContextProcessor, CredentialVault, HandProvider, LLMProvider, MemoryStore, PlatformAdapter (+1 more)

### Community 5 - "Event System"
Cohesion: 0.18
Nodes (1): Event

### Community 6 - "Brain IDs & Event Ranges"
Cohesion: 0.33
Nodes (2): BrainId, EventRange

### Community 7 - "Hand Handle Variants"
Cohesion: 0.29
Nodes (1): HandHandle

### Community 8 - "Error Handling Conventions"
Cohesion: 0.4
Nodes (5): Per-Crate Error Enum Convention, moa-cli Binary Crate, moa-tui Binary Crate, No unwrap() in Library Code, thiserror for libraries, anyhow for binaries

### Community 9 - "Message IDs"
Cohesion: 0.5
Nodes (1): MessageId

### Community 10 - "Context Messages"
Cohesion: 0.5
Nodes (1): ContextMessage

### Community 11 - "Binary Entrypoint"
Cohesion: 1.0
Nodes (0): 

### Community 12 - "Async Runtime & Logging"
Cohesion: 1.0
Nodes (2): Tokio Async Runtime, Use tracing for Logging

### Community 13 - "ID Newtype Convention"
Cohesion: 1.0
Nodes (1): ID Newtype Convention

### Community 14 - "Timestamp Convention"
Cohesion: 1.0
Nodes (1): Timestamp Convention (chrono ISO 8601)

### Community 15 - "TOML Config Convention"
Cohesion: 1.0
Nodes (1): TOML Config Convention

### Community 16 - "Dynamic JSON Payloads"
Cohesion: 1.0
Nodes (1): serde_json::Value Dynamic Payloads

### Community 17 - "Path Convention"
Cohesion: 1.0
Nodes (1): Path Convention

## Knowledge Gaps
- **86 isolated node(s):** `Platform`, `SessionStatus`, `SandboxTier`, `RiskLevel`, `ApprovalDecision` (+81 more)
  These have ≤1 connection - possible missing edges or undocumented components.
- **Thin community `Binary Entrypoint`** (2 nodes): `main.rs`, `main()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Async Runtime & Logging`** (2 nodes): `Tokio Async Runtime`, `Use tracing for Logging`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `ID Newtype Convention`** (1 nodes): `ID Newtype Convention`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Timestamp Convention`** (1 nodes): `Timestamp Convention (chrono ISO 8601)`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `TOML Config Convention`** (1 nodes): `TOML Config Convention`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Dynamic JSON Payloads`** (1 nodes): `serde_json::Value Dynamic Payloads`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Path Convention`** (1 nodes): `Path Convention`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **Why does `HandHandle` connect `Hand Handle Variants` to `Core Domain Types`?**
  _High betweenness centrality (0.044) - this node is a cross-community bridge._
- **Why does `UserId` connect `Sessions & Memory Paths` to `Core Domain Types`?**
  _High betweenness centrality (0.034) - this node is a cross-community bridge._
- **What connects `Platform`, `SessionStatus`, `SandboxTier` to the rest of the system?**
  _86 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `Core Domain Types` be split into smaller, more focused modules?**
  _Cohesion score 0.04 - nodes in this community are weakly interconnected._
- **Should `Config & Cloud Settings` be split into smaller, more focused modules?**
  _Cohesion score 0.09 - nodes in this community are weakly interconnected._
- **Should `Sessions & Memory Paths` be split into smaller, more focused modules?**
  _Cohesion score 0.11 - nodes in this community are weakly interconnected._
- **Should `Specification Docs` be split into smaller, more focused modules?**
  _Cohesion score 0.11 - nodes in this community are weakly interconnected._