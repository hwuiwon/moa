# Graph Report - .  (2026-05-05)

## Corpus Check
- Large corpus: 381 files · ~297,097 words. Semantic extraction will be expensive (many Claude tokens). Consider running on a subfolder, or use --no-semantic to run AST-only.

## Summary
- 5555 nodes · 9796 edges · 174 communities detected
- Extraction: 100% EXTRACTED · 0% INFERRED · 0% AMBIGUOUS · INFERRED: 13 edges (avg confidence: 0.8)
- Token cost: 0 input · 0 output

## God Nodes (most connected - your core abstractions)
1. `PostgresSessionStore` - 73 edges
2. `SessionStoreImpl` - 39 edges
3. `CountedSessionStore` - 32 edges
4. `SkillFrontmatter` - 31 edges
5. `start_session()` - 30 edges
6. `ToolRouter` - 30 edges
7. `LocalChatRuntime` - 29 edges
8. `DaemonChatRuntime` - 28 edges
9. `LocalOrchestrator` - 26 edges
10. `E2BHandProvider` - 23 edges

## Surprising Connections (you probably didn't know these)
- `LIVE-E2E-OPENAI Fixture` --semantically_similar_to--> `LIVE-E2E-ANTHROPIC Fixture`  [INFERRED] [semantically similar]
  live/openai.txt → live/anthropic.txt
- `LIVE-E2E-OPENAI Fixture` --semantically_similar_to--> `LIVE-E2E-GOOGLE Fixture`  [INFERRED] [semantically similar]
  live/openai.txt → live/google.txt
- `LIVE-E2E-ANTHROPIC Fixture` --semantically_similar_to--> `LIVE-E2E-GOOGLE Fixture`  [INFERRED] [semantically similar]
  live/anthropic.txt → live/google.txt
- `main()` --calls--> `bind_listener()`  [EXTRACTED]
  crates/moa-cli/src/main.rs → crates/moa-orchestrator/src/main.rs
- `cohere_rerank_v4_fast_prioritizes_relevant_retrieval_candidate()` --calls--> `live_cohere_key()`  [EXTRACTED]
  crates/moa-brain/tests/cohere_reranker_live.rs → crates/moa-memory-ingest/tests/cohere_reranker_live.rs

## Hyperedges (group relationships)
- **Surgical Workflow Validation Flow** — doc_e2e_watch_for, concept_str_replace_edit, concept_session_lifecycle, concept_approval_rules [INFERRED 0.85]
- **Retest Validation Artifacts** — doc_e2e_post_run_validation, concept_sessions_db, concept_approval_rules, concept_applied_workspace [INFERRED 0.80]
- **Live E2E Provider Fixture Set** — live_e2e_openai_doc, live_e2e_anthropic_doc, live_e2e_google_doc, live_e2e_marker_concept [INFERRED 0.80]

## Communities

### Community 0 - "File Memory Store"
Cohesion: 0.02
Nodes (124): ChangelogRecord, validate_scope(), write_and_bump(), EdgeLabel, EdgeWriteIntent, Evaluator, accept_user_message(), append_event() (+116 more)

### Community 1 - "Runtime Events & SSE"
Cohesion: 0.02
Nodes (88): CatalogIntent, IntentSource, IntentStatus, LearningEntry, TenantIntent, append_footer(), artifact_storage_footer(), bash_invocation() (+80 more)

### Community 2 - "Core Errors & Traits"
Cohesion: 0.02
Nodes (67): classifies_rate_limit_as_retryable(), classifies_repeated_timeout_as_reprovision(), classifies_unknown_tool_as_fatal(), classify_message_error(), classify_timeout_like_message(), classify_tool_error(), EvalError, GraphError (+59 more)

### Community 3 - "Core Config"
Cohesion: 0.02
Nodes (72): budget_config_defaults_are_applied(), BudgetConfig, CloudConfig, CloudFlyioConfig, CloudHandsConfig, compaction_config_defaults_are_applied(), CompactionConfig, config_loads_from_file() (+64 more)

### Community 4 - "CLI Entry Point"
Cohesion: 0.03
Nodes (111): BaseModel, apply_config_update(), Args, best_effort_deregister(), bind_listener(), cache_stats_report(), CacheCommand, CacheStatsArgs (+103 more)

### Community 5 - "Pgvector Store"
Cohesion: 0.03
Nodes (81): changelog_rejects_updates_for_app_role(), changelog_write_bumps_workspace_version_and_respects_read_rls(), record(), set_app_role(), set_auditor_role(), set_workspace_gucs(), AgTypeParam, Cypher (+73 more)

### Community 6 - "Skill Document Format"
Cohesion: 0.03
Nodes (70): append_skill_learning(), build_distillation_prompt(), count_tool_calls(), extract_task_summary(), find_similar_skill(), maybe_distill_skill(), maybe_distill_skill_with_learning(), normalize_new_skill() (+62 more)

### Community 7 - "Session State Types"
Cohesion: 0.03
Nodes (64): PreparedTurnRequest, ResolutionLabel, ResolutionScore, ScoringPhase, SegmentBaseline, SkillResolutionRate, BufferedUserMessage, CancelMode (+56 more)

### Community 8 - "File Search Tool"
Cohesion: 0.02
Nodes (74): build_file_search_output(), collect_matches(), default_skipped_dirs(), default_skipped_dirs_includes_polyglot_ecosystem_directories(), execute(), execute_docker(), execute_respects_custom_skip_directories(), execute_skips_python_virtualenv_matches() (+66 more)

### Community 9 - "Local Orchestrator Tests"
Cohesion: 0.05
Nodes (74): approval_requested_event_persists_full_prompt_details(), blank_session_waits_for_first_message(), burst_of_queued_messages_preserves_fifo_under_hot_session_pressure(), collect_runtime_events_until(), compaction_uses_auxiliary_model_router_tier(), completed_tool_turn_destroys_cached_hand(), create_test_store(), CurrentDirGuard (+66 more)

### Community 10 - "Exec Command Flow"
Cohesion: 0.03
Nodes (74): bash_output_preserves_full_process_streams(), bash_output_small_streams_are_not_truncated(), BashToolInput, build_bash_output(), execute_docker(), execute_local(), batcher_holds_events_until_interval_elapses(), flush_returns_remaining_events() (+66 more)

### Community 11 - "Postgres Session Store"
Cohesion: 0.04
Nodes (21): compile_for_gemini(), compile_for_gemini_removes_additional_properties_recursively(), compile_for_openai_strict(), compile_for_openai_strict_adds_additional_properties_false_recursively(), compile_for_openai_strict_does_not_duplicate_null_in_type_arrays(), compile_for_openai_strict_makes_optional_properties_required_and_nullable(), compile_for_openai_strict_preserves_existing_required_properties(), compile_for_openai_strict_strips_validation_only_keywords() (+13 more)

### Community 12 - "Brain Turn Tests"
Cohesion: 0.04
Nodes (36): always_allow_rule_persists_and_skips_next_approval(), ArtifactRetrievalLlmProvider, ArtifactStderrLlmProvider, canary_leaks_in_tool_input_are_detected_and_blocked(), CanaryLeakLlmProvider, CapturingTextLlmProvider, count_lines(), extract_tool_id_field() (+28 more)

### Community 13 - "History Compiler"
Cohesion: 0.05
Nodes (52): build_events_from_turn_specs(), build_file_read_dedup_state(), build_full_file_read_path_map(), build_snapshot_state(), capabilities(), compacted_view_preserves_old_errors_and_respects_budget(), compaction_triggers_at_threshold_and_keeps_full_log(), compile_records() (+44 more)

### Community 14 - "Skill Injection Stage"
Cohesion: 0.06
Nodes (65): alphabetical_name_cmp(), bootstrap_global_skills(), capabilities(), cli_export_import_round_trips_skill_body(), compare_ranked_skills(), compute_budget_uses_context_window_percentage_or_default_floor(), current_workspace_id(), different_queries_keep_manifest_identical_when_selected_set_does_not_change() (+57 more)

### Community 15 - "Daemon Service"
Cohesion: 0.05
Nodes (47): daemon_create_session_uses_explicit_client_scope(), daemon_health_endpoint_responds_when_cloud_enabled(), daemon_info(), daemon_lists_session_previews(), daemon_log_path(), daemon_logs(), daemon_pid_path(), daemon_ping_create_and_shutdown_roundtrip() (+39 more)

### Community 16 - "Anthropic Provider"
Cohesion: 0.06
Nodes (54): annotate_cache_control(), annotate_message_cache_control(), anthropic_content_blocks(), anthropic_content_blocks_render_text_and_json_as_text_blocks(), anthropic_message(), anthropic_message_wraps_assistant_tool_calls_as_tool_use_blocks(), anthropic_message_wraps_tool_results_with_tool_use_id(), anthropic_output_config() (+46 more)

### Community 17 - "Eval Engine & Plan"
Cohesion: 0.05
Nodes (47): CollectedExecution, collector_tracks_tool_steps_and_metrics(), estimate_cost(), TrajectoryCollector, truncate(), build_error_result(), cleanup_workspace(), dry_run_marks_results_skipped() (+39 more)

### Community 18 - "Local Chat Runtime"
Cohesion: 0.04
Nodes (18): expand_local_path(), relay_runtime_events(), relay_runtime_events_emits_notice_after_lag(), relay_session_runtime_events(), relay_session_runtime_events_emits_gap_marker_after_lag(), SessionPreview, SessionRuntimeEvent, workspace_id_for_root() (+10 more)

### Community 19 - "Gemini Provider"
Cohesion: 0.06
Nodes (52): build_cache_create_body(), build_contents_from_messages(), build_explicit_cache_plan(), build_generation_config(), build_request_body(), build_request_body_from_parts(), build_request_parts(), build_tools() (+44 more)

### Community 20 - "Fast Ingestion Path"
Cohesion: 0.05
Nodes (57): active_uids_for_pattern(), begin_scoped(), build_intent(), cohere_api_key(), deterministic_vector(), execute_forget_tool(), execute_memory_tool(), execute_remember_tool() (+49 more)

### Community 21 - "File Read & Write Tools"
Cohesion: 0.05
Nodes (63): container_path_validation_accepts_workspace_absolute_paths(), container_path_validation_rejects_absolute_paths_outside_workspace(), container_path_validation_rejects_traversal(), docker_file_read(), docker_file_search(), docker_file_write(), docker_find_args(), docker_read_args() (+55 more)

### Community 22 - "Session Replay Snapshots"
Cohesion: 0.05
Nodes (17): approval_decision_size(), approval_prompt_size(), approx_event_bytes(), counted_store_is_noop_outside_scope(), counted_store_records_get_events_within_scope(), CountedSessionStore, display_duration_ms(), event_payload_size() (+9 more)

### Community 23 - "Orchestrator Test Harness"
Cohesion: 0.05
Nodes (45): approval_allow_once_round_trip_through_restate(), configured_env(), live_model(), object_url(), register_deployment(), spawn_orchestrator(), wait_for_approval_request(), wait_for_brain_response_count() (+37 more)

### Community 24 - "Sub-Agent Dispatch"
Cohesion: 0.06
Nodes (23): build_completion_request(), build_result_uses_terminal_state(), configured_model_capabilities(), DispatchSubAgentInput, filtered_tool_schemas(), follow_up_queues_message(), initial_task(), initial_task_seeds_state() (+15 more)

### Community 25 - "Query Rewrite Pipeline"
Cohesion: 0.07
Nodes (44): allowed_terms(), approximate_query_tokens(), available_skill_lines(), available_tool_names(), build_rewriter_prompt(), capabilities(), circuit_breaker_resets_after_cooldown(), circuit_breaker_trips_after_failures() (+36 more)

### Community 26 - "Checkpoint Compaction"
Cohesion: 0.05
Nodes (38): calculate_cost_cents(), CheckpointState, compaction_request(), event_summary_line(), latest_checkpoint_state(), maybe_compact(), maybe_compact_events(), non_checkpoint_events() (+30 more)

### Community 27 - "Tool Types & Policy"
Cohesion: 0.04
Nodes (27): default_budget_for_tool(), execute_tool_policy(), RegisteredTool, ToolExecution, ToolRegistry, capabilities(), IdempotencyClass, tool_name() (+19 more)

### Community 28 - "Memory Store Tests"
Cohesion: 0.05
Nodes (33): Consolidate, ConsolidateImpl, ConsolidateReport, ConsolidateRequest, object_url(), register_deployment(), spawn_orchestrator(), workflow_url() (+25 more)

### Community 29 - "Contradiction Detection"
Cohesion: 0.05
Nodes (39): cohere_rerank_v4_fast_prioritizes_contradiction_candidate(), cohere_rerank_v4_fast_prioritizes_relevant_retrieval_candidate(), live_cohere_key(), live_cohere_requested(), build_judge_prompt(), candidate(), candidate_text(), CohereReranker (+31 more)

### Community 30 - "Hybrid Retriever"
Cohesion: 0.04
Nodes (36): apply_layer_bias(), build_hits(), EmptyGraph, EmptyVector, hit(), HybridRetriever, layer_bias_prefers_user_over_workspace_for_matching_scores(), leg_or_empty() (+28 more)

### Community 31 - "Session Store Service"
Cohesion: 0.05
Nodes (24): append_event_increments_sequence(), AppendEventRequest, cleanup(), CompleteSegmentRequest, CreateSegmentRequest, get_events_respects_range(), GetEventsRequest, GetSegmentBaselineRequest (+16 more)

### Community 32 - "OpenAI Responses Provider"
Cohesion: 0.06
Nodes (33): canonical_model_id(), capabilities_for_model(), gpt_5_4_family_reports_expected_capabilities(), native_web_search_tools(), OpenAIProvider, build_function_tool(), build_responses_request(), build_responses_request_omits_temperature_for_reasoning_models() (+25 more)

### Community 33 - "LLM Gateway Service"
Cohesion: 0.06
Nodes (34): build_anthropic_provider(), build_google_provider(), build_openai_provider(), CompletionRequest, CompletionRequestExt, CompletionStreamHandle, compute_cost_cents(), configured_env() (+26 more)

### Community 34 - "Cache Optimizer"
Cohesion: 0.08
Nodes (34): cache_eviction_at_capacity_does_not_crash(), cache_hit_reuses_successful_workspace_retrieval(), cache_invalidation_on_write_version_bump_misses(), cache_optimizer_plans_tool_static_and_conversation_breakpoints(), cache_optimizer_skips_conversation_breakpoint_for_short_sessions(), CachedEntry, CachedHybridRetriever, CachedHybridRetrieverConfig (+26 more)

### Community 35 - "Memory Scope Tool"
Cohesion: 0.05
Nodes (16): extract_search_keywords(), extract_search_query(), extract_search_query_from_messages(), fast_memory_policy(), graph_hit_excerpt(), GraphMemoryRetriever, keyword_extraction_filters_stopwords_and_duplicates(), MemoryForgetTool (+8 more)

### Community 36 - "Cache Observability"
Cohesion: 0.06
Nodes (31): add_session_trace_link(), apply_session_trace(), CacheReport, fingerprint_json(), full_request_fingerprint(), generate_trace_tags(), normalize_environment(), sanitize_langfuse_id() (+23 more)

### Community 37 - "Daytona Sandbox Provider"
Cohesion: 0.08
Nodes (25): build_url(), classify_error(), DaytonaHandProvider, default_headers(), derive_toolbox_url(), expect_success(), expect_success_json(), extract_workspace_id() (+17 more)

### Community 38 - "Turn Streaming & Approvals"
Cohesion: 0.05
Nodes (30): build_turn_context(), BuildTurnContextOptions, persist_context_snapshot(), drain_signal_queue(), handle_stream_signal(), run_streamed_turn_with_tools_mode(), append_tool_call_event(), emit_tool_output_warning() (+22 more)

### Community 39 - "Fact Extraction & SSE"
Cohesion: 0.06
Nodes (39): ApiState, build_api_router(), health_endpoint_returns_ok(), runtime_event_stream(), session_stream(), session_stream_emits_gap_event_when_runtime_subscriber_lags(), session_stream_returns_not_found_when_runtime_is_unavailable(), session_stream_returns_sse_content_type() (+31 more)

### Community 40 - "Markdown Stream Healing"
Cohesion: 0.08
Nodes (35): byte_is_word_char(), content_is_empty_or_only_markers(), count_double_asterisks_outside_code(), count_double_marker_outside_code(), count_double_underscores_outside_code(), count_single_asterisks(), count_single_backticks(), count_single_marker() (+27 more)

### Community 41 - "Query Planner & NER"
Cohesion: 0.07
Nodes (31): dedupe_spans(), extract_code_like_spans(), extract_noun_phrases(), extract_quoted_spans(), extract_relation_targets(), flush_noun_group(), is_boundary(), is_stopword() (+23 more)

### Community 42 - "Memory Detail Panel"
Cohesion: 0.07
Nodes (22): aggregate_brain_usage(), collect_turn_costs(), count_event_type(), count_pending_approvals(), count_turns(), DetailPanel, DetailTab, estimated_context_window() (+14 more)

### Community 43 - "Neon Branch Manager"
Cohesion: 0.09
Nodes (25): checkpoint_branch_names_follow_moa_prefix(), checkpoint_info_from_branch(), checkpoint_label_from_name(), cleanup_expired_deletes_only_old_moa_branches(), create_checkpoint_refuses_to_exceed_capacity(), create_checkpoint_sends_expected_request_and_returns_handle(), discard_checkpoint_calls_delete_endpoint(), format_checkpoint_branch_name() (+17 more)

### Community 44 - "Tool Result Store"
Cohesion: 0.06
Nodes (16): collect_context(), load_tool_result_text(), MockSessionStore, parse_tool_id(), render_search_summary(), search_tool_result(), SearchContextLine, SearchMatch (+8 more)

### Community 45 - "Tool Executor Service"
Cohesion: 0.08
Nodes (31): append_tool_call_event(), append_tool_error_event(), append_tool_result_event(), build_tool_run_plan(), CountingTool, has_prior_non_idempotent_result(), has_prior_tool_call_event(), keyed_tool_requires_idempotency_key() (+23 more)

### Community 46 - "E2B Sandbox Provider"
Cohesion: 0.12
Nodes (17): build_url(), classify_error(), ConnectedSandbox, decode_stream_chunk(), default_headers(), E2BHandProvider, encode_connect_request(), encode_test_envelopes() (+9 more)

### Community 47 - "Gateway Message Renderer"
Cohesion: 0.09
Nodes (23): append_piece(), discord_renderer_attaches_buttons_to_last_chunk_only(), discord_renderer_uses_embed_limit_for_long_text(), DiscordRenderChunk, DiscordRenderer, render_approval_request(), render_diff(), render_tool_card() (+15 more)

### Community 48 - "Broadcast Lag Handling"
Cohesion: 0.07
Nodes (16): record_broadcast_lag(), recv_with_lag_handling(), RecvResult, BroadcastChannel, ClaimCheck, event_stream_abort_policy_surfaces_error(), event_stream_emits_gap_marker_when_lagged(), EventFilter (+8 more)

### Community 49 - "MCP Client Discovery"
Cohesion: 0.11
Nodes (19): classify_error(), flatten_call_result(), flatten_tool_result_aggregates_text_items(), header_map_from_pairs(), http_client_sends_headers_and_parses_jsonrpc(), MCPClient, McpDiscoveredTool, McpTransport (+11 more)

### Community 50 - "Completion Request Types"
Cohesion: 0.07
Nodes (14): CacheBreakpoint, CacheBreakpointTarget, CacheTtl, completion_stream_abort_stops_completion_task(), CompletionContent, CompletionRequest, CompletionResponse, CompletionStream (+6 more)

### Community 51 - "Working Context"
Cohesion: 0.09
Nodes (10): context_message_assistant_tool_call_preserves_invocation(), context_message_tool_result_preserves_text_and_blocks(), context_message_tool_still_defaults_to_text_only(), ContextMessage, estimate_text_tokens(), ExcludedItem, MessageRole, ProcessorOutput (+2 more)

### Community 52 - "Approval Request Types"
Cohesion: 0.08
Nodes (27): append_session_event(), approval_buttons(), approval_outcome_label(), approval_request(), approval_wait_timeout(), approval_wait_timeout_from_env(), ApprovalCallbackAction, ApprovalDecision (+19 more)

### Community 53 - "Slow Ingestion Path"
Cohesion: 0.11
Nodes (31): apply_decisions(), apply_one_decision(), ApplyOutcome, classifier_from_env(), ClassifierBackend, classify_fact(), classify_facts(), decision_fact() (+23 more)

### Community 54 - "Discord Adapter"
Cohesion: 0.12
Nodes (18): approval_callback_maps_to_control_message(), attachments_from_message(), context_from_component(), discord_button(), discord_create_message(), discord_create_message_includes_buttons_for_last_chunk(), discord_edit_message(), discord_embed() (+10 more)

### Community 55 - "Provider Selection & Routing"
Cohesion: 0.11
Nodes (23): build_provider_from_config(), build_provider_from_selection(), default_rewriter_model(), explicit_provider_prefix_overrides_inference(), infer_provider_name(), infers_anthropic_for_claude_models(), infers_google_for_gemini_models(), infers_openai_for_gpt_models() (+15 more)

### Community 56 - "Tool Usage Stats"
Cohesion: 0.11
Nodes (29): annotate_schema(), annotation_warns_on_low_success(), apply_tool_rankings(), cache_stability_preserves_identical_ranked_output(), collect_session_tool_observations(), compare_f64_asc(), compare_f64_desc(), compare_failure_last() (+21 more)

### Community 57 - "Desktop App Shell"
Cohesion: 0.08
Nodes (9): MoaApp, compact_is_tighter_than_comfortable(), current(), Density, Spacing, build_default_icon(), install(), TrayHandle (+1 more)

### Community 58 - "Slack Adapter"
Cohesion: 0.13
Nodes (19): handle_interaction_event(), handle_push_event(), inbound_from_app_mention(), inbound_from_interaction_event(), inbound_from_message_event(), inbound_from_push_event(), interaction_origin(), parses_approval_callback_into_control_message() (+11 more)

### Community 59 - "Turn Latency Counters"
Cohesion: 0.11
Nodes (14): current_turn_root_span(), display_duration_ms(), record_turn_compaction(), record_turn_event_persist_duration(), record_turn_llm_call_duration(), record_turn_llm_ttft(), record_turn_pipeline_compile_duration(), record_turn_snapshot_load() (+6 more)

### Community 60 - "Live Ingestion E2E"
Cohesion: 0.14
Nodes (21): complex_ingestion_turn_writes_facts_pii_changelog_and_dedup(), degraded_workspace_skips_sampled_low_pii_turn_without_side_effects(), degraded_workspace_still_ingests_sensitive_turn(), fact_count(), fact_summaries(), ingestion_turn_round_trip_through_restate_is_idempotent(), LiveIngestionHarness, low_pii_degraded_skip_turn() (+13 more)

### Community 61 - "Tool Router Policy"
Cohesion: 0.09
Nodes (16): approval_diffs_for(), approval_fields_for(), approval_pattern_chained_inner_uses_first_subcommand(), approval_pattern_for(), approval_pattern_malformed_wrapper_falls_back_to_full_input(), approval_pattern_nested_shell_not_recursed(), approval_pattern_simple_command(), approval_pattern_single_token() (+8 more)

### Community 62 - "LLM Span Instrumentation"
Cohesion: 0.12
Nodes (17): calculate_cost(), calculate_cost_with_cached(), cost_calculation_correct(), has_meaningful_output(), llm_span_name(), LLMSpanAttributes, LLMSpanRecorder, metadata_f64() (+9 more)

### Community 63 - "Intent Manager Service"
Cohesion: 0.09
Nodes (13): AdoptCatalogIntentRequest, average_embeddings(), centroid_embedding(), CreateManualIntentRequest, GetLearningLogRequest, IntentIdRequest, IntentManager, IntentManagerImpl (+5 more)

### Community 64 - "Live Cache Audit Tests"
Cohesion: 0.13
Nodes (22): AuditedProvider, available_live_cache_provider_configs(), CacheTurnAudit, CacheTurnPlan, create_session(), full_request_payload(), is_query_rewrite_request(), is_repo_root() (+14 more)

### Community 65 - "MCP Credential Proxy"
Cohesion: 0.12
Nodes (11): credential_from_env(), default_scope_for(), env_var(), environment_vault_loads_from_env_backed_server_config(), EnvironmentCredentialVault, headers_from_credential(), MCPCredentialProxy, McpSessionToken (+3 more)

### Community 66 - "Command Palette & Keybindings"
Cohesion: 0.12
Nodes (9): CommandEntry, CommandPalette, default_commands(), fuzzy_score(), initial_ordering(), PaletteDismissed, PaletteHistory, rewards_consecutive() (+1 more)

### Community 67 - "Live Observability Tests"
Cohesion: 0.11
Nodes (13): FieldRecorder, global_trace_recorder(), live_observability_audit_tracks_cache_replay_and_latency(), live_orchestrator(), queue_message(), RecordedEvent, RecordedFields, RecordedSpan (+5 more)

### Community 68 - "Session DB Codecs"
Cohesion: 0.1
Nodes (16): approval_rule_from_row(), catalog_intent_from_row(), intent_source_from_db(), intent_status_from_db(), parse_resolution_signal(), parse_vector_text(), pending_signal_from_row(), pending_signal_type_from_db() (+8 more)

### Community 69 - "Local Tool Tests"
Cohesion: 0.13
Nodes (25): approval_prompt_str_replace_diff_is_surgical(), approval_prompt_uses_remembered_workspace_root_for_commands(), bash_captures_stdout_and_stderr(), bash_error_output_is_not_truncated(), bash_respects_timeout(), bash_success_output_is_truncated_to_router_budget(), docker_bash_hard_cancel_stops_container_exec(), docker_file_tools_roundtrip_inside_container_workspace() (+17 more)

### Community 70 - "Telegram Adapter"
Cohesion: 0.16
Nodes (13): channel_from_chat_and_reply(), handle_callback_query(), handle_message(), inbound_from_callback_query(), inbound_from_message(), inline_keyboard(), parse_message_id(), parses_approval_callback_into_control_message() (+5 more)

### Community 71 - "Graph Write Protocol"
Cohesion: 0.16
Nodes (25): actor_uuid(), age_table(), close_node_index(), create_changelog(), create_edge(), create_node(), create_node_in_conn(), delete_age_node() (+17 more)

### Community 72 - "Embedding Provider"
Cohesion: 0.12
Nodes (13): add_feature(), build_embedding_provider_from_config(), char_trigrams(), EmbeddingProvider, mock_embedding_is_deterministic(), MockEmbedding, normalize(), OpenAIEmbedding (+5 more)

### Community 73 - "Session Blob Store"
Cohesion: 0.18
Nodes (12): claim_check_from_value(), collect_blob_refs(), collect_large_strings(), decode_event_from_storage(), encode_event_for_storage(), expand_local_path(), file_blob_store_deletes_session_directory(), file_blob_store_is_content_addressed() (+4 more)

### Community 74 - "Memory Viewer Panel"
Cohesion: 0.09
Nodes (6): empty_state(), empty_state_any(), MemoryList, MemoryPageSelected, MemoryViewer, SkillList

### Community 75 - "Scripted Test Provider"
Cohesion: 0.14
Nodes (3): ScriptedBlock, ScriptedProvider, ScriptedResponse

### Community 76 - "Runtime Context Stage"
Cohesion: 0.19
Nodes (12): build_runtime_reminder(), capabilities(), Clock, detect_git_branch(), FixedClock, runtime_context_changes_when_clock_advances(), runtime_context_insertion_index(), runtime_context_inserts_before_trailing_user_turn() (+4 more)

### Community 77 - "Session Analytics"
Cohesion: 0.15
Nodes (15): analytics_window_start(), CacheDailyMetric, get_session_summary(), get_workspace_stats(), list_cache_daily_metrics(), list_session_turn_metrics(), list_tool_call_summaries(), normalized_days() (+7 more)

### Community 78 - "Chat Message Bubbles"
Cohesion: 0.17
Nodes (18): agent_bubble(), approval_card(), decision_button(), detail_card(), error_bubble(), render_message(), system_bubble(), thinking_bubble() (+10 more)

### Community 79 - "OpenAI Privacy Filter"
Cohesion: 0.14
Nodes (6): normalize_base_url(), OpenAiPrivacyFilterClassifier, PrivacyFilterThresholds, resolve_class(), ServiceResponse, ServiceSpan

### Community 80 - "Model Capabilities"
Cohesion: 0.11
Nodes (6): Credential, ModelCapabilities, ModelCapabilitiesBuilder, ProviderNativeTool, TokenPricing, ToolCallFormat

### Community 81 - "Orchestrator Contract Harness"
Cohesion: 0.22
Nodes (14): assert_blank_session_waits_for_first_message(), assert_processes_multiple_queued_messages_fifo(), assert_processes_two_sessions_independently(), assert_queued_message_waiting_for_approval_runs_after_allowed_turn(), assert_soft_cancel_waiting_for_approval_cancels_cleanly(), OrchestratorContractHarness, start_request(), start_session_with_timeout() (+6 more)

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

### Community 86 - "Desktop Service Bridge"
Cohesion: 0.12
Nodes (3): ServiceBridge, ServiceBridgeHandle, ServiceStatus

### Community 87 - "Settings Panel"
Cohesion: 0.19
Nodes (3): SettingsDismissed, SettingsPage, SettingsSection

### Community 88 - "Platform & Inbound Messages"
Cohesion: 0.14
Nodes (12): ActionButton, Attachment, ButtonStyle, ChannelRef, DiffHunk, InboundMessage, MessageContent, OutboundMessage (+4 more)

### Community 89 - "Encrypted Secret Vault"
Cohesion: 0.3
Nodes (4): decrypt_bytes(), encrypt_bytes(), file_vault_encrypts_and_decrypts_roundtrip(), FileVault

### Community 90 - "Intent Discovery Workflow"
Cohesion: 0.2
Nodes (13): average_embeddings(), average_embeddings_skips_mismatched_vectors(), build_discovery_prompt(), DiscoveredCluster, DiscoverySegment, extract_json_array(), IntentDiscovery, IntentDiscoveryImpl (+5 more)

### Community 91 - "Session Search Tool"
Cohesion: 0.16
Nodes (6): event_snippet(), render_results(), SessionSearchEventType, SessionSearchInput, SessionSearchTool, truncate()

### Community 92 - "Graph Node Index"
Cohesion: 0.17
Nodes (6): decode_node_label(), decode_pii_class(), NodeIndexRow, NodeLabel, NodeWriteIntent, PiiClass

### Community 93 - "Cohere Embedder"
Cohesion: 0.16
Nodes (5): CohereEmbeddings, CohereEmbedRequest, CohereEmbedResponse, CohereV4Embedder, Embedder

### Community 94 - "Chat Panel"
Cohesion: 0.27
Nodes (2): ChatPanel, PendingToast

### Community 95 - "Neon Branch Manager Tests"
Cohesion: 0.32
Nodes (12): live_neon_config(), live_neon_config_with_limit(), live_session_store(), neon_branch_manager_create_list_get_rollback_and_discard_checkpoint(), neon_checkpoint_branch_connection_is_copy_on_write(), neon_checkpoint_capacity_limit_rejects_extra_branch(), neon_checkpoint_cleanup_without_expired_branches_returns_zero(), neon_live_lock() (+4 more)

### Community 96 - "Prompt Injection Defense"
Cohesion: 0.26
Nodes (12): canary_detection_works(), check_canary(), classifier_flags_known_attack_patterns(), classify_input(), contains_canary_tokens(), inject_canary(), InputClassification, InputInspection (+4 more)

### Community 97 - "Query Planner Tests"
Cohesion: 0.15
Nodes (1): SeedGraph

### Community 98 - "Window State Persistence"
Cohesion: 0.28
Nodes (4): round_trips_through_disk(), validate_bounds_clamps_offscreen_positions(), validate_bounds_clamps_tiny_panels(), WindowState

### Community 99 - "Ingest Context Runtime"
Cohesion: 0.2
Nodes (5): IngestCtx, IngestRuntime, install_runtime(), install_runtime_with_pool(), OrchestratorCtx

### Community 100 - "Continuation Signal"
Cohesion: 0.23
Nodes (6): ContinuationInput, is_acknowledgment(), is_correction(), lexical_cosine_similarity(), score(), token_counts()

### Community 101 - "AGE Graph Store"
Cohesion: 0.18
Nodes (1): AgeGraphStore

### Community 102 - "Eval Terminal Reporter"
Cohesion: 0.3
Nodes (5): format_scores(), render_includes_case_names_and_summary(), render_verbose_case(), result_index(), TerminalReporter

### Community 103 - "Session Row Badges"
Cohesion: 0.24
Nodes (8): risk_badge(), risk_color(), risk_label(), status_badge(), status_color(), status_label(), SessionRow, truncate_single_line()

### Community 104 - "E2E Retest Plan"
Cohesion: 0.21
Nodes (12): Applied Workspace, Approval Rules Store, moa exec CLI, Session Lifecycle (running/waiting_approval/cancelled), ~/.moa/sessions.db, str_replace Surgical Edit Path, Retest Objective (Narrow Surgical Workflow), Pass Criteria (+4 more)

### Community 105 - "Turn Loop Detector"
Cohesion: 0.33
Nodes (6): loop_detector_disabled_at_zero_threshold(), loop_detector_does_not_trigger_on_varied_calls(), loop_detector_resets(), loop_detector_sliding_window(), loop_detector_triggers_after_threshold(), LoopDetector

### Community 106 - "Intent Classifier"
Cohesion: 0.33
Nodes (6): best_within_threshold(), classification_text(), embedding_below_threshold_returns_none(), exact_match_returns_high_confidence(), intent(), IntentClassifier

### Community 107 - "Skill Renderer"
Cohesion: 0.24
Nodes (5): load_addenda(), render(), set_app_role(), SkillAddendum, SkillRenderContext

### Community 108 - "Session List"
Cohesion: 0.4
Nodes (2): SessionList, SessionSelected

### Community 109 - "Provider Model Catalog"
Cohesion: 0.29
Nodes (7): by_provider(), by_provider_partitions_correctly(), claude_opus_has_million_token_context(), context_window(), find(), gpt_5_4_has_extended_context(), ProviderModel

### Community 110 - "Tool Approval Store"
Cohesion: 0.22
Nodes (6): PreparedToolApproval, PrepareToolApprovalRequest, StoreApprovalRuleRequest, to_handler_error(), WorkspaceStore, WorkspaceStoreImpl

### Community 111 - "Instruction Stage"
Cohesion: 0.38
Nodes (4): combine_workspace_instructions(), instruction_processor_appends_config_backed_sections(), instruction_processor_combines_config_and_discovered_workspace_instructions(), InstructionProcessor

### Community 112 - "Session Sidebar"
Cohesion: 0.24
Nodes (1): SessionSidebar

### Community 113 - "Identity Stage"
Cohesion: 0.44
Nodes (3): identity_processor_appends_system_prompt(), identity_prompt_includes_coding_guardrails(), IdentityProcessor

### Community 114 - "Session VO Tests"
Cohesion: 0.5
Nodes (6): session_vo_destroy_clears_state(), session_vo_post_message_queues_in_state(), session_vo_post_message_updates_status_to_running_then_idle_parks_paused(), session_vo_post_message_without_meta_errors(), test_message(), test_meta()

### Community 115 - "Health Service"
Cohesion: 0.32
Nodes (4): Health, HealthImpl, version_info_reports_expected_versions(), VersionInfo

### Community 116 - "Self Assessment Signal"
Cohesion: 0.29
Nodes (2): contains_any(), score()

### Community 117 - "Structural Signal"
Cohesion: 0.32
Nodes (5): baseline(), cold_start_returns_none(), is_high_outlier(), score(), SegmentMetrics

### Community 118 - "WCAG Contrast Helpers"
Cohesion: 0.39
Nodes (6): black_on_white_is_max_ratio(), classify(), contrast_ratio(), equal_colors_are_ratio_one(), relative_luminance(), WcagPass

### Community 119 - "Query Rewrite Live Tests"
Cohesion: 0.38
Nodes (2): CapturingProvider, live_query_rewriter_resolves_coreference_without_new_entities()

### Community 120 - "Lexical Seed Store"
Cohesion: 0.43
Nodes (2): LexicalStore, lookup_seed_rows()

### Community 121 - "Trajectory Match Evaluator"
Cohesion: 0.43
Nodes (4): exact_match_scores_one(), lcs_len(), partial_match_scores_below_one(), TrajectoryMatchEvaluator

### Community 122 - "Output Match Evaluator"
Cohesion: 0.43
Nodes (4): contains_rules_pass_when_all_terms_match(), evaluate_output(), missing_contains_term_reduces_score(), OutputMatchEvaluator

### Community 123 - "Live E2E Fixtures"
Cohesion: 0.43
Nodes (7): LIVE-E2E-ANTHROPIC Fixture, LIVE-E2E-GOOGLE Fixture, Live End-to-End Test Marker, LIVE-E2E-OPENAI Fixture, Anthropic Provider, Google Provider, OpenAI Provider

### Community 124 - "PII Classifier Smoke"
Cohesion: 0.4
Nodes (2): classify_smoke_maps_ssn_to_phi_and_clean_text_to_none(), spawn_test_service()

### Community 125 - "Scope Hierarchy Tests"
Cohesion: 0.33
Nodes (0): 

### Community 126 - "Legacy Memory Audit"
Cohesion: 0.6
Nodes (5): audit(), contains_legacy_crate_name(), legacy_name_positions(), main(), walk_files()

### Community 127 - "Threshold Evaluator"
Cohesion: 0.53
Nodes (3): cost_over_budget_fails_boolean_score(), limit_score(), ThresholdEvaluator

### Community 128 - "Desktop Notifications"
Cohesion: 0.4
Nodes (2): error(), sticky_error()

### Community 129 - "Providers Settings Tab"
Cohesion: 0.47
Nodes (4): collect_providers(), provider_control(), ProviderInfo, render_providers_tab()

### Community 130 - "Shell Chain Splitter"
Cohesion: 0.5
Nodes (2): push_sub_command(), split_shell_chain()

### Community 131 - "Scoped Transaction Lifecycle"
Cohesion: 0.5
Nodes (1): ScopedConn<'p>

### Community 132 - "Unified Diff"
Cohesion: 0.6
Nodes (3): compute_unified_diff(), small_edit_diff_is_substantially_smaller_than_full_file(), unified_diff_contains_standard_headers_and_hunks()

### Community 133 - "Gemini Live Tests"
Cohesion: 0.6
Nodes (3): gemini_live_completion_returns_expected_answer(), gemini_live_model(), gemini_live_web_search_returns_current_information()

### Community 134 - "Anthropic Provider Tests"
Cohesion: 0.4
Nodes (0): 

### Community 135 - "Desktop Status Bar"
Cohesion: 0.5
Nodes (1): MoaStatusBar

### Community 136 - "Event Color Mapping"
Cohesion: 0.6
Nodes (2): event_color(), EventColor

### Community 137 - "DB Error Mapping"
Cohesion: 0.5
Nodes (1): ScopedConn

### Community 138 - "Cohere Live Test"
Cohesion: 0.83
Nodes (3): cohere_embed_v4_returns_1024_dimensional_float_embeddings(), live_cohere_key(), live_cohere_requested()

### Community 139 - "Cost Budget Enforcement"
Cohesion: 0.67
Nodes (2): enforce_workspace_budget(), format_budget_exhausted_message()

### Community 140 - "Tool Success Evaluator"
Cohesion: 0.5
Nodes (1): ToolSuccessEvaluator

### Community 141 - "Settings Integration Tests"
Cohesion: 0.83
Nodes (3): removing_from_auto_approve_persists(), scratch_dir(), settings_mutations_round_trip_through_config_file()

### Community 142 - "Desktop Title Bar"
Cohesion: 0.67
Nodes (1): MoaTitleBar

### Community 143 - "General Settings Tab"
Cohesion: 0.67
Nodes (2): render_general_tab(), render_model_card()

### Community 144 - "Error Banner Component"
Cohesion: 0.67
Nodes (2): error_banner(), error_banner_any()

### Community 145 - "PII Live Sidecar Test"
Cohesion: 1.0
Nodes (2): live_service_url(), live_sidecar_classifies_private_and_clean_text()

### Community 146 - "Mock PII Classifier"
Cohesion: 0.67
Nodes (1): MockClassifier

### Community 147 - "OpenAI Live Test"
Cohesion: 0.67
Nodes (0): 

### Community 148 - "Anthropic Live Test"
Cohesion: 0.67
Nodes (0): 

### Community 149 - "Chat Harness Example"
Cohesion: 1.0
Nodes (2): main(), run_prompt()

### Community 150 - "Sidebar Tab Enum"
Cohesion: 0.67
Nodes (1): SidebarTab

### Community 151 - "Permissions Settings Tab"
Cohesion: 1.0
Nodes (2): render_permissions_tab(), tool_list_card()

### Community 152 - "Desktop Service Init"
Cohesion: 0.67
Nodes (1): InitializedServices

### Community 153 - "Ingest Connector"
Cohesion: 1.0
Nodes (1): IngestConnector

### Community 154 - "Model Identifier"
Cohesion: 1.0
Nodes (1): ModelId

### Community 155 - "Tool Call Identifier"
Cohesion: 1.0
Nodes (1): ToolCallId

### Community 156 - "Provider HTTP Client"
Cohesion: 1.0
Nodes (0): 

### Community 157 - "Session Engine Gating"
Cohesion: 1.0
Nodes (0): 

### Community 158 - "Object Context"
Cohesion: 1.0
Nodes (1): ObjectContext<'a>

### Community 159 - "Shared Object Context"
Cohesion: 1.0
Nodes (1): SharedObjectContext<'a>

### Community 160 - "Agent Adapter"
Cohesion: 1.0
Nodes (1): AgentAdapter

### Community 161 - "Docker Hardening Test"
Cohesion: 1.0
Nodes (0): 

### Community 162 - "Eval Live Tests"
Cohesion: 1.0
Nodes (0): 

### Community 163 - "Chat Runtime Trait"
Cohesion: 1.0
Nodes (1): ChatRuntime

### Community 164 - "Keyboard Shortcuts Tab"
Cohesion: 1.0
Nodes (0): 

### Community 165 - "Icon Button"
Cohesion: 1.0
Nodes (0): 

### Community 166 - "Section Card"
Cohesion: 1.0
Nodes (0): 

### Community 167 - "Segmented Control"
Cohesion: 1.0
Nodes (0): 

### Community 168 - "Markdown Style"
Cohesion: 1.0
Nodes (0): 

### Community 169 - "Nav Item"
Cohesion: 1.0
Nodes (0): 

### Community 170 - "Skills Bootstrap Script"
Cohesion: 1.0
Nodes (0): 

### Community 171 - "Core Type Macros"
Cohesion: 1.0
Nodes (0): 

### Community 172 - "Integration Test Entry"
Cohesion: 1.0
Nodes (0): 

### Community 173 - "Restate Registration"
Cohesion: 1.0
Nodes (0): 

## Knowledge Gaps
- **499 isolated node(s):** `IngestError`, `SessionTurn`, `TurnChunk`, `ExtractedFact`, `ClassifiedFact` (+494 more)
  These have ≤1 connection - possible missing edges or undocumented components.
- **Thin community `Ingest Connector`** (2 nodes): `connector.rs`, `IngestConnector`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Model Identifier`** (2 nodes): `ModelId`, `.default()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Tool Call Identifier`** (2 nodes): `ToolCallId`, `.from()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Provider HTTP Client`** (2 nodes): `http.rs`, `build_http_client()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Session Engine Gating`** (2 nodes): `session_engine.rs`, `session_requires_processing()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Object Context`** (2 nodes): `ObjectContext<'a>`, `.get_json()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Shared Object Context`** (2 nodes): `SharedObjectContext<'a>`, `.get_json()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Agent Adapter`** (2 nodes): `adapter.rs`, `AgentAdapter`
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
- **Thin community `Markdown Style`** (2 nodes): `markdown.rs`, `markdown_style()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Nav Item`** (2 nodes): `nav.rs`, `nav_item()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Skills Bootstrap Script`** (2 nodes): `bootstrap_global_skills.rs`, `main()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Core Type Macros`** (1 nodes): `macros.rs`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Integration Test Entry`** (1 nodes): `integration.rs`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Restate Registration`** (1 nodes): `restate_register.rs`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **What connects `IngestError`, `SessionTurn`, `TurnChunk` to the rest of the system?**
  _499 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `File Memory Store` be split into smaller, more focused modules?**
  _Cohesion score 0.02 - nodes in this community are weakly interconnected._
- **Should `Runtime Events & SSE` be split into smaller, more focused modules?**
  _Cohesion score 0.02 - nodes in this community are weakly interconnected._
- **Should `Core Errors & Traits` be split into smaller, more focused modules?**
  _Cohesion score 0.02 - nodes in this community are weakly interconnected._
- **Should `Core Config` be split into smaller, more focused modules?**
  _Cohesion score 0.02 - nodes in this community are weakly interconnected._
- **Should `CLI Entry Point` be split into smaller, more focused modules?**
  _Cohesion score 0.03 - nodes in this community are weakly interconnected._
- **Should `Pgvector Store` be split into smaller, more focused modules?**
  _Cohesion score 0.03 - nodes in this community are weakly interconnected._