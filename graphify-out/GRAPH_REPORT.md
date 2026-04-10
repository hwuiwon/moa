# Graph Report - .  (2026-04-10)

## Corpus Check
- 104 files · ~84,192 words
- Verdict: corpus is large enough that graph structure adds value.

## Summary
- 1696 nodes · 2950 edges · 72 communities detected
- Extraction: 100% EXTRACTED · 0% INFERRED · 0% AMBIGUOUS
- Token cost: 0 input · 0 output

## God Nodes (most connected - your core abstractions)
1. `App` - 42 edges
2. `SkillFrontmatter` - 30 edges
3. `FileMemoryStore` - 26 edges
4. `ChatRuntime` - 21 edges
5. `LocalOrchestrator` - 19 edges
6. `E2BHandProvider` - 19 edges
7. `DaytonaHandProvider` - 18 edges
8. `TursoSessionStore` - 16 edges
9. `LocalHandProvider` - 16 edges
10. `ToolRouter` - 16 edges

## Surprising Connections (you probably didn't know these)
- None detected - all connections are within the same source files.

## Communities

### Community 0 - "Core Domain IDs"
Cohesion: 0.02
Nodes (78): Event, ActionButton, ApprovalDecision, ApprovalField, ApprovalFileDiff, ApprovalPrompt, ApprovalRequest, ApprovalRule (+70 more)

### Community 1 - "Context Pipeline"
Cohesion: 0.04
Nodes (27): MemoryReadInput, MemoryReadTool, MemorySearchInput, MemorySearchScope, MemorySearchTool, MemoryWriteInput, MemoryWriteTool, build_default_pipeline() (+19 more)

### Community 2 - "TUI App State"
Cohesion: 0.09
Nodes (20): App, app_state_transitions_follow_idle_composing_running_waiting_idle(), AppMode, approval_status_and_note(), ApprovalCardStatus, ApprovalEntry, ChatEntry, diff_overlay_renders_for_file_write_approval() (+12 more)

### Community 3 - "Skill Document Format"
Cohesion: 0.07
Nodes (22): build_skill_path(), confidence_for_skill(), defaults_missing_moa_metadata(), estimate_skill_tokens(), format_timestamp(), humanize_skill_name(), is_valid_skill_name(), metadata_csv() (+14 more)

### Community 4 - "Tool Router & Policies"
Cohesion: 0.06
Nodes (27): approval_diffs_for(), approval_fields_for(), approval_pattern_for(), BuiltInTool, default_cloud_provider(), execute_tool_policy(), expand_local_path(), hand_id() (+19 more)

### Community 5 - "Brain Turn Tests"
Cohesion: 0.05
Nodes (15): always_allow_rule_persists_and_skips_next_approval(), canary_leaks_in_tool_input_are_detected_and_blocked(), CanaryLeakLlmProvider, CapturingTextLlmProvider, FixedPageMemoryStore, malicious_tool_results_are_wrapped_as_untrusted_content(), MaliciousToolOutputLlmProvider, MockLlmProvider (+7 more)

### Community 6 - "Provider Common Layer"
Cohesion: 0.06
Nodes (30): build_function_tool(), build_http_client(), build_responses_request(), consume_responses_stream_once(), is_rate_limit_error(), is_rate_limit_message(), map_openai_error(), metadata_as_strings() (+22 more)

### Community 7 - "File Memory Store"
Cohesion: 0.07
Nodes (27): build_provider_from_config(), build_provider_from_selection(), explicit_provider_prefix_overrides_inference(), infer_provider_name(), infers_anthropic_for_claude_models(), infers_openai_for_gpt_models(), infers_openrouter_for_vendor_prefixed_models(), is_openai_model() (+19 more)

### Community 8 - "Temporal Orchestrator"
Cohesion: 0.06
Nodes (24): activity_options(), apply_approval_decision(), ApprovalDecisionActivityInput, ApprovalSignalInput, CancelMode, connect_temporal_client(), flush_all_queued_messages(), FlushQueuedMessagesActivityInput (+16 more)

### Community 9 - "Local Orchestrator"
Cohesion: 0.09
Nodes (26): accept_user_message(), append_event(), buffer_queued_message(), DockerSandbox, drain_signal_queue(), drive_turn(), execute_tool(), flush_next_queued_message() (+18 more)

### Community 10 - "Anthropic Provider"
Cohesion: 0.08
Nodes (25): anthropic_message(), AnthropicProvider, AnthropicStreamState, BlockAccumulator, build_request_body(), canonical_model_id(), capabilities_for_model(), completion_request_serializes_to_anthropic_format() (+17 more)

### Community 11 - "Telegram Renderer"
Cohesion: 0.09
Nodes (23): append_piece(), discord_renderer_attaches_buttons_to_last_chunk_only(), discord_renderer_uses_embed_limit_for_long_text(), DiscordRenderChunk, DiscordRenderer, render_approval_request(), render_diff(), render_tool_card() (+15 more)

### Community 12 - "Turso Session Store"
Cohesion: 0.08
Nodes (13): event_record_from_row(), event_type_from_db(), optional_i64(), optional_text(), parse_timestamp(), platform_from_db(), session_meta_from_row(), session_status_from_db() (+5 more)

### Community 13 - "Local Orchestrator Tests"
Cohesion: 0.11
Nodes (24): denied_tool_preserves_queued_follow_up(), last_user_message(), list_sessions_includes_active_session(), memory_maintenance_runs_due_workspace_consolidation(), memory_maintenance_skips_when_threshold_or_cooldown_not_met(), MockProvider, multiple_queued_messages_are_processed_fifo_one_turn_at_a_time(), observe_stream_receives_events_in_order() (+16 more)

### Community 14 - "Config & Errors"
Cohesion: 0.07
Nodes (18): CloudConfig, CloudFlyioConfig, CloudHandsConfig, CloudTemporalConfig, config_loads_from_file(), default_config_is_valid(), GatewayConfig, GeneralConfig (+10 more)

### Community 15 - "Diff View"
Cohesion: 0.1
Nodes (25): build_diff_file_view(), default_mode_for_width(), diff_line_style(), DiffFileView, DiffMode, DiffViewState, highlighted_spans(), pad_or_truncate() (+17 more)

### Community 16 - "Discord Adapter"
Cohesion: 0.12
Nodes (18): approval_callback_maps_to_control_message(), attachments_from_message(), context_from_component(), discord_button(), discord_create_message(), discord_create_message_includes_buttons_for_last_chunk(), discord_edit_message(), discord_embed() (+10 more)

### Community 17 - "E2B Hand Provider"
Cohesion: 0.14
Nodes (16): build_url(), ConnectedSandbox, decode_stream_chunk(), default_headers(), E2BHandProvider, encode_connect_request(), encode_test_envelopes(), envd_headers() (+8 more)

### Community 18 - "Slack Adapter"
Cohesion: 0.13
Nodes (19): handle_interaction_event(), handle_push_event(), inbound_from_app_mention(), inbound_from_interaction_event(), inbound_from_message_event(), inbound_from_push_event(), interaction_origin(), parses_approval_callback_into_control_message() (+11 more)

### Community 19 - "Temporal Orchestrator Tests"
Cohesion: 0.13
Nodes (21): delayed_text_stream(), last_user_message(), mock_capabilities(), temporal_orchestrator_live_anthropic_smoke(), temporal_orchestrator_processes_multiple_queued_messages_fifo(), temporal_orchestrator_processes_two_sessions_independently(), temporal_orchestrator_queues_message_while_waiting_for_approval(), temporal_orchestrator_runs_workflow_and_unblocks_on_approval() (+13 more)

### Community 20 - "Wiki & Branching Markdown"
Cohesion: 0.12
Nodes (28): append_change_manifest(), branch_dir(), branch_file_path(), branch_root(), ChangeOperation, ChangeRecord, list_branches(), merge_markdown() (+20 more)

### Community 21 - "MCP Client"
Cohesion: 0.14
Nodes (16): flatten_call_result(), flatten_tool_result_aggregates_text_items(), header_map_from_pairs(), http_client_sends_headers_and_parses_jsonrpc(), MCPClient, McpDiscoveredTool, McpTransport, parse_jsonrpc_result() (+8 more)

### Community 22 - "TUI Chat Runtime"
Cohesion: 0.1
Nodes (8): ChatRuntime, last_session_message(), local_user_id(), relay_runtime_events(), relay_session_runtime_events(), SessionPreview, SessionRuntimeEvent, start_empty_session()

### Community 23 - "MCP Credential Proxy"
Cohesion: 0.12
Nodes (11): credential_from_env(), default_scope_for(), env_var(), environment_vault_loads_from_env_backed_server_config(), EnvironmentCredentialVault, headers_from_credential(), MCPCredentialProxy, McpSessionToken (+3 more)

### Community 24 - "Daytona Hand Provider"
Cohesion: 0.18
Nodes (10): build_url(), DaytonaHandProvider, default_headers(), derive_toolbox_url(), expect_success(), expect_success_json(), extract_workspace_id(), http_error() (+2 more)

### Community 25 - "Telegram Adapter"
Cohesion: 0.16
Nodes (13): channel_from_chat_and_reply(), handle_callback_query(), handle_message(), inbound_from_callback_query(), inbound_from_message(), inline_keyboard(), parse_message_id(), parses_approval_callback_into_control_message() (+5 more)

### Community 26 - "Memory Index & Log"
Cohesion: 0.13
Nodes (21): append_log_entry(), append_only_log_keeps_prior_entries(), compile_index(), compiled_index_stays_within_line_budget(), load_index_file(), load_log_file(), LogChange, LogEntry (+13 more)

### Community 27 - "Approval Card Widget"
Cohesion: 0.13
Nodes (15): approval_buttons(), approval_request(), ApprovalCallbackAction, border_line(), callback_data_roundtrips(), content_line(), prepare_outbound_message(), prepare_outbound_message_adds_inline_buttons_when_supported() (+7 more)

### Community 28 - "Brain Turn Streaming"
Cohesion: 0.13
Nodes (16): emit_tool_output_warning(), execute_pending_tool(), format_tool_output(), process_resolved_approval(), run_brain_turn(), run_brain_turn_with_tools(), run_brain_turn_with_tools_mode(), run_brain_turn_with_tools_stepwise() (+8 more)

### Community 29 - "Memory Consolidation"
Cohesion: 0.19
Nodes (19): canonical_port_claims(), confidence_rank(), consolidation_due_for_scope(), consolidation_resolves_dates_prunes_and_refreshes_index(), ConsolidationReport, decay_confidence(), extract_port_claims(), inbound_reference_counts() (+11 more)

### Community 30 - "FTS5 Search Index"
Cohesion: 0.19
Nodes (10): build_fts_query(), delete_page_entries(), FtsIndex, insert_page(), migrate(), parse_confidence(), parse_page_type(), parse_scope_key() (+2 more)

### Community 31 - "Session Picker View"
Cohesion: 0.18
Nodes (8): centered_rect(), filtered_sessions(), fuzzy_search_matches_title_and_last_message(), picker_haystack(), picker_selection_wraps_and_clamps(), preview(), render_session_picker(), SessionPickerState

### Community 32 - "Tool Approval Policies"
Cohesion: 0.22
Nodes (12): ApprovalRuleStore, glob_match(), parse_and_match_bash(), persistent_rule_matching_uses_glob_patterns(), PolicyCheck, read_tools_are_auto_approved_and_bash_requires_approval(), rule_matches(), rule_visible_to_workspace() (+4 more)

### Community 33 - "Skill Improver & Distiller"
Cohesion: 0.21
Nodes (15): build_distillation_prompt(), count_tool_calls(), extract_task_summary(), find_similar_skill(), maybe_distill_skill(), normalize_new_skill(), similarity_score(), tokenize() (+7 more)

### Community 34 - "Local Hand Provider"
Cohesion: 0.19
Nodes (2): detect_docker(), LocalHandProvider

### Community 35 - "Local Tool Tests"
Cohesion: 0.17
Nodes (9): bash_captures_stdout_and_stderr(), bash_respects_timeout(), EmptyMemoryStore, file_operations_reject_path_traversal(), file_read_reads_written_content(), file_search_finds_files_by_glob(), memory_read_returns_page_contents(), memory_search_returns_indexed_results() (+1 more)

### Community 36 - "Daytona Live Tests"
Cohesion: 0.18
Nodes (9): daytona_live_provider_handles_roundtrip_and_lifecycle(), daytona_live_router_lazy_provisions_reuses_and_isolates_sessions(), destroy_and_wait(), EmptyMemoryStore, live_config(), live_provider(), session(), wait_for_destroyed() (+1 more)

### Community 37 - "Memory Maintenance Tests"
Cohesion: 0.29
Nodes (12): branch_reconciliation_merges_conflicting_writes(), consolidation_decays_confidence_once_and_is_stable_on_repeat_runs(), consolidation_normalizes_dates_and_resolves_conflicts(), ingest_source_creates_summary_and_updates_related_pages(), maintenance_operations_append_log_and_keep_results_searchable(), manual_seeded_memory_fuzz_preserves_core_invariants(), manual_stress_ingest_reconcile_and_consolidate_preserves_invariants(), reconciliation_merges_multiple_branches_and_cleans_branch_directory() (+4 more)

### Community 38 - "E2B Live Tests"
Cohesion: 0.17
Nodes (8): destroy_and_wait(), e2b_live_provider_handles_roundtrip_and_lifecycle(), e2b_live_router_lazy_provisions_reuses_and_isolates_sessions(), EmptyMemoryStore, live_config(), live_provider(), session(), wait_for_destroyed()

### Community 39 - "OpenAI Provider Tests"
Cohesion: 0.25
Nodes (13): openai_provider_does_not_retry_after_partial_stream_output(), openai_provider_drops_oversized_metadata_values(), openai_provider_retries_after_rate_limit(), openai_provider_streams_parallel_tool_calls_in_order(), openai_provider_streams_tool_calls_from_responses_events(), openai_provider_translates_requests_to_responses_api(), openrouter_provider_normalizes_bare_claude_models(), openrouter_provider_sets_attribution_headers() (+5 more)

### Community 40 - "Encrypted Secret Vault"
Cohesion: 0.3
Nodes (4): decrypt_bytes(), encrypt_bytes(), file_vault_encrypts_and_decrypts_roundtrip(), FileVault

### Community 41 - "Prompt Injection Defense"
Cohesion: 0.26
Nodes (12): canary_detection_works(), check_canary(), classifier_flags_known_attack_patterns(), classify_input(), contains_canary_tokens(), inject_canary(), InputClassification, InputInspection (+4 more)

### Community 42 - "MCP Router Tests"
Cohesion: 0.2
Nodes (4): EmptyMemoryStore, router_discovers_stdio_mcp_tools_from_config(), router_injects_mcp_credentials_via_proxy(), session()

### Community 43 - "Toolbar Widget"
Cohesion: 0.27
Nodes (6): build_tab_spans(), render_toolbar(), short_session_id(), tab_title(), toolbar_labels_include_status_icons_and_tab_limit(), visible_window()

### Community 44 - "History Compilation Stage"
Cohesion: 0.25
Nodes (4): capabilities(), history_compiler_formats_user_and_assistant_turns(), history_processor_uses_preloaded_events(), HistoryCompiler

### Community 45 - "TUI Keybindings"
Cohesion: 0.2
Nodes (1): KeyAction

### Community 46 - "Prompt Widget"
Cohesion: 0.31
Nodes (2): build_textarea(), PromptWidget

### Community 47 - "Session Store Tests"
Cohesion: 0.2
Nodes (0): 

### Community 48 - "Tool Card Widget"
Cohesion: 0.42
Nodes (7): border_line(), content_line(), render_tool_card(), status_label(), status_style(), truncate_to_width(), wrap_text()

### Community 49 - "Memory Store Tests"
Cohesion: 0.43
Nodes (6): create_read_update_and_delete_wiki_pages(), fts_search_finds_ranked_results(), fts_search_handles_hyphenated_queries(), rebuild_search_index_from_files_restores_results(), sample_page(), user_and_workspace_scopes_are_separate()

### Community 50 - "CLI Entry Point"
Cohesion: 0.29
Nodes (4): Cli, Command, doctor_report(), doctor_report_includes_model_and_paths()

### Community 51 - "Instruction Stage"
Cohesion: 0.38
Nodes (2): instruction_processor_appends_config_backed_sections(), InstructionProcessor

### Community 52 - "Chat View"
Cohesion: 0.6
Nodes (5): max_scroll(), render_chat(), transcript_lines(), wrap_line(), wrap_prefixed()

### Community 53 - "Cache Optimizer Stage"
Cohesion: 0.4
Nodes (2): cache_optimizer_validates_cache_breakpoint(), CacheOptimizer

### Community 54 - "Identity Stage"
Cohesion: 0.4
Nodes (2): identity_processor_appends_system_prompt(), IdentityProcessor

### Community 55 - "CLI Exec Mode"
Cohesion: 0.6
Nodes (4): exec_mode_formats_tool_updates_compactly(), format_tool_update(), resolve_exec_approval(), run_exec()

### Community 56 - "Anthropic Provider Tests"
Cohesion: 0.4
Nodes (0): 

### Community 57 - "Bash Tool"
Cohesion: 0.6
Nodes (3): BashToolInput, execute_docker(), execute_local()

### Community 58 - "Memory Preload Stage"
Cohesion: 0.5
Nodes (5): load_preloaded_memory, MemoryRetriever (Stage 5), MEMORY_STAGE_DATA_METADATA_KEY, PreloadedMemoryStageData, truncate_excerpt

### Community 59 - "File Search Tool"
Cohesion: 0.67
Nodes (3): collect_matches(), execute(), FileSearchInput

### Community 60 - "File Read Tool"
Cohesion: 0.67
Nodes (3): execute(), FileReadInput, resolve_sandbox_path()

### Community 61 - "Chat Harness Example"
Cohesion: 0.83
Nodes (3): main(), resolve_session_db_path(), run_prompt()

### Community 62 - "Mock MCP Server"
Cohesion: 0.67
Nodes (0): 

### Community 63 - "File Write Tool"
Cohesion: 0.67
Nodes (1): FileWriteInput

### Community 64 - "OpenAI Live Test"
Cohesion: 1.0
Nodes (0): 

### Community 65 - "OpenRouter Live Test"
Cohesion: 1.0
Nodes (0): 

### Community 66 - "Anthropic Live Test"
Cohesion: 1.0
Nodes (0): 

### Community 67 - "Docker Hardening Test"
Cohesion: 1.0
Nodes (0): 

### Community 68 - "Brain Live Harness Test"
Cohesion: 1.0
Nodes (0): 

### Community 69 - "Compaction"
Cohesion: 1.0
Nodes (0): 

### Community 70 - "Search Query Extraction"
Cohesion: 1.0
Nodes (2): extract_search_keywords (stopword filter), extract_search_query

### Community 71 - "Relevant Memory Page"
Cohesion: 1.0
Nodes (1): RelevantMemoryPage

## Knowledge Gaps
- **168 isolated node(s):** `LogChange`, `LogEntry`, `IngestReport`, `PageFrontmatter`, `ChangeOperation` (+163 more)
  These have ≤1 connection - possible missing edges or undocumented components.
- **Thin community `OpenAI Live Test`** (2 nodes): `openai_live.rs`, `openai_live_completion_returns_expected_answer()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `OpenRouter Live Test`** (2 nodes): `openrouter_live.rs`, `openrouter_live_completion_returns_expected_answer()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Anthropic Live Test`** (2 nodes): `anthropic_live.rs`, `anthropic_live_completion_returns_expected_answer()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Docker Hardening Test`** (2 nodes): `docker_hardening.rs`, `docker_container_runs_with_hardening()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Brain Live Harness Test`** (2 nodes): `live_harness.rs`, `live_brain_turn_returns_brain_response()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Compaction`** (2 nodes): `compaction.rs`, `maybe_compact()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Search Query Extraction`** (2 nodes): `extract_search_keywords (stopword filter)`, `extract_search_query`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Relevant Memory Page`** (1 nodes): `RelevantMemoryPage`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **What connects `LogChange`, `LogEntry`, `IngestReport` to the rest of the system?**
  _168 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `Core Domain IDs` be split into smaller, more focused modules?**
  _Cohesion score 0.02 - nodes in this community are weakly interconnected._
- **Should `Context Pipeline` be split into smaller, more focused modules?**
  _Cohesion score 0.04 - nodes in this community are weakly interconnected._
- **Should `TUI App State` be split into smaller, more focused modules?**
  _Cohesion score 0.09 - nodes in this community are weakly interconnected._
- **Should `Skill Document Format` be split into smaller, more focused modules?**
  _Cohesion score 0.07 - nodes in this community are weakly interconnected._
- **Should `Tool Router & Policies` be split into smaller, more focused modules?**
  _Cohesion score 0.06 - nodes in this community are weakly interconnected._
- **Should `Brain Turn Tests` be split into smaller, more focused modules?**
  _Cohesion score 0.05 - nodes in this community are weakly interconnected._