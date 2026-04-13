# Graph Report - .  (2026-04-13)

## Corpus Check
- 171 files · ~147,532 words
- Verdict: corpus is large enough that graph structure adds value.

## Summary
- 2806 nodes · 4810 edges · 94 communities detected
- Extraction: 100% EXTRACTED · 0% INFERRED · 0% AMBIGUOUS
- Token cost: 0 input · 0 output

## God Nodes (most connected - your core abstractions)
1. `LocalChatRuntime` - 36 edges
2. `DaemonChatRuntime` - 35 edges
3. `SkillFrontmatter` - 33 edges
4. `PostgresSessionStore` - 29 edges
5. `TursoSessionStore` - 28 edges
6. `start_session()` - 25 edges
7. `LocalOrchestrator` - 25 edges
8. `FileMemoryStore` - 22 edges
9. `session()` - 21 edges
10. `SessionDatabase` - 20 edges

## Surprising Connections (you probably didn't know these)
- None detected - all connections are within the same source files.

## Communities

### Community 0 - "Eval Loader Tests"
Cohesion: 0.03
Nodes (57): EvalError, MoaError, approval_requested_event_round_trips_full_prompt(), Event, sample_approval_prompt(), HandHandle, HandResources, HandSpec (+49 more)

### Community 1 - "Brain Turn Tests"
Cohesion: 0.04
Nodes (29): always_allow_rule_persists_and_skips_next_approval(), canary_leaks_in_tool_input_are_detected_and_blocked(), CanaryLeakLlmProvider, CapturingTextLlmProvider, FixedPageMemoryStore, malicious_tool_results_are_wrapped_as_untrusted_content(), MaliciousToolOutputLlmProvider, MemoryIngestLoopLlmProvider (+21 more)

### Community 2 - "Pipeline & Session Helpers"
Cohesion: 0.03
Nodes (51): build_default_pipeline(), build_default_pipeline_with_runtime(), build_default_pipeline_with_tools(), cache_prefix_ratio(), ContextPipeline, estimate_tokens(), EvaluatorOptions, hand_id() (+43 more)

### Community 3 - "Memory Pipeline & Views"
Cohesion: 0.03
Nodes (38): ConfidenceLevel, count_ingest_pages(), derive_source_name_from_content(), extract_search_keywords(), extract_search_query(), format_ingest_report(), infer_page_title(), infer_page_type() (+30 more)

### Community 4 - "Local Orchestrator Tests"
Cohesion: 0.05
Nodes (50): approval_requested_event_persists_full_prompt_details(), collect_runtime_events_until(), completed_tool_turn_destroys_cached_hand(), CurrentDirGuard, cwd_lock(), denied_tool_preserves_queued_follow_up(), DestroyTrackingHandProvider, FileWriteApprovalProvider (+42 more)

### Community 5 - "Event Taxonomy"
Cohesion: 0.04
Nodes (41): CollectedExecution, collector_tracks_tool_steps_and_metrics(), estimate_cost(), TrajectoryCollector, truncate(), destroy_and_wait(), e2b_live_provider_handles_roundtrip_and_lifecycle(), e2b_live_router_lazy_provisions_reuses_and_isolates_sessions() (+33 more)

### Community 6 - "Session Database Backend"
Cohesion: 0.04
Nodes (31): Evaluator, build_provider_from_config(), build_provider_from_selection(), explicit_provider_prefix_overrides_inference(), infer_provider_name(), infers_anthropic_for_claude_models(), infers_google_for_gemini_models(), infers_openai_for_gpt_models() (+23 more)

### Community 7 - "Config & Errors"
Cohesion: 0.04
Nodes (32): budget_config_defaults_are_applied(), BudgetConfig, CloudConfig, CloudFlyioConfig, CloudHandsConfig, CloudTemporalConfig, compaction_config_defaults_are_applied(), CompactionConfig (+24 more)

### Community 8 - "Local Orchestrator"
Cohesion: 0.06
Nodes (23): accept_user_message(), append_event(), best_effort_resolve_pending_signal(), detect_docker(), detect_workspace_path(), docker_status(), DockerSandbox, flush_next_queued_message() (+15 more)

### Community 9 - "Skill Document Format"
Cohesion: 0.07
Nodes (22): build_skill_path(), confidence_for_skill(), defaults_missing_moa_metadata(), estimate_skill_tokens(), format_timestamp(), humanize_skill_name(), is_valid_skill_name(), metadata_csv() (+14 more)

### Community 10 - "CLI Entry Point"
Cohesion: 0.05
Nodes (54): apply_config_update(), checkpoint_cleanup_report(), checkpoint_create_report(), checkpoint_list_report(), checkpoint_rollback_report(), CheckpointCommand, Cli, cloud_sync_status() (+46 more)

### Community 11 - "Daemon Service"
Cohesion: 0.06
Nodes (49): daemon_create_session_uses_explicit_client_scope(), daemon_health_endpoint_responds_when_cloud_enabled(), daemon_info(), daemon_lists_session_previews(), daemon_log_path(), daemon_logs(), daemon_pid_path(), daemon_ping_create_and_shutdown_roundtrip() (+41 more)

### Community 12 - "Anthropic Provider"
Cohesion: 0.07
Nodes (39): anthropic_content_blocks(), anthropic_content_blocks_render_text_and_json_as_text_blocks(), anthropic_message(), anthropic_message_wraps_assistant_tool_calls_as_tool_use_blocks(), anthropic_message_wraps_tool_results_with_tool_use_id(), anthropic_tool_from_schema(), anthropic_tool_from_schema_moves_parameters_into_input_schema(), AnthropicProvider (+31 more)

### Community 13 - "Tool Policy & Content Types"
Cohesion: 0.05
Nodes (22): execute_tool_policy(), RegisteredTool, ToolExecution, ToolRegistry, page_key(), ranked_tools_prefer_successful_workspace_tools(), StaticMemoryStore, tool_output_error_sets_error_flag() (+14 more)

### Community 14 - "Postgres Session Store"
Cohesion: 0.06
Nodes (18): checkpoint_view(), event_hand_id(), normalize_event_search_query(), PostgresSessionStore, qualified_name(), approval_rule_from_row(), pending_signal_from_row(), pending_signal_type_from_db() (+10 more)

### Community 15 - "Temporal Orchestrator"
Cohesion: 0.06
Nodes (27): activity_options(), apply_approval_decision(), ApprovalDecisionActivityInput, ApprovalSignalInput, brain_turn_activity_options(), CancelMode, connect_temporal_client(), event_to_runtime_event() (+19 more)

### Community 16 - "History Compilation Stage"
Cohesion: 0.07
Nodes (32): calculate_cost_cents(), CheckpointState, compaction_request(), event_summary_line(), latest_checkpoint_state(), maybe_compact(), maybe_compact_events(), non_checkpoint_events() (+24 more)

### Community 17 - "Turso Session Store"
Cohesion: 0.06
Nodes (17): optional_i64(), optional_text(), parse_timestamp(), platform_from_db(), session_meta_from_row(), session_status_from_db(), session_summary_from_row(), build_event_fts_query() (+9 more)

### Community 18 - "Gemini Provider"
Cohesion: 0.09
Nodes (32): build_contents(), build_request_body(), canonical_model_id(), capabilities_for_model(), consume_sse_events(), content_message(), finish_reason_to_stop_reason(), flush_pending_parts() (+24 more)

### Community 19 - "Skill Regression Testing"
Cohesion: 0.07
Nodes (41): build_distillation_prompt(), count_tool_calls(), extract_task_summary(), find_similar_skill(), maybe_distill_skill(), normalize_new_skill(), similarity_score(), tokenize() (+33 more)

### Community 20 - "Turn Streaming & Approval"
Cohesion: 0.06
Nodes (26): drain_signal_queue(), handle_stream_signal(), run_streamed_turn_with_tools_mode(), append_tool_call_event(), emit_tool_output_warning(), execute_pending_tool(), execute_tool(), format_tool_output() (+18 more)

### Community 21 - "Neon Branch Manager"
Cohesion: 0.09
Nodes (25): checkpoint_branch_names_follow_moa_prefix(), checkpoint_info_from_branch(), checkpoint_label_from_name(), cleanup_expired_deletes_only_old_moa_branches(), create_checkpoint_refuses_to_exceed_capacity(), create_checkpoint_sends_expected_request_and_returns_handle(), discard_checkpoint_calls_delete_endpoint(), format_checkpoint_branch_name() (+17 more)

### Community 22 - "Skill Injection Stage"
Cohesion: 0.08
Nodes (27): allowed_tools(), budget_limit_skips_expensive_tests(), distills_skill_after_tool_heavy_session(), estimate_skill_tokens(), improvement_accepted_when_scores_better(), improvement_rejected_on_regression(), ImprovementAndEvalLlm, improves_existing_skill_when_better_flow_is_found() (+19 more)

### Community 23 - "Memory Consolidation"
Cohesion: 0.08
Nodes (40): canonical_port_claims(), confidence_rank(), consolidation_due_for_scope(), consolidation_resolves_dates_prunes_and_refreshes_index(), ConsolidationReport, decay_confidence(), extract_port_claims(), inbound_reference_counts() (+32 more)

### Community 24 - "OpenAI Responses Provider"
Cohesion: 0.07
Nodes (26): canonical_model_id(), capabilities_for_model(), gpt_5_4_family_reports_expected_capabilities(), native_web_search_tools(), OpenAIProvider, build_function_tool(), build_responses_request(), consume_responses_stream_once() (+18 more)

### Community 25 - "Adaptive Tool Stats"
Cohesion: 0.1
Nodes (36): annotate_schema(), annotation_warns_on_low_success(), apply_tool_rankings(), cache_stability_preserves_identical_ranked_output(), collect_session_tool_observations(), compare_f64_asc(), compare_f64_desc(), compare_failure_last() (+28 more)

### Community 26 - "Message Renderer"
Cohesion: 0.09
Nodes (23): append_piece(), discord_renderer_attaches_buttons_to_last_chunk_only(), discord_renderer_uses_embed_limit_for_long_text(), DiscordRenderChunk, DiscordRenderer, render_approval_request(), render_diff(), render_tool_card() (+15 more)

### Community 27 - "Temporal Orchestrator Tests"
Cohesion: 0.11
Nodes (28): build_temporal_helper_binary(), delayed_text_stream(), last_user_message(), mock_capabilities(), spawn_temporal_helper(), temporal_helper_binary(), temporal_orchestrator_live_anthropic_smoke(), temporal_orchestrator_processes_multiple_queued_messages_fifo() (+20 more)

### Community 28 - "Local Chat Runtime"
Cohesion: 0.07
Nodes (2): ChatRuntime, LocalChatRuntime

### Community 29 - "Discord Adapter"
Cohesion: 0.12
Nodes (18): approval_callback_maps_to_control_message(), attachments_from_message(), context_from_component(), discord_button(), discord_create_message(), discord_create_message_includes_buttons_for_last_chunk(), discord_edit_message(), discord_embed() (+10 more)

### Community 30 - "E2B Sandbox Provider"
Cohesion: 0.14
Nodes (16): build_url(), ConnectedSandbox, decode_stream_chunk(), default_headers(), E2BHandProvider, encode_connect_request(), encode_test_envelopes(), envd_headers() (+8 more)

### Community 31 - "Eval Execution Engine"
Cohesion: 0.12
Nodes (20): build_error_result(), cleanup_workspace(), dry_run_marks_results_skipped(), EngineOptions, EvalEngine, EvalRun, extract_trace_id(), fs_try_exists() (+12 more)

### Community 32 - "Daemon Chat Runtime"
Cohesion: 0.07
Nodes (1): DaemonChatRuntime

### Community 33 - "Slack Adapter"
Cohesion: 0.13
Nodes (19): handle_interaction_event(), handle_push_event(), inbound_from_app_mention(), inbound_from_interaction_event(), inbound_from_message_event(), inbound_from_push_event(), interaction_origin(), parses_approval_callback_into_control_message() (+11 more)

### Community 34 - "Local Tool Tests"
Cohesion: 0.11
Nodes (23): approval_prompt_uses_remembered_workspace_root_for_commands(), bash_captures_stdout_and_stderr(), bash_respects_timeout(), EmptyMemoryStore, file_operations_reject_path_traversal(), file_read_reads_written_content(), file_search_finds_files_by_glob(), file_search_skips_git_directory_contents() (+15 more)

### Community 35 - "Eval Agent Setup"
Cohesion: 0.13
Nodes (27): AgentEnvironment, apply_skill_overrides(), build_agent_environment(), build_agent_environment_with_provider(), build_eval_policies(), build_pipeline(), build_skill_memory_path(), build_tool_router() (+19 more)

### Community 36 - "Wiki & Memory Branching"
Cohesion: 0.12
Nodes (28): append_change_manifest(), branch_dir(), branch_file_path(), branch_root(), ChangeOperation, ChangeRecord, list_branches(), merge_markdown() (+20 more)

### Community 37 - "Working Context Messages"
Cohesion: 0.09
Nodes (8): context_message_assistant_tool_call_preserves_invocation(), context_message_tool_result_preserves_text_and_blocks(), context_message_tool_still_defaults_to_text_only(), ContextMessage, estimate_text_tokens(), MessageRole, ProcessorOutput, WorkingContext

### Community 38 - "MCP Client Discovery"
Cohesion: 0.14
Nodes (16): flatten_call_result(), flatten_tool_result_aggregates_text_items(), header_map_from_pairs(), http_client_sends_headers_and_parses_jsonrpc(), MCPClient, McpDiscoveredTool, McpTransport, parse_jsonrpc_result() (+8 more)

### Community 39 - "MCP Credential Proxy"
Cohesion: 0.12
Nodes (11): credential_from_env(), default_scope_for(), env_var(), environment_vault_loads_from_env_backed_server_config(), EnvironmentCredentialVault, headers_from_credential(), MCPCredentialProxy, McpSessionToken (+3 more)

### Community 40 - "LLM Span Instrumentation"
Cohesion: 0.13
Nodes (17): calculate_cost(), calculate_cost_with_cached(), cost_calculation_correct(), has_meaningful_output(), llm_span_name(), LLMSpanAttributes, LLMSpanRecorder, metadata_f64() (+9 more)

### Community 41 - "Daytona Workspace Provider"
Cohesion: 0.17
Nodes (10): build_url(), DaytonaHandProvider, default_headers(), derive_toolbox_url(), expect_success(), expect_success_json(), extract_workspace_id(), http_error() (+2 more)

### Community 42 - "Telegram Adapter"
Cohesion: 0.16
Nodes (13): channel_from_chat_and_reply(), handle_callback_query(), handle_message(), inbound_from_callback_query(), inbound_from_message(), inline_keyboard(), parse_message_id(), parses_approval_callback_into_control_message() (+5 more)

### Community 43 - "Docker File Operations"
Cohesion: 0.11
Nodes (15): container_path_validation_accepts_workspace_absolute_paths(), container_path_validation_rejects_absolute_paths_outside_workspace(), container_path_validation_rejects_traversal(), docker_file_read(), docker_file_search(), docker_file_write(), docker_find_args(), docker_read_args() (+7 more)

### Community 44 - "Session State Store"
Cohesion: 0.09
Nodes (18): BufferedUserMessage, CheckpointHandle, CheckpointInfo, ObserveLevel, pending_signal_queue_message_round_trip(), PendingSignal, PendingSignalType, session_meta_default_builds_created_session() (+10 more)

### Community 45 - "Session Store Tests"
Cohesion: 0.13
Nodes (18): approval_rules_round_trip(), create_session_and_emit_events(), fts_search_finds_events(), fts_search_uses_blob_preview(), get_events_with_range_filter(), identical_large_payloads_share_one_blob(), large_payload_offloaded_to_blob_store(), list_sessions_filters_by_workspace() (+10 more)

### Community 46 - "Session Blob Store"
Cohesion: 0.18
Nodes (12): claim_check_from_value(), collect_blob_refs(), collect_large_strings(), decode_event_from_storage(), encode_event_for_storage(), expand_local_path(), file_blob_store_deletes_session_directory(), file_blob_store_is_content_addressed() (+4 more)

### Community 47 - "Approval Request Types"
Cohesion: 0.12
Nodes (18): approval_buttons(), approval_request(), ApprovalCallbackAction, ApprovalDecision, ApprovalField, ApprovalFileDiff, ApprovalPrompt, ApprovalRequest (+10 more)

### Community 48 - "Session Database Interface"
Cohesion: 0.09
Nodes (3): create_session_store(), SessionDatabase, SessionStoreDispatch

### Community 49 - "Completion API Types"
Cohesion: 0.13
Nodes (9): completion_stream_abort_stops_completion_task(), CompletionContent, CompletionRequest, CompletionResponse, CompletionStream, ProviderToolCallMetadata, StopReason, ToolCallContent (+1 more)

### Community 50 - "Event Stream Types"
Cohesion: 0.12
Nodes (8): ClaimCheck, event_stream_reports_lagged_broadcasts(), EventFilter, EventRange, EventRecord, EventStream, EventType, MaybeBlob

### Community 51 - "Tool Router Policy"
Cohesion: 0.11
Nodes (8): approval_diffs_for(), approval_fields_for(), normalized_input_for(), read_existing_text_file(), required_string_field(), single_approval_field(), PreparedToolInvocation, ToolRouter

### Community 52 - "CLI HTTP API Server"
Cohesion: 0.13
Nodes (10): ApiState, build_api_router(), health_endpoint_returns_ok(), runtime_event_stream(), session_stream(), session_stream_returns_not_found_when_runtime_is_unavailable(), session_stream_returns_sse_content_type(), start_api_server() (+2 more)

### Community 53 - "Telemetry & Observability"
Cohesion: 0.16
Nodes (16): build_grpc_metadata(), build_http_headers(), build_resource(), build_sampler(), build_span_exporter(), grpc_metadata_uses_header_values(), init_observability(), init_observability_disabled_returns_guard() (+8 more)

### Community 54 - "Full Text Search"
Cohesion: 0.19
Nodes (10): build_fts_query(), delete_page_entries(), FtsIndex, insert_page(), migrate(), parse_confidence(), parse_page_type(), parse_scope_key() (+2 more)

### Community 55 - "Tool Approval Policies"
Cohesion: 0.22
Nodes (12): ApprovalRuleStore, glob_match(), parse_and_match_bash(), persistent_rule_matching_uses_glob_patterns(), PolicyCheck, read_tools_are_auto_approved_and_bash_requires_approval(), rule_matches(), rule_visible_to_workspace() (+4 more)

### Community 56 - "Memory Maintenance Tests"
Cohesion: 0.27
Nodes (13): branch_reconciliation_merges_conflicting_writes(), consolidation_decays_confidence_once_and_is_stable_on_repeat_runs(), consolidation_normalizes_dates_and_resolves_conflicts(), ingest_source_creates_summary_and_updates_related_pages(), ingest_source_truncates_large_content(), maintenance_operations_append_log_and_keep_results_searchable(), manual_seeded_memory_fuzz_preserves_core_invariants(), manual_stress_ingest_reconcile_and_consolidate_preserves_invariants() (+5 more)

### Community 57 - "OpenAI Provider Tests"
Cohesion: 0.22
Nodes (15): openai_provider_does_not_retry_after_partial_stream_output(), openai_provider_drops_oversized_metadata_values(), openai_provider_includes_native_web_search_when_enabled(), openai_provider_omits_native_web_search_when_disabled(), openai_provider_retries_after_rate_limit(), openai_provider_serializes_assistant_tool_calls_as_function_call_items(), openai_provider_serializes_tool_result_messages_as_function_call_output(), openai_provider_streams_parallel_tool_calls_in_order() (+7 more)

### Community 58 - "Daytona Memory Store Tests"
Cohesion: 0.18
Nodes (9): daytona_live_provider_handles_roundtrip_and_lifecycle(), daytona_live_router_lazy_provisions_reuses_and_isolates_sessions(), destroy_and_wait(), EmptyMemoryStore, live_config(), live_provider(), session(), wait_for_destroyed() (+1 more)

### Community 59 - "Memory Bootstrap"
Cohesion: 0.24
Nodes (14): BootstrapReport, BootstrapSentinel, find_instruction_file(), index_page_with_instructions(), is_bootstrap_index(), minimal_index_page(), project_instructions_page(), run_bootstrap() (+6 more)

### Community 60 - "Encrypted Secret Vault"
Cohesion: 0.3
Nodes (4): decrypt_bytes(), encrypt_bytes(), file_vault_encrypts_and_decrypts_roundtrip(), FileVault

### Community 61 - "Tool Router Construction"
Cohesion: 0.18
Nodes (2): default_cloud_provider(), ToolRouter

### Community 62 - "Provider Retry Policy"
Cohesion: 0.27
Nodes (6): parse_retry_after(), response_text(), retries_on_rate_limit(), retry_after_delay(), retry_after_delay_from_message(), RetryPolicy

### Community 63 - "Prompt Injection Detection"
Cohesion: 0.26
Nodes (12): canary_detection_works(), check_canary(), classifier_flags_known_attack_patterns(), classify_input(), contains_canary_tokens(), inject_canary(), InputClassification, InputInspection (+4 more)

### Community 64 - "Session Search Tool"
Cohesion: 0.19
Nodes (6): event_snippet(), render_results(), SessionSearchEventType, SessionSearchInput, SessionSearchTool, truncate()

### Community 65 - "Live Provider Roundtrip Tests"
Cohesion: 0.3
Nodes (11): available_live_providers(), google_live_provider(), live_google_provider_complete_tool_approval_roundtrip_when_available(), live_orchestrator_with_provider(), live_providers_complete_tool_approval_roundtrip_when_available(), LiveProvider, run_live_provider_tool_approval_roundtrip(), wait_for_approval_request() (+3 more)

### Community 66 - "Eval Terminal Reporter"
Cohesion: 0.3
Nodes (5): format_scores(), render_includes_case_names_and_summary(), render_verbose_case(), result_index(), TerminalReporter

### Community 67 - "Memory Store Tests"
Cohesion: 0.36
Nodes (8): create_read_update_and_delete_wiki_pages(), delete_page_removes_only_the_requested_scope(), fts_search_finds_ranked_results(), fts_search_handles_hyphenated_queries(), rebuild_search_index_from_files_restores_results(), sample_page(), user_and_workspace_scopes_are_separate(), write_page_creates_and_reads_pages_in_explicit_scopes()

### Community 68 - "Postgres Store Tests"
Cohesion: 0.36
Nodes (6): cleanup_schema(), create_test_store(), postgres_event_payloads_round_trip_as_jsonb(), postgres_session_ids_are_native_uuid_and_concurrent_emits_are_serialized(), postgres_shared_session_store_contract(), with_test_store()

### Community 69 - "Neon Branch Manager Tests"
Cohesion: 0.5
Nodes (7): live_neon_config(), live_neon_config_with_limit(), neon_branch_manager_create_list_get_rollback_and_discard_checkpoint(), neon_checkpoint_branch_connection_is_copy_on_write(), neon_checkpoint_capacity_limit_rejects_extra_branch(), neon_checkpoint_cleanup_without_expired_branches_returns_zero(), wait_for_workspace_session_count()

### Community 70 - "Temporal Worker Helper"
Cohesion: 0.32
Nodes (4): helper_config(), HelperProvider, main(), main_impl()

### Community 71 - "Instruction Stage"
Cohesion: 0.43
Nodes (2): instruction_processor_appends_config_backed_sections(), InstructionProcessor

### Community 72 - "Identity Stage"
Cohesion: 0.43
Nodes (2): identity_processor_appends_system_prompt(), IdentityProcessor

### Community 73 - "File Search Tool"
Cohesion: 0.52
Nodes (6): build_file_search_output(), collect_matches(), execute(), execute_docker(), FileSearchInput, should_skip_search_path()

### Community 74 - "Trajectory Match Evaluator"
Cohesion: 0.43
Nodes (4): exact_match_scores_one(), lcs_len(), partial_match_scores_below_one(), TrajectoryMatchEvaluator

### Community 75 - "Output Match Evaluator"
Cohesion: 0.43
Nodes (4): contains_rules_pass_when_all_terms_match(), evaluate_output(), missing_contains_term_reduces_score(), OutputMatchEvaluator

### Community 76 - "Bash Tool"
Cohesion: 0.47
Nodes (3): BashToolInput, execute_docker(), execute_local()

### Community 77 - "Cache Optimizer Stage"
Cohesion: 0.4
Nodes (2): cache_optimizer_validates_cache_breakpoint(), CacheOptimizer

### Community 78 - "Threshold Evaluator"
Cohesion: 0.53
Nodes (3): cost_over_budget_fails_boolean_score(), limit_score(), ThresholdEvaluator

### Community 79 - "CLI Exec Mode"
Cohesion: 0.6
Nodes (4): exec_mode_formats_tool_updates_compactly(), format_tool_update(), resolve_exec_approval(), run_exec()

### Community 80 - "Anthropic Provider Tests"
Cohesion: 0.4
Nodes (0): 

### Community 81 - "Gemini Live Tests"
Cohesion: 0.83
Nodes (3): gemini_live_completion_returns_expected_answer(), gemini_live_model(), gemini_live_web_search_returns_current_information()

### Community 82 - "Chat Harness Example"
Cohesion: 0.83
Nodes (3): main(), resolve_session_db_path(), run_prompt()

### Community 83 - "Cost Budget Enforcement"
Cohesion: 0.67
Nodes (2): enforce_workspace_budget(), format_budget_exhausted_message()

### Community 84 - "Tool Success Evaluator"
Cohesion: 0.5
Nodes (1): ToolSuccessEvaluator

### Community 85 - "OpenAI Live Test"
Cohesion: 0.67
Nodes (0): 

### Community 86 - "Anthropic Live Test"
Cohesion: 0.67
Nodes (0): 

### Community 87 - "Turso Schema Migration"
Cohesion: 1.0
Nodes (0): 

### Community 88 - "moa-providers"
Cohesion: 1.0
Nodes (0): 

### Community 89 - "Docker Hardening Test"
Cohesion: 1.0
Nodes (0): 

### Community 90 - "Brain Live Harness Test"
Cohesion: 1.0
Nodes (0): 

### Community 91 - "Eval Live Tests"
Cohesion: 1.0
Nodes (0): 

### Community 92 - "moa-runtime"
Cohesion: 1.0
Nodes (1): ChatRuntime

### Community 93 - "Core Type Macros"
Cohesion: 1.0
Nodes (0): 

## Knowledge Gaps
- **241 isolated node(s):** `LogChange`, `LogEntry`, `BootstrapReport`, `BootstrapSentinel`, `PageFrontmatter` (+236 more)
  These have ≤1 connection - possible missing edges or undocumented components.
- **Thin community `Turso Schema Migration`** (2 nodes): `schema_turso.rs`, `migrate()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `moa-providers`** (2 nodes): `http.rs`, `build_http_client()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Docker Hardening Test`** (2 nodes): `docker_hardening.rs`, `docker_container_runs_with_hardening()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Brain Live Harness Test`** (2 nodes): `live_harness.rs`, `live_brain_turn_returns_brain_response()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Eval Live Tests`** (2 nodes): `engine_live.rs`, `live_run_single_produces_eval_result()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `moa-runtime`** (2 nodes): `ChatRuntime`, `.set_model()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Core Type Macros`** (1 nodes): `macros.rs`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **Why does `LocalChatRuntime` connect `Local Chat Runtime` to `Local Orchestrator`?**
  _High betweenness centrality (0.018) - this node is a cross-community bridge._
- **Why does `DaemonChatRuntime` connect `Daemon Chat Runtime` to `Daemon Service`?**
  _High betweenness centrality (0.016) - this node is a cross-community bridge._
- **What connects `LogChange`, `LogEntry`, `BootstrapReport` to the rest of the system?**
  _241 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `Eval Loader Tests` be split into smaller, more focused modules?**
  _Cohesion score 0.03 - nodes in this community are weakly interconnected._
- **Should `Brain Turn Tests` be split into smaller, more focused modules?**
  _Cohesion score 0.04 - nodes in this community are weakly interconnected._
- **Should `Pipeline & Session Helpers` be split into smaller, more focused modules?**
  _Cohesion score 0.03 - nodes in this community are weakly interconnected._
- **Should `Memory Pipeline & Views` be split into smaller, more focused modules?**
  _Cohesion score 0.03 - nodes in this community are weakly interconnected._