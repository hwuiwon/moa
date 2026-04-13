# Graph Report - .  (2026-04-13)

## Corpus Check
- Large corpus: 305 files · ~187,085 words. Semantic extraction will be expensive (many Claude tokens). Consider running on a subfolder, or use --no-semantic to run AST-only.

## Summary
- 2994 nodes · 5161 edges · 122 communities detected
- Extraction: 99% EXTRACTED · 1% INFERRED · 0% AMBIGUOUS · INFERRED: 33 edges (avg confidence: 0.8)
- Token cost: 0 input · 0 output

## God Nodes (most connected - your core abstractions)
1. `ChatRuntime` - 38 edges
2. `LocalChatRuntime` - 33 edges
3. `SkillFrontmatter` - 33 edges
4. `DaemonChatRuntime` - 32 edges
5. `clone_runtime()` - 30 edges
6. `PostgresSessionStore` - 28 edges
7. `TursoSessionStore` - 27 edges
8. `FileMemoryStore` - 22 edges
9. `LocalOrchestrator` - 22 edges
10. `ToolRouter` - 22 edges

## Surprising Connections (you probably didn't know these)
- `Instruction Layer Hierarchy (workspace over user instructions)` --semantically_similar_to--> `Provider-Native Web Search (bypass MOA tools)`  [AMBIGUOUS] [semantically similar]
  src/components/settings/general-settings.tsx → src/components/settings/tools-and-mcp-settings.tsx
- `AppearanceSettings Component` --semantically_similar_to--> `ThemeToggle Component`  [INFERRED] [semantically similar]
  src/components/settings/appearance-settings.tsx → src/components/theme-toggle.tsx
- `Approval Rules Read-Only Desktop Posture` --semantically_similar_to--> `MCP Server Editing Desktop UI Gap`  [INFERRED] [semantically similar]
  src/components/settings/approval-rules-settings.tsx → src/components/settings/tools-and-mcp-settings.tsx
- `MemoryTree Component` --semantically_similar_to--> `MemorySearch Component`  [INFERRED] [semantically similar]
  src/components/memory/memory-tree.tsx → src/components/memory/memory-search.tsx
- `MemoryEditor Component` --semantically_similar_to--> `MemoryPageViewer Component`  [INFERRED] [semantically similar]
  src/components/memory/memory-editor.tsx → src/components/memory/memory-page-viewer.tsx

## Hyperedges (group relationships)
- **Tool Approval Interrupt Flow** — src_components_chat_approval_card_tsx, src_components_chat_diff_viewer_tsx, concept_approval_store_registration [INFERRED 0.88]
- **Tool Approval Visibility Flow** — content_block_renderer, approval_card_component, session_info_panel_component [INFERRED 0.75]
- **Settings ArkType-to-Rust Validation Chain** — concept_arktype_validated_settings_forms, concept_rust_backed_config_validation, src_views_settings_view_tsx, src_components_settings_general_settings_tsx [INFERRED 0.85]
- **Memory Browser CRUD Flow** — src_views_memory_view_tsx, src_components_memory_memory_editor_tsx, src_components_memory_memory_page_viewer_tsx, src_components_memory_memory_tree_tsx [INFERRED 0.90]

## Communities

### Community 0 - "Core Domain Types"
Cohesion: 0.01
Nodes (124): ActionButton, AgentConfig, AgentConfigBody, AgentConfigDocument, ApprovalDecision, ApprovalField, ApprovalFileDiff, ApprovalPrompt (+116 more)

### Community 1 - "File Memory Store"
Cohesion: 0.02
Nodes (50): Evaluator, build_provider_from_config(), build_provider_from_selection(), explicit_provider_prefix_overrides_inference(), infer_provider_name(), infers_anthropic_for_claude_models(), infers_openai_for_gpt_models(), infers_openrouter_for_vendor_prefixed_models() (+42 more)

### Community 2 - "Brain Turn Tests"
Cohesion: 0.04
Nodes (25): always_allow_rule_persists_and_skips_next_approval(), canary_leaks_in_tool_input_are_detected_and_blocked(), CanaryLeakLlmProvider, CapturingTextLlmProvider, FixedPageMemoryStore, malicious_tool_results_are_wrapped_as_untrusted_content(), MaliciousToolOutputLlmProvider, MemoryIngestLoopLlmProvider (+17 more)

### Community 3 - "Event Taxonomy"
Cohesion: 0.04
Nodes (36): destroy_and_wait(), e2b_live_provider_handles_roundtrip_and_lifecycle(), e2b_live_router_lazy_provisions_reuses_and_isolates_sessions(), EmptyMemoryStore, live_config(), live_provider(), session(), wait_for_destroyed() (+28 more)

### Community 4 - "Memory Pipeline & Views"
Cohesion: 0.04
Nodes (30): count_ingest_pages(), derive_source_name_from_content(), extract_search_keywords(), extract_search_query(), format_ingest_report(), infer_page_title(), infer_page_type(), ingest_report_json() (+22 more)

### Community 5 - "Skill Document Format"
Cohesion: 0.07
Nodes (22): build_skill_path(), confidence_for_skill(), defaults_missing_moa_metadata(), estimate_skill_tokens(), format_timestamp(), humanize_skill_name(), is_valid_skill_name(), metadata_csv() (+14 more)

### Community 6 - "Config & Errors"
Cohesion: 0.04
Nodes (28): CloudConfig, CloudFlyioConfig, CloudHandsConfig, CloudTemporalConfig, compaction_config_defaults_are_applied(), CompactionConfig, config_loads_from_file(), config_rejects_zero_neon_checkpoint_limit_when_enabled() (+20 more)

### Community 7 - "CLI Entry Point"
Cohesion: 0.05
Nodes (53): apply_config_update(), checkpoint_cleanup_report(), checkpoint_create_report(), checkpoint_list_report(), checkpoint_rollback_report(), CheckpointCommand, Cli, cloud_sync_status() (+45 more)

### Community 8 - "Tool Router & Policies"
Cohesion: 0.07
Nodes (24): approval_diffs_for(), approval_fields_for(), approval_pattern_for(), default_cloud_provider(), execute_tool_policy(), expand_local_path(), hand_id(), language_hint_for_path() (+16 more)

### Community 9 - "Adaptive Tool Stats"
Cohesion: 0.06
Nodes (41): annotate_schema(), annotation_warns_on_low_success(), apply_tool_rankings(), cache_stability_preserves_identical_ranked_output(), collect_session_tool_observations(), compare_f64_asc(), compare_f64_desc(), compare_failure_last() (+33 more)

### Community 10 - "Anthropic Provider"
Cohesion: 0.07
Nodes (38): anthropic_content_blocks(), anthropic_content_blocks_render_text_and_json_as_text_blocks(), anthropic_message(), anthropic_message_wraps_assistant_tool_calls_as_tool_use_blocks(), anthropic_message_wraps_tool_results_with_tool_use_id(), anthropic_tool_from_schema(), anthropic_tool_from_schema_moves_parameters_into_input_schema(), AnthropicProvider (+30 more)

### Community 11 - "Postgres Session Store"
Cohesion: 0.06
Nodes (18): checkpoint_view(), event_hand_id(), normalize_event_search_query(), PostgresSessionStore, qualified_name(), approval_rule_from_row(), pending_signal_from_row(), pending_signal_type_from_db() (+10 more)

### Community 12 - "Local Orchestrator"
Cohesion: 0.07
Nodes (19): accept_user_message(), append_event(), detect_docker(), docker_status(), DockerSandbox, flush_next_queued_message(), flush_pending_signal(), flush_queued_messages() (+11 more)

### Community 13 - "Temporal Orchestrator"
Cohesion: 0.06
Nodes (27): activity_options(), apply_approval_decision(), ApprovalDecisionActivityInput, ApprovalSignalInput, brain_turn_activity_options(), CancelMode, connect_temporal_client(), event_to_runtime_event() (+19 more)

### Community 14 - "Approval Card Widget"
Cohesion: 0.05
Nodes (29): approval_buttons(), approval_request(), ApprovalCallbackAction, callback_data_roundtrips(), prepare_outbound_message(), prepare_outbound_message_adds_inline_buttons_when_supported(), prepare_outbound_message_degrades_to_text_prompt_without_buttons(), renderer_builds_platform_approval_buttons() (+21 more)

### Community 15 - "Turso Session Store"
Cohesion: 0.07
Nodes (17): optional_i64(), optional_text(), parse_timestamp(), platform_from_db(), session_meta_from_row(), session_status_from_db(), session_summary_from_row(), build_event_fts_query() (+9 more)

### Community 16 - "Local Orchestrator Tests"
Cohesion: 0.08
Nodes (35): approval_requested_event_persists_full_prompt_details(), collect_runtime_events_until(), denied_tool_preserves_queued_follow_up(), FileWriteApprovalProvider, hard_cancel_aborts_stream_and_emits_cancelled_status(), last_user_message(), list_sessions_includes_active_session(), memory_maintenance_runs_due_workspace_consolidation() (+27 more)

### Community 17 - "Eval Loader Tests"
Cohesion: 0.05
Nodes (38): EvalError, MoaAppError, MoaError, discover_configs(), discover_matching_toml_files(), discover_suites(), discover_toml_files(), load_agent_config() (+30 more)

### Community 18 - "Brain Turn Execution"
Cohesion: 0.07
Nodes (43): append_event(), approval_decision_label(), buffer_queued_message(), calculate_response_cost_cents(), drain_signal_queue(), emit_tool_output_warning(), execute_pending_tool(), execute_tool() (+35 more)

### Community 19 - "History Compilation Stage"
Cohesion: 0.08
Nodes (28): calculate_cost_cents(), CheckpointState, compaction_request(), event_summary_line(), latest_checkpoint_state(), maybe_compact(), maybe_compact_events(), non_checkpoint_events() (+20 more)

### Community 20 - "Eval Execution Engine"
Cohesion: 0.08
Nodes (25): CollectedExecution, collector_tracks_tool_steps_and_metrics(), estimate_cost(), TrajectoryCollector, truncate(), build_error_result(), cleanup_workspace(), dry_run_marks_results_skipped() (+17 more)

### Community 21 - "Skill Regression Testing"
Cohesion: 0.07
Nodes (41): build_distillation_prompt(), count_tool_calls(), extract_task_summary(), find_similar_skill(), maybe_distill_skill(), normalize_new_skill(), similarity_score(), tokenize() (+33 more)

### Community 22 - "Provider Common Layer"
Cohesion: 0.07
Nodes (32): build_function_tool(), build_http_client(), build_responses_request(), consume_responses_stream_once(), is_ignorable_openai_stream_error(), is_rate_limit_error(), is_rate_limit_message(), map_openai_error() (+24 more)

### Community 23 - "Neon Branch Manager"
Cohesion: 0.09
Nodes (25): checkpoint_branch_names_follow_moa_prefix(), checkpoint_info_from_branch(), checkpoint_label_from_name(), cleanup_expired_deletes_only_old_moa_branches(), create_checkpoint_refuses_to_exceed_capacity(), create_checkpoint_sends_expected_request_and_returns_handle(), discard_checkpoint_calls_delete_endpoint(), format_checkpoint_branch_name() (+17 more)

### Community 24 - "Skill Injection Stage"
Cohesion: 0.08
Nodes (27): allowed_tools(), budget_limit_skips_expensive_tests(), distills_skill_after_tool_heavy_session(), estimate_skill_tokens(), improvement_accepted_when_scores_better(), improvement_rejected_on_regression(), ImprovementAndEvalLlm, improves_existing_skill_when_better_flow_is_found() (+19 more)

### Community 25 - "Memory Consolidation"
Cohesion: 0.08
Nodes (40): canonical_port_claims(), confidence_rank(), consolidation_due_for_scope(), consolidation_resolves_dates_prunes_and_refreshes_index(), ConsolidationReport, decay_confidence(), extract_port_claims(), inbound_reference_counts() (+32 more)

### Community 26 - "Tauri Session Commands"
Cohesion: 0.1
Nodes (42): attach_runtime(), cancel_active_generation(), clone_runtime(), create_session(), delete_memory_page(), get_config(), get_runtime_info(), get_session() (+34 more)

### Community 27 - "Chat View"
Cohesion: 0.11
Nodes (40): appendNoticeBlock(), appendTextDelta(), applyApprovalDecision(), approvalBlockFromEvent(), approvalDecisionFromEvent(), asBoolean(), asNumber(), asPayload() (+32 more)

### Community 28 - "Frontend Shell & Settings"
Cohesion: 0.06
Nodes (41): AppLayout Chrome Query Invalidation Pattern, Approval Rules Read-Only Desktop Posture, ArkType-Validated Settings Forms Pattern, Global Cmd+K Command Palette Pattern, Context Window Pressure Visualization, Daemon Auto-Connect Runtime Flag, Dark-First Theme Strategy, Deferred Appearance Persistence (surface stability gate) (+33 more)

### Community 29 - "Daemon Service"
Cohesion: 0.12
Nodes (39): daemon_create_session_uses_explicit_client_scope(), daemon_health_endpoint_responds_when_cloud_enabled(), daemon_info(), daemon_lists_session_previews(), daemon_log_path(), daemon_logs(), daemon_pid_path(), daemon_ping_create_and_shutdown_roundtrip() (+31 more)

### Community 30 - "Message Renderer"
Cohesion: 0.09
Nodes (23): append_piece(), discord_renderer_attaches_buttons_to_last_chunk_only(), discord_renderer_uses_embed_limit_for_long_text(), DiscordRenderChunk, DiscordRenderer, render_approval_request(), render_diff(), render_tool_card() (+15 more)

### Community 31 - "Temporal Orchestrator Tests"
Cohesion: 0.11
Nodes (28): build_temporal_helper_binary(), delayed_text_stream(), last_user_message(), mock_capabilities(), spawn_temporal_helper(), temporal_helper_binary(), temporal_orchestrator_live_anthropic_smoke(), temporal_orchestrator_processes_multiple_queued_messages_fifo() (+20 more)

### Community 32 - "OpenRouter Provider"
Cohesion: 0.12
Nodes (15): build_request_body(), canonical_model_id(), capabilities_for_model(), capability_lookup_reuses_known_model_families(), completion_response_from_response(), consume_sse_events(), error_message(), native_tools() (+7 more)

### Community 33 - "Discord Adapter"
Cohesion: 0.12
Nodes (18): approval_callback_maps_to_control_message(), attachments_from_message(), context_from_component(), discord_button(), discord_create_message(), discord_create_message_includes_buttons_for_last_chunk(), discord_edit_message(), discord_embed() (+10 more)

### Community 34 - "E2B Sandbox Provider"
Cohesion: 0.14
Nodes (16): build_url(), ConnectedSandbox, decode_stream_chunk(), default_headers(), E2BHandProvider, encode_connect_request(), encode_test_envelopes(), envd_headers() (+8 more)

### Community 35 - "Slack Adapter"
Cohesion: 0.13
Nodes (19): handle_interaction_event(), handle_push_event(), inbound_from_app_mention(), inbound_from_interaction_event(), inbound_from_message_event(), inbound_from_push_event(), interaction_origin(), parses_approval_callback_into_control_message() (+11 more)

### Community 36 - "Pipeline & Session Helpers"
Cohesion: 0.09
Nodes (18): build_default_pipeline(), build_default_pipeline_with_runtime(), build_default_pipeline_with_tools(), cache_prefix_ratio(), ContextPipeline, estimate_tokens(), EvaluatorOptions, pipeline_runner_executes_stages_in_order() (+10 more)

### Community 37 - "Eval Agent Setup"
Cohesion: 0.13
Nodes (27): AgentEnvironment, apply_skill_overrides(), build_agent_environment(), build_agent_environment_with_provider(), build_eval_policies(), build_pipeline(), build_skill_memory_path(), build_tool_router() (+19 more)

### Community 38 - "Wiki & Memory Branching"
Cohesion: 0.12
Nodes (28): append_change_manifest(), branch_dir(), branch_file_path(), branch_root(), ChangeOperation, ChangeRecord, list_branches(), merge_markdown() (+20 more)

### Community 39 - "Tauri DTO Layer"
Cohesion: 0.1
Nodes (19): enum_label(), event_payload(), EventRecordDto, iso(), memory_scope_label(), MemorySearchResultDto, MoaConfigDto, ModelOptionDto (+11 more)

### Community 40 - "Local Tool Tests"
Cohesion: 0.12
Nodes (20): bash_captures_stdout_and_stderr(), bash_respects_timeout(), EmptyMemoryStore, file_operations_reject_path_traversal(), file_read_reads_written_content(), file_search_finds_files_by_glob(), local_bash_hard_cancel_kills_running_process(), memory_ingest_creates_source_page_and_related_pages() (+12 more)

### Community 41 - "MCP Client Discovery"
Cohesion: 0.14
Nodes (16): flatten_call_result(), flatten_tool_result_aggregates_text_items(), header_map_from_pairs(), http_client_sends_headers_and_parses_jsonrpc(), MCPClient, McpDiscoveredTool, McpTransport, parse_jsonrpc_result() (+8 more)

### Community 42 - "MCP Credential Proxy"
Cohesion: 0.12
Nodes (11): credential_from_env(), default_scope_for(), env_var(), environment_vault_loads_from_env_backed_server_config(), EnvironmentCredentialVault, headers_from_credential(), MCPCredentialProxy, McpSessionToken (+3 more)

### Community 43 - "Daytona Workspace Provider"
Cohesion: 0.17
Nodes (10): build_url(), DaytonaHandProvider, default_headers(), derive_toolbox_url(), expect_success(), expect_success_json(), extract_workspace_id(), http_error() (+2 more)

### Community 44 - "Telegram Adapter"
Cohesion: 0.16
Nodes (13): channel_from_chat_and_reply(), handle_callback_query(), handle_message(), inbound_from_callback_query(), inbound_from_message(), inline_keyboard(), parse_message_id(), parses_approval_callback_into_control_message() (+5 more)

### Community 45 - "Docker File Operations"
Cohesion: 0.11
Nodes (15): container_path_validation_accepts_workspace_absolute_paths(), container_path_validation_rejects_absolute_paths_outside_workspace(), container_path_validation_rejects_traversal(), docker_file_read(), docker_file_search(), docker_file_write(), docker_find_args(), docker_read_args() (+7 more)

### Community 46 - "CLI HTTP API Server"
Cohesion: 0.11
Nodes (14): ApiState, build_api_router(), health_endpoint_returns_ok(), runtime_event_stream(), session_stream(), session_stream_returns_not_found_when_runtime_is_unavailable(), session_stream_returns_sse_content_type(), start_api_server() (+6 more)

### Community 47 - "Session Store Tests"
Cohesion: 0.14
Nodes (17): approval_rules_round_trip(), create_session_and_emit_events(), fts_search_finds_events(), fts_search_uses_blob_preview(), get_events_with_range_filter(), identical_large_payloads_share_one_blob(), large_payload_offloaded_to_blob_store(), list_sessions_filters_by_workspace() (+9 more)

### Community 48 - "File Blob Store"
Cohesion: 0.18
Nodes (12): claim_check_from_value(), collect_blob_refs(), collect_large_strings(), decode_event_from_storage(), encode_event_for_storage(), expand_local_path(), file_blob_store_deletes_session_directory(), file_blob_store_is_content_addressed() (+4 more)

### Community 49 - "LLM Span Instrumentation"
Cohesion: 0.15
Nodes (15): calculate_cost(), calculate_cost_with_cached(), cost_calculation_correct(), has_meaningful_output(), llm_span_name(), LLMSpanAttributes, LLMSpanRecorder, metadata_f64() (+7 more)

### Community 50 - "OpenAI Provider Tests"
Cohesion: 0.17
Nodes (19): openai_provider_does_not_retry_after_partial_stream_output(), openai_provider_drops_oversized_metadata_values(), openai_provider_includes_native_web_search_when_enabled(), openai_provider_omits_native_web_search_when_disabled(), openai_provider_retries_after_rate_limit(), openai_provider_serializes_assistant_tool_calls_as_function_call_items(), openai_provider_serializes_tool_result_messages_as_function_call_output(), openai_provider_streams_parallel_tool_calls_in_order() (+11 more)

### Community 51 - "Session Database Backend"
Cohesion: 0.1
Nodes (2): create_session_store(), SessionDatabase

### Community 52 - "Full Text Search"
Cohesion: 0.19
Nodes (10): build_fts_query(), delete_page_entries(), FtsIndex, insert_page(), migrate(), parse_confidence(), parse_page_type(), parse_scope_key() (+2 more)

### Community 53 - "Tool Approval Policies"
Cohesion: 0.22
Nodes (12): ApprovalRuleStore, glob_match(), parse_and_match_bash(), persistent_rule_matching_uses_glob_patterns(), PolicyCheck, read_tools_are_auto_approved_and_bash_requires_approval(), rule_matches(), rule_visible_to_workspace() (+4 more)

### Community 54 - "Memory Maintenance Tests"
Cohesion: 0.27
Nodes (13): branch_reconciliation_merges_conflicting_writes(), consolidation_decays_confidence_once_and_is_stable_on_repeat_runs(), consolidation_normalizes_dates_and_resolves_conflicts(), ingest_source_creates_summary_and_updates_related_pages(), ingest_source_truncates_large_content(), maintenance_operations_append_log_and_keep_results_searchable(), manual_seeded_memory_fuzz_preserves_core_invariants(), manual_stress_ingest_reconcile_and_consolidate_preserves_invariants() (+5 more)

### Community 55 - "Daytona Live Tests"
Cohesion: 0.18
Nodes (9): daytona_live_provider_handles_roundtrip_and_lifecycle(), daytona_live_router_lazy_provisions_reuses_and_isolates_sessions(), destroy_and_wait(), EmptyMemoryStore, live_config(), live_provider(), session(), wait_for_destroyed() (+1 more)

### Community 56 - "Encrypted Secret Vault"
Cohesion: 0.3
Nodes (4): decrypt_bytes(), encrypt_bytes(), file_vault_encrypts_and_decrypts_roundtrip(), FileVault

### Community 57 - "Prompt Injection Defense"
Cohesion: 0.26
Nodes (12): canary_detection_works(), check_canary(), classifier_flags_known_attack_patterns(), classify_input(), contains_canary_tokens(), inject_canary(), InputClassification, InputInspection (+4 more)

### Community 58 - "Session Search Tool"
Cohesion: 0.19
Nodes (6): event_snippet(), render_results(), SessionSearchEventType, SessionSearchInput, SessionSearchTool, truncate()

### Community 59 - "Memory Browser Components"
Cohesion: 0.26
Nodes (13): Memory Page Confidence Field (high/medium/low), Memory Page Type Taxonomy (topic/entity/decision/skill/source/schema/log/index), Source Component Hover-Card Pattern (internal vs external links), Wiki-Link Fuzzy Slug Resolution Algorithm, Wiki-Link Internal Navigation Scheme (memory: prefix), Workspace Wiki Pages (markdown knowledge store), MemoryEditor Component, MemoryPageViewer Component (+5 more)

### Community 60 - "Eval Terminal Reporter"
Cohesion: 0.3
Nodes (5): format_scores(), render_includes_case_names_and_summary(), render_verbose_case(), result_index(), TerminalReporter

### Community 61 - "Streaming Code Display"
Cohesion: 0.17
Nodes (12): Code Block (Chat), Markdown Per-Block Memoization, Reasoning Auto-Open on Streaming, Shiki Syntax Highlighting (lazy-loaded), Streaming Cursor (CSS pulse after-element), Prompt-Kit Chain of Thought, Prompt-Kit Markdown, Prompt-Kit Reasoning (+4 more)

### Community 62 - "Memory Store Tests"
Cohesion: 0.36
Nodes (8): create_read_update_and_delete_wiki_pages(), delete_page_removes_only_the_requested_scope(), fts_search_finds_ranked_results(), fts_search_handles_hyphenated_queries(), rebuild_search_index_from_files_restores_results(), sample_page(), user_and_workspace_scopes_are_separate(), write_page_creates_and_reads_pages_in_explicit_scopes()

### Community 63 - "Postgres Store Tests"
Cohesion: 0.36
Nodes (6): cleanup_schema(), create_test_store(), postgres_event_payloads_round_trip_as_jsonb(), postgres_session_ids_are_native_uuid_and_concurrent_emits_are_serialized(), postgres_shared_session_store_contract(), with_test_store()

### Community 64 - "Chat Message Rendering"
Cohesion: 0.22
Nodes (10): AssistantMessage Component, ContentBlockRenderer Component, FeedbackBar Post-Stream Pattern, Mixed Block Rendering Pattern, MOA Product Name, Prompt-Kit Adapter Pattern, Streaming Text Last-Block Heuristic, ToolGroup Component (+2 more)

### Community 65 - "Live Provider Roundtrip Tests"
Cohesion: 0.39
Nodes (8): available_live_providers(), live_orchestrator_with_provider(), live_providers_complete_tool_approval_roundtrip_when_available(), LiveProvider, wait_for_approval_request(), wait_for_file(), wait_for_final_response(), wait_for_status()

### Community 66 - "Tailwind Merge Util"
Cohesion: 0.28
Nodes (4): formatAbsoluteDate(), formatRelativeTime(), formatUsd(), formatUsdFromCents()

### Community 67 - "Neon Branch Manager Tests"
Cohesion: 0.5
Nodes (7): live_neon_config(), live_neon_config_with_limit(), neon_branch_manager_create_list_get_rollback_and_discard_checkpoint(), neon_checkpoint_branch_connection_is_copy_on_write(), neon_checkpoint_capacity_limit_rejects_extra_branch(), neon_checkpoint_cleanup_without_expired_branches_returns_zero(), wait_for_workspace_session_count()

### Community 68 - "Temporal Worker Helper"
Cohesion: 0.32
Nodes (4): helper_config(), HelperProvider, main(), main_impl()

### Community 69 - "Instruction Stage"
Cohesion: 0.43
Nodes (2): instruction_processor_appends_config_backed_sections(), InstructionProcessor

### Community 70 - "Identity Stage"
Cohesion: 0.43
Nodes (2): identity_processor_appends_system_prompt(), IdentityProcessor

### Community 71 - "Trajectory Match Evaluator"
Cohesion: 0.43
Nodes (4): exact_match_scores_one(), lcs_len(), partial_match_scores_below_one(), TrajectoryMatchEvaluator

### Community 72 - "Output Match Evaluator"
Cohesion: 0.43
Nodes (4): contains_rules_pass_when_all_terms_match(), evaluate_output(), missing_contains_term_reduces_score(), OutputMatchEvaluator

### Community 73 - "Bash Tool"
Cohesion: 0.47
Nodes (3): BashToolInput, execute_docker(), execute_local()

### Community 74 - "Cache Optimizer Stage"
Cohesion: 0.4
Nodes (2): cache_optimizer_validates_cache_breakpoint(), CacheOptimizer

### Community 75 - "Threshold Evaluator"
Cohesion: 0.53
Nodes (3): cost_over_budget_fails_boolean_score(), limit_score(), ThresholdEvaluator

### Community 76 - "CLI Exec Mode"
Cohesion: 0.6
Nodes (4): exec_mode_formats_tool_updates_compactly(), format_tool_update(), resolve_exec_approval(), run_exec()

### Community 77 - "Anthropic Provider Tests"
Cohesion: 0.4
Nodes (0): 

### Community 78 - "File Search Tool"
Cohesion: 0.5
Nodes (3): collect_matches(), execute(), FileSearchInput

### Community 79 - "Approval Card & Diff Viewer"
Cohesion: 0.4
Nodes (5): Approval Risk Tone Levels (low/moderate/high), Approval Store Registration Pattern (keyboard shortcut hooks), Diff Unified/Split Toggle View, ApprovalCard Component, DiffViewer Component

### Community 80 - "OpenRouter Live Test"
Cohesion: 0.83
Nodes (3): openrouter_live_completion_returns_expected_answer(), openrouter_live_model(), openrouter_live_web_search_returns_current_information()

### Community 81 - "Chat Harness Example"
Cohesion: 0.83
Nodes (3): main(), resolve_session_db_path(), run_prompt()

### Community 82 - "Tool Success Evaluator"
Cohesion: 0.5
Nodes (1): ToolSuccessEvaluator

### Community 83 - "Command Actions & Layout"
Cohesion: 0.5
Nodes (0): 

### Community 84 - "Chat List & View"
Cohesion: 0.5
Nodes (4): Empty Session Starter Suggestions, Virtualization Threshold at 100 Messages, MessageList Component, ChatView

### Community 85 - "OpenAI Live Test"
Cohesion: 0.67
Nodes (0): 

### Community 86 - "Anthropic Live Test"
Cohesion: 0.67
Nodes (0): 

### Community 87 - "Frontend Toolchain Assets"
Cohesion: 0.67
Nodes (3): Tauri Logo SVG, Tauri + Vite Frontend Toolchain, Vite Logo SVG

### Community 88 - "Turso Schema Migration"
Cohesion: 1.0
Nodes (0): 

### Community 89 - "Tauri Build Entry"
Cohesion: 1.0
Nodes (0): 

### Community 90 - "Docker Hardening Test"
Cohesion: 1.0
Nodes (0): 

### Community 91 - "Brain Live Harness Test"
Cohesion: 1.0
Nodes (0): 

### Community 92 - "Eval Live Tests"
Cohesion: 1.0
Nodes (0): 

### Community 93 - "Session Tab Store"
Cohesion: 1.0
Nodes (0): 

### Community 94 - "Mobile Breakpoint Hook"
Cohesion: 1.0
Nodes (0): 

### Community 95 - "Session Preview DTOs"
Cohesion: 1.0
Nodes (0): 

### Community 96 - "Hash Router Strategy"
Cohesion: 1.0
Nodes (2): Hash History Routing Strategy, Hash Router (router.tsx)

### Community 97 - "Chat Prompt Input"
Cohesion: 1.0
Nodes (2): Prompt-Kit Prompt Suggestion, Prompt Input (Chat)

### Community 98 - "Context Window Bar"
Cohesion: 1.0
Nodes (2): Context Pressure Visualization (24-segment bar), ContextWindowBar Component

### Community 99 - "Vite Config"
Cohesion: 1.0
Nodes (0): 

### Community 100 - "Vite Env Types"
Cohesion: 1.0
Nodes (0): 

### Community 101 - "Session State Store"
Cohesion: 1.0
Nodes (0): 

### Community 102 - "Settings Type Definitions"
Cohesion: 1.0
Nodes (0): 

### Community 103 - "Memory Search DTO"
Cohesion: 1.0
Nodes (0): 

### Community 104 - "App Error Type"
Cohesion: 1.0
Nodes (0): 

### Community 105 - "Wiki Page DTO"
Cohesion: 1.0
Nodes (0): 

### Community 106 - "Page Summary DTO"
Cohesion: 1.0
Nodes (0): 

### Community 107 - "Runtime Info DTO"
Cohesion: 1.0
Nodes (0): 

### Community 108 - "Session Meta DTO"
Cohesion: 1.0
Nodes (0): 

### Community 109 - "Event Record DTO"
Cohesion: 1.0
Nodes (0): 

### Community 110 - "App Config DTO"
Cohesion: 1.0
Nodes (0): 

### Community 111 - "Model Option DTO"
Cohesion: 1.0
Nodes (0): 

### Community 112 - "Desktop Shell Root"
Cohesion: 1.0
Nodes (1): Desktop Shell (Tauri-backed React app)

### Community 113 - "User Message Component"
Cohesion: 1.0
Nodes (1): UserMessage Component

### Community 114 - "Prompt-Kit Chat Container"
Cohesion: 1.0
Nodes (1): Prompt-Kit Chat Container

### Community 115 - "Prompt-Kit Tool"
Cohesion: 1.0
Nodes (1): Prompt-Kit Tool

### Community 116 - "Prompt-Kit Loader"
Cohesion: 1.0
Nodes (1): Prompt-Kit Loader

### Community 117 - "Prompt-Kit System Message"
Cohesion: 1.0
Nodes (1): Prompt-Kit System Message

### Community 118 - "Text Shimmer Animation"
Cohesion: 1.0
Nodes (1): Text Shimmer CSS Animation

### Community 119 - "Prompt-Kit Scroll Button"
Cohesion: 1.0
Nodes (1): Prompt-Kit Scroll Button

### Community 120 - "Prompt-Kit Message"
Cohesion: 1.0
Nodes (1): Prompt-Kit Message

### Community 121 - "Prompt-Kit Feedback Bar"
Cohesion: 1.0
Nodes (1): Prompt-Kit Feedback Bar

## Ambiguous Edges - Review These
- `Instruction Layer Hierarchy (workspace over user instructions)` → `Provider-Native Web Search (bypass MOA tools)`  [AMBIGUOUS]
  src/components/settings/general-settings.tsx · relation: semantically_similar_to
- `Prompt-Kit Markdown` → `Prompt-Kit Chain of Thought`  [AMBIGUOUS]
  src/components/prompt-kit/markdown.tsx · relation: references

## Knowledge Gaps
- **275 isolated node(s):** `LogChange`, `LogEntry`, `PageFrontmatter`, `ChangeOperation`, `ChangeRecord` (+270 more)
  These have ≤1 connection - possible missing edges or undocumented components.
- **Thin community `Turso Schema Migration`** (2 nodes): `schema_turso.rs`, `migrate()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Tauri Build Entry`** (2 nodes): `build.rs`, `main()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Docker Hardening Test`** (2 nodes): `docker_hardening.rs`, `docker_container_runs_with_hardening()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Brain Live Harness Test`** (2 nodes): `live_harness.rs`, `live_brain_turn_returns_brain_response()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Eval Live Tests`** (2 nodes): `engine_live.rs`, `live_run_single_produces_eval_result()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Session Tab Store`** (2 nodes): `tabs.ts`, `moveItem()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Mobile Breakpoint Hook`** (2 nodes): `use-mobile.ts`, `useIsMobile()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Session Preview DTOs`** (2 nodes): `SessionPreviewDto.ts`, `SessionSummaryDto.ts`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Hash Router Strategy`** (2 nodes): `Hash History Routing Strategy`, `Hash Router (router.tsx)`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Chat Prompt Input`** (2 nodes): `Prompt-Kit Prompt Suggestion`, `Prompt Input (Chat)`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Context Window Bar`** (2 nodes): `Context Pressure Visualization (24-segment bar)`, `ContextWindowBar Component`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Vite Config`** (1 nodes): `vite.config.ts`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Vite Env Types`** (1 nodes): `vite-env.d.ts`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Session State Store`** (1 nodes): `session.ts`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Settings Type Definitions`** (1 nodes): `settings-types.ts`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Memory Search DTO`** (1 nodes): `MemorySearchResultDto.ts`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `App Error Type`** (1 nodes): `MoaAppError.ts`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Wiki Page DTO`** (1 nodes): `WikiPageDto.ts`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Page Summary DTO`** (1 nodes): `PageSummaryDto.ts`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Runtime Info DTO`** (1 nodes): `RuntimeInfoDto.ts`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Session Meta DTO`** (1 nodes): `SessionMetaDto.ts`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Event Record DTO`** (1 nodes): `EventRecordDto.ts`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `App Config DTO`** (1 nodes): `MoaConfigDto.ts`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Model Option DTO`** (1 nodes): `ModelOptionDto.ts`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Desktop Shell Root`** (1 nodes): `Desktop Shell (Tauri-backed React app)`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `User Message Component`** (1 nodes): `UserMessage Component`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Prompt-Kit Chat Container`** (1 nodes): `Prompt-Kit Chat Container`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Prompt-Kit Tool`** (1 nodes): `Prompt-Kit Tool`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Prompt-Kit Loader`** (1 nodes): `Prompt-Kit Loader`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Prompt-Kit System Message`** (1 nodes): `Prompt-Kit System Message`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Text Shimmer Animation`** (1 nodes): `Text Shimmer CSS Animation`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Prompt-Kit Scroll Button`** (1 nodes): `Prompt-Kit Scroll Button`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Prompt-Kit Message`** (1 nodes): `Prompt-Kit Message`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Prompt-Kit Feedback Bar`** (1 nodes): `Prompt-Kit Feedback Bar`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **What is the exact relationship between `Instruction Layer Hierarchy (workspace over user instructions)` and `Provider-Native Web Search (bypass MOA tools)`?**
  _Edge tagged AMBIGUOUS (relation: semantically_similar_to) - confidence is low._
- **What is the exact relationship between `Prompt-Kit Markdown` and `Prompt-Kit Chain of Thought`?**
  _Edge tagged AMBIGUOUS (relation: references) - confidence is low._
- **What connects `LogChange`, `LogEntry`, `PageFrontmatter` to the rest of the system?**
  _275 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `Core Domain Types` be split into smaller, more focused modules?**
  _Cohesion score 0.01 - nodes in this community are weakly interconnected._
- **Should `File Memory Store` be split into smaller, more focused modules?**
  _Cohesion score 0.02 - nodes in this community are weakly interconnected._
- **Should `Brain Turn Tests` be split into smaller, more focused modules?**
  _Cohesion score 0.04 - nodes in this community are weakly interconnected._
- **Should `Event Taxonomy` be split into smaller, more focused modules?**
  _Cohesion score 0.04 - nodes in this community are weakly interconnected._