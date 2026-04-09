# Graph Report - .  (2026-04-09)

## Corpus Check
- Corpus is ~23,449 words - fits in a single context window. You may not need a graph.

## Summary
- 505 nodes · 698 edges · 31 communities detected
- Extraction: 98% EXTRACTED · 2% INFERRED · 0% AMBIGUOUS · INFERRED: 15 edges (avg confidence: 0.79)
- Token cost: 75,585 input · 7,500 output

## God Nodes (most connected - your core abstractions)
1. `FileMemoryStore` - 18 edges
2. `TursoSessionStore` - 13 edges
3. `WorkingContext` - 10 edges
4. `AnthropicProvider` - 9 edges
5. `AnthropicStreamState` - 9 edges
6. `MockSessionStore` - 9 edges
7. `MockSessionStore` - 9 edges
8. `FileMemoryStore` - 9 edges
9. `scope_key()` - 8 edges
10. `parse_markdown()` - 8 edges

## Surprising Connections (you probably didn't know these)
- `FtsIndex::search (BM25 + recency boost)` --semantically_similar_to--> `extract_search_keywords (stopword filter)`  [INFERRED] [semantically similar]
  moa-memory/src/fts.rs → moa-brain/src/pipeline/memory.rs
- `ContextPipeline` --rationale_for--> `docs/07-context-pipeline.md reference`  [INFERRED]
  moa-brain/src/pipeline/mod.rs → AGENTS.md
- `impl MemoryStore for FileMemoryStore` --semantically_similar_to--> `MockMemoryStore (brain_turn)`  [INFERRED] [semantically similar]
  moa-memory/src/lib.rs → moa-brain/tests/brain_turn.rs
- `ContextPipeline` --conceptually_related_to--> `WorkingContext god node observation`  [INFERRED]
  moa-brain/src/pipeline/mod.rs → graphify-out/GRAPH_REPORT.md
- `FileMemoryStore` --rationale_for--> `docs/04-memory-architecture.md reference`  [INFERRED]
  moa-memory/src/lib.rs → AGENTS.md

## Hyperedges (group relationships)
- **File-wiki memory store write/search flow** — lib_file_memory_store, wiki_render_markdown, fts_upsert_page, fts_search_query [EXTRACTED 0.95]
- **Stage 5 memory preload and injection** — pipeline_context_pipeline, pipeline_preload_memory_stage_data, pipeline_memory_retriever, pipeline_preloaded_memory_data, lib_memory_store_impl [EXTRACTED 0.90]
- **Brain turn harness mocks** — brain_turn_test, brain_turn_mock_session_store, brain_turn_mock_memory_store, brain_turn_mock_llm_provider, pipeline_build_default [EXTRACTED 0.95]
- **File-wiki memory store write/search flow** — lib_file_memory_store, wiki_render_markdown, fts_upsert_page, fts_search_query [EXTRACTED 0.95]
- **Stage 5 memory preload and injection** — pipeline_context_pipeline, pipeline_preload_memory_stage_data, pipeline_memory_retriever, pipeline_preloaded_memory_data, lib_memory_store_impl [EXTRACTED 0.90]
- **Brain turn harness mocks** — brain_turn_test, brain_turn_mock_session_store, brain_turn_mock_memory_store, brain_turn_mock_llm_provider, pipeline_build_default [EXTRACTED 0.95]

## Communities

### Community 0 - "Core Domain Types"
Cohesion: 0.03
Nodes (61): ActionButton, ApprovalDecision, ApprovalRequest, Attachment, BrainId, ButtonStyle, ChannelRef, CompletionContent (+53 more)

### Community 1 - "Anthropic Provider"
Cohesion: 0.08
Nodes (25): anthropic_message(), AnthropicProvider, AnthropicStreamState, BlockAccumulator, build_request_body(), canonical_model_id(), capabilities_for_model(), completion_request_serializes_to_anthropic_format() (+17 more)

### Community 2 - "Brain Test Harness"
Cohesion: 0.06
Nodes (36): MockLlmProvider (brain_turn), MockMemoryStore (brain_turn), MockSessionStore (brain_turn), run_brain_turn_emits_brain_response_event test, main(), resolve_session_db_path, resolve_session_db_path(), run_prompt() (+28 more)

### Community 3 - "Context Pipeline"
Cohesion: 0.1
Nodes (10): build_default_pipeline(), ContextPipeline, estimate_tokens(), load_history_events(), MockMemoryStore, MockSessionStore, pipeline_runner_executes_stages_in_order(), PipelineStageReport (+2 more)

### Community 4 - "Memory Architecture Docs + FTS"
Cohesion: 0.1
Nodes (27): docs/04-memory-architecture.md reference, docs/07-context-pipeline.md reference, AGENTS.md (MOA instructions), FtsIndex, FtsIndex::search (BM25 + recency boost), CREATE_WIKI_PAGES_TABLE, CREATE_WIKI_SEARCH_TABLE (FTS5 virtual table), Graphify Graph Report (+19 more)

### Community 5 - "Cloud + Hands Config"
Cohesion: 0.09
Nodes (14): CloudConfig, CloudFlyioConfig, CloudHandsConfig, CloudTemporalConfig, config_loads_from_file(), default_config_is_valid(), GatewayConfig, GeneralConfig (+6 more)

### Community 6 - "Brain Turn + File Memory Store"
Cohesion: 0.17
Nodes (4): TurnResult, collect_markdown_files(), FileMemoryStore, try_exists()

### Community 7 - "Brain Turn Mocks"
Cohesion: 0.11
Nodes (4): MockLlmProvider, MockMemoryStore, MockSessionStore, run_brain_turn_emits_brain_response_event()

### Community 8 - "Context Messages + Token Estimation"
Cohesion: 0.1
Nodes (4): ContextMessage, estimate_text_tokens(), HandHandle, WorkingContext

### Community 9 - "FTS Index Internals"
Cohesion: 0.17
Nodes (11): delete_page_entries(), FtsIndex, insert_page(), migrate(), parse_confidence(), parse_page_type(), parse_scope_key(), parse_timestamp() (+3 more)

### Community 10 - "Session Schema + Turso"
Cohesion: 0.16
Nodes (2): is_remote_url(), TursoSessionStore

### Community 11 - "Session Queries"
Cohesion: 0.29
Nodes (9): event_record_from_row(), event_type_from_db(), optional_i64(), optional_text(), parse_timestamp(), platform_from_db(), session_meta_from_row(), session_status_from_db() (+1 more)

### Community 12 - "Memory Retriever Stage"
Cohesion: 0.22
Nodes (9): extract_search_keywords(), extract_search_query(), keyword_extraction_filters_stopwords_and_duplicates(), load_preloaded_memory(), memory_retriever_loads_preloaded_indexes_and_results(), MemoryRetriever, PreloadedMemoryStageData, RelevantMemoryPage (+1 more)

### Community 13 - "Event Types"
Cohesion: 0.18
Nodes (1): Event

### Community 14 - "History Pipeline Stage"
Cohesion: 0.25
Nodes (4): capabilities(), history_compiler_formats_user_and_assistant_turns(), history_processor_uses_preloaded_events(), HistoryCompiler

### Community 15 - "Session Store Tests"
Cohesion: 0.2
Nodes (0): 

### Community 16 - "Core Trait Definitions"
Cohesion: 0.22
Nodes (8): BrainOrchestrator, ContextProcessor, CredentialVault, HandProvider, LLMProvider, MemoryStore, PlatformAdapter, SessionStore

### Community 17 - "Completion Stream Plumbing"
Cohesion: 0.32
Nodes (1): CompletionStream

### Community 18 - "Provider HTTP + SSE Common"
Cohesion: 0.48
Nodes (5): build_http_client(), response_text(), retries_on_rate_limit(), retry_delay(), send_with_retry()

### Community 19 - "Instruction Pipeline Stage"
Cohesion: 0.38
Nodes (2): instruction_processor_appends_config_backed_sections(), InstructionProcessor

### Community 20 - "Tool Definition Stage"
Cohesion: 0.38
Nodes (2): tool_processor_serializes_tool_schemas(), ToolDefinitionProcessor

### Community 21 - "Cache Optimizer Stage"
Cohesion: 0.4
Nodes (2): cache_optimizer_validates_cache_breakpoint(), CacheOptimizer

### Community 22 - "Skill Injector Stage"
Cohesion: 0.4
Nodes (2): skill_injector_marks_cache_breakpoint(), SkillInjector

### Community 23 - "Identity Pipeline Stage"
Cohesion: 0.4
Nodes (2): identity_processor_appends_system_prompt(), IdentityProcessor

### Community 24 - "Anthropic Provider Tests"
Cohesion: 0.67
Nodes (0): 

### Community 25 - "TUI Entry Point"
Cohesion: 1.0
Nodes (0): 

### Community 26 - "Error Type"
Cohesion: 2.0
Nodes (1): MoaError

### Community 27 - "Anthropic Live Test"
Cohesion: 1.0
Nodes (0): 

### Community 28 - "Live Brain Harness Test"
Cohesion: 1.0
Nodes (0): 

### Community 29 - "Compaction"
Cohesion: 1.0
Nodes (0): 

### Community 30 - "Memory Store Test File"
Cohesion: 1.0
Nodes (1): FileMemoryStore Integration Tests

## Knowledge Gaps
- **95 isolated node(s):** `BrainOrchestrator`, `SessionStore`, `HandProvider`, `LLMProvider`, `PlatformAdapter` (+90 more)
  These have ≤1 connection - possible missing edges or undocumented components.
- **Thin community `TUI Entry Point`** (2 nodes): `main.rs`, `main()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Error Type`** (2 nodes): `error.rs`, `MoaError`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Anthropic Live Test`** (2 nodes): `anthropic_live.rs`, `anthropic_live_completion_returns_expected_answer()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Live Brain Harness Test`** (2 nodes): `live_harness.rs`, `live_brain_turn_returns_brain_response()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Compaction`** (2 nodes): `compaction.rs`, `maybe_compact()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Memory Store Test File`** (1 nodes): `FileMemoryStore Integration Tests`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **Why does `impl MemoryStore for FileMemoryStore` connect `Brain Test Harness` to `Memory Architecture Docs + FTS`, `Brain Turn + File Memory Store`?**
  _High betweenness centrality (0.019) - this node is a cross-community bridge._
- **Why does `FileMemoryStore` connect `Memory Architecture Docs + FTS` to `FTS Index Internals`, `Brain Test Harness`?**
  _High betweenness centrality (0.015) - this node is a cross-community bridge._
- **Why does `scope_key()` connect `FTS Index Internals` to `Brain Test Harness`, `Memory Architecture Docs + FTS`?**
  _High betweenness centrality (0.015) - this node is a cross-community bridge._
- **What connects `BrainOrchestrator`, `SessionStore`, `HandProvider` to the rest of the system?**
  _95 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `Core Domain Types` be split into smaller, more focused modules?**
  _Cohesion score 0.03 - nodes in this community are weakly interconnected._
- **Should `Anthropic Provider` be split into smaller, more focused modules?**
  _Cohesion score 0.08 - nodes in this community are weakly interconnected._
- **Should `Brain Test Harness` be split into smaller, more focused modules?**
  _Cohesion score 0.06 - nodes in this community are weakly interconnected._