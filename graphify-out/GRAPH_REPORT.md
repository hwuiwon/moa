# Graph Report - .  (2026-04-20)

## Corpus Check
- Large corpus: 305 files · ~262,118 words. Semantic extraction will be expensive (many Claude tokens). Consider running on a subfolder, or use --no-semantic to run AST-only.

## Summary
- 4760 nodes · 8420 edges · 149 communities detected
- Extraction: 100% EXTRACTED · 0% INFERRED · 0% AMBIGUOUS · INFERRED: 19 edges (avg confidence: 0.8)
- Token cost: 0 input · 0 output

## God Nodes (most connected - your core abstractions)
1. `PostgresSessionStore` - 43 edges
2. `LocalChatRuntime` - 36 edges
3. `DaemonChatRuntime` - 35 edges
4. `SkillFrontmatter` - 33 edges
5. `FileMemoryStore` - 31 edges
6. `session()` - 31 edges
7. `start_session()` - 30 edges
8. `LocalOrchestrator` - 27 edges
9. `WikiSearchIndex` - 21 edges
10. `wait_for_status()` - 21 edges

## Surprising Connections (you probably didn't know these)
- `main()` --calls--> `bind_listener()`  [EXTRACTED]
  moa-cli/src/main.rs → moa-orchestrator/src/main.rs
- `build_daemon_target()` --calls--> `expand_local_path()`  [EXTRACTED]
  moa-loadtest/src/lib.rs → moa-memory/src/lib.rs
- `main()` --calls--> `spawn_restate_server()`  [EXTRACTED]
  moa-cli/src/main.rs → moa-orchestrator/src/main.rs
- `main()` --calls--> `spawn_health_server()`  [EXTRACTED]
  moa-cli/src/main.rs → moa-orchestrator/src/main.rs
- `cache_prefix_ratio()` --calls--> `estimate_tokens()`  [EXTRACTED]
  moa-brain/src/pipeline/mod.rs → moa-hands/src/router/mod.rs

## Hyperedges (group relationships)
- **Surgical Workflow Validation Flow** — doc_e2e_watch_for, concept_str_replace_edit, concept_session_lifecycle, concept_approval_rules [INFERRED 0.85]
- **Retest Validation Artifacts** — doc_e2e_post_run_validation, concept_sessions_db, concept_approval_rules, concept_applied_workspace [INFERRED 0.80]
- **Live E2E Provider Fixture Set** — live_e2e_openai_doc, live_e2e_anthropic_doc, live_e2e_google_doc, live_e2e_marker_concept [INFERRED 0.80]
- **Required Markdown Summary Sections** — concept_markdown_summary, section_goal, section_decisions, section_files_touched, section_current_state, section_open_questions [EXTRACTED 1.00]
- **Summarizer Input-to-Output Flow** — prompt_summarizer, concept_checkpoint_summary, concept_session_events, concept_markdown_summary [INFERRED 0.90]
- **macOS build chain (GPUI + xcrun metal + full Xcode requirement)** — concept_gpui, concept_xcrun_metal, readme_prerequisites, readme_troubleshooting [INFERRED 0.90]
- **Density Mode Flow (setting, struct, reader, persistence)** — doc_design_density, concept_density_comfortable_compact, code_density_current, code_config_toml [INFERRED 0.90]
- **Semantic Token Three-Bucket Grouping** — doc_design_semantic_tokens, code_theme_tokens_rs, concept_linear_three_variable_theme, code_gpui_component_theme [EXTRACTED 0.95]
- **Shared Component Composition Rule** — doc_design_components, code_components_icon_button, code_components_settings_row, code_components_section_card, code_components_nav_item [EXTRACTED 0.95]

## Communities

### Community 0 - "File Memory Store"
Cohesion: 0.02
Nodes (89): Evaluator, approval_heavy_plans(), approval_heavy_sessions_auto_deny_cleanly_under_concurrency(), auxiliary_scripted_response(), build_backend(), build_daemon_target(), build_local_target(), build_session_plans() (+81 more)

### Community 1 - "Runtime Events & SSE"
Cohesion: 0.02
Nodes (70): group_by_type(), hash_path(), MemoryList, MemoryPageSelected, type_badge(), type_label(), append_footer(), artifact_storage_footer() (+62 more)

### Community 2 - "Core Errors & Traits"
Cohesion: 0.02
Nodes (58): EvalError, MoaError, approval_requested_event_round_trips_full_prompt(), Event, sample_approval_prompt(), HandHandle, HandResources, HandSpec (+50 more)

### Community 3 - "Brain Turn Tests"
Cohesion: 0.03
Nodes (44): always_allow_rule_persists_and_skips_next_approval(), ArtifactRetrievalLlmProvider, ArtifactStderrLlmProvider, canary_leaks_in_tool_input_are_detected_and_blocked(), CanaryLeakLlmProvider, CapturingTextLlmProvider, count_lines(), extract_tool_id_field() (+36 more)

### Community 4 - "Local Orchestrator Tests"
Cohesion: 0.05
Nodes (71): approval_requested_event_persists_full_prompt_details(), blank_session_waits_for_first_message(), burst_of_queued_messages_preserves_fifo_under_hot_session_pressure(), collect_runtime_events_until(), compaction_uses_auxiliary_model_router_tier(), completed_tool_turn_destroys_cached_hand(), create_test_store(), CurrentDirGuard (+63 more)

### Community 5 - "Memory Store Tests"
Cohesion: 0.03
Nodes (62): Consolidate, ConsolidateImpl, ConsolidateReport, ConsolidateRequest, memory_page(), object_url(), register_deployment(), spawn_orchestrator() (+54 more)

### Community 6 - "Schema Migrations"
Cohesion: 0.03
Nodes (55): cache_optimizer_plans_tool_static_and_conversation_breakpoints(), cache_optimizer_skips_conversation_breakpoint_for_short_sessions(), CacheOptimizer, history_end_index(), latest_conversation_cache_candidate(), place_bp4_conversation(), plan_cache_controls(), replan_on_lookback_overflow() (+47 more)

### Community 7 - "CLI Entry Point"
Cohesion: 0.03
Nodes (84): apply_config_update(), Args, best_effort_deregister(), bind_listener(), cache_stats_report(), CacheCommand, CacheStatsArgs, checkpoint_cleanup_report() (+76 more)

### Community 8 - "Core Config"
Cohesion: 0.03
Nodes (46): budget_config_defaults_are_applied(), BudgetConfig, CloudConfig, CloudFlyioConfig, CloudHandsConfig, compaction_config_defaults_are_applied(), CompactionConfig, config_loads_from_file() (+38 more)

### Community 9 - "Exec Command Flow"
Cohesion: 0.04
Nodes (56): bash_output_preserves_full_process_streams(), bash_output_small_streams_are_not_truncated(), BashToolInput, build_bash_output(), execute_docker(), execute_local(), batcher_holds_events_until_interval_elapses(), flush_returns_remaining_events() (+48 more)

### Community 10 - "Session State Types"
Cohesion: 0.04
Nodes (59): approval_outcome_label(), approval_wait_timeout(), approval_wait_timeout_from_env(), awakeable_decision_round_trips_through_json_payload(), BufferedUserMessage, cancellation_requested(), cancelled_response_maps_to_cancelled_outcome(), CancelMode (+51 more)

### Community 11 - "History Compiler"
Cohesion: 0.05
Nodes (52): build_events_from_turn_specs(), build_file_read_dedup_state(), build_full_file_read_path_map(), build_snapshot_state(), capabilities(), compacted_view_preserves_old_errors_and_respects_budget(), compaction_triggers_at_threshold_and_keeps_full_log(), compile_records() (+44 more)

### Community 12 - "Skill Document Format"
Cohesion: 0.05
Nodes (38): build_distillation_prompt(), count_tool_calls(), extract_task_summary(), find_similar_skill(), maybe_distill_skill(), normalize_new_skill(), similarity_score(), tokenize() (+30 more)

### Community 13 - "Sub-Agent Dispatch"
Cohesion: 0.04
Nodes (53): append_parent_event(), apply_response_to_history(), approval_wait_timeout(), approval_wait_timeout_from_env(), build_completion_request(), build_result_uses_terminal_state(), configured_model_capabilities(), dispatch_sub_agent() (+45 more)

### Community 14 - "Memory Types & Scopes"
Cohesion: 0.04
Nodes (40): ConfidenceLevel, count_ingest_pages(), derive_source_name_from_content(), extract_search_keywords(), extract_search_query_from_messages(), format_ingest_report(), infer_page_title(), infer_page_type() (+32 more)

### Community 15 - "Live Cache Audit Tests"
Cohesion: 0.05
Nodes (51): backfill_from(), listen_stream_backfills_from_last_seen_sequence(), listen_stream_receives_cross_store_events_in_order(), NotifyPayload, rolled_back_notify_is_not_observed(), run_listener_task(), seed_session(), session_channel_name() (+43 more)

### Community 16 - "Local Orchestrator"
Cohesion: 0.06
Nodes (34): accept_user_message(), append_event(), append_pause_summary(), best_effort_resolve_pending_signal(), collect_turn_tool_summaries(), detect_docker(), detect_workspace_path(), docker_status() (+26 more)

### Community 17 - "Anthropic Provider"
Cohesion: 0.06
Nodes (52): annotate_cache_control(), annotate_message_cache_control(), anthropic_content_blocks(), anthropic_content_blocks_render_text_and_json_as_text_blocks(), anthropic_message(), anthropic_message_wraps_assistant_tool_calls_as_tool_use_blocks(), anthropic_message_wraps_tool_results_with_tool_use_id(), anthropic_text_block() (+44 more)

### Community 18 - "Gemini Provider"
Cohesion: 0.06
Nodes (51): build_cache_create_body(), build_contents_from_messages(), build_explicit_cache_plan(), build_generation_config(), build_request_body(), build_request_body_from_parts(), build_request_parts(), build_tools() (+43 more)

### Community 19 - "File Read & Write Tools"
Cohesion: 0.05
Nodes (63): container_path_validation_accepts_workspace_absolute_paths(), container_path_validation_rejects_absolute_paths_outside_workspace(), container_path_validation_rejects_traversal(), docker_file_read(), docker_file_search(), docker_file_write(), docker_find_args(), docker_read_args() (+55 more)

### Community 20 - "Postgres Session Store"
Cohesion: 0.06
Nodes (14): approval_rule_from_row(), pending_signal_from_row(), pending_signal_type_from_db(), platform_from_db(), policy_action_from_db(), policy_scope_from_db(), session_meta_from_row(), session_status_from_db() (+6 more)

### Community 21 - "Checkpoint Compaction"
Cohesion: 0.05
Nodes (38): calculate_cost_cents(), CheckpointState, compaction_request(), event_summary_line(), latest_checkpoint_state(), maybe_compact(), maybe_compact_events(), non_checkpoint_events() (+30 more)

### Community 22 - "Tool Types & Policy"
Cohesion: 0.04
Nodes (27): default_budget_for_tool(), execute_tool_policy(), RegisteredTool, ToolExecution, ToolRegistry, capabilities(), IdempotencyClass, tool_name() (+19 more)

### Community 23 - "Daemon Service"
Cohesion: 0.06
Nodes (54): daemon_create_session_uses_explicit_client_scope(), daemon_health_endpoint_responds_when_cloud_enabled(), daemon_info(), daemon_lists_session_previews(), daemon_log_path(), daemon_logs(), daemon_pid_path(), daemon_ping_create_and_shutdown_roundtrip() (+46 more)

### Community 24 - "Orchestrator Test Harness"
Cohesion: 0.05
Nodes (43): approval_allow_once_round_trip_through_restate(), configured_env(), live_model(), object_url(), register_deployment(), spawn_orchestrator(), wait_for_approval_request(), wait_for_status() (+35 more)

### Community 25 - "Wiki Search Index"
Cohesion: 0.08
Nodes (37): add_rrf_score(), claim_embedding_batch_sql(), classify_embedding_error(), confidence_as_str(), delete_queue_rows(), embedding_failures_counter(), embedding_queue_depth_gauge(), EmbeddingIndexStatus (+29 more)

### Community 26 - "Memory Detail Panel"
Cohesion: 0.06
Nodes (24): aggregate_brain_usage(), collect_turn_costs(), count_event_type(), count_pending_approvals(), count_turns(), DetailPanel, DetailTab, estimated_context_window() (+16 more)

### Community 27 - "OpenAI Responses Provider"
Cohesion: 0.06
Nodes (31): canonical_model_id(), capabilities_for_model(), gpt_5_4_family_reports_expected_capabilities(), native_web_search_tools(), OpenAIProvider, build_function_tool(), build_responses_request(), build_responses_request_sets_prompt_cache_key_and_retention() (+23 more)

### Community 28 - "Session Replay Snapshots"
Cohesion: 0.06
Nodes (17): approval_decision_size(), approval_prompt_size(), approx_event_bytes(), counted_store_is_noop_outside_scope(), counted_store_records_get_events_within_scope(), CountedSessionStore, display_duration_ms(), event_payload_size() (+9 more)

### Community 29 - "LLM Gateway Service"
Cohesion: 0.07
Nodes (31): build_anthropic_provider(), build_google_provider(), build_openai_provider(), CompletionRequest, CompletionRequestExt, CompletionStreamHandle, compute_cost_cents(), configured_env() (+23 more)

### Community 30 - "Local Tool Tests"
Cohesion: 0.07
Nodes (35): approval_prompt_str_replace_diff_is_surgical(), approval_prompt_uses_remembered_workspace_root_for_commands(), bash_captures_stdout_and_stderr(), bash_error_output_is_not_truncated(), bash_respects_timeout(), bash_success_output_is_truncated_to_router_budget(), EmptyMemoryStore, file_operations_reject_path_traversal() (+27 more)

### Community 31 - "Turn Streaming & Approval"
Cohesion: 0.05
Nodes (30): build_turn_context(), BuildTurnContextOptions, persist_context_snapshot(), drain_signal_queue(), handle_stream_signal(), run_streamed_turn_with_tools_mode(), append_tool_call_event(), emit_tool_output_warning() (+22 more)

### Community 32 - "Skill Injection Stage"
Cohesion: 0.08
Nodes (31): allowed_tools(), budget_limit_skips_expensive_tests(), distills_skill_after_tool_heavy_session(), estimate_skill_tokens(), fixed_time(), improvement_accepted_when_scores_better(), improvement_rejected_on_regression(), ImprovementAndEvalLlm (+23 more)

### Community 33 - "Markdown Stream Healing"
Cohesion: 0.08
Nodes (35): byte_is_word_char(), content_is_empty_or_only_markers(), count_double_asterisks_outside_code(), count_double_marker_outside_code(), count_double_underscores_outside_code(), count_single_asterisks(), count_single_backticks(), count_single_marker() (+27 more)

### Community 34 - "Neon Branch Manager"
Cohesion: 0.09
Nodes (25): checkpoint_branch_names_follow_moa_prefix(), checkpoint_info_from_branch(), checkpoint_label_from_name(), cleanup_expired_deletes_only_old_moa_branches(), create_checkpoint_refuses_to_exceed_capacity(), create_checkpoint_sends_expected_request_and_returns_handle(), discard_checkpoint_calls_delete_endpoint(), format_checkpoint_branch_name() (+17 more)

### Community 35 - "Memory Consolidation"
Cohesion: 0.08
Nodes (40): canonical_port_claims(), confidence_rank(), consolidation_due_for_scope(), consolidation_resolves_dates_prunes_and_refreshes_index(), ConsolidationReport, decay_confidence(), extract_port_claims(), inbound_reference_counts() (+32 more)

### Community 36 - "Tool Result Store"
Cohesion: 0.06
Nodes (16): collect_context(), load_tool_result_text(), MockSessionStore, parse_tool_id(), render_search_summary(), search_tool_result(), SearchContextLine, SearchMatch (+8 more)

### Community 37 - "Daytona Sandbox Provider"
Cohesion: 0.09
Nodes (23): build_url(), DaytonaHandProvider, default_headers(), derive_toolbox_url(), expect_success(), expect_success_json(), extract_workspace_id(), http_error() (+15 more)

### Community 38 - "Tool Usage Stats"
Cohesion: 0.1
Nodes (36): annotate_schema(), annotation_warns_on_low_success(), apply_tool_rankings(), cache_stability_preserves_identical_ranked_output(), collect_session_tool_observations(), compare_f64_asc(), compare_f64_desc(), compare_failure_last() (+28 more)

### Community 39 - "Tool Executor Service"
Cohesion: 0.09
Nodes (27): append_tool_call_event(), append_tool_error_event(), append_tool_result_event(), build_tool_run_plan(), CountingTool, has_prior_non_idempotent_result(), idempotent_tool_retries_freely(), keyed_tool_requires_idempotency_key() (+19 more)

### Community 40 - "Gateway Message Renderer"
Cohesion: 0.09
Nodes (23): append_piece(), discord_renderer_attaches_buttons_to_last_chunk_only(), discord_renderer_uses_embed_limit_for_long_text(), DiscordRenderChunk, DiscordRenderer, render_approval_request(), render_diff(), render_tool_card() (+15 more)

### Community 41 - "Broadcast Lag Handling"
Cohesion: 0.07
Nodes (16): record_broadcast_lag(), recv_with_lag_handling(), RecvResult, BroadcastChannel, ClaimCheck, event_stream_abort_policy_surfaces_error(), event_stream_emits_gap_marker_when_lagged(), EventFilter (+8 more)

### Community 42 - "Local Chat Runtime"
Cohesion: 0.07
Nodes (2): ChatRuntime, LocalChatRuntime

### Community 43 - "E2B Sandbox Provider"
Cohesion: 0.14
Nodes (16): build_url(), ConnectedSandbox, decode_stream_chunk(), default_headers(), E2BHandProvider, encode_connect_request(), encode_test_envelopes(), envd_headers() (+8 more)

### Community 44 - "Eval Engine & Plan"
Cohesion: 0.11
Nodes (21): build_error_result(), cleanup_workspace(), dry_run_marks_results_skipped(), EngineOptions, EvalEngine, EvalRun, extract_trace_id(), fs_try_exists() (+13 more)

### Community 45 - "Working Context Messages"
Cohesion: 0.09
Nodes (9): context_message_assistant_tool_call_preserves_invocation(), context_message_tool_result_preserves_text_and_blocks(), context_message_tool_still_defaults_to_text_only(), ContextMessage, estimate_text_tokens(), MessageRole, ProcessorOutput, working_context_into_request_preserves_cache_breakpoints() (+1 more)

### Community 46 - "Discord Adapter"
Cohesion: 0.12
Nodes (18): approval_callback_maps_to_control_message(), attachments_from_message(), context_from_component(), discord_button(), discord_create_message(), discord_create_message_includes_buttons_for_last_chunk(), discord_edit_message(), discord_embed() (+10 more)

### Community 47 - "MCP Client Discovery"
Cohesion: 0.12
Nodes (18): flatten_call_result(), flatten_tool_result_aggregates_text_items(), header_map_from_pairs(), http_client_sends_headers_and_parses_jsonrpc(), MCPClient, McpDiscoveredTool, McpTransport, parse_jsonrpc_result() (+10 more)

### Community 48 - "Daemon Chat Runtime"
Cohesion: 0.07
Nodes (1): DaemonChatRuntime

### Community 49 - "Desktop App Shell"
Cohesion: 0.08
Nodes (9): MoaApp, compact_is_tighter_than_comfortable(), current(), Density, Spacing, build_default_icon(), install(), TrayHandle (+1 more)

### Community 50 - "Completion Request Types"
Cohesion: 0.07
Nodes (13): CacheBreakpoint, CacheBreakpointTarget, CacheTtl, completion_stream_abort_stops_completion_task(), CompletionContent, CompletionRequest, CompletionResponse, CompletionStream (+5 more)

### Community 51 - "Slack Adapter"
Cohesion: 0.13
Nodes (19): handle_interaction_event(), handle_push_event(), inbound_from_app_mention(), inbound_from_interaction_event(), inbound_from_message_event(), inbound_from_push_event(), interaction_origin(), parses_approval_callback_into_control_message() (+11 more)

### Community 52 - "Turn Latency Counters"
Cohesion: 0.11
Nodes (14): current_turn_root_span(), display_duration_ms(), record_turn_compaction(), record_turn_event_persist_duration(), record_turn_llm_call_duration(), record_turn_llm_ttft(), record_turn_pipeline_compile_duration(), record_turn_snapshot_load() (+6 more)

### Community 53 - "Eval Agent Setup"
Cohesion: 0.12
Nodes (28): AgentEnvironment, apply_skill_overrides(), build_agent_environment(), build_agent_environment_with_provider(), build_eval_policies(), build_pipeline(), build_skill_memory_path(), build_tool_router() (+20 more)

### Community 54 - "Provider Selection & Routing"
Cohesion: 0.11
Nodes (21): build_provider_from_config(), build_provider_from_selection(), explicit_provider_prefix_overrides_inference(), infer_provider_name(), infers_anthropic_for_claude_models(), infers_google_for_gemini_models(), infers_openai_for_gpt_models(), is_openai_model() (+13 more)

### Community 55 - "Skill Regression Testing"
Cohesion: 0.11
Nodes (25): append_skill_regression_log(), build_generated_suite(), compare_scores(), default_skill_evaluators(), estimate_suite_cost(), estimate_tokens(), execute_skill_suite(), extract_task_input() (+17 more)

### Community 56 - "Wiki & Memory Branching"
Cohesion: 0.12
Nodes (28): append_change_manifest(), branch_dir(), branch_file_path(), branch_root(), ChangeOperation, ChangeRecord, list_branches(), merge_markdown() (+20 more)

### Community 57 - "Session Store Service"
Cohesion: 0.14
Nodes (15): append_event_increments_sequence(), AppendEventRequest, cleanup(), get_events_respects_range(), GetEventsRequest, InitSessionVoRequest, search_events_finds_by_payload(), SearchEventsRequest (+7 more)

### Community 58 - "Tool Router Policy"
Cohesion: 0.09
Nodes (16): approval_diffs_for(), approval_fields_for(), approval_pattern_chained_inner_uses_first_subcommand(), approval_pattern_for(), approval_pattern_malformed_wrapper_falls_back_to_full_input(), approval_pattern_nested_shell_not_recursed(), approval_pattern_simple_command(), approval_pattern_single_token() (+8 more)

### Community 59 - "LLM Span Instrumentation"
Cohesion: 0.12
Nodes (17): calculate_cost(), calculate_cost_with_cached(), cost_calculation_correct(), has_meaningful_output(), llm_span_name(), LLMSpanAttributes, LLMSpanRecorder, metadata_f64() (+9 more)

### Community 60 - "MCP Credential Proxy"
Cohesion: 0.12
Nodes (11): credential_from_env(), default_scope_for(), env_var(), environment_vault_loads_from_env_backed_server_config(), EnvironmentCredentialVault, headers_from_credential(), MCPCredentialProxy, McpSessionToken (+3 more)

### Community 61 - "Command Palette & Keybindings"
Cohesion: 0.12
Nodes (9): CommandEntry, CommandPalette, default_commands(), fuzzy_score(), initial_ordering(), PaletteDismissed, PaletteHistory, rewards_consecutive() (+1 more)

### Community 62 - "Live Observability Tests"
Cohesion: 0.11
Nodes (13): FieldRecorder, global_trace_recorder(), live_observability_audit_tracks_cache_replay_and_latency(), live_orchestrator(), queue_message(), RecordedEvent, RecordedFields, RecordedSpan (+5 more)

### Community 63 - "Telegram Adapter"
Cohesion: 0.16
Nodes (13): channel_from_chat_and_reply(), handle_callback_query(), handle_message(), inbound_from_callback_query(), inbound_from_message(), inline_keyboard(), parse_message_id(), parses_approval_callback_into_control_message() (+5 more)

### Community 64 - "Cache Observability"
Cohesion: 0.13
Nodes (16): add_session_trace_link(), CacheReport, fingerprint_json(), full_request_fingerprint(), generate_trace_tags(), normalize_environment(), sanitize_langfuse_id(), stable_prefix_fingerprint() (+8 more)

### Community 65 - "Brain Integration Tests"
Cohesion: 0.11
Nodes (16): assert_replay_flattening(), assert_turn_latency_spans(), build_auth_source(), build_scripted_provider(), cached_usage(), collect_cache_control_ttls(), collect_tool_runs(), extend_tool_schemas() (+8 more)

### Community 66 - "Desktop Design System"
Cohesion: 0.09
Nodes (26): components::icon_button, components::nav_item, components::section_card, components::settings_row, ~/.moa/config.toml (desktop.density), density::current(cx), gpui_component::ActiveTheme, gpui_component::select::Select (+18 more)

### Community 67 - "CLI HTTP API Server"
Cohesion: 0.11
Nodes (14): ApiState, build_api_router(), health_endpoint_returns_ok(), runtime_event_stream(), session_stream(), session_stream_emits_gap_event_when_runtime_subscriber_lags(), session_stream_returns_not_found_when_runtime_is_unavailable(), session_stream_returns_sse_content_type() (+6 more)

### Community 68 - "Embedding Provider"
Cohesion: 0.12
Nodes (13): add_feature(), build_embedding_provider_from_config(), char_trigrams(), EmbeddingProvider, mock_embedding_is_deterministic(), MockEmbedding, normalize(), OpenAIEmbedding (+5 more)

### Community 69 - "Session Blob Store"
Cohesion: 0.18
Nodes (12): claim_check_from_value(), collect_blob_refs(), collect_large_strings(), decode_event_from_storage(), encode_event_for_storage(), expand_local_path(), file_blob_store_deletes_session_directory(), file_blob_store_is_content_addressed() (+4 more)

### Community 70 - "Brain Bridge Runtime"
Cohesion: 0.11
Nodes (10): configured_config(), configured_memory_store(), configured_model_capabilities(), configured_session_store(), configured_tool_schemas(), prepare_turn_request(), PreparedTurnRequest, ServiceBridge (+2 more)

### Community 71 - "File Search Tool"
Cohesion: 0.13
Nodes (16): build_file_search_output(), collect_matches(), default_skipped_dirs(), default_skipped_dirs_includes_polyglot_ecosystem_directories(), execute(), execute_docker(), execute_respects_custom_skip_directories(), execute_skips_python_virtualenv_matches() (+8 more)

### Community 72 - "Approval Request Types"
Cohesion: 0.12
Nodes (18): approval_buttons(), approval_request(), ApprovalCallbackAction, ApprovalDecision, ApprovalField, ApprovalFileDiff, ApprovalPrompt, ApprovalRequest (+10 more)

### Community 73 - "Telemetry Guards"
Cohesion: 0.14
Nodes (17): build_grpc_metadata(), build_http_headers(), build_resource(), build_sampler(), build_span_exporter(), default_env_filter_directive(), grpc_metadata_uses_header_values(), init_observability() (+9 more)

### Community 74 - "Scripted Test Provider"
Cohesion: 0.14
Nodes (3): ScriptedBlock, ScriptedProvider, ScriptedResponse

### Community 75 - "Runtime Context Stage"
Cohesion: 0.19
Nodes (12): build_runtime_reminder(), capabilities(), Clock, detect_git_branch(), FixedClock, runtime_context_changes_when_clock_advances(), runtime_context_insertion_index(), runtime_context_inserts_before_trailing_user_turn() (+4 more)

### Community 76 - "Session Analytics"
Cohesion: 0.15
Nodes (15): analytics_window_start(), CacheDailyMetric, get_session_summary(), get_workspace_stats(), list_cache_daily_metrics(), list_session_turn_metrics(), list_tool_call_summaries(), normalized_days() (+7 more)

### Community 77 - "Chat Message Bubbles"
Cohesion: 0.17
Nodes (18): agent_bubble(), approval_card(), decision_button(), detail_card(), error_bubble(), render_message(), system_bubble(), thinking_bubble() (+10 more)

### Community 78 - "Desktop README"
Cohesion: 0.1
Nodes (21): Approval card (Y/A/N), Drag-and-drop attachment chip, Command palette, ~/.moa/config.toml, Environment-sourced provider API keys, GPUI framework, MOA runtime (sessions/memory/skills/providers), rustup.rs toolchain installer (+13 more)

### Community 79 - "Model Capabilities"
Cohesion: 0.11
Nodes (6): Credential, ModelCapabilities, ModelCapabilitiesBuilder, ProviderNativeTool, TokenPricing, ToolCallFormat

### Community 80 - "Grep Tool"
Cohesion: 0.17
Nodes (18): build_grep_output(), collect_context(), ContextLine, execute(), grep_finds_matching_lines(), grep_includes_context_lines(), grep_respects_gitignore(), grep_respects_skip_directories() (+10 more)

### Community 81 - "Postgres Schema Migrations"
Cohesion: 0.2
Nodes (18): compile_for_gemini(), compile_for_gemini_removes_additional_properties_recursively(), compile_for_openai_strict(), compile_for_openai_strict_adds_additional_properties_false_recursively(), compile_for_openai_strict_does_not_duplicate_null_in_type_arrays(), compile_for_openai_strict_makes_optional_properties_required_and_nullable(), compile_for_openai_strict_preserves_existing_required_properties(), compile_for_openai_strict_strips_validation_only_keywords() (+10 more)

### Community 82 - "Live Provider Matrix"
Cohesion: 0.25
Nodes (13): available_live_providers(), complete_until(), google_live_model(), live_providers_answer_simple_prompt_across_available_keys(), live_providers_can_use_native_web_search_across_available_keys(), live_providers_emit_tool_calls_across_available_keys(), live_providers_obey_system_prompt_across_available_keys(), live_providers_preserve_unicode_across_available_keys() (+5 more)

### Community 83 - "OpenAI Provider Tests"
Cohesion: 0.22
Nodes (15): openai_provider_does_not_retry_after_partial_stream_output(), openai_provider_drops_oversized_metadata_values(), openai_provider_includes_native_web_search_when_enabled(), openai_provider_omits_native_web_search_when_disabled(), openai_provider_retries_after_rate_limit(), openai_provider_serializes_assistant_tool_calls_as_function_call_items(), openai_provider_serializes_tool_result_messages_as_function_call_output(), openai_provider_streams_parallel_tool_calls_in_order() (+7 more)

### Community 84 - "Security Policies"
Cohesion: 0.23
Nodes (11): ApprovalRuleStore, glob_match(), parse_and_match_bash(), persistent_rule_matching_uses_glob_patterns(), PolicyCheck, read_tools_are_auto_approved_and_bash_requires_approval(), rule_matches(), rule_visible_to_workspace() (+3 more)

### Community 85 - "Tool Router Construction"
Cohesion: 0.15
Nodes (2): default_cloud_provider(), ToolRouter

### Community 86 - "Memory Bootstrap"
Cohesion: 0.24
Nodes (14): BootstrapReport, BootstrapSentinel, find_instruction_file(), index_page_with_instructions(), is_bootstrap_index(), minimal_index_page(), project_instructions_page(), run_bootstrap() (+6 more)

### Community 87 - "Encrypted Secret Vault"
Cohesion: 0.3
Nodes (4): decrypt_bytes(), encrypt_bytes(), file_vault_encrypts_and_decrypts_roundtrip(), FileVault

### Community 88 - "Session Search Tool"
Cohesion: 0.16
Nodes (6): event_snippet(), render_results(), SessionSearchEventType, SessionSearchInput, SessionSearchTool, truncate()

### Community 89 - "Session Row Badges"
Cohesion: 0.2
Nodes (11): confidence_badge(), confidence_color(), confidence_label(), risk_badge(), risk_color(), risk_label(), status_badge(), status_color() (+3 more)

### Community 90 - "Chat Panel"
Cohesion: 0.27
Nodes (2): ChatPanel, PendingToast

### Community 91 - "Prompt Injection Defense"
Cohesion: 0.26
Nodes (12): canary_detection_works(), check_canary(), classifier_flags_known_attack_patterns(), classify_input(), contains_canary_tokens(), inject_canary(), InputClassification, InputInspection (+4 more)

### Community 92 - "Window State Persistence"
Cohesion: 0.28
Nodes (4): round_trips_through_disk(), validate_bounds_clamps_offscreen_positions(), validate_bounds_clamps_tiny_panels(), WindowState

### Community 93 - "Neon Branch Manager Tests"
Cohesion: 0.35
Nodes (11): live_neon_config(), live_neon_config_with_limit(), neon_branch_manager_create_list_get_rollback_and_discard_checkpoint(), neon_checkpoint_branch_connection_is_copy_on_write(), neon_checkpoint_capacity_limit_rejects_extra_branch(), neon_checkpoint_cleanup_without_expired_branches_returns_zero(), neon_live_lock(), NeonBranch (+3 more)

### Community 94 - "Eval Terminal Reporter"
Cohesion: 0.3
Nodes (5): format_scores(), render_includes_case_names_and_summary(), render_verbose_case(), result_index(), TerminalReporter

### Community 95 - "E2E Retest Plan"
Cohesion: 0.21
Nodes (12): Applied Workspace, Approval Rules Store, moa exec CLI, Session Lifecycle (running/waiting_approval/cancelled), ~/.moa/sessions.db, str_replace Surgical Edit Path, Retest Objective (Narrow Surgical Workflow), Pass Criteria (+4 more)

### Community 96 - "Turn Loop Detector"
Cohesion: 0.33
Nodes (6): loop_detector_disabled_at_zero_threshold(), loop_detector_does_not_trigger_on_varied_calls(), loop_detector_resets(), loop_detector_sliding_window(), loop_detector_triggers_after_threshold(), LoopDetector

### Community 97 - "Session List"
Cohesion: 0.4
Nodes (2): SessionList, SessionSelected

### Community 98 - "Instruction Stage"
Cohesion: 0.38
Nodes (4): combine_workspace_instructions(), instruction_processor_appends_config_backed_sections(), instruction_processor_combines_config_and_discovered_workspace_instructions(), InstructionProcessor

### Community 99 - "Session Sidebar"
Cohesion: 0.24
Nodes (1): SessionSidebar

### Community 100 - "Session Summarizer Prompt"
Cohesion: 0.22
Nodes (10): Checkpoint Summary, Long-Running AI-Agent Session, Compact Markdown Summary, Session Events, Session Summarizer Prompt, Current State Section, Decisions Section, Files Touched Section (+2 more)

### Community 101 - "Identity Stage"
Cohesion: 0.44
Nodes (3): identity_processor_appends_system_prompt(), identity_prompt_includes_coding_guardrails(), IdentityProcessor

### Community 102 - "Session VO Tests"
Cohesion: 0.5
Nodes (6): session_vo_destroy_clears_state(), session_vo_post_message_queues_in_state(), session_vo_post_message_updates_status_to_running_then_idle_parks_paused(), session_vo_post_message_without_meta_errors(), test_message(), test_meta()

### Community 103 - "Health Service"
Cohesion: 0.32
Nodes (4): Health, HealthImpl, version_info_reports_expected_versions(), VersionInfo

### Community 104 - "WCAG Contrast Helpers"
Cohesion: 0.39
Nodes (6): black_on_white_is_max_ratio(), classify(), contrast_ratio(), equal_colors_are_ratio_one(), relative_luminance(), WcagPass

### Community 105 - "Trajectory Match Evaluator"
Cohesion: 0.43
Nodes (4): exact_match_scores_one(), lcs_len(), partial_match_scores_below_one(), TrajectoryMatchEvaluator

### Community 106 - "Output Match Evaluator"
Cohesion: 0.43
Nodes (4): contains_rules_pass_when_all_terms_match(), evaluate_output(), missing_contains_term_reduces_score(), OutputMatchEvaluator

### Community 107 - "Skill List Panel"
Cohesion: 0.38
Nodes (1): SkillList

### Community 108 - "Live E2E Fixtures"
Cohesion: 0.43
Nodes (7): LIVE-E2E-ANTHROPIC Fixture, LIVE-E2E-GOOGLE Fixture, Live End-to-End Test Marker, LIVE-E2E-OPENAI Fixture, Anthropic Provider, Google Provider, OpenAI Provider

### Community 109 - "Threshold Evaluator"
Cohesion: 0.53
Nodes (3): cost_over_budget_fails_boolean_score(), limit_score(), ThresholdEvaluator

### Community 110 - "Desktop Notifications"
Cohesion: 0.4
Nodes (2): error(), sticky_error()

### Community 111 - "Providers Settings Tab"
Cohesion: 0.47
Nodes (4): collect_providers(), provider_control(), ProviderInfo, render_providers_tab()

### Community 112 - "Empty State Component"
Cohesion: 0.4
Nodes (2): empty_state(), empty_state_any()

### Community 113 - "Shell Chain Splitter"
Cohesion: 0.5
Nodes (2): push_sub_command(), split_shell_chain()

### Community 114 - "Unified Diff"
Cohesion: 0.6
Nodes (3): compute_unified_diff(), small_edit_diff_is_substantially_smaller_than_full_file(), unified_diff_contains_standard_headers_and_hunks()

### Community 115 - "Gemini Live Tests"
Cohesion: 0.6
Nodes (3): gemini_live_completion_returns_expected_answer(), gemini_live_model(), gemini_live_web_search_returns_current_information()

### Community 116 - "Anthropic Provider Tests"
Cohesion: 0.4
Nodes (0): 

### Community 117 - "Desktop Status Bar"
Cohesion: 0.5
Nodes (1): MoaStatusBar

### Community 118 - "Event Color Mapping"
Cohesion: 0.6
Nodes (2): event_color(), EventColor

### Community 119 - "Cost Budget Enforcement"
Cohesion: 0.67
Nodes (2): enforce_workspace_budget(), format_budget_exhausted_message()

### Community 120 - "Tool Success Evaluator"
Cohesion: 0.5
Nodes (1): ToolSuccessEvaluator

### Community 121 - "Settings Integration Tests"
Cohesion: 0.83
Nodes (3): removing_from_auto_approve_persists(), scratch_dir(), settings_mutations_round_trip_through_config_file()

### Community 122 - "Desktop Title Bar"
Cohesion: 0.67
Nodes (1): MoaTitleBar

### Community 123 - "General Settings Tab"
Cohesion: 0.67
Nodes (2): render_general_tab(), render_model_card()

### Community 124 - "Error Banner Component"
Cohesion: 0.67
Nodes (2): error_banner(), error_banner_any()

### Community 125 - "OpenAI Live Test"
Cohesion: 0.67
Nodes (0): 

### Community 126 - "Anthropic Live Test"
Cohesion: 0.67
Nodes (0): 

### Community 127 - "Chat Harness Example"
Cohesion: 1.0
Nodes (2): main(), run_prompt()

### Community 128 - "Sidebar Tab Enum"
Cohesion: 0.67
Nodes (1): SidebarTab

### Community 129 - "Permissions Settings Tab"
Cohesion: 1.0
Nodes (2): render_permissions_tab(), tool_list_card()

### Community 130 - "Desktop Service Init"
Cohesion: 0.67
Nodes (1): InitializedServices

### Community 131 - "Model Identifier"
Cohesion: 1.0
Nodes (1): ModelId

### Community 132 - "Tool Call Identifier"
Cohesion: 1.0
Nodes (1): ToolCallId

### Community 133 - "Provider HTTP Client"
Cohesion: 1.0
Nodes (0): 

### Community 134 - "Session Engine Gating"
Cohesion: 1.0
Nodes (0): 

### Community 135 - "Docker Hardening Test"
Cohesion: 1.0
Nodes (0): 

### Community 136 - "Eval Live Tests"
Cohesion: 1.0
Nodes (0): 

### Community 137 - "Chat Runtime Trait"
Cohesion: 1.0
Nodes (1): ChatRuntime

### Community 138 - "Keyboard Shortcuts Tab"
Cohesion: 1.0
Nodes (0): 

### Community 139 - "Icon Button"
Cohesion: 1.0
Nodes (0): 

### Community 140 - "Section Card"
Cohesion: 1.0
Nodes (0): 

### Community 141 - "Segmented Control"
Cohesion: 1.0
Nodes (0): 

### Community 142 - "Settings Row"
Cohesion: 1.0
Nodes (0): 

### Community 143 - "Markdown Style"
Cohesion: 1.0
Nodes (0): 

### Community 144 - "Nav Item"
Cohesion: 1.0
Nodes (0): 

### Community 145 - "Core Type Macros"
Cohesion: 1.0
Nodes (0): 

### Community 146 - "Integration Test Entry"
Cohesion: 1.0
Nodes (0): 

### Community 147 - "History Replay Regressions"
Cohesion: 1.0
Nodes (1): Proptest History Regression Seeds

### Community 148 - "History Regression Seeds"
Cohesion: 1.0
Nodes (1): FullRead Replay Regression Seed

## Knowledge Gaps
- **419 isolated node(s):** `LogChange`, `LogEntry`, `BootstrapReport`, `BootstrapSentinel`, `PageFrontmatter` (+414 more)
  These have ≤1 connection - possible missing edges or undocumented components.
- **Thin community `Model Identifier`** (2 nodes): `ModelId`, `.default()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Tool Call Identifier`** (2 nodes): `ToolCallId`, `.from()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Provider HTTP Client`** (2 nodes): `http.rs`, `build_http_client()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Session Engine Gating`** (2 nodes): `session_engine.rs`, `session_requires_processing()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Docker Hardening Test`** (2 nodes): `docker_hardening.rs`, `docker_container_runs_with_hardening()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Eval Live Tests`** (2 nodes): `engine_live.rs`, `live_run_single_produces_eval_result()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Chat Runtime Trait`** (2 nodes): `ChatRuntime`, `.set_model()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Keyboard Shortcuts Tab`** (2 nodes): `keyboard_shortcuts_tab.rs`, `render_keyboard_shortcuts_tab()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Icon Button`** (2 nodes): `icon_button.rs`, `icon_button()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Section Card`** (2 nodes): `section.rs`, `section_card()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Segmented Control`** (2 nodes): `segmented.rs`, `segmented()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Settings Row`** (2 nodes): `row.rs`, `settings_row()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Markdown Style`** (2 nodes): `markdown.rs`, `markdown_style()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Nav Item`** (2 nodes): `nav.rs`, `nav_item()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Core Type Macros`** (1 nodes): `macros.rs`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Integration Test Entry`** (1 nodes): `integration.rs`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `History Replay Regressions`** (1 nodes): `Proptest History Regression Seeds`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `History Regression Seeds`** (1 nodes): `FullRead Replay Regression Seed`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **Why does `LocalChatRuntime` connect `Local Chat Runtime` to `Local Orchestrator`?**
  _High betweenness centrality (0.013) - this node is a cross-community bridge._
- **Why does `DaemonChatRuntime` connect `Daemon Chat Runtime` to `Daemon Service`?**
  _High betweenness centrality (0.012) - this node is a cross-community bridge._
- **What connects `LogChange`, `LogEntry`, `BootstrapReport` to the rest of the system?**
  _419 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `File Memory Store` be split into smaller, more focused modules?**
  _Cohesion score 0.02 - nodes in this community are weakly interconnected._
- **Should `Runtime Events & SSE` be split into smaller, more focused modules?**
  _Cohesion score 0.02 - nodes in this community are weakly interconnected._
- **Should `Core Errors & Traits` be split into smaller, more focused modules?**
  _Cohesion score 0.02 - nodes in this community are weakly interconnected._
- **Should `Brain Turn Tests` be split into smaller, more focused modules?**
  _Cohesion score 0.03 - nodes in this community are weakly interconnected._