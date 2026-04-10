# Graph Report - .  (2026-04-09)

## Corpus Check
- 76 files · ~50,807 words
- Verdict: corpus is large enough that graph structure adds value.

## Summary
- 1052 nodes · 1742 edges · 53 communities detected
- Extraction: 100% EXTRACTED · 0% INFERRED · 0% AMBIGUOUS · INFERRED: 1 edges (avg confidence: 0.75)
- Token cost: 0 input · 0 output

## God Nodes (most connected - your core abstractions)
1. `App` - 42 edges
2. `SkillFrontmatter` - 30 edges
3. `ChatRuntime` - 21 edges
4. `FileMemoryStore` - 18 edges
5. `LocalOrchestrator` - 17 edges
6. `TursoSessionStore` - 16 edges
7. `LocalHandProvider` - 16 edges
8. `ToolRouter` - 14 edges
9. `start_session()` - 12 edges
10. `wiki_page_from_skill()` - 12 edges

## Surprising Connections (you probably didn't know these)
- `truncate_index_content` --shares_data_with--> `MemoryRetriever (Stage 5)`  [INFERRED]
  moa-memory/src/index.rs → moa-brain/src/pipeline/memory.rs

## Communities

### Community 0 - "Core Domain Types"
Cohesion: 0.02
Nodes (77): ActionButton, ApprovalDecision, ApprovalField, ApprovalFileDiff, ApprovalPrompt, ApprovalRequest, ApprovalRule, Attachment (+69 more)

### Community 1 - "TUI App State"
Cohesion: 0.09
Nodes (20): App, app_state_transitions_follow_idle_composing_running_waiting_idle(), AppMode, approval_status_and_note(), ApprovalCardStatus, ApprovalEntry, ChatEntry, diff_overlay_renders_for_file_write_approval() (+12 more)

### Community 2 - "Skill Markdown Format"
Cohesion: 0.08
Nodes (21): build_skill_path(), confidence_for_skill(), defaults_missing_moa_metadata(), estimate_skill_tokens(), format_timestamp(), humanize_skill_name(), is_valid_skill_name(), metadata_csv() (+13 more)

### Community 3 - "Tool Registry & Router"
Cohesion: 0.07
Nodes (26): approval_diffs_for(), approval_fields_for(), approval_pattern_for(), BuiltInTool, execute_tool_policy(), expand_local_path(), hand_id(), language_hint_for_path() (+18 more)

### Community 4 - "Anthropic Provider"
Cohesion: 0.07
Nodes (30): anthropic_message(), AnthropicProvider, AnthropicStreamState, BlockAccumulator, build_request_body(), canonical_model_id(), capabilities_for_model(), completion_request_serializes_to_anthropic_format() (+22 more)

### Community 5 - "File Memory Store"
Cohesion: 0.07
Nodes (22): execute_pending_tool(), format_tool_output(), process_resolved_approval(), run_brain_turn(), run_brain_turn_with_tools(), TurnResult, collect_markdown_files(), expand_local_path() (+14 more)

### Community 6 - "Local Orchestrator"
Cohesion: 0.12
Nodes (24): accept_user_message(), append_event(), buffer_queued_message(), DockerSandbox, drain_signal_queue(), drive_turn(), execute_tool(), flush_next_queued_message() (+16 more)

### Community 7 - "Brain Turn Tests"
Cohesion: 0.07
Nodes (10): always_allow_rule_persists_and_skips_next_approval(), CapturingTextLlmProvider, MockLlmProvider, MockMemoryStore, MockSessionStore, pipeline_stage_four_injects_workspace_skill_metadata(), RepeatingToolLlmProvider, run_brain_turn_emits_brain_response_event() (+2 more)

### Community 8 - "Local Orchestrator Tests"
Cohesion: 0.12
Nodes (22): denied_tool_preserves_queued_follow_up(), last_user_message(), list_sessions_includes_active_session(), MockProvider, multiple_queued_messages_are_processed_fifo_one_turn_at_a_time(), observe_stream_receives_events_in_order(), queued_follow_up_request_ends_with_user_message(), queued_message_is_processed_after_current_turn() (+14 more)

### Community 9 - "Diff View"
Cohesion: 0.1
Nodes (25): build_diff_file_view(), default_mode_for_width(), diff_line_style(), DiffFileView, DiffMode, DiffViewState, highlighted_spans(), pad_or_truncate() (+17 more)

### Community 10 - "Context Pipeline"
Cohesion: 0.1
Nodes (11): build_default_pipeline(), build_default_pipeline_with_tools(), ContextPipeline, estimate_tokens(), load_history_events(), MockMemoryStore, MockSessionStore, pipeline_runner_executes_stages_in_order() (+3 more)

### Community 11 - "Config & Errors"
Cohesion: 0.08
Nodes (15): CloudConfig, CloudFlyioConfig, CloudHandsConfig, CloudTemporalConfig, config_loads_from_file(), default_config_is_valid(), GatewayConfig, GeneralConfig (+7 more)

### Community 12 - "TUI Chat Runtime"
Cohesion: 0.1
Nodes (8): ChatRuntime, last_session_message(), local_user_id(), relay_runtime_events(), relay_session_runtime_events(), SessionPreview, SessionRuntimeEvent, start_empty_session()

### Community 13 - "Turso Session Store"
Cohesion: 0.11
Nodes (4): is_remote_url(), policy_action_from_db(), policy_scope_from_db(), TursoSessionStore

### Community 14 - "Memory Tools"
Cohesion: 0.08
Nodes (7): MemoryReadInput, MemoryReadTool, MemorySearchInput, MemorySearchScope, MemorySearchTool, MemoryWriteInput, MemoryWriteTool

### Community 15 - "FTS5 Search Index"
Cohesion: 0.19
Nodes (10): build_fts_query(), delete_page_entries(), FtsIndex, insert_page(), migrate(), parse_confidence(), parse_page_type(), parse_scope_key() (+2 more)

### Community 16 - "Session Picker View"
Cohesion: 0.18
Nodes (8): centered_rect(), filtered_sessions(), fuzzy_search_matches_title_and_last_message(), picker_haystack(), picker_selection_wraps_and_clamps(), preview(), render_session_picker(), SessionPickerState

### Community 17 - "Tool Approval Policies"
Cohesion: 0.22
Nodes (12): ApprovalRuleStore, glob_match(), parse_and_match_bash(), persistent_rule_matching_uses_glob_patterns(), PolicyCheck, read_tools_are_auto_approved_and_bash_requires_approval(), rule_matches(), rule_visible_to_workspace() (+4 more)

### Community 18 - "Skill Injector"
Cohesion: 0.16
Nodes (9): distills_skill_after_tool_heavy_session(), improves_existing_skill_when_better_flow_is_found(), load_skills(), MockLlm, session(), skill_injector_marks_breakpoint_without_skills(), skill_injector_marks_cache_breakpoint_and_formats_metadata(), SkillInjector (+1 more)

### Community 19 - "Skill Improver & Distiller"
Cohesion: 0.21
Nodes (15): build_distillation_prompt(), count_tool_calls(), extract_task_summary(), find_similar_skill(), maybe_distill_skill(), normalize_new_skill(), similarity_score(), tokenize() (+7 more)

### Community 20 - "Local Tool Tests"
Cohesion: 0.17
Nodes (9): bash_captures_stdout_and_stderr(), bash_respects_timeout(), EmptyMemoryStore, file_operations_reject_path_traversal(), file_read_reads_written_content(), file_search_finds_files_by_glob(), memory_read_returns_page_contents(), memory_search_returns_indexed_results() (+1 more)

### Community 21 - "Local Hand Provider"
Cohesion: 0.19
Nodes (2): detect_docker(), LocalHandProvider

### Community 22 - "Wiki Markdown"
Cohesion: 0.26
Nodes (13): extract_extra_metadata(), extract_title(), fallback_title(), frontmatter_parsing_reads_expected_fields(), infer_page_type(), json_to_yaml_value(), PageFrontmatter, parse_markdown() (+5 more)

### Community 23 - "Session DB Queries"
Cohesion: 0.29
Nodes (9): event_record_from_row(), event_type_from_db(), optional_i64(), optional_text(), parse_timestamp(), platform_from_db(), session_meta_from_row(), session_status_from_db() (+1 more)

### Community 24 - "Event Types"
Cohesion: 0.18
Nodes (1): Event

### Community 25 - "Approval Card Widget"
Cohesion: 0.27
Nodes (6): border_line(), content_line(), render_approval_card(), risk_border_style(), truncate_to_width(), wrap_text()

### Community 26 - "Toolbar Widget"
Cohesion: 0.27
Nodes (6): build_tab_spans(), render_toolbar(), short_session_id(), tab_title(), toolbar_labels_include_status_icons_and_tab_limit(), visible_window()

### Community 27 - "History Compilation Stage"
Cohesion: 0.25
Nodes (4): capabilities(), history_compiler_formats_user_and_assistant_turns(), history_processor_uses_preloaded_events(), HistoryCompiler

### Community 28 - "TUI Keybindings"
Cohesion: 0.2
Nodes (1): KeyAction

### Community 29 - "Prompt Widget"
Cohesion: 0.31
Nodes (2): build_textarea(), PromptWidget

### Community 30 - "Session Store Tests"
Cohesion: 0.2
Nodes (0): 

### Community 31 - "Tool Card Widget"
Cohesion: 0.42
Nodes (7): border_line(), content_line(), render_tool_card(), status_label(), status_style(), truncate_to_width(), wrap_text()

### Community 32 - "Memory Preload Stage"
Cohesion: 0.25
Nodes (9): load_index_file, MAX_INDEX_BYTES constant (25000), MAX_INDEX_LINES constant (200), truncate_index_content, load_preloaded_memory, MemoryRetriever (Stage 5), MEMORY_STAGE_DATA_METADATA_KEY, PreloadedMemoryStageData (+1 more)

### Community 33 - "Memory Store Tests"
Cohesion: 0.43
Nodes (6): create_read_update_and_delete_wiki_pages(), fts_search_finds_ranked_results(), fts_search_handles_hyphenated_queries(), rebuild_search_index_from_files_restores_results(), sample_page(), user_and_workspace_scopes_are_separate()

### Community 34 - "CLI Entry Point"
Cohesion: 0.29
Nodes (4): Cli, Command, doctor_report(), doctor_report_includes_model_and_paths()

### Community 35 - "Instruction Stage"
Cohesion: 0.38
Nodes (2): instruction_processor_appends_config_backed_sections(), InstructionProcessor

### Community 36 - "Tool Definition Stage"
Cohesion: 0.38
Nodes (2): tool_processor_serializes_tool_schemas(), ToolDefinitionProcessor

### Community 37 - "Skill Registry"
Cohesion: 0.48
Nodes (1): SkillRegistry

### Community 38 - "Chat View"
Cohesion: 0.6
Nodes (5): max_scroll(), render_chat(), transcript_lines(), wrap_line(), wrap_prefixed()

### Community 39 - "Cache Optimizer Stage"
Cohesion: 0.4
Nodes (2): cache_optimizer_validates_cache_breakpoint(), CacheOptimizer

### Community 40 - "Identity Stage"
Cohesion: 0.4
Nodes (2): identity_processor_appends_system_prompt(), IdentityProcessor

### Community 41 - "CLI Exec Mode"
Cohesion: 0.6
Nodes (4): exec_mode_formats_tool_updates_compactly(), format_tool_update(), resolve_exec_approval(), run_exec()

### Community 42 - "Bash Tool"
Cohesion: 0.6
Nodes (3): BashToolInput, execute_docker(), execute_local()

### Community 43 - "File Search Tool"
Cohesion: 0.67
Nodes (3): collect_matches(), execute(), FileSearchInput

### Community 44 - "File Read Tool"
Cohesion: 0.67
Nodes (3): execute(), FileReadInput, resolve_sandbox_path()

### Community 45 - "Chat Harness Example"
Cohesion: 0.83
Nodes (3): main(), resolve_session_db_path(), run_prompt()

### Community 46 - "Anthropic Provider Tests"
Cohesion: 0.67
Nodes (0): 

### Community 47 - "File Write Tool"
Cohesion: 0.67
Nodes (1): FileWriteInput

### Community 48 - "Anthropic Live Test"
Cohesion: 1.0
Nodes (0): 

### Community 49 - "Compaction"
Cohesion: 1.0
Nodes (0): 

### Community 50 - "Memory Search Helpers"
Cohesion: 1.0
Nodes (2): extract_search_keywords (stopword filter), extract_search_query

### Community 51 - "Live Brain Turn Test"
Cohesion: 1.0
Nodes (1): live_brain_turn_returns_brain_response test

### Community 52 - "Relevant Memory Page"
Cohesion: 1.0
Nodes (1): RelevantMemoryPage

## Knowledge Gaps
- **135 isolated node(s):** `load_index_file`, `MAX_INDEX_LINES constant (200)`, `MAX_INDEX_BYTES constant (25000)`, `PageFrontmatter`, `Platform` (+130 more)
  These have ≤1 connection - possible missing edges or undocumented components.
- **Thin community `Anthropic Live Test`** (2 nodes): `anthropic_live.rs`, `anthropic_live_completion_returns_expected_answer()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Compaction`** (2 nodes): `compaction.rs`, `maybe_compact()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Memory Search Helpers`** (2 nodes): `extract_search_keywords (stopword filter)`, `extract_search_query`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Live Brain Turn Test`** (1 nodes): `live_brain_turn_returns_brain_response test`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Relevant Memory Page`** (1 nodes): `RelevantMemoryPage`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **What connects `load_index_file`, `MAX_INDEX_LINES constant (200)`, `MAX_INDEX_BYTES constant (25000)` to the rest of the system?**
  _135 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `Core Domain Types` be split into smaller, more focused modules?**
  _Cohesion score 0.02 - nodes in this community are weakly interconnected._
- **Should `TUI App State` be split into smaller, more focused modules?**
  _Cohesion score 0.09 - nodes in this community are weakly interconnected._
- **Should `Skill Markdown Format` be split into smaller, more focused modules?**
  _Cohesion score 0.08 - nodes in this community are weakly interconnected._
- **Should `Tool Registry & Router` be split into smaller, more focused modules?**
  _Cohesion score 0.07 - nodes in this community are weakly interconnected._
- **Should `Anthropic Provider` be split into smaller, more focused modules?**
  _Cohesion score 0.07 - nodes in this community are weakly interconnected._
- **Should `File Memory Store` be split into smaller, more focused modules?**
  _Cohesion score 0.07 - nodes in this community are weakly interconnected._