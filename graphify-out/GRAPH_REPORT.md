# Graph Report - .  (2026-04-10)

## Corpus Check
- 127 files · ~124,777 words
- Verdict: corpus is large enough that graph structure adds value.

## Summary
- 2491 nodes · 4429 edges · 86 communities detected
- Extraction: 100% EXTRACTED · 0% INFERRED · 0% AMBIGUOUS
- Token cost: 0 input · 0 output

## God Nodes (most connected - your core abstractions)
1. `App` - 73 edges
2. `ChatRuntime` - 37 edges
3. `LocalChatRuntime` - 32 edges
4. `DaemonChatRuntime` - 31 edges
5. `SkillFrontmatter` - 31 edges
6. `PostgresSessionStore` - 26 edges
7. `TursoSessionStore` - 23 edges
8. `FileMemoryStore` - 22 edges
9. `MemoryViewState` - 22 edges
10. `LocalOrchestrator` - 22 edges

## Surprising Connections (you probably didn't know these)
- None detected - all connections are within the same source files.

## Communities

### Community 0 - "Core Domain Types"
Cohesion: 0.01
Nodes (98): ActionButton, ApprovalDecision, ApprovalField, ApprovalFileDiff, ApprovalPrompt, ApprovalRequest, ApprovalRule, Attachment (+90 more)

### Community 1 - "TUI App State"
Cohesion: 0.05
Nodes (46): App, app_state_transitions_follow_idle_composing_running_waiting_idle(), AppMode, approval_status_and_note(), ApprovalCardStatus, ApprovalEntry, ChatEntry, collect_sandbox_files() (+38 more)

### Community 2 - "TUI Chat Runtime"
Cohesion: 0.03
Nodes (19): ChatRuntime, daemon_connect(), daemon_expect_ack(), daemon_is_available(), daemon_request(), daemon_send_command(), DaemonChatRuntime, expand_local_path() (+11 more)

### Community 3 - "Memory Pipeline & Views"
Cohesion: 0.03
Nodes (38): centered_rect(), ensure_prefix(), extract_search_keywords(), extract_search_query(), filter_pages(), fuzzy_filter_matches_titles(), infer_page_title(), infer_page_type() (+30 more)

### Community 4 - "File Memory Store"
Cohesion: 0.04
Nodes (46): MoaError, build_provider_from_config(), build_provider_from_selection(), explicit_provider_prefix_overrides_inference(), infer_provider_name(), infers_anthropic_for_claude_models(), infers_openai_for_gpt_models(), infers_openrouter_for_vendor_prefixed_models() (+38 more)

### Community 5 - "Brain Turn Tests"
Cohesion: 0.05
Nodes (18): always_allow_rule_persists_and_skips_next_approval(), canary_leaks_in_tool_input_are_detected_and_blocked(), CanaryLeakLlmProvider, CapturingTextLlmProvider, FixedPageMemoryStore, malicious_tool_results_are_wrapped_as_untrusted_content(), MaliciousToolOutputLlmProvider, MemoryWriteLoopLlmProvider (+10 more)

### Community 6 - "Skill Document Format"
Cohesion: 0.07
Nodes (22): build_skill_path(), confidence_for_skill(), defaults_missing_moa_metadata(), estimate_skill_tokens(), format_timestamp(), humanize_skill_name(), is_valid_skill_name(), metadata_csv() (+14 more)

### Community 7 - "Local Orchestrator"
Cohesion: 0.06
Nodes (18): accept_user_message(), append_event(), detect_docker(), docker_status(), DockerSandbox, flush_next_queued_message(), flush_pending_signal(), flush_queued_messages() (+10 more)

### Community 8 - "Provider Common Layer"
Cohesion: 0.06
Nodes (31): build_function_tool(), build_http_client(), build_responses_request(), consume_responses_stream_once(), is_rate_limit_error(), is_rate_limit_message(), map_openai_error(), metadata_as_strings() (+23 more)

### Community 9 - "Config & Errors"
Cohesion: 0.05
Nodes (24): CloudConfig, CloudFlyioConfig, CloudHandsConfig, CloudTemporalConfig, config_loads_from_file(), config_rejects_zero_neon_checkpoint_limit_when_enabled(), DaemonConfig, DatabaseBackend (+16 more)

### Community 10 - "Temporal Orchestrator"
Cohesion: 0.06
Nodes (27): activity_options(), apply_approval_decision(), ApprovalDecisionActivityInput, ApprovalSignalInput, brain_turn_activity_options(), CancelMode, connect_temporal_client(), event_to_runtime_event() (+19 more)

### Community 11 - "Postgres Session Store"
Cohesion: 0.06
Nodes (19): event_hand_id(), normalize_event_search_query(), PostgresSessionStore, qualified_name(), approval_rule_from_row(), event_record_from_row(), event_type_from_db(), pending_signal_from_row() (+11 more)

### Community 12 - "Local Orchestrator Tests"
Cohesion: 0.08
Nodes (35): approval_requested_event_persists_full_prompt_details(), collect_runtime_events_until(), denied_tool_preserves_queued_follow_up(), FileWriteApprovalProvider, hard_cancel_aborts_stream_and_emits_cancelled_status(), last_user_message(), list_sessions_includes_active_session(), memory_maintenance_runs_due_workspace_consolidation() (+27 more)

### Community 13 - "Tool Router & Policies"
Cohesion: 0.08
Nodes (19): approval_diffs_for(), approval_fields_for(), approval_pattern_for(), default_cloud_provider(), execute_tool_policy(), expand_local_path(), hand_id(), language_hint_for_path() (+11 more)

### Community 14 - "Turso Session Store"
Cohesion: 0.07
Nodes (18): event_record_from_row(), event_type_from_db(), optional_i64(), optional_text(), parse_timestamp(), platform_from_db(), session_meta_from_row(), session_status_from_db() (+10 more)

### Community 15 - "Anthropic Provider"
Cohesion: 0.07
Nodes (31): anthropic_content_blocks(), anthropic_content_blocks_render_text_and_json_as_text_blocks(), anthropic_message(), anthropic_message_wraps_assistant_tool_calls_as_tool_use_blocks(), anthropic_message_wraps_tool_results_with_tool_use_id(), anthropic_tool_from_schema(), anthropic_tool_from_schema_moves_parameters_into_input_schema(), AnthropicProvider (+23 more)

### Community 16 - "CLI Entry Point"
Cohesion: 0.07
Nodes (39): apply_config_update(), checkpoint_cleanup_report(), checkpoint_create_report(), checkpoint_list_report(), checkpoint_rollback_report(), CheckpointCommand, Cli, cloud_sync_status() (+31 more)

### Community 17 - "Settings View"
Cohesion: 0.08
Nodes (29): apply_text_edit(), bool_label(), category_label(), centered_rect(), cycle_string(), cycle_u32(), cycle_u64(), cycle_usize() (+21 more)

### Community 18 - "Neon Branch Manager"
Cohesion: 0.09
Nodes (25): checkpoint_branch_names_follow_moa_prefix(), checkpoint_info_from_branch(), checkpoint_label_from_name(), cleanup_expired_deletes_only_old_moa_branches(), create_checkpoint_refuses_to_exceed_capacity(), create_checkpoint_sends_expected_request_and_returns_handle(), discard_checkpoint_calls_delete_endpoint(), format_checkpoint_branch_name() (+17 more)

### Community 19 - "Memory Consolidation"
Cohesion: 0.08
Nodes (40): canonical_port_claims(), confidence_rank(), consolidation_due_for_scope(), consolidation_resolves_dates_prunes_and_refreshes_index(), ConsolidationReport, decay_confidence(), extract_port_claims(), inbound_reference_counts() (+32 more)

### Community 20 - "Daemon Service"
Cohesion: 0.12
Nodes (39): daemon_create_session_uses_explicit_client_scope(), daemon_health_endpoint_responds_when_cloud_enabled(), daemon_info(), daemon_lists_session_previews(), daemon_log_path(), daemon_logs(), daemon_pid_path(), daemon_ping_create_and_shutdown_roundtrip() (+31 more)

### Community 21 - "Message Renderer"
Cohesion: 0.09
Nodes (23): append_piece(), discord_renderer_attaches_buttons_to_last_chunk_only(), discord_renderer_uses_embed_limit_for_long_text(), DiscordRenderChunk, DiscordRenderer, render_approval_request(), render_diff(), render_tool_card() (+15 more)

### Community 22 - "Temporal Orchestrator Tests"
Cohesion: 0.11
Nodes (28): build_temporal_helper_binary(), delayed_text_stream(), last_user_message(), mock_capabilities(), spawn_temporal_helper(), temporal_helper_binary(), temporal_orchestrator_live_anthropic_smoke(), temporal_orchestrator_processes_multiple_queued_messages_fifo() (+20 more)

### Community 23 - "Brain Turn Execution"
Cohesion: 0.1
Nodes (31): append_event(), buffer_queued_message(), drain_signal_queue(), emit_tool_output_warning(), execute_pending_tool(), execute_tool(), format_tool_output(), handle_stream_signal() (+23 more)

### Community 24 - "Diff View"
Cohesion: 0.1
Nodes (25): build_diff_file_view(), default_mode_for_width(), diff_line_style(), DiffFileView, DiffMode, DiffViewState, highlighted_spans(), pad_or_truncate() (+17 more)

### Community 25 - "Discord Adapter"
Cohesion: 0.12
Nodes (18): approval_callback_maps_to_control_message(), attachments_from_message(), context_from_component(), discord_button(), discord_create_message(), discord_create_message_includes_buttons_for_last_chunk(), discord_edit_message(), discord_embed() (+10 more)

### Community 26 - "E2B Sandbox Provider"
Cohesion: 0.14
Nodes (16): build_url(), ConnectedSandbox, decode_stream_chunk(), default_headers(), E2BHandProvider, encode_connect_request(), encode_test_envelopes(), envd_headers() (+8 more)

### Community 27 - "Slack Adapter"
Cohesion: 0.13
Nodes (19): handle_interaction_event(), handle_push_event(), inbound_from_app_mention(), inbound_from_interaction_event(), inbound_from_message_event(), inbound_from_push_event(), interaction_origin(), parses_approval_callback_into_control_message() (+11 more)

### Community 28 - "Wiki & Memory Branching"
Cohesion: 0.12
Nodes (28): append_change_manifest(), branch_dir(), branch_file_path(), branch_root(), ChangeOperation, ChangeRecord, list_branches(), merge_markdown() (+20 more)

### Community 29 - "MCP Client"
Cohesion: 0.14
Nodes (16): flatten_call_result(), flatten_tool_result_aggregates_text_items(), header_map_from_pairs(), http_client_sends_headers_and_parses_jsonrpc(), MCPClient, McpDiscoveredTool, McpTransport, parse_jsonrpc_result() (+8 more)

### Community 30 - "MCP Credential Proxy"
Cohesion: 0.12
Nodes (11): credential_from_env(), default_scope_for(), env_var(), environment_vault_loads_from_env_backed_server_config(), EnvironmentCredentialVault, headers_from_credential(), MCPCredentialProxy, McpSessionToken (+3 more)

### Community 31 - "Daytona Workspace Provider"
Cohesion: 0.17
Nodes (10): build_url(), DaytonaHandProvider, default_headers(), derive_toolbox_url(), expect_success(), expect_success_json(), extract_workspace_id(), http_error() (+2 more)

### Community 32 - "Telegram Adapter"
Cohesion: 0.16
Nodes (13): channel_from_chat_and_reply(), handle_callback_query(), handle_message(), inbound_from_callback_query(), inbound_from_message(), inline_keyboard(), parse_message_id(), parses_approval_callback_into_control_message() (+5 more)

### Community 33 - "Local Tool Tests"
Cohesion: 0.15
Nodes (16): bash_captures_stdout_and_stderr(), bash_respects_timeout(), EmptyMemoryStore, file_operations_reject_path_traversal(), file_read_reads_written_content(), file_search_finds_files_by_glob(), local_bash_hard_cancel_kills_running_process(), memory_read_returns_page_contents() (+8 more)

### Community 34 - "Docker File Operations"
Cohesion: 0.11
Nodes (15): container_path_validation_accepts_workspace_absolute_paths(), container_path_validation_rejects_absolute_paths_outside_workspace(), container_path_validation_rejects_traversal(), docker_file_read(), docker_file_search(), docker_file_write(), docker_find_args(), docker_read_args() (+7 more)

### Community 35 - "History Compilation Stage"
Cohesion: 0.12
Nodes (7): capabilities(), history_compiler_formats_user_and_assistant_turns(), history_compiler_preserves_structured_tool_call_invocation(), history_compiler_preserves_structured_tool_result_blocks(), history_processor_loads_events_directly_from_session_store(), HistoryCompiler, MockSessionStore

### Community 36 - "Skill Injection Stage"
Cohesion: 0.11
Nodes (9): distills_skill_after_tool_heavy_session(), improves_existing_skill_when_better_flow_is_found(), MockLlm, session(), skill_injector_marks_breakpoint_without_skills(), skill_injector_marks_cache_breakpoint_and_formats_metadata(), SkillInjector, StubSkillMemoryStore (+1 more)

### Community 37 - "Approval Card Widget"
Cohesion: 0.13
Nodes (15): approval_buttons(), approval_request(), ApprovalCallbackAction, border_line(), callback_data_roundtrips(), content_line(), prepare_outbound_message(), prepare_outbound_message_adds_inline_buttons_when_supported() (+7 more)

### Community 38 - "Pipeline & Session Helpers"
Cohesion: 0.15
Nodes (14): build_default_pipeline(), build_default_pipeline_with_tools(), ContextPipeline, estimate_tokens(), pipeline_runner_executes_stages_in_order(), PipelineStageReport, test_create_and_get_session(), test_emit_and_get_events() (+6 more)

### Community 39 - "Prompt Widget"
Cohesion: 0.17
Nodes (8): build_textarea(), byte_index_for_char_position(), offset_for_position(), position_for_offset(), PromptCompletionKind, PromptCompletionState, PromptWidget, render_completion_menu()

### Community 40 - "Session Database Backend"
Cohesion: 0.1
Nodes (2): create_session_store(), SessionDatabase

### Community 41 - "Full Text Search"
Cohesion: 0.19
Nodes (10): build_fts_query(), delete_page_entries(), FtsIndex, insert_page(), migrate(), parse_confidence(), parse_page_type(), parse_scope_key() (+2 more)

### Community 42 - "CLI API Server"
Cohesion: 0.15
Nodes (10): ApiState, build_api_router(), health_endpoint_returns_ok(), runtime_event_stream(), session_stream(), session_stream_returns_not_found_when_runtime_is_unavailable(), session_stream_returns_sse_content_type(), start_api_server() (+2 more)

### Community 43 - "Session Picker View"
Cohesion: 0.18
Nodes (8): centered_rect(), filtered_sessions(), fuzzy_search_matches_title_and_last_message(), picker_haystack(), picker_selection_wraps_and_clamps(), preview(), render_session_picker(), SessionPickerState

### Community 44 - "Tool Approval Policies"
Cohesion: 0.22
Nodes (12): ApprovalRuleStore, glob_match(), parse_and_match_bash(), persistent_rule_matching_uses_glob_patterns(), PolicyCheck, read_tools_are_auto_approved_and_bash_requires_approval(), rule_matches(), rule_visible_to_workspace() (+4 more)

### Community 45 - "Skill Improver & Distiller"
Cohesion: 0.21
Nodes (15): build_distillation_prompt(), count_tool_calls(), extract_task_summary(), find_similar_skill(), maybe_distill_skill(), normalize_new_skill(), similarity_score(), tokenize() (+7 more)

### Community 46 - "OpenAI Provider Tests"
Cohesion: 0.22
Nodes (15): openai_provider_does_not_retry_after_partial_stream_output(), openai_provider_drops_oversized_metadata_values(), openai_provider_retries_after_rate_limit(), openai_provider_serializes_assistant_tool_calls_as_function_call_items(), openai_provider_serializes_tool_result_messages_as_function_call_output(), openai_provider_streams_parallel_tool_calls_in_order(), openai_provider_streams_tool_calls_from_responses_events(), openai_provider_translates_requests_to_responses_api() (+7 more)

### Community 47 - "Daytona Live Tests"
Cohesion: 0.18
Nodes (9): daytona_live_provider_handles_roundtrip_and_lifecycle(), daytona_live_router_lazy_provisions_reuses_and_isolates_sessions(), destroy_and_wait(), EmptyMemoryStore, live_config(), live_provider(), session(), wait_for_destroyed() (+1 more)

### Community 48 - "Memory Maintenance Tests"
Cohesion: 0.29
Nodes (12): branch_reconciliation_merges_conflicting_writes(), consolidation_decays_confidence_once_and_is_stable_on_repeat_runs(), consolidation_normalizes_dates_and_resolves_conflicts(), ingest_source_creates_summary_and_updates_related_pages(), maintenance_operations_append_log_and_keep_results_searchable(), manual_seeded_memory_fuzz_preserves_core_invariants(), manual_stress_ingest_reconcile_and_consolidate_preserves_invariants(), reconciliation_merges_multiple_branches_and_cleans_branch_directory() (+4 more)

### Community 49 - "Session Store Tests"
Cohesion: 0.21
Nodes (11): approval_rules_round_trip(), create_session_and_emit_events(), fts_search_finds_events(), get_events_with_range_filter(), list_sessions_filters_by_workspace(), new_local_store(), pending_signal_round_trip_and_resolution(), update_status_persists_changes() (+3 more)

### Community 50 - "E2B Live Tests"
Cohesion: 0.17
Nodes (8): destroy_and_wait(), e2b_live_provider_handles_roundtrip_and_lifecycle(), e2b_live_router_lazy_provisions_reuses_and_isolates_sessions(), EmptyMemoryStore, live_config(), live_provider(), session(), wait_for_destroyed()

### Community 51 - "Encrypted Secret Vault"
Cohesion: 0.3
Nodes (4): decrypt_bytes(), encrypt_bytes(), file_vault_encrypts_and_decrypts_roundtrip(), FileVault

### Community 52 - "Core Event Model"
Cohesion: 0.15
Nodes (3): approval_requested_event_round_trips_full_prompt(), Event, sample_approval_prompt()

### Community 53 - "Command Palette"
Cohesion: 0.22
Nodes (6): centered_rect(), filtered_actions(), palette_fuzzy_search_prefers_matching_actions(), PaletteAction, PaletteState, render_palette()

### Community 54 - "MCP Router Tests"
Cohesion: 0.19
Nodes (6): EmptyMemoryStore, router_calls_http_mcp_server_and_surfaces_jsonrpc_errors(), router_discovers_and_calls_streamable_http_tools_with_sse_responses(), router_discovers_stdio_mcp_tools_from_config(), router_injects_mcp_credentials_via_proxy(), session()

### Community 55 - "Injection Detection"
Cohesion: 0.26
Nodes (12): canary_detection_works(), check_canary(), classifier_flags_known_attack_patterns(), classify_input(), contains_canary_tokens(), inject_canary(), InputClassification, InputInspection (+4 more)

### Community 56 - "TUI Keybindings"
Cohesion: 0.17
Nodes (1): KeyAction

### Community 57 - "Toolbar Widget"
Cohesion: 0.24
Nodes (7): build_tab_spans(), render_toolbar(), short_session_id(), tab_title(), toolbar_labels_include_status_icons_and_tab_limit(), ToolbarMetrics, visible_window()

### Community 58 - "Live Provider Roundtrip Tests"
Cohesion: 0.3
Nodes (8): available_live_providers(), live_orchestrator_with_provider(), live_providers_complete_tool_approval_roundtrip_when_available(), LiveProvider, wait_for_approval_request(), wait_for_file(), wait_for_final_response(), wait_for_status()

### Community 59 - "Memory Store Tests"
Cohesion: 0.36
Nodes (8): create_read_update_and_delete_wiki_pages(), delete_page_removes_only_the_requested_scope(), fts_search_finds_ranked_results(), fts_search_handles_hyphenated_queries(), rebuild_search_index_from_files_restores_results(), sample_page(), user_and_workspace_scopes_are_separate(), write_page_creates_and_reads_pages_in_explicit_scopes()

### Community 60 - "Postgres Store Tests"
Cohesion: 0.36
Nodes (6): cleanup_schema(), create_test_store(), postgres_event_payloads_round_trip_as_jsonb(), postgres_session_ids_are_native_uuid_and_concurrent_emits_are_serialized(), postgres_shared_session_store_contract(), with_test_store()

### Community 61 - "OpenAI Schema Compiler"
Cohesion: 0.38
Nodes (9): compile_for_openai_strict(), compile_for_openai_strict_adds_additional_properties_false_recursively(), compile_for_openai_strict_does_not_duplicate_null_in_type_arrays(), compile_for_openai_strict_makes_optional_properties_required_and_nullable(), compile_for_openai_strict_preserves_existing_required_properties(), compile_for_openai_strict_strips_validation_only_keywords(), make_nullable(), make_strict_compatible() (+1 more)

### Community 62 - "Tool Card Widget"
Cohesion: 0.42
Nodes (7): border_line(), content_line(), render_tool_card(), status_label(), status_style(), truncate_to_width(), wrap_text()

### Community 63 - "Neon Branch Manager Tests"
Cohesion: 0.5
Nodes (7): live_neon_config(), live_neon_config_with_limit(), neon_branch_manager_create_list_get_rollback_and_discard_checkpoint(), neon_checkpoint_branch_connection_is_copy_on_write(), neon_checkpoint_capacity_limit_rejects_extra_branch(), neon_checkpoint_cleanup_without_expired_branches_returns_zero(), wait_for_workspace_session_count()

### Community 64 - "Live Provider Matrix Tests"
Cohesion: 0.39
Nodes (4): available_live_providers(), live_providers_answer_simple_prompt_across_available_keys(), live_providers_emit_tool_calls_across_available_keys(), LiveProvider

### Community 65 - "Temporal Worker Helper"
Cohesion: 0.32
Nodes (4): helper_config(), HelperProvider, main(), main_impl()

### Community 66 - "Stub Tool Definition"
Cohesion: 0.25
Nodes (1): StubTool

### Community 67 - "Instruction Stage"
Cohesion: 0.38
Nodes (2): instruction_processor_appends_config_backed_sections(), InstructionProcessor

### Community 68 - "Chat View"
Cohesion: 0.6
Nodes (5): max_scroll(), render_chat(), transcript_lines(), wrap_line(), wrap_prefixed()

### Community 69 - "Bash Tool"
Cohesion: 0.47
Nodes (3): BashToolInput, execute_docker(), execute_local()

### Community 70 - "Cache Optimizer Stage"
Cohesion: 0.4
Nodes (2): cache_optimizer_validates_cache_breakpoint(), CacheOptimizer

### Community 71 - "Identity Stage"
Cohesion: 0.4
Nodes (2): identity_processor_appends_system_prompt(), IdentityProcessor

### Community 72 - "CLI Exec Mode"
Cohesion: 0.6
Nodes (4): exec_mode_formats_tool_updates_compactly(), format_tool_update(), resolve_exec_approval(), run_exec()

### Community 73 - "Sidebar Widget"
Cohesion: 0.5
Nodes (2): render_sidebar(), section_title()

### Community 74 - "Anthropic Provider Tests"
Cohesion: 0.4
Nodes (0): 

### Community 75 - "File Search Tool"
Cohesion: 0.5
Nodes (3): collect_matches(), execute(), FileSearchInput

### Community 76 - "Chat Harness Example"
Cohesion: 0.83
Nodes (3): main(), resolve_session_db_path(), run_prompt()

### Community 77 - "Mock MCP Server"
Cohesion: 0.67
Nodes (0): 

### Community 78 - "TUI Workflow Tests"
Cohesion: 1.0
Nodes (0): 

### Community 79 - "Turso Schema Migration"
Cohesion: 1.0
Nodes (0): 

### Community 80 - "OpenAI Live Test"
Cohesion: 1.0
Nodes (0): 

### Community 81 - "OpenRouter Live Test"
Cohesion: 1.0
Nodes (0): 

### Community 82 - "Anthropic Live Test"
Cohesion: 1.0
Nodes (0): 

### Community 83 - "Docker Hardening Test"
Cohesion: 1.0
Nodes (0): 

### Community 84 - "Brain Live Harness Test"
Cohesion: 1.0
Nodes (0): 

### Community 85 - "Compaction"
Cohesion: 1.0
Nodes (0): 

## Knowledge Gaps
- **199 isolated node(s):** `LogChange`, `LogEntry`, `IngestReport`, `PageFrontmatter`, `ChangeOperation` (+194 more)
  These have ≤1 connection - possible missing edges or undocumented components.
- **Thin community `TUI Workflow Tests`** (2 nodes): `tui_workflows.rs`, `tui_manual_workflow_opens_memory_settings_and_palette()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Turso Schema Migration`** (2 nodes): `schema_turso.rs`, `migrate()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
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

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **What connects `LogChange`, `LogEntry`, `IngestReport` to the rest of the system?**
  _199 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `Core Domain Types` be split into smaller, more focused modules?**
  _Cohesion score 0.01 - nodes in this community are weakly interconnected._
- **Should `TUI App State` be split into smaller, more focused modules?**
  _Cohesion score 0.05 - nodes in this community are weakly interconnected._
- **Should `TUI Chat Runtime` be split into smaller, more focused modules?**
  _Cohesion score 0.03 - nodes in this community are weakly interconnected._
- **Should `Memory Pipeline & Views` be split into smaller, more focused modules?**
  _Cohesion score 0.03 - nodes in this community are weakly interconnected._
- **Should `File Memory Store` be split into smaller, more focused modules?**
  _Cohesion score 0.04 - nodes in this community are weakly interconnected._
- **Should `Brain Turn Tests` be split into smaller, more focused modules?**
  _Cohesion score 0.05 - nodes in this community are weakly interconnected._