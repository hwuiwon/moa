# Graph Report - .  (2026-04-09)

## Corpus Check
- Corpus is ~26,691 words - fits in a single context window. You may not need a graph.

## Summary
- 575 nodes · 770 edges · 39 communities detected
- Extraction: 98% EXTRACTED · 2% INFERRED · 0% AMBIGUOUS · INFERRED: 12 edges (avg confidence: 0.79)
- Token cost: 0 input · 0 output

## God Nodes (most connected - your core abstractions)
1. `FileMemoryStore` - 18 edges
2. `LocalHandProvider` - 16 edges
3. `TursoSessionStore` - 13 edges
4. `Spec Location Table` - 12 edges
5. `WorkingContext` - 10 edges
6. `AnthropicProvider` - 9 edges
7. `AnthropicStreamState` - 9 edges
8. `MockSessionStore` - 9 edges
9. `MockSessionStore` - 9 edges
10. `CompletionStream` - 8 edges

## Surprising Connections (you probably didn't know these)
- `extract_search_keywords (stopword filter)` --semantically_similar_to--> `FtsIndex::search (BM25 + recency boost)`  [INFERRED] [semantically similar]
  moa-brain/src/pipeline/memory.rs → moa-memory/src/fts.rs
- `truncate_index_content` --shares_data_with--> `MemoryRetriever (Stage 5)`  [INFERRED]
  moa-memory/src/index.rs → moa-brain/src/pipeline/memory.rs
- `fts_search_finds_ranked_results test` --calls--> `FtsIndex::search (BM25 + recency boost)`  [INFERRED]
  moa-memory/tests/memory_store.rs → moa-memory/src/fts.rs
- `get_index_truncates_memory_md_to_200_lines test` --references--> `truncate_index_content`  [INFERRED]
  moa-memory/tests/memory_store.rs → moa-memory/src/index.rs
- `rebuild_search_index_from_files_restores_results test` --calls--> `FtsIndex::rebuild_scope`  [INFERRED]
  moa-memory/tests/memory_store.rs → moa-memory/src/fts.rs

## Hyperedges (group relationships)
- **Rust Library Discipline Policy Bundle** — concept_thiserror_library_policy, concept_tracing_logging_policy, concept_no_unwrap_library, concept_error_enum_per_crate [INFERRED 0.85]
- **Docs-as-Source-of-Truth Mapping** — doc_01_architecture_overview, concept_trait_source_of_truth, agents_md_rules [INFERRED 0.80]
- **Async Runtime and Logging Stack** — concept_async_tokio_runtime, concept_tracing_logging_policy, doc_10_technology_stack [INFERRED 0.75]

## Communities

### Community 0 - "Community 0"
Cohesion: 0.02
Nodes (61): ActionButton, ApprovalDecision, ApprovalRequest, Attachment, BrainId, ButtonStyle, ChannelRef, CompletionContent (+53 more)

### Community 1 - "Community 1"
Cohesion: 0.08
Nodes (25): anthropic_message(), AnthropicProvider, AnthropicStreamState, BlockAccumulator, build_request_body(), canonical_model_id(), capabilities_for_model(), completion_request_serializes_to_anthropic_format() (+17 more)

### Community 2 - "Community 2"
Cohesion: 0.1
Nodes (11): build_default_pipeline(), build_default_pipeline_with_tools(), ContextPipeline, estimate_tokens(), load_history_events(), MockMemoryStore, MockSessionStore, pipeline_runner_executes_stages_in_order() (+3 more)

### Community 3 - "Community 3"
Cohesion: 0.08
Nodes (15): CloudConfig, CloudFlyioConfig, CloudHandsConfig, CloudTemporalConfig, config_loads_from_file(), default_config_is_valid(), GatewayConfig, GeneralConfig (+7 more)

### Community 4 - "Community 4"
Cohesion: 0.1
Nodes (10): BuiltInTool, expand_local_path(), hand_id(), session_provider_key(), ToolContext, ToolDefinition, ToolExecution, ToolRegistry (+2 more)

### Community 5 - "Community 5"
Cohesion: 0.09
Nodes (6): MockLlmProvider, MockMemoryStore, MockSessionStore, run_brain_turn_emits_brain_response_event(), run_brain_turn_executes_tool_calls_and_feeds_results_back(), ToolLoopLlmProvider

### Community 6 - "Community 6"
Cohesion: 0.15
Nodes (8): format_tool_output(), run_brain_turn(), run_brain_turn_with_tools(), TurnResult, collect_markdown_files(), expand_local_path(), FileMemoryStore, try_exists()

### Community 7 - "Community 7"
Cohesion: 0.09
Nodes (27): MOA Agent Instructions, Code Conventions, Graphify Usage Section, Implementation Rules, Spec Location Table, Tokio Async I/O Mandate, One Error Enum Per Crate, Optional Feature Flags (+19 more)

### Community 8 - "Community 8"
Cohesion: 0.12
Nodes (5): detect_docker(), DockerSandbox, LocalHandProvider, tool_processor_serializes_tool_schemas(), ToolDefinitionProcessor

### Community 9 - "Community 9"
Cohesion: 0.09
Nodes (5): ContextMessage, estimate_text_tokens(), HandHandle, MemoryPath, WorkingContext

### Community 10 - "Community 10"
Cohesion: 0.16
Nodes (2): is_remote_url(), TursoSessionStore

### Community 11 - "Community 11"
Cohesion: 0.11
Nodes (5): MemorySearchInput, MemorySearchScope, MemorySearchTool, MemoryWriteInput, MemoryWriteTool

### Community 12 - "Community 12"
Cohesion: 0.12
Nodes (17): FtsIndex, FtsIndex::rebuild_scope, scope_key encoding, FtsIndex::search (BM25 + recency boost), FtsIndex::upsert_page, CREATE_WIKI_PAGES_TABLE, CREATE_WIKI_SEARCH_TABLE (FTS5 virtual table), fts_search_finds_ranked_results test (+9 more)

### Community 13 - "Community 13"
Cohesion: 0.17
Nodes (8): bash_captures_stdout_and_stderr(), bash_respects_timeout(), EmptyMemoryStore, file_operations_reject_path_traversal(), file_read_reads_written_content(), file_search_finds_files_by_glob(), memory_search_returns_indexed_results(), session()

### Community 14 - "Community 14"
Cohesion: 0.29
Nodes (9): event_record_from_row(), event_type_from_db(), optional_i64(), optional_text(), parse_timestamp(), platform_from_db(), session_meta_from_row(), session_status_from_db() (+1 more)

### Community 15 - "Community 15"
Cohesion: 0.18
Nodes (1): Event

### Community 16 - "Community 16"
Cohesion: 0.25
Nodes (4): capabilities(), history_compiler_formats_user_and_assistant_turns(), history_processor_uses_preloaded_events(), HistoryCompiler

### Community 17 - "Community 17"
Cohesion: 0.2
Nodes (0): 

### Community 18 - "Community 18"
Cohesion: 0.24
Nodes (10): load_index_file, MAX_INDEX_BYTES constant (25000), MAX_INDEX_LINES constant (200), truncate_index_content, get_index_truncates_memory_md_to_200_lines test, load_preloaded_memory, MemoryRetriever (Stage 5), MEMORY_STAGE_DATA_METADATA_KEY (+2 more)

### Community 19 - "Community 19"
Cohesion: 0.22
Nodes (8): BrainOrchestrator, ContextProcessor, CredentialVault, HandProvider, LLMProvider, MemoryStore, PlatformAdapter, SessionStore

### Community 20 - "Community 20"
Cohesion: 0.48
Nodes (5): build_http_client(), response_text(), retries_on_rate_limit(), retry_delay(), send_with_retry()

### Community 21 - "Community 21"
Cohesion: 0.38
Nodes (2): instruction_processor_appends_config_backed_sections(), InstructionProcessor

### Community 22 - "Community 22"
Cohesion: 0.4
Nodes (2): cache_optimizer_validates_cache_breakpoint(), CacheOptimizer

### Community 23 - "Community 23"
Cohesion: 0.4
Nodes (2): skill_injector_marks_cache_breakpoint(), SkillInjector

### Community 24 - "Community 24"
Cohesion: 0.4
Nodes (2): identity_processor_appends_system_prompt(), IdentityProcessor

### Community 25 - "Community 25"
Cohesion: 0.6
Nodes (3): BashToolInput, execute_docker(), execute_local()

### Community 26 - "Community 26"
Cohesion: 0.67
Nodes (3): collect_matches(), execute(), FileSearchInput

### Community 27 - "Community 27"
Cohesion: 0.67
Nodes (3): execute(), FileReadInput, resolve_sandbox_path()

### Community 28 - "Community 28"
Cohesion: 0.83
Nodes (3): main(), resolve_session_db_path(), run_prompt()

### Community 29 - "Community 29"
Cohesion: 0.67
Nodes (0): 

### Community 30 - "Community 30"
Cohesion: 0.67
Nodes (1): FileWriteInput

### Community 31 - "Community 31"
Cohesion: 1.0
Nodes (2): sample_page helper, create_read_update_and_delete_wiki_pages test

### Community 32 - "Community 32"
Cohesion: 1.0
Nodes (0): 

### Community 33 - "Community 33"
Cohesion: 1.0
Nodes (0): 

### Community 34 - "Community 34"
Cohesion: 1.0
Nodes (0): 

### Community 35 - "Community 35"
Cohesion: 1.0
Nodes (1): FileMemoryStore Integration Tests

### Community 36 - "Community 36"
Cohesion: 1.0
Nodes (1): FtsIndex::scopes_for_path

### Community 37 - "Community 37"
Cohesion: 1.0
Nodes (1): live_brain_turn_returns_brain_response test

### Community 38 - "Community 38"
Cohesion: 1.0
Nodes (1): RelevantMemoryPage

## Ambiguous Edges - Review These
- `No unwrap in Library Code` → `docs/08-security.md`  [AMBIGUOUS]
  AGENTS.md · relation: conceptually_related_to

## Knowledge Gaps
- **113 isolated node(s):** `FileMemoryStore Integration Tests`, `sample_page helper`, `create_read_update_and_delete_wiki_pages test`, `fts_search_finds_ranked_results test`, `rebuild_search_index_from_files_restores_results test` (+108 more)
  These have ≤1 connection - possible missing edges or undocumented components.
- **Thin community `Community 31`** (2 nodes): `sample_page helper`, `create_read_update_and_delete_wiki_pages test`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Community 32`** (2 nodes): `main.rs`, `main()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Community 33`** (2 nodes): `anthropic_live.rs`, `anthropic_live_completion_returns_expected_answer()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Community 34`** (2 nodes): `compaction.rs`, `maybe_compact()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Community 35`** (1 nodes): `FileMemoryStore Integration Tests`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Community 36`** (1 nodes): `FtsIndex::scopes_for_path`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Community 37`** (1 nodes): `live_brain_turn_returns_brain_response test`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Community 38`** (1 nodes): `RelevantMemoryPage`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **What is the exact relationship between `No unwrap in Library Code` and `docs/08-security.md`?**
  _Edge tagged AMBIGUOUS (relation: conceptually_related_to) - confidence is low._
- **What connects `FileMemoryStore Integration Tests`, `sample_page helper`, `create_read_update_and_delete_wiki_pages test` to the rest of the system?**
  _113 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `Community 0` be split into smaller, more focused modules?**
  _Cohesion score 0.02 - nodes in this community are weakly interconnected._
- **Should `Community 1` be split into smaller, more focused modules?**
  _Cohesion score 0.08 - nodes in this community are weakly interconnected._
- **Should `Community 2` be split into smaller, more focused modules?**
  _Cohesion score 0.1 - nodes in this community are weakly interconnected._
- **Should `Community 3` be split into smaller, more focused modules?**
  _Cohesion score 0.08 - nodes in this community are weakly interconnected._
- **Should `Community 4` be split into smaller, more focused modules?**
  _Cohesion score 0.1 - nodes in this community are weakly interconnected._