# Graph Report - .  (2026-06-08)

## Corpus Check
- Large corpus: 717 files · ~391,179 words. Semantic extraction will be expensive (many Claude tokens). Consider running on a subfolder, or use --no-semantic to run AST-only.

## Summary
- 7545 nodes · 12269 edges · 237 communities detected
- Extraction: 100% EXTRACTED · 0% INFERRED · 0% AMBIGUOUS · INFERRED: 6 edges (avg confidence: 0.82)
- Token cost: 0 input · 0 output

## God Nodes (most connected - your core abstractions)
1. `OrchestratorClient` - 45 edges
2. `ChatRuntime` - 35 edges
3. `CountedSessionStore` - 32 edges
4. `SkillFrontmatter` - 32 edges
5. `PostgresSessionStore` - 30 edges
6. `E2BHandProvider` - 23 edges
7. `LocalHandProvider` - 22 edges
8. `DaytonaHandProvider` - 22 edges
9. `main()` - 21 edges
10. `SubAgentTurnAdapter` - 21 edges

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
Nodes (206): AsyncAuthzConfig, AsyncAuthzKind, default_timeout_secs(), build_default_pipeline(), build_default_pipeline_with_tools(), GraphMemoryPipelineOptions, CompiledRequest, TurnUsage (+198 more)

### Community 1 - "PII Redaction Core"
Cohesion: 0.01
Nodes (138): ChangelogRecord, validate_scope(), write_and_bump(), acknowledgement(), command_action(), control_action_for_inbound(), GatewayControlAction, DaemonCommand (+130 more)

### Community 2 - "CLI Entry Points"
Cohesion: 0.01
Nodes (162): AdminCommand, format_promotion_report(), handle_admin_command(), promote_workspace_e2e(), PromoteWorkspaceArgs, WorkspacePromotionArgs, analytics_window_start(), CacheDailyMetric (+154 more)

### Community 3 - "Postgres RLS & Audit Tests"
Cohesion: 0.02
Nodes (128): anthropic_offline_429_response_triggers_retry_with_backoff(), anthropic_offline_500_response_triggers_retry_then_surfaces_typed_error(), anthropic_offline_completion_returns_text_for_minimal_request(), anthropic_offline_malformed_json_response_returns_typed_parse_error(), anthropic_offline_streaming_disconnect_mid_response_surfaces_typed_error_with_partial_events(), anthropic_offline_streaming_yields_text_deltas_then_terminal_event(), anthropic_offline_tool_call_response_parses_into_provider_event(), provider() (+120 more)

### Community 4 - "CLI Command Tests"
Cohesion: 0.02
Nodes (66): collect_session_tool_observations(), merge_session_observation(), normalize_error_pattern(), record_error_pattern(), SessionToolObservation, top_error_patterns(), truncate_with_ellipsis(), workspace_tool_stats_from_events() (+58 more)

### Community 5 - "Merkle Tree Audit"
Cohesion: 0.02
Nodes (85): ApprovalReaper, ApprovalReaperHandle, HttpAwakeableResolver, ReaperError, AwakeableResolveError, AwakeableResolver, canonical_json_bytes(), canonical_payload_hash() (+77 more)

### Community 6 - "Embedding Provider"
Cohesion: 0.02
Nodes (58): AgentActAsRequest, AgentSummary, AgentTemplateSummary, apply_auth_headers(), ApprovalSummary, AuditVerifyResponse, body_for_tuple_op(), client_from_credentials() (+50 more)

### Community 7 - "Sub-Agent Handlers"
Cohesion: 0.02
Nodes (63): AgentRegistry, AgentRegistryImpl, operator_user_id(), register_agent_inner(), RegisterAgentRequest, RegisterAgentResponse, AgentTemplates, AgentTemplatesImpl (+55 more)

### Community 8 - "Skill Frontmatter Format"
Cohesion: 0.03
Nodes (56): append_skill_learning(), build_distillation_prompt(), count_tool_calls(), distill_skill_with_learning(), DistillationOutcome, DistillationSkipReason, extract_task_summary(), find_similar_skill() (+48 more)

### Community 9 - "Schema Migration"
Cohesion: 0.03
Nodes (93): cascade_deactivate_user(), CascadeError, CascadeSummary, delete_group_memberships(), enqueue_direct_user_tuple_deletes(), orphan_user_agents(), revoke_user_api_keys(), table_exists() (+85 more)

### Community 10 - "pgvector Store"
Cohesion: 0.03
Nodes (88): changelog_rejects_updates_for_app_role(), changelog_write_bumps_workspace_version_and_respects_read_rls(), record(), set_app_role(), set_auditor_role(), set_workspace_gucs(), AgTypeParam, Cypher (+80 more)

### Community 11 - "Orchestrator Test Harness"
Cohesion: 0.03
Nodes (82): approval_allow_once_round_trip_through_restate(), configured_env(), live_model(), object_url(), register_deployment(), spawn_orchestrator(), wait_for_approval_request(), wait_for_brain_response_count() (+74 more)

### Community 12 - "Tool Types & Policy"
Cohesion: 0.03
Nodes (42): default_budget_for_tool(), execute_tool_policy(), RegisteredTool, ToolExecution, ToolRegistry, anthropic_content_blocks(), anthropic_message(), build_function_tool() (+34 more)

### Community 13 - "Slow Path Ingestion Tests"
Cohesion: 0.03
Nodes (46): approval_button_click_with_unknown_request_id_returns_stale_error_message(), approval_button_click_with_valid_callback_data_emits_decision_signal(), approval_buttons_after_decision_re_render_as_disabled_with_decision_marker(), approval_request_after_orchestrator_timeout_marks_buttons_as_expired(), ApprovalClickOutcome, ApprovalLifecycleState, ApprovalRecord, ApprovalStateTracker (+38 more)

### Community 14 - "Eval Fixtures & Pricing"
Cohesion: 0.04
Nodes (50): CollectedExecution, collector_tracks_tool_steps_and_metrics(), estimate_cost(), TrajectoryCollector, truncate(), fixture_path(), transcript_jsonl_round_trips_through_read_and_write(), ConsolidationOutcomes (+42 more)

### Community 15 - "Brain Turn Tests"
Cohesion: 0.04
Nodes (36): always_allow_rule_persists_and_skips_next_approval(), ArtifactRetrievalLlmProvider, ArtifactStderrLlmProvider, canary_leaks_in_tool_input_are_detected_and_blocked(), CanaryLeakLlmProvider, CapturingTextLlmProvider, count_lines(), extract_tool_id_field() (+28 more)

### Community 16 - "File Read & Write Tools"
Cohesion: 0.04
Nodes (77): container_path_validation_accepts_workspace_absolute_paths(), container_path_validation_rejects_absolute_paths_outside_workspace(), container_path_validation_rejects_traversal(), docker_file_read(), docker_file_search(), docker_file_write(), docker_find_args(), docker_read_args() (+69 more)

### Community 17 - "Workspace Instructions"
Cohesion: 0.04
Nodes (48): Consolidate, ConsolidateDurableSteps, ConsolidateImpl, ConsolidateReport, ConsolidateRequest, object_url(), register_deployment(), spawn_orchestrator() (+40 more)

### Community 18 - "Memory Scope Tool"
Cohesion: 0.03
Nodes (35): CohereEmbedderConfig, contribution(), derive_ingest_source_name(), duration_ms_u32(), execute_memory_tool(), extract_search_keywords(), extract_search_query(), extract_search_query_from_messages() (+27 more)

### Community 19 - "LLM Gateway Service"
Cohesion: 0.05
Nodes (43): build_anthropic_provider(), build_google_provider(), build_openai_provider(), CompletionRequest, CompletionRequestExt, CompletionStreamHandle, compute_cost_cents(), configured_env() (+35 more)

### Community 20 - "Hybrid Retriever"
Cohesion: 0.04
Nodes (41): apply_layer_bias(), build_hits(), EmptyGraph, EmptyVector, hit(), HybridRetriever, layer_bias_prefers_user_over_workspace_for_matching_scores(), leg_or_empty() (+33 more)

### Community 21 - "Telemetry Init"
Cohesion: 0.04
Nodes (45): auth0_without_feature_returns_feature_missing(), build_providers(), build_providers_with_resolver(), BuildError, disabled_auth_provider_accepts_any_credential_as_service_identity(), env_value(), HybridAuthProvider, Providers (+37 more)

### Community 22 - "Session Enum Conversions"
Cohesion: 0.05
Nodes (52): AccountChangeEvent, Actor, AuthenticationEvent, AuthorizationEvent, EntityManagementEvent, Metadata, NetworkEndpoint, Product (+44 more)

### Community 23 - "Fast Ingestion Path"
Cohesion: 0.05
Nodes (55): active_uids_for_pattern(), begin_scoped(), build_intent(), cohere_api_key(), deterministic_vector(), execute_forget_tool(), execute_memory_tool(), execute_remember_tool() (+47 more)

### Community 24 - "Turn Execution Workflow"
Cohesion: 0.06
Nodes (56): PreparedTurnRequest, ResolutionLabel, ResolutionScore, ScoringPhase, SegmentBaseline, SkillResolutionRate, dispatch_sub_agent(), DispatchedSubAgent (+48 more)

### Community 25 - "Object State Management"
Cohesion: 0.05
Nodes (31): AppState, Auth0ConnectionLinkedWebhook, credential_for_request(), DisabledAuth, extract_credential(), handle_auth0_connection_webhook(), handle_proxy(), lookup_auth0_user_id() (+23 more)

### Community 26 - "Eval Engine & Plan"
Cohesion: 0.05
Nodes (50): build_error_result(), cleanup_workspace(), dry_run_marks_results_skipped(), EngineOptions, EvalEngine, EvalRun, extract_trace_id(), fs_try_exists() (+42 more)

### Community 27 - "Lineage MPSC Sink Writer"
Cohesion: 0.05
Nodes (42): expand_home(), mpsc_sink_drops_when_channel_is_full(), MpscSink, MpscSinkBuilder, MpscSinkConfig, null_sink_never_records_drops(), NullSink, sample_event() (+34 more)

### Community 28 - "Turn & Tool Dispatch"
Cohesion: 0.04
Nodes (28): build_turn_context(), BuildTurnContextOptions, persist_context_snapshot(), approval_requested_event_round_trips_full_prompt(), Event, sample_approval_prompt(), PendingToolApproval, StoredApprovalDecision (+20 more)

### Community 29 - "Session Replay Snapshots"
Cohesion: 0.05
Nodes (16): approval_decision_size(), approval_prompt_size(), approx_event_bytes(), counted_store_records_get_events_within_scope(), CountedSessionStore, display_duration_ms(), event_payload_size(), event_record() (+8 more)

### Community 30 - "Approval Token Auth"
Cohesion: 0.04
Nodes (48): ApprovalClaims, ApprovalDecision, ApprovalHandle, ApprovalRequest, ApprovalTokenVerifier, async_authz_provider_trait_object_returns_handle(), AsyncAuthzError, AsyncAuthzProvider (+40 more)

### Community 31 - "DSAR Privacy Export"
Cohesion: 0.06
Nodes (59): Args, begin_audited_read(), collect_changelog(), collect_embeddings(), collect_entities(), collect_facts(), collect_nodes(), collect_relationships() (+51 more)

### Community 32 - "Turn Runner Helpers"
Cohesion: 0.05
Nodes (31): add_session_trace_link(), apply_session_trace(), session_turn_span(), synthetic_session_span_context(), append_session_event(), cache_prefix_ratio(), cache_prefix_ratio_includes_tool_tokens(), capabilities() (+23 more)

### Community 33 - "Provider Streaming"
Cohesion: 0.05
Nodes (38): GeminiCachedContent, GeminiCandidate, GeminiContent, GeminiFunctionCall, GeminiGenerateContentResponse, GeminiPart, GeminiUsageMetadata, ResponsesStreamError (+30 more)

### Community 34 - "Contradiction Detection"
Cohesion: 0.06
Nodes (40): build_judge_prompt(), candidate(), candidate_text(), CohereReranker, CohereRerankHit, CohereRerankRequest, CohereRerankResponse, Conflict (+32 more)

### Community 35 - "Cross-Tenant Pentest Suite"
Cohesion: 0.07
Nodes (39): assert_attack(), attack_a_forgotten_guc(), attack_a_impl(), attack_b_cross_tenant_write(), attack_b_impl(), attack_c_cross_tenant_fk_leakage(), attack_c_impl(), attack_d_impl() (+31 more)

### Community 36 - "Audit Signing Keys"
Cohesion: 0.06
Nodes (36): canonicalize(), canonicalize_rejects_floats(), canonicalize_sorts_keys(), JcsError, utf16_cmp(), write_number(), write_string(), write_value() (+28 more)

### Community 37 - "Eval Budget Checker"
Cohesion: 0.06
Nodes (39): AnalyticsScoreRow, Baselines, BudgetExpectations, CacheScores, check_bool(), check_max_u64(), check_min_f64(), check_min_u64() (+31 more)

### Community 38 - "API Key Service"
Cohesion: 0.06
Nodes (40): actor_user_id(), ApiKeyError, ApiKeyRow, ApiKeys, ApiKeysImpl, authorize_key_management(), create(), create_key_inner() (+32 more)

### Community 39 - "Concurrent Event Monotonicity Tests"
Cohesion: 0.08
Nodes (48): basis_vector(), configured_test_db(), item(), query(), reembed_in_progress_state_blocks_concurrent_knn_queries_until_complete(), reembed_workspace_with_new_embedder_overwrites_existing_vectors_atomically(), scope(), scoped_conn() (+40 more)

### Community 40 - "Cache Optimizer"
Cohesion: 0.08
Nodes (34): cache_eviction_at_capacity_does_not_crash(), cache_hit_reuses_successful_workspace_retrieval(), cache_invalidation_on_write_version_bump_misses(), cache_optimizer_plans_tool_static_and_conversation_breakpoints(), cache_optimizer_skips_conversation_breakpoint_for_short_sessions(), CachedEntry, CachedHybridRetriever, CachedHybridRetrieverConfig (+26 more)

### Community 41 - "Working Context"
Cohesion: 0.05
Nodes (19): BudgetConfig, CompactionConfig, context_message_assistant_tool_call_preserves_invocation(), context_message_tool_result_preserves_text_and_blocks(), ContextMessage, ContextSnapshotConfig, estimate_text_tokens(), ExcludedItem (+11 more)

### Community 42 - "Skill Tier1 Metadata"
Cohesion: 0.06
Nodes (34): capabilities(), compiled_snapshot(), compiler_with_recent_turns(), event_record(), file_read_tool_call(), file_read_tool_result(), fixed_time(), MockLlmProvider (+26 more)

### Community 43 - "Golden E2E Fixtures"
Cohesion: 0.07
Nodes (39): assert_top_k_within_window(), compare_top_k_within_window(), dump_traces(), ExpectedRankMismatch, GoldenRankingMismatch, box_error(), box_message(), changelog_count() (+31 more)

### Community 44 - "Tool Executor Service"
Cohesion: 0.08
Nodes (37): append_event_increments_sequence(), cleanup(), get_events_respects_range(), search_events_finds_by_payload(), test_session_meta(), test_store(), update_status_affects_get_session(), append_tool_call_event() (+29 more)

### Community 45 - "Query Planner & NER"
Cohesion: 0.07
Nodes (31): dedupe_spans(), extract_code_like_spans(), extract_noun_phrases(), extract_quoted_spans(), extract_relation_targets(), flush_noun_group(), is_boundary(), is_stopword() (+23 more)

### Community 46 - "Session Types"
Cohesion: 0.05
Nodes (30): BufferedUserMessage, CancelMode, CheckpointHandle, CheckpointInfo, DaemonConfig, ObserveLevel, pending_signal_queue_message_round_trip(), PendingSignal (+22 more)

### Community 47 - "History Compilation"
Cohesion: 0.05
Nodes (23): build_events_from_turn_specs(), checkpoint_list_report(), format_checkpoint_age(), full_read_fixture(), HistoryCompiler, incremental_history_replaces_prior_full_file_reads_across_turns(), SnapshotHistory, test_action_strategy() (+15 more)

### Community 48 - "Neon Branch Manager"
Cohesion: 0.09
Nodes (25): checkpoint_branch_names_follow_moa_prefix(), checkpoint_info_from_branch(), checkpoint_label_from_name(), cleanup_expired_deletes_only_old_moa_branches(), create_checkpoint_refuses_to_exceed_capacity(), create_checkpoint_sends_expected_request_and_returns_handle(), discard_checkpoint_calls_delete_endpoint(), format_checkpoint_branch_name() (+17 more)

### Community 49 - "Runtime Context Stage"
Cohesion: 0.08
Nodes (29): assert_stage_contract(), cache_stage_inserts_breakpoints_at_4_segment_boundaries(), delete_global_vector_noise(), delete_memory_rows(), FixedClock, identity_stage_emits_stable_system_message_with_workspace_and_runtime_metadata(), instruction_stage_appends_workspace_instructions_when_present_and_skips_when_absent(), memory_stage_includes_top_k_hits_with_lineage_uids_and_excludes_invalidated_nodes() (+21 more)

### Community 50 - "Fact Extraction & Chunking"
Cohesion: 0.08
Nodes (42): chunk_turn(), estimate_tokens(), flush_paragraph(), is_explicit_fact_line(), join_units(), joined_len_with(), overlap_units(), push_chunk() (+34 more)

### Community 51 - "Tool Result Store"
Cohesion: 0.06
Nodes (16): collect_context(), load_tool_result_text(), MockSessionStore, parse_tool_id(), render_search_summary(), search_tool_result(), SearchContextLine, SearchMatch (+8 more)

### Community 52 - "Runtime Task Monitor"
Cohesion: 0.06
Nodes (16): percentile(), summarize_percentiles(), init_metrics(), metrics_endpoint_url(), metrics_endpoint_url_uses_localhost_for_unspecified_listener(), parse_metrics_listen_addr(), prometheus_endpoint_exports_recorded_metric_families(), record_cache_hit_rate() (+8 more)

### Community 53 - "Provider Selection & Routing"
Cohesion: 0.08
Nodes (30): build_provider_from_config(), build_provider_from_selection(), default_rewriter_model(), explicit_provider_prefix_overrides_inference(), infer_provider_name(), infers_anthropic_for_claude_models(), infers_google_for_gemini_models(), infers_openai_for_gpt_models() (+22 more)

### Community 54 - "Workspace Promotion"
Cohesion: 0.08
Nodes (24): basis_vector(), configured_test_db(), EmbeddingRow, fetch_embedding_batch(), fetch_validation_sample(), NodePromotionReport, promote_workspace_node_to_global_creates_global_row_with_same_uid(), promote_workspace_node_to_global_invalidates_workspace_row() (+16 more)

### Community 55 - "Agent Adapter"
Cohesion: 0.06
Nodes (3): AgentAdapter, SessionTurnAdapter, SubAgentTurnAdapter

### Community 56 - "Long Conversation Smoke Tests"
Cohesion: 0.08
Nodes (34): agent_config_for(), approval_allow_once_then_always_allow_then_deny_in_same_session_meets_budgets(), approval_decisions(), assert_approval_modes(), assert_canary_leak_blocked(), assert_compaction_invariants(), assert_multi_observer_parity(), assert_prompt_cache_metrics() (+26 more)

### Community 57 - "Slow Ingestion Path"
Cohesion: 0.1
Nodes (38): apply_decisions(), apply_decisions_with_graph(), apply_one_decision(), apply_one_decision_with_graph(), ApplyOutcome, classifier_from_env(), ClassifierBackend, classify_facts() (+30 more)

### Community 58 - "Lineage Emission"
Cohesion: 0.08
Nodes (35): AuditRootRow, build_lineage_sink(), build_lineage_sink_from_env_value(), ComplianceRow, context_chunk(), emit_context_lineage(), emit_generation_lineage(), estimate_tokens() (+27 more)

### Community 59 - "Gateway Message Renderer"
Cohesion: 0.09
Nodes (23): append_piece(), discord_renderer_attaches_buttons_to_last_chunk_only(), discord_renderer_uses_message_limit_for_long_text(), DiscordRenderChunk, DiscordRenderer, render_approval_request(), render_diff(), render_tool_card() (+15 more)

### Community 60 - "Discord Adapter"
Cohesion: 0.11
Nodes (23): approval_callback_maps_to_control_message(), attachments_from_message(), context_from_component(), discord_button(), discord_button_with_disabled(), discord_create_message(), discord_create_message_includes_buttons_for_last_chunk(), discord_edit_message() (+15 more)

### Community 61 - "Provider Request Builder"
Cohesion: 0.09
Nodes (31): annotate_cache_control(), annotate_message_cache_control(), anthropic_output_config(), anthropic_text_block(), apply_cache_breakpoints(), build_cache_create_body(), build_completion_request(), build_contents_from_messages() (+23 more)

### Community 62 - "Lineage Event Records"
Cohesion: 0.05
Nodes (33): AclFilterDecision, AgeIntrospection, BackendIntrospection, Citation, CitationLineage, ContextChunk, ContextLineage, DecisionKind (+25 more)

### Community 63 - "Approval Request Types"
Cohesion: 0.07
Nodes (28): append_session_event(), approval_buttons(), approval_outcome_label(), approval_request(), approval_wait_timeout(), approval_wait_timeout_from_env(), ApprovalCallbackAction, ApprovalDecision (+20 more)

### Community 64 - "Skill Regression Runs"
Cohesion: 0.09
Nodes (28): append_skill_regression_log(), build_generated_suite(), compare_scores(), default_skill_evaluators(), estimate_suite_cost(), estimate_tokens(), execute_skill_suite(), expand_local_path() (+20 more)

### Community 65 - "Turbopuffer Vector Store"
Cohesion: 0.12
Nodes (18): basis_vector(), filter_expr(), find_header_end(), MockResponse, MockServer, namespace_segment(), parse_matches(), query_path() (+10 more)

### Community 66 - "Broadcast Lag Handling"
Cohesion: 0.07
Nodes (16): record_broadcast_lag(), recv_with_lag_handling(), RecvResult, BroadcastChannel, ClaimCheck, event_stream_abort_policy_surfaces_error(), event_stream_emits_gap_marker_when_lagged(), EventFilter (+8 more)

### Community 67 - "Tool Router Policy"
Cohesion: 0.07
Nodes (16): approval_diffs_for(), approval_fields_for(), approval_pattern_chained_inner_uses_first_subcommand(), approval_pattern_for(), approval_pattern_malformed_wrapper_falls_back_to_full_input(), approval_pattern_nested_shell_not_recursed(), approval_pattern_simple_command(), approval_pattern_single_token() (+8 more)

### Community 68 - "Completion Content Types"
Cohesion: 0.07
Nodes (14): CacheBreakpoint, CacheBreakpointTarget, CacheTtl, completion_stream_abort_stops_completion_task(), CompletionContent, CompletionRequest, CompletionResponse, CompletionStream (+6 more)

### Community 69 - "Slack Adapter"
Cohesion: 0.12
Nodes (21): handle_interaction_event(), handle_push_event(), inbound_from_app_mention(), inbound_from_interaction_event(), inbound_from_message_event(), inbound_from_push_event(), interaction_origin(), normalize_event_json() (+13 more)

### Community 70 - "Encrypted Secret Vault"
Cohesion: 0.11
Nodes (11): Auth0TokenVaultProvider, classify_token(), decrypt_bytes(), encrypt_bytes(), file_vault_encrypts_and_decrypts_roundtrip(), FileVault, PiiVault, pseudonym_is_deterministic_and_redacts_email() (+3 more)

### Community 71 - "Orchestrator Test Fixture"
Cohesion: 0.1
Nodes (22): default_script(), Deployment, DeploymentsResponse, derive_admin_url(), ensure_postgres_image(), IsolatedTest, locate_orchestrator_binary(), OrchestratorTestFixture (+14 more)

### Community 72 - "Turn Latency Counters"
Cohesion: 0.11
Nodes (14): current_turn_root_span(), display_duration_ms(), record_turn_compaction(), record_turn_event_persist_duration(), record_turn_llm_call_duration(), record_turn_llm_ttft(), record_turn_pipeline_compile_duration(), record_turn_snapshot_load() (+6 more)

### Community 73 - "Agents Service & CLI"
Cohesion: 0.12
Nodes (24): actor_user_id(), AgentActAsRequest, Agents, AgentsAction, AgentsCommand, AgentsImpl, AgentSummary, deactivate_agent_inner() (+16 more)

### Community 74 - "Citation Vendor Adapters"
Cohesion: 0.12
Nodes (22): AdapterError, answer_span_bytes(), anthropic_adapter_maps_document_index(), anthropic_chunk(), AnthropicCitations, cascade_flags_vendor_hallucinated_citation(), ChunkRef, chunks() (+14 more)

### Community 75 - "Telegram Adapter"
Cohesion: 0.14
Nodes (17): attachments_from_message(), channel_from_chat_and_reply(), handle_callback_query(), handle_message(), inbound_from_callback_query(), inbound_from_message(), inline_keyboard(), normalize_message() (+9 more)

### Community 76 - "Session Event Store"
Cohesion: 0.07
Nodes (1): PostgresSessionStore

### Community 77 - "Live Ingestion E2E Harness"
Cohesion: 0.14
Nodes (21): complex_ingestion_turn_writes_facts_pii_changelog_and_dedup(), degraded_workspace_skips_sampled_low_pii_turn_without_side_effects(), degraded_workspace_still_ingests_sensitive_turn(), fact_count(), fact_summaries(), ingestion_turn_round_trip_through_restate_is_idempotent(), LiveIngestionHarness, low_pii_degraded_skip_turn() (+13 more)

### Community 78 - "Gemini Embedder"
Cohesion: 0.09
Nodes (18): build_embedder_from_config(), EmbedderConstructionRole, embedding_response(), EmbedRole, gemini_v2_does_not_renormalize_server_output(), gemini_v2_uses_prompt_prefix_and_snake_case_output_dimensionality(), GeminiContent, GeminiEmbedding (+10 more)

### Community 79 - "LLM Span Instrumentation"
Cohesion: 0.12
Nodes (17): calculate_cost(), calculate_cost_with_cached(), cost_calculation_correct(), has_meaningful_output(), llm_span_name(), LLMSpanAttributes, LLMSpanRecorder, metadata_f64() (+9 more)

### Community 80 - "Live Cache Audit Tests"
Cohesion: 0.13
Nodes (22): AuditedProvider, available_live_cache_provider_configs(), CacheTurnAudit, CacheTurnPlan, create_session(), full_request_payload(), is_query_rewrite_request(), is_repo_root() (+14 more)

### Community 81 - "Eval Score Card"
Cohesion: 0.14
Nodes (22): CacheScores, ContextScores, CostScores, float_number(), FunctionalScores, LatencyScores, lineage_score_value(), MemoryScores (+14 more)

### Community 82 - "Skill Lessons & Render"
Cohesion: 0.08
Nodes (10): insert_addendum(), learn_lesson(), lesson_name(), LessonContext, set_app_role(), load_addenda(), render(), set_app_role() (+2 more)

### Community 83 - "MCP Credential Proxy"
Cohesion: 0.12
Nodes (11): credential_from_env(), default_scope_for(), env_var(), environment_vault_loads_from_env_backed_server_config(), EnvironmentCredentialVault, headers_from_credential(), MCPCredentialProxy, McpSessionToken (+3 more)

### Community 84 - "Cron Job Object"
Cohesion: 0.13
Nodes (18): compute_next_fire(), compute_next_fire_at(), computes_next_top_of_hour_in_utc(), CronJob, CronJobConfig, CronJobImpl, CronJobState, CronJobStatus (+10 more)

### Community 85 - "Local Tools Integration Tests"
Cohesion: 0.13
Nodes (25): approval_prompt_str_replace_diff_is_surgical(), approval_prompt_uses_remembered_workspace_root_for_commands(), bash_captures_stdout_and_stderr(), bash_error_output_is_not_truncated(), bash_respects_timeout(), bash_success_output_is_truncated_to_router_budget(), docker_bash_hard_cancel_stops_container_exec(), docker_file_tools_roundtrip_inside_container_workspace() (+17 more)

### Community 86 - "Citation Verifiers"
Cohesion: 0.13
Nodes (11): CascadeConfig, CascadeVerifier, sentence_for(), Bm25Verifier, CitationVerifier, contradiction_score(), NliVerifier, score_bm25() (+3 more)

### Community 87 - "Query Rewrite Postprocess"
Cohesion: 0.11
Nodes (17): query_rewrite_response_format(), QueryRewriter, allowed_terms(), cleanup_stripped_text(), filter_suggested_tools(), parse_rewrite_response(), parse_task_kind(), RawQueryRewriteResult (+9 more)

### Community 88 - "Embedding Providers"
Cohesion: 0.12
Nodes (13): add_feature(), build_embedding_provider_from_config(), char_trigrams(), EmbeddingProvider, mock_embedding_is_deterministic(), MockEmbedding, normalize(), OpenAIEmbedding (+5 more)

### Community 89 - "pgvector Store"
Cohesion: 0.15
Nodes (12): basis_vector(), cross_tenant_knn_cannot_see_other_workspace_vectors(), delete_items(), delete_node_index_rows(), ensure_default_workspace_embedder(), guard_workspace_embedder(), insert_node_index_rows(), pgvector_round_trip_returns_identical_seed_first() (+4 more)

### Community 90 - "Session Blob Store"
Cohesion: 0.18
Nodes (12): claim_check_from_value(), collect_blob_refs(), collect_large_strings(), decode_event_from_storage(), encode_event_for_storage(), expand_local_path(), file_blob_store_deletes_session_directory(), file_blob_store_is_content_addressed() (+4 more)

### Community 91 - "Session Store Inner Impl"
Cohesion: 0.08
Nodes (2): owner_tuple_subject(), SessionStoreImpl

### Community 92 - "File Search Tool"
Cohesion: 0.13
Nodes (16): build_file_search_output(), collect_matches(), default_skipped_dirs(), default_skipped_dirs_includes_polyglot_ecosystem_directories(), execute(), execute_docker(), execute_respects_custom_skip_directories(), execute_skips_python_virtualenv_matches() (+8 more)

### Community 93 - "Local Hand Provider"
Cohesion: 0.16
Nodes (3): detect_docker(), docker_status(), LocalHandProvider

### Community 94 - "E2B Hand Provider"
Cohesion: 0.16
Nodes (1): E2BHandProvider

### Community 95 - "Model Capabilities"
Cohesion: 0.1
Nodes (6): Credential, ModelCapabilities, ModelCapabilitiesBuilder, ProviderNativeTool, TokenPricing, ToolCallFormat

### Community 96 - "Scripted Provider"
Cohesion: 0.13
Nodes (3): ScriptedBlock, ScriptedProvider, ScriptedResponse

### Community 97 - "Session Store Handlers"
Cohesion: 0.09
Nodes (1): SessionStoreImpl

### Community 98 - "Cache Fingerprinting"
Cohesion: 0.18
Nodes (15): CacheReport, fingerprint_json(), full_request_fingerprint(), generate_trace_tags(), normalize_environment(), sanitize_langfuse_id(), stable_prefix_byte_len(), stable_prefix_fingerprint() (+7 more)

### Community 99 - "Cache Control Markers Test"
Cohesion: 0.2
Nodes (17): anthropic_body(), anthropic_request_byte_layout_changes_only_in_messages_segment_when_only_messages_change(), anthropic_request_byte_layout_is_identical_across_two_consecutive_turn_compilations(), anthropic_request_with_4_segment_pipeline_places_cache_markers_at_each_boundary(), anthropic_request_with_explicit_1h_ttl_includes_ttl_field_on_each_marker(), anthropic_request_with_long_messages_keeps_cache_markers_at_segment_boundaries_not_message_boundaries(), anthropic_request_with_no_tools_omits_tools_segment_marker(), assert_cache_ttls() (+9 more)

### Community 100 - "Conversation Compaction"
Cohesion: 0.15
Nodes (14): calculate_cost_cents(), CheckpointState, compaction_request(), compaction_triggers_even_when_incremental_snapshot_is_current(), event_summary_line(), latest_checkpoint_state(), maybe_compact_events(), non_checkpoint_events() (+6 more)

### Community 101 - "Runtime SQL Helpers"
Cohesion: 0.11
Nodes (6): expand_local_path(), SessionPreview, SessionRuntimeEvent, workspace_id_for_root(), workspace_id_for_root_falls_back_when_basename_is_missing(), workspace_id_for_root_uses_sanitized_directory_name()

### Community 102 - "OpenAI Privacy Filter"
Cohesion: 0.14
Nodes (6): normalize_base_url(), OpenAiPrivacyFilterClassifier, PrivacyFilterThresholds, resolve_class(), ServiceResponse, ServiceSpan

### Community 103 - "Vector Backend Selection"
Cohesion: 0.16
Nodes (10): build_backend(), hipaa_tier_requires_baa_enabled_turbopuffer_client(), live_fga_client(), pg_store(), RemoteTarget, resolve_backend_choice(), SessionTarget, tp_store() (+2 more)

### Community 104 - "Postgres Session Store"
Cohesion: 0.19
Nodes (1): PostgresSessionStore

### Community 105 - "Grep Tool"
Cohesion: 0.17
Nodes (18): build_grep_output(), collect_context(), ContextLine, execute(), grep_finds_matching_lines(), grep_includes_context_lines(), grep_respects_gitignore(), grep_respects_skip_directories() (+10 more)

### Community 106 - "Tool Router Construction"
Cohesion: 0.13
Nodes (2): default_cloud_provider(), ToolRouter

### Community 107 - "Authz Tuple Schema"
Cohesion: 0.13
Nodes (8): idempotency_key_is_deterministic_and_includes_model_version(), ObjectType, Relation, tuple_wire_format_user_to_workspace(), TupleKey, TupleKeyWire, TupleOp, UserType

### Community 108 - "Postgres Session Store Tests"
Cohesion: 0.25
Nodes (14): cleanup_schema(), create_test_store(), learning_log_round_trips_skill_entry_and_rollback_invalidates_batch(), postgres_event_payloads_round_trip_as_jsonb(), postgres_materialized_analytics_views_refresh(), postgres_session_ids_are_native_uuid_and_concurrent_emits_are_serialized(), postgres_session_summary_tracks_model_tier_costs(), postgres_shared_session_store_contract() (+6 more)

### Community 109 - "Concurrent Graph Writers"
Cohesion: 0.34
Nodes (17): active_nodes_named(), assert_changelog_forms_dag(), changelog_edges(), changelog_version(), ChangelogEdge, concurrent_supersede_with_contradicting_facts_chooses_one_deterministically(), concurrent_supersedes_of_same_node_serialize_with_monotonic_changelog_versions(), concurrent_writes_to_different_nodes_in_same_workspace_do_not_interfere() (+9 more)

### Community 110 - "Live Provider Matrix"
Cohesion: 0.25
Nodes (13): available_live_providers(), complete_until(), google_live_model(), live_providers_answer_simple_prompt_across_available_keys(), live_providers_can_use_native_web_search_across_available_keys(), live_providers_emit_tool_calls_across_available_keys(), live_providers_obey_system_prompt_across_available_keys(), live_providers_preserve_unicode_across_available_keys() (+5 more)

### Community 111 - "CLI Exec Command"
Cohesion: 0.15
Nodes (9): exec_mode_formats_tool_updates_compactly(), format_tool_update(), handle_exec_event(), InterruptState, is_terminal_session_status(), parse_approval_decision(), resolve_exec_approval(), run_exec() (+1 more)

### Community 112 - "OpenAI Provider Tests"
Cohesion: 0.22
Nodes (15): openai_provider_does_not_retry_after_partial_stream_output(), openai_provider_drops_oversized_metadata_values(), openai_provider_includes_native_web_search_when_enabled(), openai_provider_omits_native_web_search_when_disabled(), openai_provider_retries_after_rate_limit(), openai_provider_serializes_assistant_tool_calls_as_function_call_items(), openai_provider_serializes_tool_result_messages_as_function_call_output(), openai_provider_streams_parallel_tool_calls_in_order() (+7 more)

### Community 113 - "Security Policies"
Cohesion: 0.23
Nodes (11): ApprovalRuleStore, glob_match(), parse_and_match_bash(), persistent_rule_matching_uses_glob_patterns(), PolicyCheck, read_tools_are_auto_approved_and_bash_requires_approval(), rule_matches(), rule_visible_to_workspace() (+3 more)

### Community 114 - "Long Conversation Foundation Tests"
Cohesion: 0.18
Nodes (11): budgets_evaluate_reports_each_violation_with_metric_name_and_actual_value(), long_test_case_dispatches_to_run_scenario_with_provider(), recorded_provider_handles_compaction_requests_without_advancing_transcript_cursor(), recorded_provider_replays_two_turn_transcript_byte_for_byte(), recorded_provider_returns_typed_error_on_transcript_exhaustion(), recorded_provider_with_strict_matching_rejects_user_message_drift(), RecordingSink, score_card() (+3 more)

### Community 115 - "Retrieval Load Scenario"
Cohesion: 0.2
Nodes (13): build_query_mix(), build_repeated_pool(), canonical_query(), drive_load(), hydrate_queries(), LoadReport, novel_query(), paraphrase() (+5 more)

### Community 116 - "Client Smoke Tests"
Cohesion: 0.21
Nodes (9): clear_endpoint_env(), env_guard(), from_env_defaults_to_compose_ingress_when_unset(), from_env_reads_endpoint(), from_env_reads_restate_ingress_fallback(), grant_session_participant(), grant_tuple(), grant_workspace_member() (+1 more)

### Community 117 - "Tool Output Budget"
Cohesion: 0.24
Nodes (9): append_footer(), artifact_storage_footer(), count_lines(), estimate_tokens(), format_artifact_summary(), inline_artifact_preview_budget(), ToolRouter, truncate_text_for_budget() (+1 more)

### Community 118 - "Mock Smoke Loadtest"
Cohesion: 0.2
Nodes (12): enforce_gates(), error_rate(), mock_short_profile_completes_within_budget_with_zero_errors(), MockSmokeConfig, print_summary_table(), render_prometheus(), repo_root(), run_mock_smoke_gate() (+4 more)

### Community 119 - "AGE Graph Read"
Cohesion: 0.16
Nodes (5): AgeGraphStore, fetch_node(), fetch_nodes(), fetch_nodes_by_uid(), parse_agtype_uuid()

### Community 120 - "Graph Node Types"
Cohesion: 0.17
Nodes (6): decode_node_label(), decode_pii_class(), NodeIndexRow, NodeLabel, NodeWriteIntent, PiiClass

### Community 121 - "Platform Message Types"
Cohesion: 0.14
Nodes (12): ActionButton, Attachment, ButtonStyle, ChannelRef, DiffHunk, InboundMessage, MessageContent, OutboundMessage (+4 more)

### Community 122 - "Auth0 & OIDC Providers"
Cohesion: 0.18
Nodes (6): Auth0AuthProvider, Auth0Claims, parse_identity_type(), resolve_or_provision_static(), GenericClaims, OidcAuthProvider

### Community 123 - "Session Search Tool"
Cohesion: 0.16
Nodes (6): event_snippet(), render_results(), SessionSearchEventType, SessionSearchInput, SessionSearchTool, truncate()

### Community 124 - "Eval Replay & Datasets"
Cohesion: 0.21
Nodes (12): DatasetItem, JsonlDatasetItem, load_dataset_items(), normalized_tokens(), parse_jsonl_items(), register_dataset(), replay_dataset(), replay_dataset_live() (+4 more)

### Community 125 - "Cross-Tenant Isolation Loadtest"
Cohesion: 0.21
Nodes (11): app_scoped_conn(), attack_changelog_leak(), attack_cte_leak(), attack_dlq_leak(), attack_vector_oracle(), first_dlq(), first_embedding(), LeakReport (+3 more)

### Community 126 - "Platform Edit Window"
Cohesion: 0.25
Nodes (11): edit_fallback_preserves_message_content_byte_for_byte(), edit_request(), edit_with_followup_fallback(), followup_request(), GatewayEditOutcome, GatewayEditResponse, is_fallback_edit_error(), post_json() (+3 more)

### Community 127 - "Segment Store"
Cohesion: 0.16
Nodes (1): PostgresSessionStore

### Community 128 - "Provider Retry Policy"
Cohesion: 0.27
Nodes (6): parse_retry_after(), response_text(), retries_on_rate_limit(), retry_after_delay(), retry_after_delay_from_message(), RetryPolicy

### Community 129 - "Resolution Scorer"
Cohesion: 0.25
Nodes (11): all_tools_failed_overrides_to_failed_with_high_confidence(), cancellation_overrides_to_abandoned(), label_confidence(), label_for_score(), null_signals_are_excluded_and_weights_renormalized(), resolution_score(), ResolutionOverride, ResolutionScorer (+3 more)

### Community 130 - "Loadtest Reporting"
Cohesion: 0.19
Nodes (7): enforce_gates(), histogram_math_percentile_is_monotonic_and_within_bucket(), histogram_percentile(), label_value(), prom_histogram_p95_ms(), prometheus_histogram_percentile(), write_stderr()

### Community 131 - "Auth0 CIBA Authz"
Cohesion: 0.22
Nodes (4): Auth0AsyncAuthzProvider, binding_message(), CibaPoller, PollOutcome

### Community 132 - "OpenAI Responses Envelope Tests"
Cohesion: 0.35
Nodes (11): base_request(), openai_body(), openai_responses_request_serializes_with_separate_instructions_and_input_fields(), openai_responses_request_with_function_tools_includes_strict_mode_when_configured(), openai_responses_request_with_previous_response_id_chains_state_correctly(), openai_responses_request_with_tool_choice_required_serializes_correctly(), openai_responses_streaming_response_chunks_parse_into_provider_events(), provider_event_from_content() (+3 more)

### Community 133 - "Gemini Provider"
Cohesion: 0.26
Nodes (1): GeminiProvider

### Community 134 - "Query Planner Tests"
Cohesion: 0.15
Nodes (1): SeedGraph

### Community 135 - "Long Conversation Budgets"
Cohesion: 0.23
Nodes (9): BudgetResult, Budgets, BudgetViolation, check_bool(), check_max_u32(), check_max_u64(), check_min_f64(), check_min_u32() (+1 more)

### Community 136 - "AGE Graph Store"
Cohesion: 0.18
Nodes (1): AgeGraphStore

### Community 137 - "Sandbox Config"
Cohesion: 0.18
Nodes (7): CloudConfig, CloudFlyioConfig, CloudHandsConfig, LocalConfig, McpCredentialConfig, McpServerConfig, McpTransportConfig

### Community 138 - "Privacy Erase Command"
Cohesion: 0.29
Nodes (11): Args, begin_app_scoped_tx(), emit_erase_summary(), enumerate_erase_candidates(), erase_audit_metadata(), erase_graph_store(), EraseCandidate, EraseContext (+3 more)

### Community 139 - "Anthropic Provider"
Cohesion: 0.26
Nodes (1): AnthropicProvider

### Community 140 - "Segment Scoring"
Cohesion: 0.38
Nodes (11): first_user_message(), last_brain_response(), latest_user_message(), load_segment_baseline(), load_session_events(), query_rewrite_from_metadata(), record_resolution_learning(), score_active_segment() (+3 more)

### Community 141 - "Tool Dispatch"
Cohesion: 0.3
Nodes (1): ToolRouter

### Community 142 - "Tool Recovery Handler"
Cohesion: 0.33
Nodes (4): HandFailureContext, is_gateway_unavailable_error(), is_timeout_error(), ToolRouter

### Community 143 - "Continuation Signal"
Cohesion: 0.23
Nodes (6): ContinuationInput, is_acknowledgment(), is_correction(), lexical_cosine_similarity(), score(), token_counts()

### Community 144 - "Eval Terminal Reporter"
Cohesion: 0.3
Nodes (5): format_scores(), render_includes_case_names_and_summary(), render_verbose_case(), result_index(), TerminalReporter

### Community 145 - "Loadtest Stack"
Cohesion: 0.26
Nodes (5): embed_texts(), fact_text(), Stack, WorkspaceFixture, WorkspaceRetriever

### Community 146 - "Contradiction Detection Tests"
Cohesion: 0.33
Nodes (9): candidate(), candidate_with(), contradiction_detector_does_not_flag_self_referential_fact_repetition(), contradiction_detector_does_not_flag_two_facts_with_different_predicates(), contradiction_detector_does_not_flag_two_facts_with_different_subjects(), contradiction_detector_flags_two_facts_with_same_subject_predicate_different_object(), contradiction_detector_handles_temporal_facts_correctly_when_ranges_overlap(), contradiction_detector_handles_temporal_facts_correctly_when_ranges_overlap_partially() (+1 more)

### Community 147 - "DSAR Audit Export Tests"
Cohesion: 0.45
Nodes (10): dsar_export_excludes_records_belonging_to_other_users(), dsar_export_for_unknown_user_writes_empty_file_with_zero_lines(), dsar_export_for_user_with_5_records_writes_5_jsonl_lines_with_correct_schema(), dsar_export_includes_redaction_for_phi_class_fields_when_redaction_enabled(), dsar_export_signed_manifest_accompanies_jsonl_with_valid_signature(), dsar_export_with_concurrent_writes_to_audit_log_produces_consistent_snapshot(), exporter(), fixed_options() (+2 more)

### Community 148 - "Sub-Agent Types"
Cohesion: 0.18
Nodes (6): DispatchSubAgentInput, SubAgentChildRef, SubAgentMessage, SubAgentResult, SubAgentState, SubAgentStatus

### Community 149 - "Turn Loop Detector"
Cohesion: 0.33
Nodes (6): loop_detector_disabled_at_zero_threshold(), loop_detector_does_not_trigger_on_varied_calls(), loop_detector_resets(), loop_detector_sliding_window(), loop_detector_triggers_after_threshold(), LoopDetector

### Community 150 - "Provider Config"
Cohesion: 0.24
Nodes (4): GeneralConfig, ModelsConfig, ProviderCredentialConfig, ProvidersConfig

### Community 151 - "Gemini Safety Settings Tests"
Cohesion: 0.47
Nodes (7): base_request(), gemini_body(), gemini_request_includes_default_safety_settings_for_4_categories(), gemini_request_with_custom_safety_threshold_overrides_default(), gemini_request_with_function_declarations_serializes_correctly(), gemini_request_with_json_response_mime_type_includes_field(), snapshot_json()

### Community 152 - "Tool Approval Store"
Cohesion: 0.22
Nodes (6): PreparedToolApproval, PrepareToolApprovalRequest, StoreApprovalRuleRequest, to_handler_error(), WorkspaceStore, WorkspaceStoreImpl

### Community 153 - "Tool Lifecycle Manager"
Cohesion: 0.29
Nodes (4): hand_id(), sandbox_tier_label(), session_provider_key(), ToolRouter

### Community 154 - "Instruction Stage"
Cohesion: 0.38
Nodes (4): combine_workspace_instructions(), instruction_processor_appends_config_backed_sections(), instruction_processor_combines_config_and_discovered_workspace_instructions(), InstructionProcessor

### Community 155 - "Pipeline Triggers"
Cohesion: 0.24
Nodes (5): approximate_query_tokens(), QueryRewriter, should_apply_tier2(), starts_with_tool_like_verb(), token_count()

### Community 156 - "Auth0 Live Tests"
Cohesion: 0.44
Nodes (8): auth0_authenticate_valid_token_returns_identity(), auth0_expired_token_returns_expired(), auth0_wrong_audience_returns_rejected(), live_config(), live_valid_env(), LiveConfig, LiveEnv, required_env()

### Community 157 - "Bash Tool"
Cohesion: 0.39
Nodes (6): bash_output_preserves_full_process_streams(), bash_output_small_streams_are_not_truncated(), BashToolInput, build_bash_output(), execute_docker(), execute_local()

### Community 158 - "Rewrite Input Builder"
Cohesion: 0.31
Nodes (5): input_from_context_messages(), input_from_conversation(), input_from_event_records(), QueryRewriter, RewriteInput

### Community 159 - "Rewrite Circuit Breaker"
Cohesion: 0.47
Nodes (2): CircuitBreaker, now_epoch_millis()

### Community 160 - "Moa Config Loader"
Cohesion: 0.46
Nodes (1): MoaConfig

### Community 161 - "Provider Request Body Snapshots"
Cohesion: 0.43
Nodes (7): anthropic_request_body__minimal_request_serializes_with_stable_byte_layout(), file_read_tool(), gemini_request_body__minimal_request_serializes_with_stable_byte_layout(), minimal_request(), openai_request_body__minimal_request_serializes_with_stable_byte_layout(), shell_command_tool(), tool_schema()

### Community 162 - "Session VO Tests"
Cohesion: 0.5
Nodes (6): session_vo_destroy_clears_state(), session_vo_post_message_queues_in_state(), session_vo_post_message_updates_status_to_running_then_idle_parks_paused(), session_vo_post_message_without_meta_errors(), test_message(), test_meta()

### Community 163 - "Health Service"
Cohesion: 0.29
Nodes (3): Health, HealthImpl, VersionInfo

### Community 164 - "Self Assessment Signal"
Cohesion: 0.29
Nodes (2): contains_any(), score()

### Community 165 - "Structural Signal"
Cohesion: 0.32
Nodes (5): baseline(), cold_start_returns_none(), is_high_outlier(), score(), SegmentMetrics

### Community 166 - "Lexical Seed Store"
Cohesion: 0.43
Nodes (2): LexicalStore, lookup_seed_rows()

### Community 167 - "Lineage Fjall Journal"
Cohesion: 0.29
Nodes (1): Journal

### Community 168 - "Null Token Vault"
Cohesion: 0.29
Nodes (1): NullTokenVaultProvider

### Community 169 - "Query Rewrite Live Tests"
Cohesion: 0.38
Nodes (2): CapturingProvider, live_query_rewriter_resolves_coreference_without_new_entities()

### Community 170 - "Output Match Evaluator"
Cohesion: 0.43
Nodes (4): contains_rules_pass_when_all_terms_match(), evaluate_output(), missing_contains_term_reduces_score(), OutputMatchEvaluator

### Community 171 - "Live E2E Fixtures"
Cohesion: 0.43
Nodes (7): LIVE-E2E-ANTHROPIC Fixture, LIVE-E2E-GOOGLE Fixture, Live End-to-End Test Marker, LIVE-E2E-OPENAI Fixture, Anthropic Provider, Google Provider, OpenAI Provider

### Community 172 - "Pgaudit Smoke Tests"
Cohesion: 0.6
Nodes (5): audit_writes_log_line(), pgaudit_migration_configures_labels_when_provider_loaded_and_auditor_view(), pgaudit_smoke_requested(), quote_identifier(), test_database_url()

### Community 173 - "OpenAI Streaming Tests"
Cohesion: 0.33
Nodes (0): 

### Community 174 - "Graph Memory Maintenance"
Cohesion: 0.33
Nodes (4): CompactReport, CompactRequest, GraphMemoryMaint, GraphMemoryMaintImpl

### Community 175 - "Skill Activation"
Cohesion: 0.4
Nodes (3): extract_query_keywords_from_events(), skill_resolution_rate_map(), SkillInjector

### Community 176 - "Trajectory Match Evaluator"
Cohesion: 0.47
Nodes (3): lcs_len(), partial_match_scores_below_one(), TrajectoryMatchEvaluator

### Community 177 - "Threshold Evaluator"
Cohesion: 0.53
Nodes (3): cost_over_budget_fails_boolean_score(), limit_score(), ThresholdEvaluator

### Community 178 - "Perf Gate Binary"
Cohesion: 0.47
Nodes (3): Args, main(), Profile

### Community 179 - "PII Classifier Smoke"
Cohesion: 0.5
Nodes (2): classify_smoke_maps_ssn_to_phi_and_clean_text_to_none(), spawn_test_service()

### Community 180 - "Shell Chain Splitter"
Cohesion: 0.5
Nodes (2): push_sub_command(), split_shell_chain()

### Community 181 - "Scoped Transaction Lifecycle"
Cohesion: 0.5
Nodes (1): ScopedConn<'p>

### Community 182 - "Unified Diff"
Cohesion: 0.6
Nodes (3): compute_unified_diff(), small_edit_diff_is_substantially_smaller_than_full_file(), unified_diff_contains_standard_headers_and_hunks()

### Community 183 - "Disabled Auth Provider"
Cohesion: 0.4
Nodes (1): DisabledAuthProvider

### Community 184 - "Authz Outbox Tests"
Cohesion: 0.7
Nodes (4): outbox_basic_enqueue_is_idempotent_on_same_key(), outbox_basic_enqueue_separates_write_and_delete(), outbox_basic_failed_row_moves_to_dead_letter_at_max_attempts(), test_pool()

### Community 185 - "Auth0 Vault Live Tests"
Cohesion: 0.8
Nodes (4): live_auth0_token_vault_returns_third_party_token(), required_env(), required_uuid(), upsert_linked_user()

### Community 186 - "Anthropic Provider Tests"
Cohesion: 0.4
Nodes (0): 

### Community 187 - "Fake Clock Test Helper"
Cohesion: 0.4
Nodes (1): FakeClock

### Community 188 - "Neon Branch Maintenance"
Cohesion: 0.4
Nodes (3): NeonMaint, NeonMaintImpl, PruneReport

### Community 189 - "Client Runtime Tests"
Cohesion: 0.7
Nodes (4): from_endpoint_creates_initial_session_and_caches_tool_names(), mock_runtime_bootstrap(), run_turn_queues_message_and_relays_completed_outcome_as_runtime_events(), tool_descriptor_body()

### Community 190 - "Cohere Live Test"
Cohesion: 0.83
Nodes (3): cohere_embed_v4_returns_1024_dimensional_float_embeddings(), live_cohere_key(), live_cohere_requested()

### Community 191 - "Turbopuffer Live Tests"
Cohesion: 0.83
Nodes (3): basis_vector(), live_store(), turbopuffer_live_round_trip()

### Community 192 - "DB Error Mapping"
Cohesion: 0.5
Nodes (1): ScopedConn

### Community 193 - "FGA Bootstrap Live Test"
Cohesion: 0.83
Nodes (3): bootstrap_is_idempotent_across_two_runs(), default_preshared_key_if_unset(), grep_env()

### Community 194 - "Authz Poller Live Test"
Cohesion: 0.83
Nodes (3): fga_from_env(), poller_drains_write_to_fga(), test_pool()

### Community 195 - "JWT Validation Test"
Cohesion: 0.67
Nodes (3): Claims, jwt_validation_accepts_self_signed_auth0_token(), signed_token()

### Community 196 - "Postgres Session Store"
Cohesion: 0.5
Nodes (1): PostgresSessionStore

### Community 197 - "Approval Rule Store"
Cohesion: 0.5
Nodes (1): PostgresSessionStore

### Community 198 - "Cohere Reranker Live Test"
Cohesion: 0.83
Nodes (3): cohere_rerank_v4_fast_prioritizes_relevant_retrieval_candidate(), live_cohere_key(), live_cohere_requested()

### Community 199 - "History Compaction Compile"
Cohesion: 0.83
Nodes (1): HistoryCompiler

### Community 200 - "Cost Budget Enforcement"
Cohesion: 0.67
Nodes (2): enforce_workspace_budget(), format_budget_exhausted_message()

### Community 201 - "Eval Live Tests"
Cohesion: 0.83
Nodes (3): live_model(), live_run_single_produces_eval_result(), test_database_url()

### Community 202 - "Tool Success Evaluator"
Cohesion: 0.5
Nodes (1): ToolSuccessEvaluator

### Community 203 - "PII Live Sidecar Test"
Cohesion: 1.0
Nodes (2): live_service_url(), live_sidecar_classifies_private_and_clean_text()

### Community 204 - "Mock PII Classifier"
Cohesion: 0.67
Nodes (1): MockClassifier

### Community 205 - "Permissions Config"
Cohesion: 0.67
Nodes (1): PermissionsConfig

### Community 206 - "Gateway Config"
Cohesion: 0.67
Nodes (1): GatewayConfig

### Community 207 - "Local Orchestrator Stub"
Cohesion: 0.67
Nodes (1): OrchestratorConfig

### Community 208 - "Builtin Authz Approval Test"
Cohesion: 0.67
Nodes (0): 

### Community 209 - "Session Store Helpers"
Cohesion: 0.67
Nodes (1): PostgresSessionStore

### Community 210 - "Chat Harness Example"
Cohesion: 1.0
Nodes (2): main(), run_prompt()

### Community 211 - "Vector Error Mapping"
Cohesion: 1.0
Nodes (1): moa_core::MoaError

### Community 212 - "Identity Headers Test"
Cohesion: 1.0
Nodes (0): 

### Community 213 - "Cold Tier Partition Tests"
Cohesion: 1.0
Nodes (0): 

### Community 214 - "Memory Scope Test"
Cohesion: 1.0
Nodes (0): 

### Community 215 - "Model Identifier"
Cohesion: 1.0
Nodes (1): ModelId

### Community 216 - "Tool Call Identifier"
Cohesion: 1.0
Nodes (1): ToolCallId

### Community 217 - "Audit Security Config"
Cohesion: 1.0
Nodes (1): AuditSecurityConfig

### Community 218 - "OCSF Emit Test"
Cohesion: 1.0
Nodes (0): 

### Community 219 - "Serialized Test Guard"
Cohesion: 1.0
Nodes (1): SerializedTest<'a>

### Community 220 - "Shell Chaining Security Test"
Cohesion: 1.0
Nodes (0): 

### Community 221 - "Lineage Postgres Tests"
Cohesion: 1.0
Nodes (0): 

### Community 222 - "Restate Context Headers"
Cohesion: 1.0
Nodes (1): restate_sdk::context::Context<'_>

### Community 223 - "Restate Object Context Headers"
Cohesion: 1.0
Nodes (1): restate_sdk::context::ObjectContext<'_>

### Community 224 - "Restate Shared Context Headers"
Cohesion: 1.0
Nodes (1): restate_sdk::context::SharedObjectContext<'_>

### Community 225 - "Restate Workflow Context Headers"
Cohesion: 1.0
Nodes (1): restate_sdk::context::WorkflowContext<'_>

### Community 226 - "Object Context"
Cohesion: 1.0
Nodes (1): ObjectContext<'a>

### Community 227 - "Shared Object Context"
Cohesion: 1.0
Nodes (1): SharedObjectContext<'a>

### Community 228 - "Docker Hardening Test"
Cohesion: 1.0
Nodes (0): 

### Community 229 - "History Budgeting"
Cohesion: 1.0
Nodes (0): 

### Community 230 - "History Error Preservation"
Cohesion: 1.0
Nodes (0): 

### Community 231 - "Skills Cache Break"
Cohesion: 1.0
Nodes (0): 

### Community 232 - "Workspace Hack Build"
Cohesion: 1.0
Nodes (0): 

### Community 233 - "Skills Bootstrap Script"
Cohesion: 1.0
Nodes (0): 

### Community 234 - "Core Type Macros"
Cohesion: 1.0
Nodes (0): 

### Community 235 - "Integration Test Entry"
Cohesion: 1.0
Nodes (0): 

### Community 236 - "Restate Registration"
Cohesion: 1.0
Nodes (0): 

## Knowledge Gaps
- **846 isolated node(s):** `PiiSpan`, `PiiClassifier`, `PiiError`, `ExpectedFacts`, `ExpectedFact` (+841 more)
  These have ≤1 connection - possible missing edges or undocumented components.
- **Thin community `Vector Error Mapping`** (2 nodes): `moa_core::MoaError`, `.from()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Identity Headers Test`** (2 nodes): `identity_headers.rs`, `with_identity_attaches_all_identity_headers()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Cold Tier Partition Tests`** (2 nodes): `partition_layout.rs`, `partition_key_uses_workspace_and_day_layout()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Memory Scope Test`** (2 nodes): `scope.rs`, `memory_scope_ancestors_and_serialization_cover_all_scope_tiers()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Model Identifier`** (2 nodes): `ModelId`, `.default()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Tool Call Identifier`** (2 nodes): `ToolCallId`, `.from()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Audit Security Config`** (2 nodes): `audit_security.rs`, `AuditSecurityConfig`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `OCSF Emit Test`** (2 nodes): `emit_authn_success.rs`, `emit_authn_success_inserts_signed_security_event()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Serialized Test Guard`** (2 nodes): `SerializedTest<'a>`, `.deref()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Shell Chaining Security Test`** (2 nodes): `shell_chaining_does_not_match_simple_pattern.rs`, `shell_chain_after_matching_prefix_does_not_satisfy_simple_glob()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Lineage Postgres Tests`** (2 nodes): `lineage_postgres.rs`, `postgres_lineage_sink_writes_rows()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Restate Context Headers`** (2 nodes): `restate_sdk::context::Context<'_>`, `.request_headers()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Restate Object Context Headers`** (2 nodes): `restate_sdk::context::ObjectContext<'_>`, `.request_headers()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Restate Shared Context Headers`** (2 nodes): `restate_sdk::context::SharedObjectContext<'_>`, `.request_headers()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Restate Workflow Context Headers`** (2 nodes): `restate_sdk::context::WorkflowContext<'_>`, `.request_headers()`
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

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **What connects `PiiSpan`, `PiiClassifier`, `PiiError` to the rest of the system?**
  _846 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `Runtime Events & SSE` be split into smaller, more focused modules?**
  _Cohesion score 0.01 - nodes in this community are weakly interconnected._
- **Should `PII Redaction Core` be split into smaller, more focused modules?**
  _Cohesion score 0.01 - nodes in this community are weakly interconnected._
- **Should `CLI Entry Points` be split into smaller, more focused modules?**
  _Cohesion score 0.01 - nodes in this community are weakly interconnected._
- **Should `Postgres RLS & Audit Tests` be split into smaller, more focused modules?**
  _Cohesion score 0.02 - nodes in this community are weakly interconnected._
- **Should `CLI Command Tests` be split into smaller, more focused modules?**
  _Cohesion score 0.02 - nodes in this community are weakly interconnected._
- **Should `Merkle Tree Audit` be split into smaller, more focused modules?**
  _Cohesion score 0.02 - nodes in this community are weakly interconnected._