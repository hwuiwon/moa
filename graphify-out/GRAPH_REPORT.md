# Graph Report - .  (2026-04-09)

## Corpus Check
- 31 files · ~19,073 words
- Verdict: corpus is large enough that graph structure adds value.

## Summary
- 410 nodes · 513 edges · 35 communities detected
- Extraction: 99% EXTRACTED · 1% INFERRED · 0% AMBIGUOUS · INFERRED: 4 edges (avg confidence: 0.73)
- Token cost: 4,500 input · 6,200 output

## God Nodes (most connected - your core abstractions)
1. `TursoSessionStore` - 13 edges
2. `Architecture Spec (docs/)` - 12 edges
3. `WorkingContext` - 10 edges
4. `AnthropicProvider` - 9 edges
5. `AnthropicStreamState` - 9 edges
6. `MockSessionStore` - 9 edges
7. `MockSessionStore` - 9 edges
8. `CompletionStream` - 8 edges
9. `MOA Platform` - 8 edges
10. `Event` - 7 edges

## Surprising Connections (you probably didn't know these)
- `Commentary: Corpus Fits Single Context Window` --conceptually_related_to--> `Graphify Integration for MOA`  [INFERRED]
  graphify-out/GRAPH_REPORT.md → AGENTS.md

## Hyperedges (group relationships)
- **MOA Architecture Specification Documents** — agents_doc_00_direction, agents_doc_01_architecture, agents_doc_02_brain, agents_doc_03_communication, agents_doc_04_memory, agents_doc_05_session_log, agents_doc_06_hands_mcp, agents_doc_07_context, agents_doc_08_security, agents_doc_09_skills, agents_doc_10_tech_stack [EXTRACTED 1.00]
- **MOA Rust Hygiene Rule Set** — agents_rule_error_handling, agents_rule_tracing, agents_rule_tokio_async, agents_rule_no_unwrap, agents_rule_clippy_fmt, agents_rule_doc_comments [EXTRACTED 1.00]
- **MOA Identifier & Serialization Conventions** — agents_convention_ids, agents_convention_timestamps, agents_convention_config_toml, agents_convention_json_value, agents_convention_paths, agents_convention_errors [EXTRACTED 1.00]

## Communities

### Community 0 - "Core Types & Messaging"
Cohesion: 0.04
Nodes (53): ActionButton, ApprovalDecision, ApprovalRequest, Attachment, ButtonStyle, ChannelRef, CompletionContent, CompletionResponse (+45 more)

### Community 1 - "Anthropic Provider"
Cohesion: 0.07
Nodes (30): anthropic_message(), AnthropicProvider, AnthropicStreamState, BlockAccumulator, build_request_body(), canonical_model_id(), capabilities_for_model(), completion_request_serializes_to_anthropic_format() (+22 more)

### Community 2 - "Brain & Completion IDs"
Cohesion: 0.08
Nodes (8): BrainId, CompletionRequest, MemoryPath, MessageId, session_id_roundtrip(), SessionId, UserId, WorkspaceId

### Community 3 - "Cloud & Config"
Cohesion: 0.09
Nodes (14): CloudConfig, CloudFlyioConfig, CloudHandsConfig, CloudTemporalConfig, config_loads_from_file(), default_config_is_valid(), GatewayConfig, GeneralConfig (+6 more)

### Community 4 - "Project Docs & Conventions"
Cohesion: 0.07
Nodes (28): Convention: TOML Config via config Crate, Convention: One thiserror Enum per Crate, Convention: Newtyped uuid::Uuid IDs, Convention: chrono DateTime<Utc> ISO 8601, docs/00-direction.md, docs/01-architecture-overview.md, docs/02-brain-orchestration.md, docs/03-communication-layer.md (+20 more)

### Community 5 - "Context Pipeline"
Cohesion: 0.13
Nodes (8): build_default_pipeline(), ContextPipeline, estimate_tokens(), load_history_events(), MockSessionStore, pipeline_runner_executes_stages_in_order(), PipelineStageReport, TestStage

### Community 6 - "Context Messages & Hands"
Cohesion: 0.1
Nodes (4): ContextMessage, estimate_text_tokens(), HandHandle, WorkingContext

### Community 7 - "Turso Session Store"
Cohesion: 0.16
Nodes (2): is_remote_url(), TursoSessionStore

### Community 8 - "Brain Turn Test Fixtures"
Cohesion: 0.15
Nodes (3): MockLlmProvider, MockSessionStore, run_brain_turn_emits_brain_response_event()

### Community 9 - "Core Errors & Traits"
Cohesion: 0.14
Nodes (10): MoaError, TurnResult, BrainOrchestrator, ContextProcessor, CredentialVault, HandProvider, LLMProvider, MemoryStore (+2 more)

### Community 10 - "Session Query Helpers"
Cohesion: 0.29
Nodes (9): event_record_from_row(), event_type_from_db(), optional_i64(), optional_text(), parse_timestamp(), platform_from_db(), session_meta_from_row(), session_status_from_db() (+1 more)

### Community 11 - "Event Types"
Cohesion: 0.18
Nodes (1): Event

### Community 12 - "History Processor"
Cohesion: 0.25
Nodes (4): capabilities(), history_compiler_formats_user_and_assistant_turns(), history_processor_uses_preloaded_events(), HistoryCompiler

### Community 13 - "Session Store Tests"
Cohesion: 0.2
Nodes (0): 

### Community 14 - "Completion Stream"
Cohesion: 0.32
Nodes (1): CompletionStream

### Community 15 - "Instruction Processor"
Cohesion: 0.38
Nodes (2): instruction_processor_appends_config_backed_sections(), InstructionProcessor

### Community 16 - "Tool Definition Processor"
Cohesion: 0.38
Nodes (2): tool_processor_serializes_tool_schemas(), ToolDefinitionProcessor

### Community 17 - "Cache Optimizer"
Cohesion: 0.4
Nodes (2): cache_optimizer_validates_cache_breakpoint(), CacheOptimizer

### Community 18 - "Memory Retriever"
Cohesion: 0.4
Nodes (2): memory_retriever_is_a_no_op(), MemoryRetriever

### Community 19 - "Skill Injector"
Cohesion: 0.4
Nodes (2): skill_injector_marks_cache_breakpoint(), SkillInjector

### Community 20 - "Identity Processor"
Cohesion: 0.4
Nodes (2): identity_processor_appends_system_prompt(), IdentityProcessor

### Community 21 - "Chat Harness Example"
Cohesion: 0.83
Nodes (3): main(), resolve_session_db_path(), run_prompt()

### Community 22 - "Anthropic Mock Tests"
Cohesion: 0.67
Nodes (0): 

### Community 23 - "TUI Main"
Cohesion: 1.0
Nodes (0): 

### Community 24 - "Anthropic Live Test"
Cohesion: 1.0
Nodes (0): 

### Community 25 - "Live Brain Harness Test"
Cohesion: 1.0
Nodes (0): 

### Community 26 - "Context Compaction"
Cohesion: 1.0
Nodes (0): 

### Community 27 - "Rule: Test Layout"
Cohesion: 1.0
Nodes (1): Rule: Tests in tests/ or #[cfg(test)]

### Community 28 - "Rule: Clippy & Fmt"
Cohesion: 1.0
Nodes (1): Rule: Run clippy and fmt Before Done

### Community 29 - "Convention: JSON Value"
Cohesion: 1.0
Nodes (1): Convention: serde_json::Value for Dynamic Payloads

### Community 30 - "Convention: Paths"
Cohesion: 1.0
Nodes (1): Convention: PathBuf for FS, String for Logical Paths

### Community 31 - "Q: WorkingContext Bridge"
Cohesion: 1.0
Nodes (1): Question: Why WorkingContext Bridges Communities

### Community 32 - "Q: CompletionStream Bridge"
Cohesion: 1.0
Nodes (1): Question: Why CompletionStream Bridges Communities

### Community 33 - "Q: Isolated Nodes"
Cohesion: 1.0
Nodes (1): Question: 75 Isolated Nodes Gap (Platform, SessionStatus, etc.)

### Community 34 - "Q: Split Community 0"
Cohesion: 1.0
Nodes (1): Question: Split Weakly-Cohesive Community 0

## Knowledge Gaps
- **104 isolated node(s):** `BrainOrchestrator`, `SessionStore`, `HandProvider`, `LLMProvider`, `PlatformAdapter` (+99 more)
  These have ≤1 connection - possible missing edges or undocumented components.
- **Thin community `TUI Main`** (2 nodes): `main.rs`, `main()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Anthropic Live Test`** (2 nodes): `anthropic_live.rs`, `anthropic_live_completion_returns_expected_answer()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Live Brain Harness Test`** (2 nodes): `live_harness.rs`, `live_brain_turn_returns_brain_response()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Context Compaction`** (2 nodes): `compaction.rs`, `maybe_compact()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Rule: Test Layout`** (1 nodes): `Rule: Tests in tests/ or #[cfg(test)]`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Rule: Clippy & Fmt`** (1 nodes): `Rule: Run clippy and fmt Before Done`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Convention: JSON Value`** (1 nodes): `Convention: serde_json::Value for Dynamic Payloads`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Convention: Paths`** (1 nodes): `Convention: PathBuf for FS, String for Logical Paths`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Q: WorkingContext Bridge`** (1 nodes): `Question: Why WorkingContext Bridges Communities`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Q: CompletionStream Bridge`** (1 nodes): `Question: Why CompletionStream Bridges Communities`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Q: Isolated Nodes`** (1 nodes): `Question: 75 Isolated Nodes Gap (Platform, SessionStatus, etc.)`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Q: Split Community 0`** (1 nodes): `Question: Split Weakly-Cohesive Community 0`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **Why does `WorkingContext` connect `Context Messages & Hands` to `Core Types & Messaging`, `Brain & Completion IDs`?**
  _High betweenness centrality (0.015) - this node is a cross-community bridge._
- **Why does `CompletionStream` connect `Completion Stream` to `Core Types & Messaging`?**
  _High betweenness centrality (0.014) - this node is a cross-community bridge._
- **What connects `BrainOrchestrator`, `SessionStore`, `HandProvider` to the rest of the system?**
  _104 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `Core Types & Messaging` be split into smaller, more focused modules?**
  _Cohesion score 0.04 - nodes in this community are weakly interconnected._
- **Should `Anthropic Provider` be split into smaller, more focused modules?**
  _Cohesion score 0.07 - nodes in this community are weakly interconnected._
- **Should `Brain & Completion IDs` be split into smaller, more focused modules?**
  _Cohesion score 0.08 - nodes in this community are weakly interconnected._
- **Should `Cloud & Config` be split into smaller, more focused modules?**
  _Cohesion score 0.09 - nodes in this community are weakly interconnected._