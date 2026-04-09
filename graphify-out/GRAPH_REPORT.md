# Graph Report - .  (2026-04-09)

## Corpus Check
- 37 files · ~18,583 words
- Verdict: corpus is large enough that graph structure adds value.

## Summary
- 368 nodes · 481 edges · 25 communities detected
- Extraction: 100% EXTRACTED · 0% INFERRED · 0% AMBIGUOUS
- Token cost: 0 input · 0 output

## God Nodes (most connected - your core abstractions)
1. `TursoSessionStore` - 13 edges
2. `WorkingContext` - 10 edges
3. `AnthropicProvider` - 9 edges
4. `AnthropicStreamState` - 9 edges
5. `MockSessionStore` - 9 edges
6. `MockSessionStore` - 9 edges
7. `CompletionStream` - 8 edges
8. `Event` - 7 edges
9. `HandHandle` - 6 edges
10. `session_meta_from_row()` - 6 edges

## Surprising Connections (you probably didn't know these)
- None detected - all connections are within the same source files.

## Communities

### Community 0 - "Community 0"
Cohesion: 0.04
Nodes (53): ActionButton, ApprovalDecision, ApprovalRequest, Attachment, ButtonStyle, ChannelRef, CompletionContent, CompletionResponse (+45 more)

### Community 1 - "Community 1"
Cohesion: 0.08
Nodes (25): anthropic_message(), AnthropicProvider, AnthropicStreamState, BlockAccumulator, build_request_body(), canonical_model_id(), capabilities_for_model(), completion_request_serializes_to_anthropic_format() (+17 more)

### Community 2 - "Community 2"
Cohesion: 0.08
Nodes (8): BrainId, CompletionRequest, MemoryPath, MessageId, session_id_roundtrip(), SessionId, UserId, WorkspaceId

### Community 3 - "Community 3"
Cohesion: 0.09
Nodes (14): CloudConfig, CloudFlyioConfig, CloudHandsConfig, CloudTemporalConfig, config_loads_from_file(), default_config_is_valid(), GatewayConfig, GeneralConfig (+6 more)

### Community 4 - "Community 4"
Cohesion: 0.13
Nodes (8): build_default_pipeline(), ContextPipeline, estimate_tokens(), load_history_events(), MockSessionStore, pipeline_runner_executes_stages_in_order(), PipelineStageReport, TestStage

### Community 5 - "Community 5"
Cohesion: 0.1
Nodes (4): ContextMessage, estimate_text_tokens(), HandHandle, WorkingContext

### Community 6 - "Community 6"
Cohesion: 0.16
Nodes (2): is_remote_url(), TursoSessionStore

### Community 7 - "Community 7"
Cohesion: 0.15
Nodes (3): MockLlmProvider, MockSessionStore, run_brain_turn_emits_brain_response_event()

### Community 8 - "Community 8"
Cohesion: 0.14
Nodes (10): MoaError, TurnResult, BrainOrchestrator, ContextProcessor, CredentialVault, HandProvider, LLMProvider, MemoryStore (+2 more)

### Community 9 - "Community 9"
Cohesion: 0.29
Nodes (9): event_record_from_row(), event_type_from_db(), optional_i64(), optional_text(), parse_timestamp(), platform_from_db(), session_meta_from_row(), session_status_from_db() (+1 more)

### Community 10 - "Community 10"
Cohesion: 0.18
Nodes (1): Event

### Community 11 - "Community 11"
Cohesion: 0.25
Nodes (4): capabilities(), history_compiler_formats_user_and_assistant_turns(), history_processor_uses_preloaded_events(), HistoryCompiler

### Community 12 - "Community 12"
Cohesion: 0.2
Nodes (0): 

### Community 13 - "Community 13"
Cohesion: 0.32
Nodes (1): CompletionStream

### Community 14 - "Community 14"
Cohesion: 0.48
Nodes (5): build_http_client(), response_text(), retries_on_rate_limit(), retry_delay(), send_with_retry()

### Community 15 - "Community 15"
Cohesion: 0.38
Nodes (2): instruction_processor_appends_config_backed_sections(), InstructionProcessor

### Community 16 - "Community 16"
Cohesion: 0.38
Nodes (2): tool_processor_serializes_tool_schemas(), ToolDefinitionProcessor

### Community 17 - "Community 17"
Cohesion: 0.4
Nodes (2): cache_optimizer_validates_cache_breakpoint(), CacheOptimizer

### Community 18 - "Community 18"
Cohesion: 0.4
Nodes (2): memory_retriever_is_a_no_op(), MemoryRetriever

### Community 19 - "Community 19"
Cohesion: 0.4
Nodes (2): skill_injector_marks_cache_breakpoint(), SkillInjector

### Community 20 - "Community 20"
Cohesion: 0.4
Nodes (2): identity_processor_appends_system_prompt(), IdentityProcessor

### Community 21 - "Community 21"
Cohesion: 0.67
Nodes (0): 

### Community 22 - "Community 22"
Cohesion: 1.0
Nodes (0): 

### Community 23 - "Community 23"
Cohesion: 1.0
Nodes (0): 

### Community 24 - "Community 24"
Cohesion: 1.0
Nodes (0): 

## Knowledge Gaps
- **75 isolated node(s):** `Platform`, `SessionStatus`, `SandboxTier`, `RiskLevel`, `ApprovalDecision` (+70 more)
  These have ≤1 connection - possible missing edges or undocumented components.
- **Thin community `Community 22`** (2 nodes): `main.rs`, `main()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Community 23`** (2 nodes): `anthropic_live.rs`, `anthropic_live_completion_returns_expected_answer()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Community 24`** (2 nodes): `compaction.rs`, `maybe_compact()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **Why does `WorkingContext` connect `Community 5` to `Community 0`, `Community 2`?**
  _High betweenness centrality (0.019) - this node is a cross-community bridge._
- **Why does `CompletionStream` connect `Community 13` to `Community 0`?**
  _High betweenness centrality (0.017) - this node is a cross-community bridge._
- **What connects `Platform`, `SessionStatus`, `SandboxTier` to the rest of the system?**
  _75 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `Community 0` be split into smaller, more focused modules?**
  _Cohesion score 0.04 - nodes in this community are weakly interconnected._
- **Should `Community 1` be split into smaller, more focused modules?**
  _Cohesion score 0.08 - nodes in this community are weakly interconnected._
- **Should `Community 2` be split into smaller, more focused modules?**
  _Cohesion score 0.08 - nodes in this community are weakly interconnected._
- **Should `Community 3` be split into smaller, more focused modules?**
  _Cohesion score 0.09 - nodes in this community are weakly interconnected._