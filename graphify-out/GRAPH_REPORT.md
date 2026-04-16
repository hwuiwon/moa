# Graph Report - .  (2026-04-16)

## Corpus Check
- Large corpus: 252 files · ~204,688 words. Semantic extraction will be expensive (many Claude tokens). Consider running on a subfolder, or use --no-semantic to run AST-only.

## Summary
- 3762 nodes · 6395 edges · 143 communities detected
- Extraction: 99% EXTRACTED · 1% INFERRED · 0% AMBIGUOUS · INFERRED: 35 edges (avg confidence: 0.8)
- Token cost: 0 input · 0 output

## God Nodes (most connected - your core abstractions)
1. `LocalChatRuntime` - 36 edges
2. `DaemonChatRuntime` - 35 edges
3. `SkillFrontmatter` - 33 edges
4. `PostgresSessionStore` - 30 edges
5. `TursoSessionStore` - 29 edges
6. `start_session()` - 29 edges
7. `LocalOrchestrator` - 27 edges
8. `session()` - 25 edges
9. `FileMemoryStore` - 22 edges
10. `wait_for_status()` - 22 edges

## Surprising Connections (you probably didn't know these)
- `Findings Driving The Fix` --rationale_for--> `moa-hands file_read tool`  [INFERRED]
  task_plan.md → findings.md
- `Large-file bounded preview` --rationale_for--> `moa-hands file_read tool`  [INFERRED]
  task_plan.md → findings.md
- `What Still Failed` --conceptually_related_to--> `Search -> scoped read -> str_replace -> verify`  [INFERRED]
  findings.md → task_plan.md
- `SoftCancel then HardCancel escalation` --rationale_for--> `moa-cli exec entrypoint`  [INFERRED]
  task_plan.md → findings.md
- `TurnCompleted terminal event` --rationale_for--> `moa-cli exec entrypoint`  [INFERRED]
  task_plan.md → findings.md

## Hyperedges (group relationships)
- **Applied live retest harness** — concept_applied_workspace, concept_call_viewset_task, concept_sessions_db, section_pass_criteria, section_post_run_validation [INFERRED 0.85]
- **Search -> scoped read -> str_replace -> verify workflow** — concept_str_replace_workflow, moa_hands_tools_file_read, moa_brain_pipeline_identity, concept_scoped_file_read [INFERRED 0.90]
- **moa exec Ctrl-C cancel lifecycle** — concept_ctrl_c_handling, concept_soft_hard_cancel, concept_turn_completed, moa_cli_exec, concept_session_lifecycle [INFERRED 0.90]
- **macOS build chain (GPUI + xcrun metal + full Xcode requirement)** — concept_gpui, concept_xcrun_metal, readme_prerequisites, readme_troubleshooting [INFERRED 0.90]
- **Density mode flow (Settings UI + density module + Spacing struct + config file)** — design_density_modes, desktop_src_density, desktop_src_panels_settings_appearance_tab, concept_spacing_struct, concept_moa_config_toml [INFERRED 0.85]
- **Theme token stack (ActiveTheme + semantic tokens + WCAG validation)** — concept_active_theme, concept_theme_tokens_rs, desktop_src_theme_tokens, concept_wcag_helpers, design_tokens_section [INFERRED 0.80]
- **Component composition system (patterns + helpers + shared primitives)** — design_component_patterns, desktop_src_components_icon_button, desktop_src_components_section_card, desktop_src_components_settings_row, desktop_src_components_nav_item [INFERRED 0.80]

## Communities

### Community 0 - "Runtime Events & SSE"
Cohesion: 0.02
Nodes (65): ServiceBridge, ServiceBridgeHandle, ServiceStatus, group_by_type(), hash_path(), MemoryList, MemoryPageSelected, type_badge() (+57 more)

### Community 1 - "File Memory Store"
Cohesion: 0.03
Nodes (55): Evaluator, build_provider_from_config(), build_provider_from_selection(), explicit_provider_prefix_overrides_inference(), infer_provider_name(), infers_anthropic_for_claude_models(), infers_google_for_gemini_models(), infers_openai_for_gpt_models() (+47 more)

### Community 2 - "Local Orchestrator Tests"
Cohesion: 0.05
Nodes (59): approval_requested_event_persists_full_prompt_details(), collect_runtime_events_until(), completed_tool_turn_destroys_cached_hand(), CurrentDirGuard, cwd_lock(), denied_tool_preserves_queued_follow_up(), DestroyTrackingHandProvider, FileWriteApprovalProvider (+51 more)

### Community 3 - "Telemetry Bootstrap"
Cohesion: 0.03
Nodes (61): EvalError, MoaError, HandHandle, HandResources, HandSpec, HandStatus, SandboxTier, discover_configs() (+53 more)

### Community 4 - "Brain Turn Tests"
Cohesion: 0.04
Nodes (31): always_allow_rule_persists_and_skips_next_approval(), canary_leaks_in_tool_input_are_detected_and_blocked(), CanaryLeakLlmProvider, CapturingTextLlmProvider, FixedPageMemoryStore, malicious_tool_results_are_wrapped_as_untrusted_content(), MaliciousToolOutputLlmProvider, MemoryIngestLoopLlmProvider (+23 more)

### Community 5 - "Memory Types & Scopes"
Cohesion: 0.03
Nodes (38): ConfidenceLevel, count_ingest_pages(), derive_source_name_from_content(), extract_search_keywords(), extract_search_query(), format_ingest_report(), infer_page_title(), infer_page_type() (+30 more)

### Community 6 - "Tool Types & Policy"
Cohesion: 0.04
Nodes (54): annotate_schema(), annotation_warns_on_low_success(), apply_tool_rankings(), cache_stability_preserves_identical_ranked_output(), collect_session_tool_observations(), compare_f64_asc(), compare_f64_desc(), compare_failure_last() (+46 more)

### Community 7 - "Skill Document Format"
Cohesion: 0.05
Nodes (38): build_distillation_prompt(), count_tool_calls(), extract_task_summary(), find_similar_skill(), maybe_distill_skill(), normalize_new_skill(), similarity_score(), tokenize() (+30 more)

### Community 8 - "Event Stream Types"
Cohesion: 0.03
Nodes (39): destroy_and_wait(), e2b_live_provider_handles_roundtrip_and_lifecycle(), e2b_live_router_lazy_provisions_reuses_and_isolates_sessions(), EmptyMemoryStore, live_config(), live_provider(), session(), wait_for_destroyed() (+31 more)

### Community 9 - "Local Orchestrator"
Cohesion: 0.05
Nodes (35): accept_user_message(), append_event(), append_pause_summary(), best_effort_resolve_pending_signal(), collect_turn_tool_summaries(), detect_docker(), detect_workspace_path(), docker_status() (+27 more)

### Community 10 - "Exec Command Flow"
Cohesion: 0.04
Nodes (45): bash_output_small_streams_are_not_truncated(), bash_output_truncates_with_head_and_tail_preserved(), BashToolInput, build_bash_output(), execute_docker(), execute_local(), truncate_shell_stream(), batcher_holds_events_until_interval_elapses() (+37 more)

### Community 11 - "Core Config"
Cohesion: 0.04
Nodes (36): budget_config_defaults_are_applied(), BudgetConfig, CloudConfig, CloudFlyioConfig, CloudHandsConfig, CloudTemporalConfig, compaction_config_defaults_are_applied(), CompactionConfig (+28 more)

### Community 12 - "Anthropic Provider"
Cohesion: 0.06
Nodes (52): annotate_cache_control(), annotate_message_cache_control(), anthropic_content_blocks(), anthropic_content_blocks_render_text_and_json_as_text_blocks(), anthropic_message(), anthropic_message_wraps_assistant_tool_calls_as_tool_use_blocks(), anthropic_message_wraps_tool_results_with_tool_use_id(), anthropic_text_block() (+44 more)

### Community 13 - "Daemon Service"
Cohesion: 0.06
Nodes (53): daemon_create_session_uses_explicit_client_scope(), daemon_health_endpoint_responds_when_cloud_enabled(), daemon_info(), daemon_lists_session_previews(), daemon_log_path(), daemon_logs(), daemon_pid_path(), daemon_ping_create_and_shutdown_roundtrip() (+45 more)

### Community 14 - "History Compilation Stage"
Cohesion: 0.07
Nodes (36): build_full_file_read_path_map(), capabilities(), compacted_view_preserves_old_errors_and_respects_budget(), compaction_triggers_at_threshold_and_keeps_full_log(), compile_records(), CompiledHistory, CompiledRecordMessage, compiler_with_recent_turns() (+28 more)

### Community 15 - "CLI Entry Point"
Cohesion: 0.05
Nodes (53): apply_config_update(), checkpoint_cleanup_report(), checkpoint_create_report(), checkpoint_list_report(), checkpoint_rollback_report(), CheckpointCommand, Cli, cloud_sync_status() (+45 more)

### Community 16 - "Postgres Session Store"
Cohesion: 0.06
Nodes (18): checkpoint_view(), event_hand_id(), normalize_event_search_query(), PostgresSessionStore, qualified_name(), approval_rule_from_row(), pending_signal_from_row(), pending_signal_type_from_db() (+10 more)

### Community 17 - "Memory Detail Panel"
Cohesion: 0.06
Nodes (24): aggregate_brain_usage(), collect_turn_costs(), count_event_type(), count_pending_approvals(), count_turns(), DetailPanel, DetailTab, estimated_context_window() (+16 more)

### Community 18 - "Turso Session Store"
Cohesion: 0.06
Nodes (17): optional_i64(), optional_text(), parse_timestamp(), platform_from_db(), session_meta_from_row(), session_status_from_db(), session_summary_from_row(), build_event_fts_query() (+9 more)

### Community 19 - "Temporal Orchestrator"
Cohesion: 0.06
Nodes (27): activity_options(), apply_approval_decision(), ApprovalDecisionActivityInput, ApprovalSignalInput, brain_turn_activity_options(), CancelMode, connect_temporal_client(), event_to_runtime_event() (+19 more)

### Community 20 - "File Read & Str Replace Tools"
Cohesion: 0.06
Nodes (43): container_path_validation_accepts_workspace_absolute_paths(), container_path_validation_rejects_absolute_paths_outside_workspace(), container_path_validation_rejects_traversal(), docker_file_read(), docker_file_search(), docker_file_write(), docker_find_args(), docker_read_args() (+35 more)

### Community 21 - "Gemini Provider"
Cohesion: 0.08
Nodes (34): build_contents(), build_request_body(), canonical_model_id(), capabilities_for_model(), consume_sse_events(), content_message(), finish_reason_to_stop_reason(), flush_pending_parts() (+26 more)

### Community 22 - "OpenAI Responses Provider"
Cohesion: 0.06
Nodes (28): canonical_model_id(), capabilities_for_model(), gpt_5_4_family_reports_expected_capabilities(), native_web_search_tools(), OpenAIProvider, build_function_tool(), build_responses_request(), consume_responses_stream_once() (+20 more)

### Community 23 - "Eval Engine & Plan"
Cohesion: 0.08
Nodes (26): CollectedExecution, collector_tracks_tool_steps_and_metrics(), estimate_cost(), TrajectoryCollector, truncate(), build_error_result(), cleanup_workspace(), dry_run_marks_results_skipped() (+18 more)

### Community 24 - "Markdown Stream Healing"
Cohesion: 0.08
Nodes (35): byte_is_word_char(), content_is_empty_or_only_markers(), count_double_asterisks_outside_code(), count_double_marker_outside_code(), count_double_underscores_outside_code(), count_single_asterisks(), count_single_backticks(), count_single_marker() (+27 more)

### Community 25 - "Turn Streaming & Approval"
Cohesion: 0.05
Nodes (27): drain_signal_queue(), handle_stream_signal(), run_streamed_turn_with_tools_mode(), append_tool_call_event(), emit_tool_output_warning(), execute_pending_tool(), execute_tool(), format_tool_output() (+19 more)

### Community 26 - "Session Replay Snapshots"
Cohesion: 0.07
Nodes (17): approval_decision_size(), approval_prompt_size(), approx_event_bytes(), counted_store_is_noop_outside_scope(), counted_store_records_get_events_within_scope(), CountedSessionStore, display_duration_ms(), event_payload_size() (+9 more)

### Community 27 - "Skill Injection Stage"
Cohesion: 0.08
Nodes (28): allowed_tools(), budget_limit_skips_expensive_tests(), distills_skill_after_tool_heavy_session(), estimate_skill_tokens(), improvement_accepted_when_scores_better(), improvement_rejected_on_regression(), ImprovementAndEvalLlm, improves_existing_skill_when_better_flow_is_found() (+20 more)

### Community 28 - "Neon Branch Manager"
Cohesion: 0.09
Nodes (25): checkpoint_branch_names_follow_moa_prefix(), checkpoint_info_from_branch(), checkpoint_label_from_name(), cleanup_expired_deletes_only_old_moa_branches(), create_checkpoint_refuses_to_exceed_capacity(), create_checkpoint_sends_expected_request_and_returns_handle(), discard_checkpoint_calls_delete_endpoint(), format_checkpoint_branch_name() (+17 more)

### Community 29 - "Memory Consolidation"
Cohesion: 0.08
Nodes (40): canonical_port_claims(), confidence_rank(), consolidation_due_for_scope(), consolidation_resolves_dates_prunes_and_refreshes_index(), ConsolidationReport, decay_confidence(), extract_port_claims(), inbound_reference_counts() (+32 more)

### Community 30 - "Gateway Message Renderer"
Cohesion: 0.09
Nodes (23): append_piece(), discord_renderer_attaches_buttons_to_last_chunk_only(), discord_renderer_uses_embed_limit_for_long_text(), DiscordRenderChunk, DiscordRenderer, render_approval_request(), render_diff(), render_tool_card() (+15 more)

### Community 31 - "Temporal Orchestrator Tests"
Cohesion: 0.11
Nodes (28): build_temporal_helper_binary(), delayed_text_stream(), last_user_message(), mock_capabilities(), spawn_temporal_helper(), temporal_helper_binary(), temporal_orchestrator_live_anthropic_smoke(), temporal_orchestrator_processes_multiple_queued_messages_fifo() (+20 more)

### Community 32 - "Desktop Design Docs"
Cohesion: 0.05
Nodes (39): gpui_component::ActiveTheme, Approval card (Y/A/N), Drag-and-drop attachment chip, Command palette, ~/.moa/config.toml, Density Over Decoration, Environment-sourced provider API keys, GPUI framework (+31 more)

### Community 33 - "Local Tool Tests"
Cohesion: 0.1
Nodes (27): approval_prompt_str_replace_diff_is_surgical(), approval_prompt_uses_remembered_workspace_root_for_commands(), bash_captures_stdout_and_stderr(), bash_respects_timeout(), EmptyMemoryStore, file_operations_reject_path_traversal(), file_read_reads_written_content(), file_search_finds_files_by_glob() (+19 more)

### Community 34 - "Local Chat Runtime"
Cohesion: 0.07
Nodes (2): ChatRuntime, LocalChatRuntime

### Community 35 - "Broadcast Lag Handling"
Cohesion: 0.08
Nodes (17): lag_counter(), lag_counter_by_channel(), record_broadcast_lag(), recv_with_lag_handling(), RecvResult, BroadcastChannel, ClaimCheck, event_stream_abort_policy_surfaces_error() (+9 more)

### Community 36 - "E2B Sandbox Provider"
Cohesion: 0.14
Nodes (16): build_url(), ConnectedSandbox, decode_stream_chunk(), default_headers(), E2BHandProvider, encode_connect_request(), encode_test_envelopes(), envd_headers() (+8 more)

### Community 37 - "Discord Adapter"
Cohesion: 0.12
Nodes (18): approval_callback_maps_to_control_message(), attachments_from_message(), context_from_component(), discord_button(), discord_create_message(), discord_create_message_includes_buttons_for_last_chunk(), discord_edit_message(), discord_embed() (+10 more)

### Community 38 - "Daemon Chat Runtime"
Cohesion: 0.07
Nodes (1): DaemonChatRuntime

### Community 39 - "Desktop App Shell"
Cohesion: 0.08
Nodes (9): MoaApp, compact_is_tighter_than_comfortable(), current(), Density, Spacing, build_default_icon(), install(), TrayHandle (+1 more)

### Community 40 - "Slack Adapter"
Cohesion: 0.13
Nodes (19): handle_interaction_event(), handle_push_event(), inbound_from_app_mention(), inbound_from_interaction_event(), inbound_from_message_event(), inbound_from_push_event(), interaction_origin(), parses_approval_callback_into_control_message() (+11 more)

### Community 41 - "Eval Agent Setup"
Cohesion: 0.12
Nodes (28): AgentEnvironment, apply_skill_overrides(), build_agent_environment(), build_agent_environment_with_provider(), build_eval_policies(), build_pipeline(), build_skill_memory_path(), build_tool_router() (+20 more)

### Community 42 - "Working Context Messages"
Cohesion: 0.09
Nodes (9): context_message_assistant_tool_call_preserves_invocation(), context_message_tool_result_preserves_text_and_blocks(), context_message_tool_still_defaults_to_text_only(), ContextMessage, estimate_text_tokens(), MessageRole, ProcessorOutput, working_context_into_request_preserves_cache_breakpoints() (+1 more)

### Community 43 - "Skill Regression Testing"
Cohesion: 0.11
Nodes (25): append_skill_regression_log(), build_generated_suite(), compare_scores(), default_skill_evaluators(), estimate_suite_cost(), estimate_tokens(), execute_skill_suite(), extract_task_input() (+17 more)

### Community 44 - "Wiki & Memory Branching"
Cohesion: 0.12
Nodes (28): append_change_manifest(), branch_dir(), branch_file_path(), branch_root(), ChangeOperation, ChangeRecord, list_branches(), merge_markdown() (+20 more)

### Community 45 - "Tool Router Policy"
Cohesion: 0.09
Nodes (16): approval_diffs_for(), approval_fields_for(), approval_pattern_chained_inner_uses_first_subcommand(), approval_pattern_for(), approval_pattern_malformed_wrapper_falls_back_to_full_input(), approval_pattern_nested_shell_not_recursed(), approval_pattern_simple_command(), approval_pattern_single_token() (+8 more)

### Community 46 - "LLM Span Instrumentation"
Cohesion: 0.12
Nodes (17): calculate_cost(), calculate_cost_with_cached(), cost_calculation_correct(), has_meaningful_output(), llm_span_name(), LLMSpanAttributes, LLMSpanRecorder, metadata_f64() (+9 more)

### Community 47 - "MCP Client Discovery"
Cohesion: 0.14
Nodes (16): flatten_call_result(), flatten_tool_result_aggregates_text_items(), header_map_from_pairs(), http_client_sends_headers_and_parses_jsonrpc(), MCPClient, McpDiscoveredTool, McpTransport, parse_jsonrpc_result() (+8 more)

### Community 48 - "MCP Credential Proxy"
Cohesion: 0.12
Nodes (11): credential_from_env(), default_scope_for(), env_var(), environment_vault_loads_from_env_backed_server_config(), EnvironmentCredentialVault, headers_from_credential(), MCPCredentialProxy, McpSessionToken (+3 more)

### Community 49 - "Daytona Sandbox Provider"
Cohesion: 0.17
Nodes (10): build_url(), DaytonaHandProvider, default_headers(), derive_toolbox_url(), expect_success(), expect_success_json(), extract_workspace_id(), http_error() (+2 more)

### Community 50 - "Command Palette & Keybindings"
Cohesion: 0.12
Nodes (9): CommandEntry, CommandPalette, default_commands(), fuzzy_score(), initial_ordering(), PaletteDismissed, PaletteHistory, rewards_consecutive() (+1 more)

### Community 51 - "Live Observability Tests"
Cohesion: 0.11
Nodes (13): FieldRecorder, global_trace_recorder(), live_observability_audit_tracks_cache_replay_and_latency(), live_orchestrator(), queue_message(), RecordedEvent, RecordedFields, RecordedSpan (+5 more)

### Community 52 - "Completion API Types"
Cohesion: 0.1
Nodes (10): completion_stream_abort_stops_completion_task(), CompletionContent, CompletionRequest, CompletionResponse, CompletionStream, ProviderToolCallMetadata, StopReason, TokenUsage (+2 more)

### Community 53 - "Telegram Adapter"
Cohesion: 0.16
Nodes (13): channel_from_chat_and_reply(), handle_callback_query(), handle_message(), inbound_from_callback_query(), inbound_from_message(), inline_keyboard(), parse_message_id(), parses_approval_callback_into_control_message() (+5 more)

### Community 54 - "Session State Types"
Cohesion: 0.09
Nodes (18): BufferedUserMessage, CheckpointHandle, CheckpointInfo, ObserveLevel, pending_signal_queue_message_round_trip(), PendingSignal, PendingSignalType, session_meta_default_builds_created_session() (+10 more)

### Community 55 - "Turn Latency Counters"
Cohesion: 0.14
Nodes (11): current_turn_root_span(), display_duration_ms(), record_turn_event_persist_duration(), record_turn_llm_call_duration(), record_turn_llm_ttft(), record_turn_pipeline_compile_duration(), record_turn_tool_dispatch_duration(), scope_turn_latency_counters() (+3 more)

### Community 56 - "CLI HTTP API Server"
Cohesion: 0.11
Nodes (14): ApiState, build_api_router(), health_endpoint_returns_ok(), runtime_event_stream(), session_stream(), session_stream_emits_gap_event_when_runtime_subscriber_lags(), session_stream_returns_not_found_when_runtime_is_unavailable(), session_stream_returns_sse_content_type() (+6 more)

### Community 57 - "Session Store Tests"
Cohesion: 0.13
Nodes (18): approval_rules_round_trip(), create_session_and_emit_events(), fts_search_finds_events(), fts_search_uses_blob_preview(), get_events_with_range_filter(), identical_large_payloads_share_one_blob(), large_payload_offloaded_to_blob_store(), list_sessions_filters_by_workspace() (+10 more)

### Community 58 - "Session Database Dispatch"
Cohesion: 0.09
Nodes (3): create_session_store(), SessionDatabase, SessionStoreDispatch

### Community 59 - "Session Blob Store"
Cohesion: 0.18
Nodes (12): claim_check_from_value(), collect_blob_refs(), collect_large_strings(), decode_event_from_storage(), encode_event_for_storage(), expand_local_path(), file_blob_store_deletes_session_directory(), file_blob_store_is_content_addressed() (+4 more)

### Community 60 - "Tool Approval Policies"
Cohesion: 0.16
Nodes (15): approval_rule(), ApprovalRuleStore, cleanup_overly_broad_shell_rules(), cleanup_overly_broad_shell_rules_removes_visible_legacy_patterns(), glob_match(), MemoryApprovalRuleStore, parse_and_match_bash(), persistent_rule_matching_uses_glob_patterns() (+7 more)

### Community 61 - "File Search Tool"
Cohesion: 0.13
Nodes (16): build_file_search_output(), collect_matches(), default_skipped_dirs(), default_skipped_dirs_includes_polyglot_ecosystem_directories(), execute(), execute_docker(), execute_respects_custom_skip_directories(), execute_skips_python_virtualenv_matches() (+8 more)

### Community 62 - "Live Cache Audit Tests"
Cohesion: 0.14
Nodes (16): AuditedProvider, CacheTurnAudit, CacheTurnPlan, create_session(), full_request_payload(), is_repo_root(), live_cache_audit_tracks_same_session_cross_session_and_model_switch(), MessageSummary (+8 more)

### Community 63 - "Approval Request Types"
Cohesion: 0.12
Nodes (18): approval_buttons(), approval_request(), ApprovalCallbackAction, ApprovalDecision, ApprovalField, ApprovalFileDiff, ApprovalPrompt, ApprovalRequest (+10 more)

### Community 64 - "Scripted Test Provider"
Cohesion: 0.14
Nodes (3): ScriptedBlock, ScriptedProvider, ScriptedResponse

### Community 65 - "Str Replace Tool"
Cohesion: 0.19
Nodes (20): build_ambiguous_match_error(), display_path(), execute(), execute_docker(), line_count(), line_number_at_offset(), match_positions(), plan_str_replace() (+12 more)

### Community 66 - "Chat Message Bubbles"
Cohesion: 0.17
Nodes (18): agent_bubble(), approval_card(), decision_button(), detail_card(), error_bubble(), render_message(), system_bubble(), thinking_bubble() (+10 more)

### Community 67 - "Grep Tool"
Cohesion: 0.17
Nodes (18): build_grep_output(), collect_context(), ContextLine, execute(), grep_finds_matching_lines(), grep_includes_context_lines(), grep_respects_gitignore(), grep_respects_skip_directories() (+10 more)

### Community 68 - "Full Text Search"
Cohesion: 0.19
Nodes (10): build_fts_query(), delete_page_entries(), FtsIndex, insert_page(), migrate(), parse_confidence(), parse_page_type(), parse_scope_key() (+2 more)

### Community 69 - "Workspace Instruction Discovery"
Cohesion: 0.14
Nodes (5): discover_workspace_instructions(), discovers_agents_md(), ignores_non_agents_instruction_files(), truncates_oversized_files(), Workspace

### Community 70 - "CallViewSet Task Plan"
Cohesion: 0.15
Nodes (19): moa exec Ctrl-C interrupt handling, Identity prompt guardrails, Large-file bounded preview, Scoped file_read with line ranges, Session pause/cancel lifecycle, SoftCancel then HardCancel escalation, Search -> scoped read -> str_replace -> verify, TurnCompleted terminal event (+11 more)

### Community 71 - "Cache Report Fingerprinting"
Cohesion: 0.2
Nodes (13): CacheReport, fingerprint_json(), full_request_fingerprint(), generate_trace_tags(), normalize_environment(), sanitize_langfuse_id(), stable_prefix_fingerprint(), tags_include_platform_and_workspace() (+5 more)

### Community 72 - "Tool Registry"
Cohesion: 0.18
Nodes (4): execute_tool_policy(), RegisteredTool, ToolExecution, ToolRegistry

### Community 73 - "Memory Maintenance Tests"
Cohesion: 0.27
Nodes (13): branch_reconciliation_merges_conflicting_writes(), consolidation_decays_confidence_once_and_is_stable_on_repeat_runs(), consolidation_normalizes_dates_and_resolves_conflicts(), ingest_source_creates_summary_and_updates_related_pages(), ingest_source_truncates_large_content(), maintenance_operations_append_log_and_keep_results_searchable(), manual_seeded_memory_fuzz_preserves_core_invariants(), manual_stress_ingest_reconcile_and_consolidate_preserves_invariants() (+5 more)

### Community 74 - "OpenAI Provider Tests"
Cohesion: 0.22
Nodes (15): openai_provider_does_not_retry_after_partial_stream_output(), openai_provider_drops_oversized_metadata_values(), openai_provider_includes_native_web_search_when_enabled(), openai_provider_omits_native_web_search_when_disabled(), openai_provider_retries_after_rate_limit(), openai_provider_serializes_assistant_tool_calls_as_function_call_items(), openai_provider_serializes_tool_result_messages_as_function_call_output(), openai_provider_streams_parallel_tool_calls_in_order() (+7 more)

### Community 75 - "Memory Bootstrap"
Cohesion: 0.24
Nodes (14): BootstrapReport, BootstrapSentinel, find_instruction_file(), index_page_with_instructions(), is_bootstrap_index(), minimal_index_page(), project_instructions_page(), run_bootstrap() (+6 more)

### Community 76 - "Encrypted Secret Vault"
Cohesion: 0.3
Nodes (4): decrypt_bytes(), encrypt_bytes(), file_vault_encrypts_and_decrypts_roundtrip(), FileVault

### Community 77 - "Tool Router Construction"
Cohesion: 0.18
Nodes (2): default_cloud_provider(), ToolRouter

### Community 78 - "Brain Integration Steps 72-77"
Cohesion: 0.22
Nodes (12): assert_replay_growth(), assert_turn_latency_spans(), build_auth_source(), build_scripted_provider(), cached_usage(), collect_tool_runs(), extend_tool_schemas(), last_user_message() (+4 more)

### Community 79 - "Session Row Badges"
Cohesion: 0.2
Nodes (11): confidence_badge(), confidence_color(), confidence_label(), risk_badge(), risk_color(), risk_label(), status_badge(), status_color() (+3 more)

### Community 80 - "Compaction"
Cohesion: 0.27
Nodes (12): calculate_cost_cents(), CheckpointState, compaction_request(), event_summary_line(), latest_checkpoint_state(), maybe_compact(), maybe_compact_events(), non_checkpoint_events() (+4 more)

### Community 81 - "Chat Panel"
Cohesion: 0.27
Nodes (2): ChatPanel, PendingToast

### Community 82 - "Session Search Tool"
Cohesion: 0.19
Nodes (6): event_snippet(), render_results(), SessionSearchEventType, SessionSearchInput, SessionSearchTool, truncate()

### Community 83 - "Window State Persistence"
Cohesion: 0.28
Nodes (4): round_trips_through_disk(), validate_bounds_clamps_offscreen_positions(), validate_bounds_clamps_tiny_panels(), WindowState

### Community 84 - "Eval Terminal Reporter"
Cohesion: 0.3
Nodes (5): format_scores(), render_includes_case_names_and_summary(), render_verbose_case(), result_index(), TerminalReporter

### Community 85 - "Turn Loop Detector"
Cohesion: 0.33
Nodes (6): loop_detector_disabled_at_zero_threshold(), loop_detector_does_not_trigger_on_varied_calls(), loop_detector_resets(), loop_detector_sliding_window(), loop_detector_triggers_after_threshold(), LoopDetector

### Community 86 - "Session List"
Cohesion: 0.4
Nodes (2): SessionList, SessionSelected

### Community 87 - "Memory Store Tests"
Cohesion: 0.36
Nodes (8): create_read_update_and_delete_wiki_pages(), delete_page_removes_only_the_requested_scope(), fts_search_finds_ranked_results(), fts_search_handles_hyphenated_queries(), rebuild_search_index_from_files_restores_results(), sample_page(), user_and_workspace_scopes_are_separate(), write_page_creates_and_reads_pages_in_explicit_scopes()

### Community 88 - "Postgres Store Tests"
Cohesion: 0.36
Nodes (6): cleanup_schema(), create_test_store(), postgres_event_payloads_round_trip_as_jsonb(), postgres_session_ids_are_native_uuid_and_concurrent_emits_are_serialized(), postgres_shared_session_store_contract(), with_test_store()

### Community 89 - "Instruction Stage"
Cohesion: 0.38
Nodes (4): combine_workspace_instructions(), instruction_processor_appends_config_backed_sections(), instruction_processor_combines_config_and_discovered_workspace_instructions(), InstructionProcessor

### Community 90 - "Session Sidebar"
Cohesion: 0.24
Nodes (1): SessionSidebar

### Community 91 - "Identity Stage"
Cohesion: 0.44
Nodes (3): identity_processor_appends_system_prompt(), identity_prompt_includes_coding_guardrails(), IdentityProcessor

### Community 92 - "Neon Branch Manager Tests"
Cohesion: 0.5
Nodes (7): live_neon_config(), live_neon_config_with_limit(), neon_branch_manager_create_list_get_rollback_and_discard_checkpoint(), neon_checkpoint_branch_connection_is_copy_on_write(), neon_checkpoint_capacity_limit_rejects_extra_branch(), neon_checkpoint_cleanup_without_expired_branches_returns_zero(), wait_for_workspace_session_count()

### Community 93 - "Temporal Worker Helper"
Cohesion: 0.32
Nodes (4): helper_config(), HelperProvider, main(), main_impl()

### Community 94 - "WCAG Contrast Helpers"
Cohesion: 0.39
Nodes (6): black_on_white_is_max_ratio(), classify(), contrast_ratio(), equal_colors_are_ratio_one(), relative_luminance(), WcagPass

### Community 95 - "Component Patterns"
Cohesion: 0.36
Nodes (8): Component Patterns, Dropdown Pattern, Icon Button Pattern, Left-Nav Item Pattern, Pill / Primary Button Pattern, Section Card Pattern, Status Chip / Inline Badge, Toggle Row Pattern

### Community 96 - "Trajectory Match Evaluator"
Cohesion: 0.43
Nodes (4): exact_match_scores_one(), lcs_len(), partial_match_scores_below_one(), TrajectoryMatchEvaluator

### Community 97 - "Output Match Evaluator"
Cohesion: 0.43
Nodes (4): contains_rules_pass_when_all_terms_match(), evaluate_output(), missing_contains_term_reduces_score(), OutputMatchEvaluator

### Community 98 - "Skill List Panel"
Cohesion: 0.38
Nodes (1): SkillList

### Community 99 - "Cache Optimizer Stage"
Cohesion: 0.4
Nodes (2): cache_optimizer_validates_cache_breakpoint(), CacheOptimizer

### Community 100 - "Threshold Evaluator"
Cohesion: 0.53
Nodes (3): cost_over_budget_fails_boolean_score(), limit_score(), ThresholdEvaluator

### Community 101 - "Desktop Notifications"
Cohesion: 0.4
Nodes (2): error(), sticky_error()

### Community 102 - "Providers Settings Tab"
Cohesion: 0.47
Nodes (4): collect_providers(), provider_control(), ProviderInfo, render_providers_tab()

### Community 103 - "Empty State Component"
Cohesion: 0.4
Nodes (2): empty_state(), empty_state_any()

### Community 104 - "Shell Chain Splitter"
Cohesion: 0.5
Nodes (2): push_sub_command(), split_shell_chain()

### Community 105 - "Anthropic Provider Tests"
Cohesion: 0.4
Nodes (0): 

### Community 106 - "Desktop Status Bar"
Cohesion: 0.5
Nodes (1): MoaStatusBar

### Community 107 - "Event Color Mapping"
Cohesion: 0.6
Nodes (2): event_color(), EventColor

### Community 108 - "Gemini Live Tests"
Cohesion: 0.83
Nodes (3): gemini_live_completion_returns_expected_answer(), gemini_live_model(), gemini_live_web_search_returns_current_information()

### Community 109 - "Chat Harness Example"
Cohesion: 0.83
Nodes (3): main(), resolve_session_db_path(), run_prompt()

### Community 110 - "Cost Budget Enforcement"
Cohesion: 0.67
Nodes (2): enforce_workspace_budget(), format_budget_exhausted_message()

### Community 111 - "Tool Success Evaluator"
Cohesion: 0.5
Nodes (1): ToolSuccessEvaluator

### Community 112 - "Settings Integration Tests"
Cohesion: 0.83
Nodes (3): removing_from_auto_approve_persists(), scratch_dir(), settings_mutations_round_trip_through_config_file()

### Community 113 - "Desktop Title Bar"
Cohesion: 0.67
Nodes (1): MoaTitleBar

### Community 114 - "General Settings Tab"
Cohesion: 0.67
Nodes (2): render_general_tab(), render_model_card()

### Community 115 - "Error Banner Component"
Cohesion: 0.67
Nodes (2): error_banner(), error_banner_any()

### Community 116 - "Retest Plan Overview"
Cohesion: 1.0
Nodes (4): Applied Retest Plan, Live Retest Findings, Progress Tracker, Task Plan: CallViewSet Retest

### Community 117 - "Applied Workspace Harness"
Cohesion: 0.5
Nodes (4): Applied workspace (~/github/applied), CallViewSet refactor task, Live Run Command, E2E Preconditions

### Community 118 - "Retest Improvements"
Cohesion: 0.67
Nodes (4): AGENTS.md workspace instructions, Approval rule hygiene, .venv search exclusion, What Improved

### Community 119 - "Turso Schema Migration"
Cohesion: 1.0
Nodes (2): add_sessions_column_if_missing(), migrate()

### Community 120 - "OpenAI Live Test"
Cohesion: 0.67
Nodes (0): 

### Community 121 - "Anthropic Live Test"
Cohesion: 0.67
Nodes (0): 

### Community 122 - "Sidebar Tab Enum"
Cohesion: 0.67
Nodes (1): SidebarTab

### Community 123 - "Permissions Settings Tab"
Cohesion: 1.0
Nodes (2): render_permissions_tab(), tool_list_card()

### Community 124 - "Desktop Service Init"
Cohesion: 0.67
Nodes (1): InitializedServices

### Community 125 - "Progress Tracker"
Cohesion: 0.67
Nodes (3): Turn counting and loop detection, Stepwise orchestrator tool boundaries, Likely Root Causes

### Community 126 - "Provider HTTP Client"
Cohesion: 1.0
Nodes (0): 

### Community 127 - "Docker Hardening Test"
Cohesion: 1.0
Nodes (0): 

### Community 128 - "Brain Live Harness Test"
Cohesion: 1.0
Nodes (0): 

### Community 129 - "Eval Live Tests"
Cohesion: 1.0
Nodes (0): 

### Community 130 - "Chat Runtime Trait"
Cohesion: 1.0
Nodes (1): ChatRuntime

### Community 131 - "Keyboard Shortcuts Tab"
Cohesion: 1.0
Nodes (0): 

### Community 132 - "Icon Button"
Cohesion: 1.0
Nodes (0): 

### Community 133 - "Section Card"
Cohesion: 1.0
Nodes (0): 

### Community 134 - "Segmented Control"
Cohesion: 1.0
Nodes (0): 

### Community 135 - "Settings Row"
Cohesion: 1.0
Nodes (0): 

### Community 136 - "Markdown Style"
Cohesion: 1.0
Nodes (0): 

### Community 137 - "Nav Item"
Cohesion: 1.0
Nodes (0): 

### Community 138 - "Retest Validation Steps"
Cohesion: 1.0
Nodes (2): sessions.db sqlite store, Post-Run Validation

### Community 139 - "Core Type Macros"
Cohesion: 1.0
Nodes (0): 

### Community 140 - "Desktop Design System"
Cohesion: 1.0
Nodes (1): MOA Desktop Design System

### Community 141 - "Typography"
Cohesion: 1.0
Nodes (1): Typography

### Community 142 - "Layout Rules"
Cohesion: 1.0
Nodes (1): Layout Rules

## Knowledge Gaps
- **314 isolated node(s):** `LogChange`, `LogEntry`, `BootstrapReport`, `BootstrapSentinel`, `PageFrontmatter` (+309 more)
  These have ≤1 connection - possible missing edges or undocumented components.
- **Thin community `Provider HTTP Client`** (2 nodes): `http.rs`, `build_http_client()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Docker Hardening Test`** (2 nodes): `docker_hardening.rs`, `docker_container_runs_with_hardening()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Brain Live Harness Test`** (2 nodes): `live_harness.rs`, `live_brain_turn_returns_brain_response()`
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
- **Thin community `Retest Validation Steps`** (2 nodes): `sessions.db sqlite store`, `Post-Run Validation`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Core Type Macros`** (1 nodes): `macros.rs`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Desktop Design System`** (1 nodes): `MOA Desktop Design System`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Typography`** (1 nodes): `Typography`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Layout Rules`** (1 nodes): `Layout Rules`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **Why does `LocalChatRuntime` connect `Local Chat Runtime` to `Local Orchestrator`?**
  _High betweenness centrality (0.013) - this node is a cross-community bridge._
- **Why does `DaemonChatRuntime` connect `Daemon Chat Runtime` to `Daemon Service`?**
  _High betweenness centrality (0.012) - this node is a cross-community bridge._
- **What connects `LogChange`, `LogEntry`, `BootstrapReport` to the rest of the system?**
  _314 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `Runtime Events & SSE` be split into smaller, more focused modules?**
  _Cohesion score 0.02 - nodes in this community are weakly interconnected._
- **Should `File Memory Store` be split into smaller, more focused modules?**
  _Cohesion score 0.03 - nodes in this community are weakly interconnected._
- **Should `Local Orchestrator Tests` be split into smaller, more focused modules?**
  _Cohesion score 0.05 - nodes in this community are weakly interconnected._
- **Should `Telemetry Bootstrap` be split into smaller, more focused modules?**
  _Cohesion score 0.03 - nodes in this community are weakly interconnected._