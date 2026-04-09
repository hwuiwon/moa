# Graph Report - .  (2026-04-09)

## Corpus Check
- Corpus is ~40,668 words - fits in a single context window. You may not need a graph.

## Summary
- 819 nodes · 1237 edges · 49 communities detected
- Extraction: 99% EXTRACTED · 1% INFERRED · 0% AMBIGUOUS · INFERRED: 8 edges (avg confidence: 0.79)
- Token cost: 0 input · 0 output

## God Nodes (most connected - your core abstractions)
1. `App` - 24 edges
2. `FileMemoryStore` - 18 edges
3. `LocalOrchestrator` - 17 edges
4. `TursoSessionStore` - 16 edges
5. `LocalHandProvider` - 16 edges
6. `ToolRouter` - 12 edges
7. `ChatRuntime` - 11 edges
8. `wait_for_approval()` - 11 edges
9. `WorkingContext` - 10 edges
10. `handle_tool_call()` - 10 edges

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

## Communities

### Community 0 - "Core Domain Types"
Cohesion: 0.02
Nodes (74): ActionButton, ApprovalDecision, ApprovalField, ApprovalFileDiff, ApprovalPrompt, ApprovalRequest, ApprovalRule, Attachment (+66 more)

### Community 1 - "Local Orchestrator"
Cohesion: 0.09
Nodes (35): accept_user_message(), always_allow_pattern(), append_event(), approval_diffs_for_call(), approval_fields_for_call(), DockerSandbox, drain_signal_queue(), drive_turn() (+27 more)

### Community 2 - "Anthropic Provider"
Cohesion: 0.07
Nodes (30): anthropic_message(), AnthropicProvider, AnthropicStreamState, BlockAccumulator, build_request_body(), canonical_model_id(), capabilities_for_model(), completion_request_serializes_to_anthropic_format() (+22 more)

### Community 3 - "Tool Registry & Router"
Cohesion: 0.07
Nodes (12): BuiltInTool, expand_local_path(), hand_id(), session_provider_key(), ToolContext, ToolDefinition, ToolExecution, ToolRegistry (+4 more)

### Community 4 - "TUI App State"
Cohesion: 0.12
Nodes (13): App, app_state_transitions_follow_idle_composing_running_idle(), AppMode, approval_status_and_note(), ApprovalCardStatus, ApprovalEntry, ChatEntry, diff_overlay_renders_for_file_write_approval() (+5 more)

### Community 5 - "Diff View"
Cohesion: 0.1
Nodes (25): build_diff_file_view(), default_mode_for_width(), diff_line_style(), DiffFileView, DiffMode, DiffViewState, highlighted_spans(), pad_or_truncate() (+17 more)

### Community 6 - "Brain Turn Tests"
Cohesion: 0.08
Nodes (8): always_allow_rule_persists_and_skips_next_approval(), MockLlmProvider, MockMemoryStore, MockSessionStore, RepeatingToolLlmProvider, run_brain_turn_emits_brain_response_event(), run_brain_turn_pauses_for_approval_then_executes_tool(), ToolLoopLlmProvider

### Community 7 - "Context Pipeline"
Cohesion: 0.1
Nodes (11): build_default_pipeline(), build_default_pipeline_with_tools(), ContextPipeline, estimate_tokens(), load_history_events(), MockMemoryStore, MockSessionStore, pipeline_runner_executes_stages_in_order() (+3 more)

### Community 8 - "File Memory Store"
Cohesion: 0.12
Nodes (12): collect_markdown_files(), expand_local_path(), FileMemoryStore, try_exists(), BrainOrchestrator, ContextProcessor, CredentialVault, HandProvider (+4 more)

### Community 9 - "Config & Errors"
Cohesion: 0.08
Nodes (15): CloudConfig, CloudFlyioConfig, CloudHandsConfig, CloudTemporalConfig, config_loads_from_file(), default_config_is_valid(), GatewayConfig, GeneralConfig (+7 more)

### Community 10 - "Turso Session Store"
Cohesion: 0.11
Nodes (4): is_remote_url(), policy_action_from_db(), policy_scope_from_db(), TursoSessionStore

### Community 11 - "Tool Approval Policies"
Cohesion: 0.16
Nodes (18): ApprovalRuleStore, categorize_tool(), glob_match(), normalize_tool_input(), parse_and_match_bash(), persistent_rule_matching_uses_glob_patterns(), PolicyCheck, read_tools_are_auto_approved_and_bash_requires_approval() (+10 more)

### Community 12 - "Memory Tools"
Cohesion: 0.11
Nodes (5): MemorySearchInput, MemorySearchScope, MemorySearchTool, MemoryWriteInput, MemoryWriteTool

### Community 13 - "FTS5 Search Index"
Cohesion: 0.12
Nodes (17): FtsIndex, FtsIndex::rebuild_scope, scope_key encoding, FtsIndex::search (BM25 + recency boost), FtsIndex::upsert_page, CREATE_WIKI_PAGES_TABLE, CREATE_WIKI_SEARCH_TABLE (FTS5 virtual table), fts_search_finds_ranked_results test (+9 more)

### Community 14 - "Local Hand Provider"
Cohesion: 0.19
Nodes (2): detect_docker(), LocalHandProvider

### Community 15 - "Local Tool Tests"
Cohesion: 0.17
Nodes (8): bash_captures_stdout_and_stderr(), bash_respects_timeout(), EmptyMemoryStore, file_operations_reject_path_traversal(), file_read_reads_written_content(), file_search_finds_files_by_glob(), memory_search_returns_indexed_results(), session()

### Community 16 - "TUI Chat Runtime"
Cohesion: 0.2
Nodes (4): ChatRuntime, local_user_id(), relay_runtime_events(), start_empty_session()

### Community 17 - "Local Orchestrator Tests"
Cohesion: 0.28
Nodes (11): last_user_message(), list_sessions_includes_active_session(), MockProvider, observe_stream_receives_events_in_order(), queued_message_is_processed_after_current_turn(), soft_cancel_marks_session_cancelled(), start_session(), starts_two_sessions_and_processes_both() (+3 more)

### Community 18 - "Session DB Queries"
Cohesion: 0.29
Nodes (9): event_record_from_row(), event_type_from_db(), optional_i64(), optional_text(), parse_timestamp(), platform_from_db(), session_meta_from_row(), session_status_from_db() (+1 more)

### Community 19 - "Brain Turn Harness"
Cohesion: 0.31
Nodes (10): execute_pending_tool(), find_pending_approval(), find_resolved_pending_tool(), format_tool_output(), PendingToolApproval, process_resolved_approval(), run_brain_turn(), run_brain_turn_with_tools() (+2 more)

### Community 20 - "Event Types"
Cohesion: 0.18
Nodes (1): Event

### Community 21 - "Approval Card Widget"
Cohesion: 0.27
Nodes (6): border_line(), content_line(), render_approval_card(), risk_border_style(), truncate_to_width(), wrap_text()

### Community 22 - "History Compilation Stage"
Cohesion: 0.25
Nodes (4): capabilities(), history_compiler_formats_user_and_assistant_turns(), history_processor_uses_preloaded_events(), HistoryCompiler

### Community 23 - "Memory Preload Stage"
Cohesion: 0.24
Nodes (10): load_index_file, MAX_INDEX_BYTES constant (25000), MAX_INDEX_LINES constant (200), truncate_index_content, get_index_truncates_memory_md_to_200_lines test, load_preloaded_memory, MemoryRetriever (Stage 5), MEMORY_STAGE_DATA_METADATA_KEY (+2 more)

### Community 24 - "Prompt Widget"
Cohesion: 0.31
Nodes (2): build_textarea(), PromptWidget

### Community 25 - "Session Store Tests"
Cohesion: 0.2
Nodes (0): 

### Community 26 - "Tool Card Widget"
Cohesion: 0.42
Nodes (7): border_line(), content_line(), render_tool_card(), status_label(), status_style(), truncate_to_width(), wrap_text()

### Community 27 - "CLI Entry Point"
Cohesion: 0.29
Nodes (4): Cli, Command, doctor_report(), doctor_report_includes_model_and_paths()

### Community 28 - "TUI Keybindings"
Cohesion: 0.25
Nodes (1): KeyAction

### Community 29 - "Completion Stream"
Cohesion: 0.38
Nodes (1): CompletionStream

### Community 30 - "Instruction Stage"
Cohesion: 0.38
Nodes (2): instruction_processor_appends_config_backed_sections(), InstructionProcessor

### Community 31 - "Chat View"
Cohesion: 0.6
Nodes (5): max_scroll(), render_chat(), transcript_lines(), wrap_line(), wrap_prefixed()

### Community 32 - "Cache Optimizer Stage"
Cohesion: 0.4
Nodes (2): cache_optimizer_validates_cache_breakpoint(), CacheOptimizer

### Community 33 - "Skill Injector Stage"
Cohesion: 0.4
Nodes (2): skill_injector_marks_cache_breakpoint(), SkillInjector

### Community 34 - "Identity Stage"
Cohesion: 0.4
Nodes (2): identity_processor_appends_system_prompt(), IdentityProcessor

### Community 35 - "CLI Exec Mode"
Cohesion: 0.6
Nodes (4): exec_mode_formats_tool_updates_compactly(), format_tool_update(), resolve_exec_approval(), run_exec()

### Community 36 - "Bash Tool"
Cohesion: 0.6
Nodes (3): BashToolInput, execute_docker(), execute_local()

### Community 37 - "File Search Tool"
Cohesion: 0.67
Nodes (3): collect_matches(), execute(), FileSearchInput

### Community 38 - "File Read Tool"
Cohesion: 0.67
Nodes (3): execute(), FileReadInput, resolve_sandbox_path()

### Community 39 - "Chat Harness Example"
Cohesion: 0.83
Nodes (3): main(), resolve_session_db_path(), run_prompt()

### Community 40 - "Anthropic Provider Tests"
Cohesion: 0.67
Nodes (0): 

### Community 41 - "File Write Tool"
Cohesion: 0.67
Nodes (1): FileWriteInput

### Community 42 - "Wiki Page Tests"
Cohesion: 1.0
Nodes (2): sample_page helper, create_read_update_and_delete_wiki_pages test

### Community 43 - "Anthropic Live Test"
Cohesion: 1.0
Nodes (0): 

### Community 44 - "Compaction"
Cohesion: 1.0
Nodes (0): 

### Community 45 - "Memory Store Integration"
Cohesion: 1.0
Nodes (1): FileMemoryStore Integration Tests

### Community 46 - "FTS Scope Helper"
Cohesion: 1.0
Nodes (1): FtsIndex::scopes_for_path

### Community 47 - "Live Brain Turn Test"
Cohesion: 1.0
Nodes (1): live_brain_turn_returns_brain_response test

### Community 48 - "Relevant Memory Page"
Cohesion: 1.0
Nodes (1): RelevantMemoryPage

## Knowledge Gaps
- **131 isolated node(s):** `FileMemoryStore Integration Tests`, `sample_page helper`, `create_read_update_and_delete_wiki_pages test`, `fts_search_finds_ranked_results test`, `rebuild_search_index_from_files_restores_results test` (+126 more)
  These have ≤1 connection - possible missing edges or undocumented components.
- **Thin community `Wiki Page Tests`** (2 nodes): `sample_page helper`, `create_read_update_and_delete_wiki_pages test`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Anthropic Live Test`** (2 nodes): `anthropic_live.rs`, `anthropic_live_completion_returns_expected_answer()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Compaction`** (2 nodes): `compaction.rs`, `maybe_compact()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Memory Store Integration`** (1 nodes): `FileMemoryStore Integration Tests`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `FTS Scope Helper`** (1 nodes): `FtsIndex::scopes_for_path`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Live Brain Turn Test`** (1 nodes): `live_brain_turn_returns_brain_response test`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Relevant Memory Page`** (1 nodes): `RelevantMemoryPage`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **Why does `LocalHandProvider` connect `Local Hand Provider` to `Local Orchestrator`?**
  _High betweenness centrality (0.019) - this node is a cross-community bridge._
- **What connects `FileMemoryStore Integration Tests`, `sample_page helper`, `create_read_update_and_delete_wiki_pages test` to the rest of the system?**
  _131 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `Core Domain Types` be split into smaller, more focused modules?**
  _Cohesion score 0.02 - nodes in this community are weakly interconnected._
- **Should `Local Orchestrator` be split into smaller, more focused modules?**
  _Cohesion score 0.09 - nodes in this community are weakly interconnected._
- **Should `Anthropic Provider` be split into smaller, more focused modules?**
  _Cohesion score 0.07 - nodes in this community are weakly interconnected._
- **Should `Tool Registry & Router` be split into smaller, more focused modules?**
  _Cohesion score 0.07 - nodes in this community are weakly interconnected._
- **Should `TUI App State` be split into smaller, more focused modules?**
  _Cohesion score 0.12 - nodes in this community are weakly interconnected._