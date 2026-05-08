# Graph Report - .  (2026-05-08)

## Corpus Check
- Large corpus: 627 files · ~351,275 words. Semantic extraction will be expensive (many Claude tokens). Consider running on a subfolder, or use --no-semantic to run AST-only.

## Summary
- 6815 nodes · 11061 edges · 219 communities detected
- Extraction: 100% EXTRACTED · 0% INFERRED · 0% AMBIGUOUS · INFERRED: 6 edges (avg confidence: 0.82)
- Token cost: 0 input · 0 output

## God Nodes (most connected - your core abstractions)
1. `ChatRuntime` - 35 edges
2. `CountedSessionStore` - 32 edges
3. `SkillFrontmatter` - 32 edges
4. `PostgresSessionStore` - 29 edges
5. `E2BHandProvider` - 23 edges
6. `LocalHandProvider` - 22 edges
7. `DaytonaHandProvider` - 22 edges
8. `SubAgentTurnAdapter` - 21 edges
9. `SessionStoreImpl` - 21 edges
10. `session()` - 21 edges

## Surprising Connections (you probably didn't know these)
- `Runs the audit shipper loop.` --rationale_for--> `main()`  [EXTRACTED]
  services/audit-shipper/main.py → crates/moa-cli/src/main.rs
- `main()` --calls--> `load_settings()`  [EXTRACTED]
  crates/moa-cli/src/main.rs → services/audit-shipper/main.py
- `main()` --calls--> `ship_once()`  [EXTRACTED]
  crates/moa-cli/src/main.rs → services/audit-shipper/main.py
- `LIVE-E2E-OPENAI Fixture` --semantically_similar_to--> `LIVE-E2E-ANTHROPIC Fixture`  [INFERRED] [semantically similar]
  live/openai.txt → live/anthropic.txt
- `LIVE-E2E-OPENAI Fixture` --semantically_similar_to--> `LIVE-E2E-GOOGLE Fixture`  [INFERRED] [semantically similar]
  live/openai.txt → live/google.txt

## Hyperedges (group relationships)
- **Live E2E Provider Fixture Set** — live_e2e_openai_doc, live_e2e_anthropic_doc, live_e2e_google_doc, live_e2e_marker_concept [INFERRED 0.80]

## Communities

### Community 0 - "Runtime Events & SSE"
Cohesion: 0.01
Nodes (198): build_default_pipeline(), build_default_pipeline_with_tools(), GraphMemoryPipelineOptions, CompiledRequest, TurnUsage, DatabaseConfig, DatabaseNeonConfig, CatalogIntent (+190 more)

### Community 1 - "PII Redaction Core"
Cohesion: 0.01
Nodes (111): ChangelogRecord, validate_scope(), write_and_bump(), acknowledgement(), command_action(), control_action_for_inbound(), GatewayControlAction, IngestCtx (+103 more)

### Community 2 - "Postgres RLS & Audit Tests"
Cohesion: 0.02
Nodes (114): anthropic_offline_429_response_triggers_retry_with_backoff(), anthropic_offline_500_response_triggers_retry_then_surfaces_typed_error(), anthropic_offline_completion_returns_text_for_minimal_request(), anthropic_offline_malformed_json_response_returns_typed_parse_error(), anthropic_offline_streaming_disconnect_mid_response_surfaces_typed_error_with_partial_events(), anthropic_offline_streaming_yields_text_deltas_then_terminal_event(), anthropic_offline_tool_call_response_parses_into_provider_event(), provider() (+106 more)

### Community 3 - "CLI Entry Points"
Cohesion: 0.02
Nodes (110): AdminCommand, format_promotion_report(), handle_admin_command(), promote_workspace_e2e(), PromoteWorkspaceArgs, WorkspacePromotionArgs, BaseModel, CacheCommand (+102 more)

### Community 4 - "CLI Command Tests"
Cohesion: 0.02
Nodes (66): collect_session_tool_observations(), merge_session_observation(), normalize_error_pattern(), record_error_pattern(), SessionToolObservation, top_error_patterns(), truncate_with_ellipsis(), workspace_tool_stats_from_events() (+58 more)

### Community 5 - "Slow Path Ingestion Tests"
Cohesion: 0.02
Nodes (74): approval_button_click_with_unknown_request_id_returns_stale_error_message(), approval_button_click_with_valid_callback_data_emits_decision_signal(), approval_buttons_after_decision_re_render_as_disabled_with_decision_marker(), approval_request_after_orchestrator_timeout_marks_buttons_as_expired(), ApprovalClickOutcome, ApprovalLifecycleState, ApprovalRecord, ApprovalStateTracker (+66 more)

### Community 6 - "Skill Document Format"
Cohesion: 0.03
Nodes (56): append_skill_learning(), build_distillation_prompt(), count_tool_calls(), distill_skill_with_learning(), DistillationOutcome, DistillationSkipReason, extract_task_summary(), find_similar_skill() (+48 more)

### Community 7 - "Brain Steps 72-77 Integration"
Cohesion: 0.03
Nodes (88): changelog_rejects_updates_for_app_role(), changelog_write_bumps_workspace_version_and_respects_read_rls(), record(), set_app_role(), set_auditor_role(), set_workspace_gucs(), AgTypeParam, Cypher (+80 more)

### Community 8 - "Hybrid Retriever"
Cohesion: 0.03
Nodes (59): cache_eviction_at_capacity_does_not_crash(), cache_hit_reuses_successful_workspace_retrieval(), cache_invalidation_on_write_version_bump_misses(), cache_optimizer_plans_tool_static_and_conversation_breakpoints(), cache_optimizer_skips_conversation_breakpoint_for_short_sessions(), CachedEntry, CachedHybridRetriever, CachedHybridRetrieverConfig (+51 more)

### Community 9 - "Audit Errors & Merkle Chain"
Cohesion: 0.02
Nodes (57): canonical_json_bytes(), canonical_payload_hash(), chain_detects_tampered_payload(), HashChain, next_chain_hash(), cost_cents_for_anthropic_sonnet_matches_pricing_table_v1_for_known_token_counts(), cost_cents_for_gemini_pro_matches_pricing_table_v1_for_known_token_counts(), cost_cents_for_openai_gpt41_matches_pricing_table_v1_for_known_token_counts() (+49 more)

### Community 10 - "Orchestrator Test Harness"
Cohesion: 0.04
Nodes (73): approval_allow_once_round_trip_through_restate(), configured_env(), live_model(), object_url(), register_deployment(), spawn_orchestrator(), wait_for_approval_request(), wait_for_brain_response_count() (+65 more)

### Community 11 - "Tool Types & Policy"
Cohesion: 0.03
Nodes (41): default_budget_for_tool(), execute_tool_policy(), RegisteredTool, ToolExecution, ToolRegistry, anthropic_content_blocks(), anthropic_message(), build_function_tool() (+33 more)

### Community 12 - "Long Conversation Smoke Tests"
Cohesion: 0.03
Nodes (59): fixture_path(), transcript_jsonl_round_trips_through_read_and_write(), budgets_evaluate_reports_each_violation_with_metric_name_and_actual_value(), long_test_case_dispatches_to_run_scenario_with_provider(), recorded_provider_handles_compaction_requests_without_advancing_transcript_cursor(), recorded_provider_replays_two_turn_transcript_byte_for_byte(), recorded_provider_returns_typed_error_on_transcript_exhaustion(), recorded_provider_with_strict_matching_rejects_user_message_drift() (+51 more)

### Community 13 - "Brain Turn Tests"
Cohesion: 0.04
Nodes (36): always_allow_rule_persists_and_skips_next_approval(), ArtifactRetrievalLlmProvider, ArtifactStderrLlmProvider, canary_leaks_in_tool_input_are_detected_and_blocked(), CanaryLeakLlmProvider, CapturingTextLlmProvider, count_lines(), extract_tool_id_field() (+28 more)

### Community 14 - "File Read & Write Tools"
Cohesion: 0.04
Nodes (77): container_path_validation_accepts_workspace_absolute_paths(), container_path_validation_rejects_absolute_paths_outside_workspace(), container_path_validation_rejects_traversal(), docker_file_read(), docker_file_search(), docker_file_write(), docker_find_args(), docker_read_args() (+69 more)

### Community 15 - "Core Event Type"
Cohesion: 0.03
Nodes (68): DaemonCommand, DaemonInfo, DaemonReply, DaemonSessionPreview, DaemonStreamEvent, CacheReport, fingerprint_json(), full_request_fingerprint() (+60 more)

### Community 16 - "Embedding Provider"
Cohesion: 0.03
Nodes (37): ClientConfig, decode_response(), decode_stream_chunk(), expect_success(), expect_success_json(), extract_exit_code(), http_error(), OrchestratorClient (+29 more)

### Community 17 - "Memory Scope Tool"
Cohesion: 0.03
Nodes (35): CohereEmbedderConfig, contribution(), derive_ingest_source_name(), duration_ms_u32(), execute_memory_tool(), extract_search_keywords(), extract_search_query(), extract_search_query_from_messages() (+27 more)

### Community 18 - "LLM Gateway Service"
Cohesion: 0.05
Nodes (43): build_anthropic_provider(), build_google_provider(), build_openai_provider(), CompletionRequest, CompletionRequestExt, CompletionStreamHandle, compute_cost_cents(), configured_env() (+35 more)

### Community 19 - "Turn Execution Workflow"
Cohesion: 0.06
Nodes (59): PreparedTurnRequest, ResolutionLabel, ResolutionScore, ScoringPhase, SegmentBaseline, SkillResolutionRate, dispatch_sub_agent(), DispatchedSubAgent (+51 more)

### Community 20 - "Fast Ingestion Path"
Cohesion: 0.05
Nodes (55): active_uids_for_pattern(), begin_scoped(), build_intent(), cohere_api_key(), deterministic_vector(), execute_forget_tool(), execute_memory_tool(), execute_remember_tool() (+47 more)

### Community 21 - "Eval Engine & Plan"
Cohesion: 0.05
Nodes (50): build_error_result(), cleanup_workspace(), dry_run_marks_results_skipped(), EngineOptions, EvalEngine, EvalRun, extract_trace_id(), fs_try_exists() (+42 more)

### Community 22 - "Lineage MPSC Sink Writer"
Cohesion: 0.05
Nodes (42): expand_home(), mpsc_sink_drops_when_channel_is_full(), MpscSink, MpscSinkBuilder, MpscSinkConfig, null_sink_never_records_drops(), NullSink, sample_event() (+34 more)

### Community 23 - "Turn & Tool Dispatch"
Cohesion: 0.04
Nodes (28): build_turn_context(), BuildTurnContextOptions, persist_context_snapshot(), approval_requested_event_round_trips_full_prompt(), Event, sample_approval_prompt(), PendingToolApproval, StoredApprovalDecision (+20 more)

### Community 24 - "Session Replay Snapshots"
Cohesion: 0.05
Nodes (16): approval_decision_size(), approval_prompt_size(), approx_event_bytes(), counted_store_records_get_events_within_scope(), CountedSessionStore, display_duration_ms(), event_payload_size(), event_record() (+8 more)

### Community 25 - "Telemetry Init"
Cohesion: 0.04
Nodes (42): from_reader_requires_postgres_url(), from_reader_uses_defaults_and_optional_values(), is_prod_value(), linux_memory_gb(), OrchestratorConfig, parse_otlp_headers(), parse_otlp_protocol(), production_environment() (+34 more)

### Community 26 - "Graph Write & Export"
Cohesion: 0.06
Nodes (59): Args, begin_audited_read(), collect_changelog(), collect_embeddings(), collect_entities(), collect_facts(), collect_nodes(), collect_relationships() (+51 more)

### Community 27 - "Turn Runner Helpers"
Cohesion: 0.05
Nodes (31): add_session_trace_link(), apply_session_trace(), session_turn_span(), synthetic_session_span_context(), append_session_event(), cache_prefix_ratio(), cache_prefix_ratio_includes_tool_tokens(), capabilities() (+23 more)

### Community 28 - "Provider Streaming"
Cohesion: 0.05
Nodes (38): GeminiCachedContent, GeminiCandidate, GeminiContent, GeminiFunctionCall, GeminiGenerateContentResponse, GeminiPart, GeminiUsageMetadata, ResponsesStreamError (+30 more)

### Community 29 - "Intent Discovery Workflow"
Cohesion: 0.05
Nodes (40): canonical_json(), DurableStep, Recorder, step_canonical_json(), average_embeddings(), average_embeddings_skips_mismatched_vectors(), build_discovery_prompt(), DiscoveredCluster (+32 more)

### Community 30 - "Contradiction Detection"
Cohesion: 0.06
Nodes (40): build_judge_prompt(), candidate(), candidate_text(), CohereReranker, CohereRerankHit, CohereRerankRequest, CohereRerankResponse, Conflict (+32 more)

### Community 31 - "Workspace Instructions"
Cohesion: 0.06
Nodes (36): Consolidate, ConsolidateDurableSteps, ConsolidateImpl, ConsolidateReport, ConsolidateRequest, object_url(), register_deployment(), spawn_orchestrator() (+28 more)

### Community 32 - "Cross-Tenant Pentest Suite"
Cohesion: 0.07
Nodes (39): assert_attack(), attack_a_forgotten_guc(), attack_a_impl(), attack_b_cross_tenant_write(), attack_b_impl(), attack_c_cross_tenant_fk_leakage(), attack_c_impl(), attack_d_impl() (+31 more)

### Community 33 - "Eval Budget Checker"
Cohesion: 0.06
Nodes (39): AnalyticsScoreRow, Baselines, BudgetExpectations, CacheScores, check_bool(), check_max_u64(), check_min_f64(), check_min_u64() (+31 more)

### Community 34 - "Eval Fixtures & Pricing"
Cohesion: 0.06
Nodes (36): CollectedExecution, collector_tracks_tool_steps_and_metrics(), estimate_cost(), TrajectoryCollector, truncate(), ConsolidationOutcomes, MemoryScenario, build_score_card() (+28 more)

### Community 35 - "Working Context"
Cohesion: 0.05
Nodes (20): BudgetConfig, CompactionConfig, context_message_assistant_tool_call_preserves_invocation(), context_message_tool_result_preserves_text_and_blocks(), ContextMessage, ContextSnapshotConfig, estimate_text_tokens(), ExcludedItem (+12 more)

### Community 36 - "Concurrent Event Monotonicity Tests"
Cohesion: 0.08
Nodes (48): basis_vector(), configured_test_db(), item(), query(), reembed_in_progress_state_blocks_concurrent_knn_queries_until_complete(), reembed_workspace_with_new_embedder_overwrites_existing_vectors_atomically(), scope(), scoped_conn() (+40 more)

### Community 37 - "Skill Tier1 Metadata"
Cohesion: 0.06
Nodes (34): capabilities(), compiled_snapshot(), compiler_with_recent_turns(), event_record(), file_read_tool_call(), file_read_tool_result(), fixed_time(), MockLlmProvider (+26 more)

### Community 38 - "Golden E2E Fixtures"
Cohesion: 0.07
Nodes (39): assert_top_k_within_window(), compare_top_k_within_window(), dump_traces(), ExpectedRankMismatch, GoldenRankingMismatch, box_error(), box_message(), changelog_count() (+31 more)

### Community 39 - "Query Planner & NER"
Cohesion: 0.07
Nodes (31): dedupe_spans(), extract_code_like_spans(), extract_noun_phrases(), extract_quoted_spans(), extract_relation_targets(), flush_noun_group(), is_boundary(), is_stopword() (+23 more)

### Community 40 - "Session Types"
Cohesion: 0.05
Nodes (30): BufferedUserMessage, CancelMode, CheckpointHandle, CheckpointInfo, DaemonConfig, ObserveLevel, pending_signal_queue_message_round_trip(), PendingSignal (+22 more)

### Community 41 - "History Compilation"
Cohesion: 0.05
Nodes (23): build_events_from_turn_specs(), checkpoint_list_report(), format_checkpoint_age(), full_read_fixture(), HistoryCompiler, incremental_history_replaces_prior_full_file_reads_across_turns(), SnapshotHistory, test_action_strategy() (+15 more)

### Community 42 - "Neon Branch Manager"
Cohesion: 0.09
Nodes (25): checkpoint_branch_names_follow_moa_prefix(), checkpoint_info_from_branch(), checkpoint_label_from_name(), cleanup_expired_deletes_only_old_moa_branches(), create_checkpoint_refuses_to_exceed_capacity(), create_checkpoint_sends_expected_request_and_returns_handle(), discard_checkpoint_calls_delete_endpoint(), format_checkpoint_branch_name() (+17 more)

### Community 43 - "Fact Extraction & Chunking"
Cohesion: 0.08
Nodes (42): chunk_turn(), estimate_tokens(), flush_paragraph(), is_explicit_fact_line(), join_units(), joined_len_with(), overlap_units(), push_chunk() (+34 more)

### Community 44 - "Tool Result Store"
Cohesion: 0.06
Nodes (16): collect_context(), load_tool_result_text(), MockSessionStore, parse_tool_id(), render_search_summary(), search_tool_result(), SearchContextLine, SearchMatch (+8 more)

### Community 45 - "Runtime Context Stage"
Cohesion: 0.08
Nodes (27): assert_stage_contract(), cache_stage_inserts_breakpoints_at_4_segment_boundaries(), delete_memory_rows(), FixedClock, identity_stage_emits_stable_system_message_with_workspace_and_runtime_metadata(), instruction_stage_appends_workspace_instructions_when_present_and_skips_when_absent(), memory_stage_includes_top_k_hits_with_lineage_uids_and_excludes_invalidated_nodes(), panic_message() (+19 more)

### Community 46 - "Provider Selection & Routing"
Cohesion: 0.08
Nodes (30): build_provider_from_config(), build_provider_from_selection(), default_rewriter_model(), explicit_provider_prefix_overrides_inference(), infer_provider_name(), infers_anthropic_for_claude_models(), infers_google_for_gemini_models(), infers_openai_for_gpt_models() (+22 more)

### Community 47 - "Workspace Promotion"
Cohesion: 0.08
Nodes (24): basis_vector(), configured_test_db(), EmbeddingRow, fetch_embedding_batch(), fetch_validation_sample(), NodePromotionReport, promote_workspace_node_to_global_creates_global_row_with_same_uid(), promote_workspace_node_to_global_invalidates_workspace_row() (+16 more)

### Community 48 - "Tool Executor Service"
Cohesion: 0.09
Nodes (30): append_tool_call_event(), append_tool_error_event(), append_tool_result_event(), build_tool_run_plan(), build_tool_run_plan_uses_max_attempts_one_for_idempotent_tools(), CountingTool, has_prior_non_idempotent_result(), has_prior_tool_call_event() (+22 more)

### Community 49 - "Agent Adapter"
Cohesion: 0.06
Nodes (3): AgentAdapter, SessionTurnAdapter, SubAgentTurnAdapter

### Community 50 - "Object State Management"
Cohesion: 0.08
Nodes (15): build_result_uses_terminal_state(), follow_up_queues_message(), initial_task(), initial_task_seeds_state(), latest_assistant_text(), session_vo_cancel_flag_round_trips(), session_vo_destroy_clears_projection(), session_vo_idle_turn_maps_to_paused_status() (+7 more)

### Community 51 - "Slow Ingestion Path"
Cohesion: 0.1
Nodes (38): apply_decisions(), apply_decisions_with_graph(), apply_one_decision(), apply_one_decision_with_graph(), ApplyOutcome, classifier_from_env(), ClassifierBackend, classify_facts() (+30 more)

### Community 52 - "Lineage Emission"
Cohesion: 0.08
Nodes (35): AuditRootRow, build_lineage_sink(), build_lineage_sink_from_env_value(), ComplianceRow, context_chunk(), emit_context_lineage(), emit_generation_lineage(), estimate_tokens() (+27 more)

### Community 53 - "Gateway Message Renderer"
Cohesion: 0.09
Nodes (23): append_piece(), discord_renderer_attaches_buttons_to_last_chunk_only(), discord_renderer_uses_message_limit_for_long_text(), DiscordRenderChunk, DiscordRenderer, render_approval_request(), render_diff(), render_tool_card() (+15 more)

### Community 54 - "Discord Adapter"
Cohesion: 0.11
Nodes (23): approval_callback_maps_to_control_message(), attachments_from_message(), context_from_component(), discord_button(), discord_button_with_disabled(), discord_create_message(), discord_create_message_includes_buttons_for_last_chunk(), discord_edit_message() (+15 more)

### Community 55 - "Provider Request Builder"
Cohesion: 0.09
Nodes (31): annotate_cache_control(), annotate_message_cache_control(), anthropic_output_config(), anthropic_text_block(), apply_cache_breakpoints(), build_cache_create_body(), build_completion_request(), build_contents_from_messages() (+23 more)

### Community 56 - "Approval Request Types"
Cohesion: 0.07
Nodes (28): append_session_event(), approval_buttons(), approval_outcome_label(), approval_request(), approval_wait_timeout(), approval_wait_timeout_from_env(), ApprovalCallbackAction, ApprovalDecision (+20 more)

### Community 57 - "Turbopuffer Vector Store"
Cohesion: 0.12
Nodes (18): basis_vector(), filter_expr(), find_header_end(), MockResponse, MockServer, namespace_segment(), parse_matches(), query_path() (+10 more)

### Community 58 - "Broadcast Lag Handling"
Cohesion: 0.07
Nodes (16): record_broadcast_lag(), recv_with_lag_handling(), RecvResult, BroadcastChannel, ClaimCheck, event_stream_abort_policy_surfaces_error(), event_stream_emits_gap_marker_when_lagged(), EventFilter (+8 more)

### Community 59 - "Tool Router Policy"
Cohesion: 0.07
Nodes (16): approval_diffs_for(), approval_fields_for(), approval_pattern_chained_inner_uses_first_subcommand(), approval_pattern_for(), approval_pattern_malformed_wrapper_falls_back_to_full_input(), approval_pattern_nested_shell_not_recursed(), approval_pattern_simple_command(), approval_pattern_single_token() (+8 more)

### Community 60 - "Completion Content Types"
Cohesion: 0.07
Nodes (14): CacheBreakpoint, CacheBreakpointTarget, CacheTtl, completion_stream_abort_stops_completion_task(), CompletionContent, CompletionRequest, CompletionResponse, CompletionStream (+6 more)

### Community 61 - "Slack Adapter"
Cohesion: 0.12
Nodes (21): handle_interaction_event(), handle_push_event(), inbound_from_app_mention(), inbound_from_interaction_event(), inbound_from_message_event(), inbound_from_push_event(), interaction_origin(), normalize_event_json() (+13 more)

### Community 62 - "Orchestrator Test Fixture"
Cohesion: 0.1
Nodes (22): default_script(), Deployment, DeploymentsResponse, derive_admin_url(), ensure_postgres_image(), IsolatedTest, locate_orchestrator_binary(), OrchestratorTestFixture (+14 more)

### Community 63 - "Turn Latency Counters"
Cohesion: 0.11
Nodes (14): current_turn_root_span(), display_duration_ms(), record_turn_compaction(), record_turn_event_persist_duration(), record_turn_llm_call_duration(), record_turn_llm_ttft(), record_turn_pipeline_compile_duration(), record_turn_snapshot_load() (+6 more)

### Community 64 - "Citation Vendor Adapters"
Cohesion: 0.12
Nodes (22): AdapterError, answer_span_bytes(), anthropic_adapter_maps_document_index(), anthropic_chunk(), AnthropicCitations, cascade_flags_vendor_hallucinated_citation(), ChunkRef, chunks() (+14 more)

### Community 65 - "Telegram Adapter"
Cohesion: 0.14
Nodes (17): attachments_from_message(), channel_from_chat_and_reply(), handle_callback_query(), handle_message(), inbound_from_callback_query(), inbound_from_message(), inline_keyboard(), normalize_message() (+9 more)

### Community 66 - "Gemini Embedder"
Cohesion: 0.09
Nodes (18): build_embedder_from_config(), EmbedderConstructionRole, embedding_response(), EmbedRole, gemini_v2_does_not_renormalize_server_output(), gemini_v2_uses_prompt_prefix_and_snake_case_output_dimensionality(), GeminiContent, GeminiEmbedding (+10 more)

### Community 67 - "Session Event Store"
Cohesion: 0.07
Nodes (1): PostgresSessionStore

### Community 68 - "LLM Span Instrumentation"
Cohesion: 0.12
Nodes (17): calculate_cost(), calculate_cost_with_cached(), cost_calculation_correct(), has_meaningful_output(), llm_span_name(), LLMSpanAttributes, LLMSpanRecorder, metadata_f64() (+9 more)

### Community 69 - "Intent Manager Service"
Cohesion: 0.09
Nodes (13): AdoptCatalogIntentRequest, average_embeddings(), centroid_embedding(), CreateManualIntentRequest, GetLearningLogRequest, IntentIdRequest, IntentManager, IntentManagerImpl (+5 more)

### Community 70 - "Live Cache Audit Tests"
Cohesion: 0.13
Nodes (22): AuditedProvider, available_live_cache_provider_configs(), CacheTurnAudit, CacheTurnPlan, create_session(), full_request_payload(), is_query_rewrite_request(), is_repo_root() (+14 more)

### Community 71 - "Long Conversation Score Card"
Cohesion: 0.14
Nodes (22): CacheScores, ContextScores, CostScores, float_number(), FunctionalScores, LatencyScores, lineage_score_value(), MemoryScores (+14 more)

### Community 72 - "Skill Lessons & Render"
Cohesion: 0.08
Nodes (10): insert_addendum(), learn_lesson(), lesson_name(), LessonContext, set_app_role(), load_addenda(), render(), set_app_role() (+2 more)

### Community 73 - "Merkle Tree Audit"
Cohesion: 0.13
Nodes (21): assert_merkle_root_matches_fixture(), AuditRootManifest, blake3_inclusion_proof(), blake3_merkle_root(), Blake3Case, Blake3Vectors, ct_merkle_root_is_available_for_rfc6962_shape(), ct_sha256_root() (+13 more)

### Community 74 - "MCP Credential Proxy"
Cohesion: 0.12
Nodes (11): credential_from_env(), default_scope_for(), env_var(), environment_vault_loads_from_env_backed_server_config(), EnvironmentCredentialVault, headers_from_credential(), MCPCredentialProxy, McpSessionToken (+3 more)

### Community 75 - "Audit Signing Keys"
Cohesion: 0.13
Nodes (15): deterministic_seed(), ed25519_sign_with_rfc8032_test_keypair_produces_expected_signature(), ed25519_verify_fails_for_corrupted_signature_with_one_flipped_bit(), ed25519_verify_fails_for_message_with_one_flipped_bit(), ed25519_verify_fails_for_signature_under_different_keypair(), ed25519_verify_succeeds_for_valid_signature_and_keypair_pair(), Ed25519Case, Ed25519Vectors (+7 more)

### Community 76 - "Encrypted Secret Vault"
Cohesion: 0.14
Nodes (10): classify_token(), decrypt_bytes(), encrypt_bytes(), file_vault_encrypts_and_decrypts_roundtrip(), FileVault, PiiVault, pseudonym_is_deterministic_and_redacts_email(), PseudonymizationOutcome (+2 more)

### Community 77 - "Cron Job Object"
Cohesion: 0.13
Nodes (18): compute_next_fire(), compute_next_fire_at(), computes_next_top_of_hour_in_utc(), CronJob, CronJobConfig, CronJobImpl, CronJobState, CronJobStatus (+10 more)

### Community 78 - "Local Tools Integration Tests"
Cohesion: 0.13
Nodes (25): approval_prompt_str_replace_diff_is_surgical(), approval_prompt_uses_remembered_workspace_root_for_commands(), bash_captures_stdout_and_stderr(), bash_error_output_is_not_truncated(), bash_respects_timeout(), bash_success_output_is_truncated_to_router_budget(), docker_bash_hard_cancel_stops_container_exec(), docker_file_tools_roundtrip_inside_container_workspace() (+17 more)

### Community 79 - "Session Analytics"
Cohesion: 0.11
Nodes (15): analytics_window_start(), CacheDailyMetric, get_session_summary(), get_workspace_stats(), list_cache_daily_metrics(), list_session_turn_metrics(), list_tool_call_summaries(), normalized_days() (+7 more)

### Community 80 - "Citation Verifiers"
Cohesion: 0.13
Nodes (11): CascadeConfig, CascadeVerifier, sentence_for(), Bm25Verifier, CitationVerifier, contradiction_score(), NliVerifier, score_bm25() (+3 more)

### Community 81 - "Pgvector Store"
Cohesion: 0.15
Nodes (12): basis_vector(), cross_tenant_knn_cannot_see_other_workspace_vectors(), delete_items(), delete_node_index_rows(), ensure_default_workspace_embedder(), guard_workspace_embedder(), insert_node_index_rows(), pgvector_round_trip_returns_identical_seed_first() (+4 more)

### Community 82 - "Query Rewrite Postprocess"
Cohesion: 0.11
Nodes (16): query_rewrite_response_format(), QueryRewriter, allowed_terms(), cleanup_stripped_text(), filter_suggested_tools(), parse_rewrite_response(), RawQueryRewriteResult, strip_unsupported_entity_tokens() (+8 more)

### Community 83 - "Session Blob Store"
Cohesion: 0.18
Nodes (12): claim_check_from_value(), collect_blob_refs(), collect_large_strings(), decode_event_from_storage(), encode_event_for_storage(), expand_local_path(), file_blob_store_deletes_session_directory(), file_blob_store_is_content_addressed() (+4 more)

### Community 84 - "Sub-Agent Handlers"
Cohesion: 0.1
Nodes (5): maybe_resolve_parent_awakeable(), pending_approval_awakeable(), SessionImpl, start_turn_inner(), SubAgentImpl

### Community 85 - "File Search Tool"
Cohesion: 0.13
Nodes (16): build_file_search_output(), collect_matches(), default_skipped_dirs(), default_skipped_dirs_includes_polyglot_ecosystem_directories(), execute(), execute_docker(), execute_respects_custom_skip_directories(), execute_skips_python_virtualenv_matches() (+8 more)

### Community 86 - "Local Hand Provider"
Cohesion: 0.16
Nodes (3): detect_docker(), docker_status(), LocalHandProvider

### Community 87 - "Task Segment Tracker"
Cohesion: 0.12
Nodes (16): ActiveSegment, classify_started_segment(), completed_from_active(), ensure_current_segment(), first_message_creates_segment_zero(), follow_up_does_not_create_transition(), IntentClassification, new_task_creates_next_segment_with_previous_id() (+8 more)

### Community 88 - "Session Turn Lifecycle Tests"
Cohesion: 0.17
Nodes (21): await_snapshot_matching(), await_turn_phase(), CancelResponse, create_initialized_session(), live_model(), queue_message(), queue_message_during_active_turn_drains_after_completion(), QueueMessageResponse (+13 more)

### Community 89 - "E2B Hand Provider"
Cohesion: 0.16
Nodes (1): E2BHandProvider

### Community 90 - "Model Capabilities"
Cohesion: 0.1
Nodes (6): Credential, ModelCapabilities, ModelCapabilitiesBuilder, ProviderNativeTool, TokenPricing, ToolCallFormat

### Community 91 - "Scripted Provider"
Cohesion: 0.13
Nodes (3): ScriptedBlock, ScriptedProvider, ScriptedResponse

### Community 92 - "Session Store Handlers"
Cohesion: 0.09
Nodes (1): SessionStoreImpl

### Community 93 - "Cache Control Markers Test"
Cohesion: 0.2
Nodes (17): anthropic_body(), anthropic_request_byte_layout_changes_only_in_messages_segment_when_only_messages_change(), anthropic_request_byte_layout_is_identical_across_two_consecutive_turn_compilations(), anthropic_request_with_4_segment_pipeline_places_cache_markers_at_each_boundary(), anthropic_request_with_explicit_1h_ttl_includes_ttl_field_on_each_marker(), anthropic_request_with_long_messages_keeps_cache_markers_at_segment_boundaries_not_message_boundaries(), anthropic_request_with_no_tools_omits_tools_segment_marker(), assert_cache_ttls() (+9 more)

### Community 94 - "Schema Migration"
Cohesion: 0.15
Nodes (16): compile_for_gemini(), compile_for_gemini_removes_additional_properties_recursively(), compile_for_openai_strict(), compile_for_openai_strict_adds_additional_properties_false_recursively(), compile_for_openai_strict_does_not_duplicate_null_in_type_arrays(), compile_for_openai_strict_makes_optional_properties_required_and_nullable(), compile_for_openai_strict_preserves_existing_required_properties(), compile_for_openai_strict_strips_validation_only_keywords() (+8 more)

### Community 95 - "Session Store Inner Impl"
Cohesion: 0.1
Nodes (1): SessionStoreImpl

### Community 96 - "Conversation Compaction"
Cohesion: 0.15
Nodes (14): calculate_cost_cents(), CheckpointState, compaction_request(), compaction_triggers_even_when_incremental_snapshot_is_current(), event_summary_line(), latest_checkpoint_state(), maybe_compact_events(), non_checkpoint_events() (+6 more)

### Community 97 - "OpenAI Privacy Filter"
Cohesion: 0.14
Nodes (6): normalize_base_url(), OpenAiPrivacyFilterClassifier, PrivacyFilterThresholds, resolve_class(), ServiceResponse, ServiceSpan

### Community 98 - "Postgres Session Store Tests"
Cohesion: 0.23
Nodes (16): catalog_adoption_creates_tenant_intent_with_catalog_ref(), cleanup_schema(), create_test_store(), learning_log_rollback_invalidates_batch(), postgres_event_payloads_round_trip_as_jsonb(), postgres_materialized_analytics_views_refresh(), postgres_session_ids_are_native_uuid_and_concurrent_emits_are_serialized(), postgres_session_summary_tracks_model_tier_costs() (+8 more)

### Community 99 - "Postgres Session Store"
Cohesion: 0.19
Nodes (1): PostgresSessionStore

### Community 100 - "Grep Tool"
Cohesion: 0.17
Nodes (18): build_grep_output(), collect_context(), ContextLine, execute(), grep_finds_matching_lines(), grep_includes_context_lines(), grep_respects_gitignore(), grep_respects_skip_directories() (+10 more)

### Community 101 - "Tool Router Construction"
Cohesion: 0.13
Nodes (2): default_cloud_provider(), ToolRouter

### Community 102 - "Approval Token Auth"
Cohesion: 0.15
Nodes (9): ApprovalClaims, ApprovalTokenVerifier, consume_approval_jti(), decode_base64url(), decode_key_material(), Ed25519ManifestSigner, ensure_jti_inserted(), JwtHeader (+1 more)

### Community 103 - "Gateway Rate Limiter"
Cohesion: 0.15
Nodes (3): GatewayRateLimiter, GatewayRateLimitMetrics, GatewaySendResponse

### Community 104 - "Tenant Intent Store"
Cohesion: 0.12
Nodes (1): PostgresSessionStore

### Community 105 - "Retrieval Leg Candidates"
Cohesion: 0.22
Nodes (16): add_leg_scores(), begin_scoped(), bump_last_accessed(), graph_leg(), hydrate_nodes(), LegCandidate, lexical_fallback_leg(), lexical_fallback_terms() (+8 more)

### Community 106 - "CLI Exec Command"
Cohesion: 0.15
Nodes (9): exec_mode_formats_tool_updates_compactly(), format_tool_update(), handle_exec_event(), InterruptState, is_terminal_session_status(), parse_approval_decision(), resolve_exec_approval(), run_exec() (+1 more)

### Community 107 - "Session Enum Conversions"
Cohesion: 0.12
Nodes (0): 

### Community 108 - "OpenAI Provider Tests"
Cohesion: 0.22
Nodes (15): openai_provider_does_not_retry_after_partial_stream_output(), openai_provider_drops_oversized_metadata_values(), openai_provider_includes_native_web_search_when_enabled(), openai_provider_omits_native_web_search_when_disabled(), openai_provider_retries_after_rate_limit(), openai_provider_serializes_assistant_tool_calls_as_function_call_items(), openai_provider_serializes_tool_result_messages_as_function_call_output(), openai_provider_streams_parallel_tool_calls_in_order() (+7 more)

### Community 109 - "Security Policies"
Cohesion: 0.23
Nodes (11): ApprovalRuleStore, glob_match(), parse_and_match_bash(), persistent_rule_matching_uses_glob_patterns(), PolicyCheck, read_tools_are_auto_approved_and_bash_requires_approval(), rule_matches(), rule_visible_to_workspace() (+3 more)

### Community 110 - "Retrieval Load Scenario"
Cohesion: 0.2
Nodes (13): build_query_mix(), build_repeated_pool(), canonical_query(), drive_load(), hydrate_queries(), LoadReport, novel_query(), paraphrase() (+5 more)

### Community 111 - "Vector Backend Selection"
Cohesion: 0.18
Nodes (8): hipaa_tier_requires_baa_enabled_turbopuffer_client(), pg_store(), RemoteTarget, resolve_backend_choice(), SessionTarget, tp_store(), turbopuffer_selected_when_configured(), vector_store_for_workspace()

### Community 112 - "Tool Output Budget"
Cohesion: 0.24
Nodes (9): append_footer(), artifact_storage_footer(), count_lines(), estimate_tokens(), format_artifact_summary(), inline_artifact_preview_budget(), ToolRouter, truncate_text_for_budget() (+1 more)

### Community 113 - "Mock Smoke Loadtest"
Cohesion: 0.2
Nodes (12): enforce_gates(), error_rate(), mock_short_profile_completes_within_budget_with_zero_errors(), MockSmokeConfig, print_summary_table(), render_prometheus(), repo_root(), run_mock_smoke_gate() (+4 more)

### Community 114 - "AGE Graph Read"
Cohesion: 0.16
Nodes (5): AgeGraphStore, fetch_node(), fetch_nodes(), fetch_nodes_by_uid(), parse_agtype_uuid()

### Community 115 - "Platform Message Types"
Cohesion: 0.14
Nodes (12): ActionButton, Attachment, ButtonStyle, ChannelRef, DiffHunk, InboundMessage, MessageContent, OutboundMessage (+4 more)

### Community 116 - "Session Search Tool"
Cohesion: 0.16
Nodes (6): event_snippet(), render_results(), SessionSearchEventType, SessionSearchInput, SessionSearchTool, truncate()

### Community 117 - "Eval Replay Runner"
Cohesion: 0.21
Nodes (12): DatasetItem, JsonlDatasetItem, load_dataset_items(), normalized_tokens(), parse_jsonl_items(), register_dataset(), replay_dataset(), replay_dataset_live() (+4 more)

### Community 118 - "Cross-Tenant Isolation Loadtest"
Cohesion: 0.21
Nodes (11): app_scoped_conn(), attack_changelog_leak(), attack_cte_leak(), attack_dlq_leak(), attack_vector_oracle(), first_dlq(), first_embedding(), LeakReport (+3 more)

### Community 119 - "Gateway Edit Window"
Cohesion: 0.25
Nodes (11): edit_fallback_preserves_message_content_byte_for_byte(), edit_request(), edit_with_followup_fallback(), followup_request(), GatewayEditOutcome, GatewayEditResponse, is_fallback_edit_error(), post_json() (+3 more)

### Community 120 - "Segment Store"
Cohesion: 0.16
Nodes (1): PostgresSessionStore

### Community 121 - "Provider Retry Policy"
Cohesion: 0.27
Nodes (6): parse_retry_after(), response_text(), retries_on_rate_limit(), retry_after_delay(), retry_after_delay_from_message(), RetryPolicy

### Community 122 - "Resolution Scorer"
Cohesion: 0.25
Nodes (11): all_tools_failed_overrides_to_failed_with_high_confidence(), cancellation_overrides_to_abandoned(), label_confidence(), label_for_score(), null_signals_are_excluded_and_weights_renormalized(), resolution_score(), ResolutionOverride, ResolutionScorer (+3 more)

### Community 123 - "Loadtest Reporting"
Cohesion: 0.19
Nodes (7): enforce_gates(), histogram_math_percentile_is_monotonic_and_within_bucket(), histogram_percentile(), label_value(), prom_histogram_p95_ms(), prometheus_histogram_percentile(), write_stderr()

### Community 124 - "Client Smoke Tests"
Cohesion: 0.23
Nodes (5): clear_endpoint_env(), env_guard(), from_env_defaults_to_compose_ingress_when_unset(), from_env_reads_endpoint(), from_env_reads_restate_ingress_fallback()

### Community 125 - "OpenAI Responses Envelope Tests"
Cohesion: 0.35
Nodes (11): base_request(), openai_body(), openai_responses_request_serializes_with_separate_instructions_and_input_fields(), openai_responses_request_with_function_tools_includes_strict_mode_when_configured(), openai_responses_request_with_previous_response_id_chains_state_correctly(), openai_responses_request_with_tool_choice_required_serializes_correctly(), openai_responses_streaming_response_chunks_parse_into_provider_events(), provider_event_from_content() (+3 more)

### Community 126 - "Gemini Provider"
Cohesion: 0.26
Nodes (1): GeminiProvider

### Community 127 - "Prompt Injection Detection"
Cohesion: 0.26
Nodes (12): canary_detection_works(), check_canary(), classifier_flags_known_attack_patterns(), classify_input(), contains_canary_tokens(), inject_canary(), InputClassification, InputInspection (+4 more)

### Community 128 - "Turn Execution Smoke Tests"
Cohesion: 0.32
Nodes (11): await_phase(), cancel_after_run_dispatch_short_circuits(), cancel_before_init_short_circuits_via_self_resolve(), create_initialized_session(), fire_run(), live_model(), poll_progress(), ProgressResponse (+3 more)

### Community 129 - "Query Planner Tests"
Cohesion: 0.15
Nodes (1): SeedGraph

### Community 130 - "Long Conversation Budgets"
Cohesion: 0.23
Nodes (9): BudgetResult, Budgets, BudgetViolation, check_bool(), check_max_u32(), check_max_u64(), check_min_f64(), check_min_u32() (+1 more)

### Community 131 - "AGE Graph Store"
Cohesion: 0.18
Nodes (1): AgeGraphStore

### Community 132 - "Sandbox Config"
Cohesion: 0.18
Nodes (7): CloudConfig, CloudFlyioConfig, CloudHandsConfig, LocalConfig, McpCredentialConfig, McpServerConfig, McpTransportConfig

### Community 133 - "Privacy Erase Command"
Cohesion: 0.29
Nodes (11): Args, begin_app_scoped_tx(), emit_erase_summary(), enumerate_erase_candidates(), erase_audit_metadata(), erase_graph_store(), EraseCandidate, EraseContext (+3 more)

### Community 134 - "Anthropic Provider"
Cohesion: 0.26
Nodes (1): AnthropicProvider

### Community 135 - "Segment Scoring"
Cohesion: 0.38
Nodes (11): first_user_message(), last_brain_response(), latest_user_message(), load_segment_baseline(), load_session_events(), query_rewrite_from_metadata(), record_resolution_learning(), score_active_segment() (+3 more)

### Community 136 - "Tool Dispatch"
Cohesion: 0.3
Nodes (1): ToolRouter

### Community 137 - "Tool Recovery Handler"
Cohesion: 0.33
Nodes (4): HandFailureContext, is_gateway_unavailable_error(), is_timeout_error(), ToolRouter

### Community 138 - "Continuation Signal"
Cohesion: 0.23
Nodes (6): ContinuationInput, is_acknowledgment(), is_correction(), lexical_cosine_similarity(), score(), token_counts()

### Community 139 - "Eval Terminal Reporter"
Cohesion: 0.3
Nodes (5): format_scores(), render_includes_case_names_and_summary(), render_verbose_case(), result_index(), TerminalReporter

### Community 140 - "Loadtest Stack"
Cohesion: 0.26
Nodes (5): embed_texts(), fact_text(), Stack, WorkspaceFixture, WorkspaceRetriever

### Community 141 - "Contradiction Detection Tests"
Cohesion: 0.33
Nodes (9): candidate(), candidate_with(), contradiction_detector_does_not_flag_self_referential_fact_repetition(), contradiction_detector_does_not_flag_two_facts_with_different_predicates(), contradiction_detector_does_not_flag_two_facts_with_different_subjects(), contradiction_detector_flags_two_facts_with_same_subject_predicate_different_object(), contradiction_detector_handles_temporal_facts_correctly_when_ranges_overlap(), contradiction_detector_handles_temporal_facts_correctly_when_ranges_overlap_partially() (+1 more)

### Community 142 - "DSAR Audit Export Tests"
Cohesion: 0.45
Nodes (10): dsar_export_excludes_records_belonging_to_other_users(), dsar_export_for_unknown_user_writes_empty_file_with_zero_lines(), dsar_export_for_user_with_5_records_writes_5_jsonl_lines_with_correct_schema(), dsar_export_includes_redaction_for_phi_class_fields_when_redaction_enabled(), dsar_export_signed_manifest_accompanies_jsonl_with_valid_signature(), dsar_export_with_concurrent_writes_to_audit_log_produces_consistent_snapshot(), exporter(), fixed_options() (+2 more)

### Community 143 - "Sub-Agent Types"
Cohesion: 0.18
Nodes (6): DispatchSubAgentInput, SubAgentChildRef, SubAgentMessage, SubAgentResult, SubAgentState, SubAgentStatus

### Community 144 - "Session Row Decoders"
Cohesion: 0.24
Nodes (5): catalog_intent_from_row(), parse_resolution_signal(), parse_vector_text(), task_segment_from_row(), tenant_intent_from_row()

### Community 145 - "Turn Loop Detector"
Cohesion: 0.33
Nodes (6): loop_detector_disabled_at_zero_threshold(), loop_detector_does_not_trigger_on_varied_calls(), loop_detector_resets(), loop_detector_sliding_window(), loop_detector_triggers_after_threshold(), LoopDetector

### Community 146 - "Intent Classifier"
Cohesion: 0.33
Nodes (6): best_within_threshold(), classification_text(), embedding_below_threshold_returns_none(), exact_match_returns_high_confidence(), intent(), IntentClassifier

### Community 147 - "Provider Config"
Cohesion: 0.24
Nodes (4): GeneralConfig, ModelsConfig, ProviderCredentialConfig, ProvidersConfig

### Community 148 - "Gemini Safety Settings Tests"
Cohesion: 0.47
Nodes (7): base_request(), gemini_body(), gemini_request_includes_default_safety_settings_for_4_categories(), gemini_request_with_custom_safety_threshold_overrides_default(), gemini_request_with_function_declarations_serializes_correctly(), gemini_request_with_json_response_mime_type_includes_field(), snapshot_json()

### Community 149 - "Tool Approval Store"
Cohesion: 0.22
Nodes (6): PreparedToolApproval, PrepareToolApprovalRequest, StoreApprovalRuleRequest, to_handler_error(), WorkspaceStore, WorkspaceStoreImpl

### Community 150 - "Tool Lifecycle Manager"
Cohesion: 0.29
Nodes (4): hand_id(), sandbox_tier_label(), session_provider_key(), ToolRouter

### Community 151 - "Instruction Stage"
Cohesion: 0.38
Nodes (4): combine_workspace_instructions(), instruction_processor_appends_config_backed_sections(), instruction_processor_combines_config_and_discovered_workspace_instructions(), InstructionProcessor

### Community 152 - "Rewrite & Compaction Triggers"
Cohesion: 0.24
Nodes (5): approximate_query_tokens(), QueryRewriter, should_apply_tier2(), starts_with_tool_like_verb(), token_count()

### Community 153 - "Bash Tool"
Cohesion: 0.39
Nodes (6): bash_output_preserves_full_process_streams(), bash_output_small_streams_are_not_truncated(), BashToolInput, build_bash_output(), execute_docker(), execute_local()

### Community 154 - "Identity Stage"
Cohesion: 0.44
Nodes (3): identity_processor_appends_system_prompt(), identity_prompt_includes_coding_guardrails(), IdentityProcessor

### Community 155 - "Rewrite Input Builder"
Cohesion: 0.31
Nodes (5): input_from_context_messages(), input_from_conversation(), input_from_event_records(), QueryRewriter, RewriteInput

### Community 156 - "Rewrite Circuit Breaker"
Cohesion: 0.47
Nodes (2): CircuitBreaker, now_epoch_millis()

### Community 157 - "Moa Config Loader"
Cohesion: 0.46
Nodes (1): MoaConfig

### Community 158 - "Provider Request Body Snapshots"
Cohesion: 0.43
Nodes (7): anthropic_request_body__minimal_request_serializes_with_stable_byte_layout(), file_read_tool(), gemini_request_body__minimal_request_serializes_with_stable_byte_layout(), minimal_request(), openai_request_body__minimal_request_serializes_with_stable_byte_layout(), shell_command_tool(), tool_schema()

### Community 159 - "Session VO Tests"
Cohesion: 0.5
Nodes (6): session_vo_destroy_clears_state(), session_vo_post_message_queues_in_state(), session_vo_post_message_updates_status_to_running_then_idle_parks_paused(), session_vo_post_message_without_meta_errors(), test_message(), test_meta()

### Community 160 - "Health Service"
Cohesion: 0.29
Nodes (3): Health, HealthImpl, VersionInfo

### Community 161 - "Self Assessment Signal"
Cohesion: 0.29
Nodes (2): contains_any(), score()

### Community 162 - "Structural Signal"
Cohesion: 0.32
Nodes (5): baseline(), cold_start_returns_none(), is_high_outlier(), score(), SegmentMetrics

### Community 163 - "Lexical Seed Store"
Cohesion: 0.43
Nodes (2): LexicalStore, lookup_seed_rows()

### Community 164 - "Lineage Fjall Journal"
Cohesion: 0.29
Nodes (1): Journal

### Community 165 - "Query Rewrite Live Tests"
Cohesion: 0.38
Nodes (2): CapturingProvider, live_query_rewriter_resolves_coreference_without_new_entities()

### Community 166 - "Output Match Evaluator"
Cohesion: 0.43
Nodes (4): contains_rules_pass_when_all_terms_match(), evaluate_output(), missing_contains_term_reduces_score(), OutputMatchEvaluator

### Community 167 - "Live E2E Fixtures"
Cohesion: 0.43
Nodes (7): LIVE-E2E-ANTHROPIC Fixture, LIVE-E2E-GOOGLE Fixture, Live End-to-End Test Marker, LIVE-E2E-OPENAI Fixture, Anthropic Provider, Google Provider, OpenAI Provider

### Community 168 - "Pgaudit Smoke Tests"
Cohesion: 0.6
Nodes (5): audit_writes_log_line(), pgaudit_migration_configures_labels_when_provider_loaded_and_auditor_view(), pgaudit_smoke_requested(), quote_identifier(), test_database_url()

### Community 169 - "OpenAI Streaming Tests"
Cohesion: 0.33
Nodes (0): 

### Community 170 - "Graph Memory Maintenance"
Cohesion: 0.33
Nodes (4): CompactReport, CompactRequest, GraphMemoryMaint, GraphMemoryMaintImpl

### Community 171 - "Skill Activation"
Cohesion: 0.4
Nodes (3): extract_query_keywords_from_events(), skill_resolution_rate_map(), SkillInjector

### Community 172 - "Trajectory Match Evaluator"
Cohesion: 0.47
Nodes (3): lcs_len(), partial_match_scores_below_one(), TrajectoryMatchEvaluator

### Community 173 - "Threshold Evaluator"
Cohesion: 0.53
Nodes (3): cost_over_budget_fails_boolean_score(), limit_score(), ThresholdEvaluator

### Community 174 - "Perf Gate Binary"
Cohesion: 0.47
Nodes (3): Args, main(), Profile

### Community 175 - "PII Classifier Smoke"
Cohesion: 0.5
Nodes (2): classify_smoke_maps_ssn_to_phi_and_clean_text_to_none(), spawn_test_service()

### Community 176 - "Shell Chain Splitter"
Cohesion: 0.5
Nodes (2): push_sub_command(), split_shell_chain()

### Community 177 - "Scoped Transaction Lifecycle"
Cohesion: 0.5
Nodes (1): ScopedConn<'p>

### Community 178 - "Unified Diff"
Cohesion: 0.6
Nodes (3): compute_unified_diff(), small_edit_diff_is_substantially_smaller_than_full_file(), unified_diff_contains_standard_headers_and_hunks()

### Community 179 - "Anthropic Provider Tests"
Cohesion: 0.4
Nodes (0): 

### Community 180 - "Fake Clock Test Helper"
Cohesion: 0.4
Nodes (1): FakeClock

### Community 181 - "Neon Branch Maintenance"
Cohesion: 0.4
Nodes (3): NeonMaint, NeonMaintImpl, PruneReport

### Community 182 - "Client Runtime Tests"
Cohesion: 0.7
Nodes (4): from_endpoint_creates_initial_session_and_caches_tool_names(), mock_runtime_bootstrap(), run_turn_queues_message_and_relays_completed_outcome_as_runtime_events(), tool_descriptor_body()

### Community 183 - "Cohere Live Test"
Cohesion: 0.83
Nodes (3): cohere_embed_v4_returns_1024_dimensional_float_embeddings(), live_cohere_key(), live_cohere_requested()

### Community 184 - "Turbopuffer Live Tests"
Cohesion: 0.83
Nodes (3): basis_vector(), live_store(), turbopuffer_live_round_trip()

### Community 185 - "DB Error Mapping"
Cohesion: 0.5
Nodes (1): ScopedConn

### Community 186 - "Approval Rule Store"
Cohesion: 0.5
Nodes (1): PostgresSessionStore

### Community 187 - "Cohere Reranker Live Test"
Cohesion: 0.83
Nodes (3): cohere_rerank_v4_fast_prioritizes_relevant_retrieval_candidate(), live_cohere_key(), live_cohere_requested()

### Community 188 - "History Compaction Compile"
Cohesion: 0.83
Nodes (1): HistoryCompiler

### Community 189 - "Cost Budget Enforcement"
Cohesion: 0.67
Nodes (2): enforce_workspace_budget(), format_budget_exhausted_message()

### Community 190 - "Tool Success Evaluator"
Cohesion: 0.5
Nodes (1): ToolSuccessEvaluator

### Community 191 - "PII Live Sidecar Test"
Cohesion: 1.0
Nodes (2): live_service_url(), live_sidecar_classifies_private_and_clean_text()

### Community 192 - "Mock PII Classifier"
Cohesion: 0.67
Nodes (1): MockClassifier

### Community 193 - "Permissions Config"
Cohesion: 0.67
Nodes (1): PermissionsConfig

### Community 194 - "Gateway Config"
Cohesion: 0.67
Nodes (1): GatewayConfig

### Community 195 - "Local Orchestrator Stub"
Cohesion: 0.67
Nodes (1): OrchestratorConfig

### Community 196 - "Session Store Helpers"
Cohesion: 0.67
Nodes (1): PostgresSessionStore

### Community 197 - "Chat Harness Example"
Cohesion: 1.0
Nodes (2): main(), run_prompt()

### Community 198 - "Vector Error Mapping"
Cohesion: 1.0
Nodes (1): moa_core::MoaError

### Community 199 - "Cold Tier Partition Tests"
Cohesion: 1.0
Nodes (0): 

### Community 200 - "Memory Scope Test"
Cohesion: 1.0
Nodes (0): 

### Community 201 - "Model Identifier"
Cohesion: 1.0
Nodes (1): ModelId

### Community 202 - "Tool Call Identifier"
Cohesion: 1.0
Nodes (1): ToolCallId

### Community 203 - "Serialized Test Guard"
Cohesion: 1.0
Nodes (1): SerializedTest<'a>

### Community 204 - "Shell Chaining Security Test"
Cohesion: 1.0
Nodes (0): 

### Community 205 - "Lineage Postgres Tests"
Cohesion: 1.0
Nodes (0): 

### Community 206 - "Object Context"
Cohesion: 1.0
Nodes (1): ObjectContext<'a>

### Community 207 - "Shared Object Context"
Cohesion: 1.0
Nodes (1): SharedObjectContext<'a>

### Community 208 - "Docker Hardening Test"
Cohesion: 1.0
Nodes (0): 

### Community 209 - "History Budgeting"
Cohesion: 1.0
Nodes (0): 

### Community 210 - "History Error Preservation"
Cohesion: 1.0
Nodes (0): 

### Community 211 - "Skills Cache Break"
Cohesion: 1.0
Nodes (0): 

### Community 212 - "Eval Live Tests"
Cohesion: 1.0
Nodes (0): 

### Community 213 - "Workspace Hack Build"
Cohesion: 1.0
Nodes (0): 

### Community 214 - "Skills Bootstrap Script"
Cohesion: 1.0
Nodes (0): 

### Community 215 - "Core Type Macros"
Cohesion: 1.0
Nodes (0): 

### Community 216 - "Integration Test Entry"
Cohesion: 1.0
Nodes (0): 

### Community 217 - "Restate Registration"
Cohesion: 1.0
Nodes (0): 

### Community 218 - "Session Store Inner"
Cohesion: 1.0
Nodes (0): 

## Knowledge Gaps
- **710 isolated node(s):** `PiiSpan`, `PiiClassifier`, `PiiError`, `ExpectedFacts`, `ExpectedFact` (+705 more)
  These have ≤1 connection - possible missing edges or undocumented components.
- **Thin community `Vector Error Mapping`** (2 nodes): `moa_core::MoaError`, `.from()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Cold Tier Partition Tests`** (2 nodes): `partition_layout.rs`, `partition_key_uses_workspace_and_day_layout()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Memory Scope Test`** (2 nodes): `scope.rs`, `memory_scope_ancestors_and_serialization_cover_all_scope_tiers()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Model Identifier`** (2 nodes): `ModelId`, `.default()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Tool Call Identifier`** (2 nodes): `ToolCallId`, `.from()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Serialized Test Guard`** (2 nodes): `SerializedTest<'a>`, `.deref()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Shell Chaining Security Test`** (2 nodes): `shell_chaining_does_not_match_simple_pattern.rs`, `shell_chain_after_matching_prefix_does_not_satisfy_simple_glob()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Lineage Postgres Tests`** (2 nodes): `lineage_postgres.rs`, `postgres_lineage_sink_writes_rows()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Object Context`** (2 nodes): `ObjectContext<'a>`, `.get_json()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Shared Object Context`** (2 nodes): `SharedObjectContext<'a>`, `.get_json()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Docker Hardening Test`** (2 nodes): `docker_hardening.rs`, `docker_container_runs_with_hardening()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `History Budgeting`** (2 nodes): `budgeting.rs`, `keep_budgeted_older_messages()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `History Error Preservation`** (2 nodes): `errors.rs`, `preserved_error_messages()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Skills Cache Break`** (2 nodes): `cache_break.rs`, `mark_stable_prefix_breakpoint()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Eval Live Tests`** (2 nodes): `engine_live.rs`, `live_run_single_produces_eval_result()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Workspace Hack Build`** (2 nodes): `build.rs`, `main()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Skills Bootstrap Script`** (2 nodes): `bootstrap_global_skills.rs`, `main()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Core Type Macros`** (1 nodes): `macros.rs`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Integration Test Entry`** (1 nodes): `integration.rs`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Restate Registration`** (1 nodes): `restate_register.rs`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Session Store Inner`** (1 nodes): `inner.rs`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **What connects `PiiSpan`, `PiiClassifier`, `PiiError` to the rest of the system?**
  _710 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `Runtime Events & SSE` be split into smaller, more focused modules?**
  _Cohesion score 0.01 - nodes in this community are weakly interconnected._
- **Should `PII Redaction Core` be split into smaller, more focused modules?**
  _Cohesion score 0.01 - nodes in this community are weakly interconnected._
- **Should `Postgres RLS & Audit Tests` be split into smaller, more focused modules?**
  _Cohesion score 0.02 - nodes in this community are weakly interconnected._
- **Should `CLI Entry Points` be split into smaller, more focused modules?**
  _Cohesion score 0.02 - nodes in this community are weakly interconnected._
- **Should `CLI Command Tests` be split into smaller, more focused modules?**
  _Cohesion score 0.02 - nodes in this community are weakly interconnected._
- **Should `Slow Path Ingestion Tests` be split into smaller, more focused modules?**
  _Cohesion score 0.02 - nodes in this community are weakly interconnected._