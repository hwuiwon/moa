# Graph Report - .  (2026-04-13)

## Corpus Check
- Large corpus: 335 files · ~194,192 words. Semantic extraction will be expensive (many Claude tokens). Consider running on a subfolder, or use --no-semantic to run AST-only.

## Summary
- 3152 nodes · 5219 edges · 180 communities detected
- Extraction: 99% EXTRACTED · 1% INFERRED · 0% AMBIGUOUS · INFERRED: 68 edges (avg confidence: 0.79)
- Token cost: 0 input · 0 output

## God Nodes (most connected - your core abstractions)
1. `LocalChatRuntime` - 36 edges
2. `DaemonChatRuntime` - 35 edges
3. `SkillFrontmatter` - 33 edges
4. `clone_runtime()` - 30 edges
5. `PostgresSessionStore` - 29 edges
6. `TursoSessionStore` - 28 edges
7. `start_session()` - 23 edges
8. `LocalOrchestrator` - 23 edges
9. `FileMemoryStore` - 22 edges
10. `SessionDatabase` - 20 edges

## Surprising Connections (you probably didn't know these)
- `Instruction Layer Hierarchy (workspace over user instructions)` --semantically_similar_to--> `Provider-Native Web Search (bypass MOA tools)`  [AMBIGUOUS] [semantically similar]
  src/components/settings/general-settings.tsx → src/components/settings/tools-and-mcp-settings.tsx
- `AlertDialog Component` --semantically_similar_to--> `Sheet Component`  [INFERRED] [semantically similar]
  src/components/ui/alert-dialog.tsx → src/components/ui/sheet.tsx
- `Alert Component` --semantically_similar_to--> `AlertDialog Component`  [INFERRED] [semantically similar]
  src/components/ui/alert.tsx → src/components/ui/alert-dialog.tsx
- `Tabs Component` --semantically_similar_to--> `Accordion Component`  [INFERRED] [semantically similar]
  src/components/ui/tabs.tsx → src/components/ui/accordion.tsx
- `NavigationMenu Component` --semantically_similar_to--> `Tabs Component`  [INFERRED] [semantically similar]
  src/components/ui/navigation-menu.tsx → src/components/ui/tabs.tsx

## Hyperedges (group relationships)
- **Sidebar Collapse System** — sidebar_provider, sidebar_component, sidebar_keyboard_shortcut, sidebar_cookie_persistence [EXTRACTED 1.00]
- **Menu-Family Components** — dropdown_menu_component, context_menu_component, menubar_component, select_component, command_component [INFERRED 0.90]
- **Provider-Model Selection Cascade Flow** — supported_providers_enum, available_models_memo, provider_model_cascade [INFERRED 0.85]
- **Form Validation and Save Pipeline** — arktype_resolver_integration, providers_settings_schema, backend_model_validation [INFERRED 0.78]
- **External Config Sync and Form Reset Flow** — config_sync_effect, values_from_config, default_provider_fallback_logic [INFERRED 0.80]
- **Tool Approval Decision Flow (ApprovalCard + ApprovalStore + TauriClient + ApprovalBlock)** — approval_card, approval_store, tauri_client, concept_approval_flow [INFERRED 0.90]
- **Tool Approval Visibility Flow** — content_block_renderer, approval_card_component, session_info_panel_component [INFERRED 0.75]
- **Session Chrome Lifecycle (AppLayout + SessionStore + TabsStore + LayoutStore driving navigation and mutations)** — app_layout, session_store, tabs_store, layout_store [INFERRED 0.85]
- **Prompt-Kit AI I/O Component Suite** — prompt_input_component, file_upload_component, code_block_component, image_component [INFERRED 0.90]
- **Chat Stream View Assembly (ChatView + useChatStream + useSessionHistory + MessageList)** — chat_view, use_chat_stream, use_session_history, message_list [INFERRED 0.88]
- **Settings ArkType-to-Rust Validation Chain** — concept_arktype_validated_settings_forms, concept_rust_backed_config_validation, src_views_settings_view_tsx, src_components_settings_general_settings_tsx [INFERRED 0.85]
- **Memory Browser CRUD Flow** — src_views_memory_view_tsx, src_components_memory_memory_editor_tsx, src_components_memory_memory_page_viewer_tsx, src_components_memory_memory_tree_tsx [INFERRED 0.90]

## Communities

### Community 0 - "Eval Loader Tests"
Cohesion: 0.02
Nodes (58): EvalError, MoaAppError, MoaError, approval_requested_event_round_trips_full_prompt(), Event, sample_approval_prompt(), HandHandle, HandResources (+50 more)

### Community 1 - "Config & Errors"
Cohesion: 0.03
Nodes (48): budget_config_defaults_are_applied(), BudgetConfig, CloudConfig, CloudFlyioConfig, CloudHandsConfig, CloudTemporalConfig, compaction_config_defaults_are_applied(), CompactionConfig (+40 more)

### Community 2 - "Tauri Session Commands"
Cohesion: 0.04
Nodes (62): attach_runtime(), cancel_active_generation(), clone_runtime(), create_session(), delete_memory_page(), get_config(), get_runtime_info(), get_session() (+54 more)

### Community 3 - "Pipeline & Session Helpers"
Cohesion: 0.03
Nodes (51): build_default_pipeline(), build_default_pipeline_with_runtime(), build_default_pipeline_with_tools(), cache_prefix_ratio(), ContextPipeline, estimate_tokens(), EvaluatorOptions, hand_id() (+43 more)

### Community 4 - "Brain Turn Tests"
Cohesion: 0.04
Nodes (27): always_allow_rule_persists_and_skips_next_approval(), canary_leaks_in_tool_input_are_detected_and_blocked(), CanaryLeakLlmProvider, CapturingTextLlmProvider, FixedPageMemoryStore, malicious_tool_results_are_wrapped_as_untrusted_content(), MaliciousToolOutputLlmProvider, MemoryIngestLoopLlmProvider (+19 more)

### Community 5 - "Memory Pipeline & Views"
Cohesion: 0.03
Nodes (38): ConfidenceLevel, count_ingest_pages(), derive_source_name_from_content(), extract_search_keywords(), extract_search_query(), format_ingest_report(), infer_page_title(), infer_page_type() (+30 more)

### Community 6 - "Event Taxonomy"
Cohesion: 0.04
Nodes (41): CollectedExecution, collector_tracks_tool_steps_and_metrics(), estimate_cost(), TrajectoryCollector, truncate(), destroy_and_wait(), e2b_live_provider_handles_roundtrip_and_lifecycle(), e2b_live_router_lazy_provisions_reuses_and_isolates_sessions() (+33 more)

### Community 7 - "Local Orchestrator Tests"
Cohesion: 0.05
Nodes (47): approval_requested_event_persists_full_prompt_details(), collect_runtime_events_until(), completed_tool_turn_destroys_cached_hand(), CurrentDirGuard, cwd_lock(), denied_tool_preserves_queued_follow_up(), DestroyTrackingHandProvider, FileWriteApprovalProvider (+39 more)

### Community 8 - "Session Database Backend"
Cohesion: 0.04
Nodes (34): Evaluator, build_provider_from_config(), build_provider_from_selection(), explicit_provider_prefix_overrides_inference(), infer_provider_name(), infers_anthropic_for_claude_models(), infers_google_for_gemini_models(), infers_openai_for_gpt_models() (+26 more)

### Community 9 - "Skill Document Format"
Cohesion: 0.07
Nodes (22): build_skill_path(), confidence_for_skill(), defaults_missing_moa_metadata(), estimate_skill_tokens(), format_timestamp(), humanize_skill_name(), is_valid_skill_name(), metadata_csv() (+14 more)

### Community 10 - "CLI Entry Point"
Cohesion: 0.05
Nodes (53): apply_config_update(), checkpoint_cleanup_report(), checkpoint_create_report(), checkpoint_list_report(), checkpoint_rollback_report(), CheckpointCommand, Cli, cloud_sync_status() (+45 more)

### Community 11 - "Local Orchestrator"
Cohesion: 0.06
Nodes (22): accept_user_message(), append_event(), best_effort_resolve_pending_signal(), detect_docker(), detect_workspace_path(), docker_status(), DockerSandbox, flush_next_queued_message() (+14 more)

### Community 12 - "Tool Policy & Content Types"
Cohesion: 0.05
Nodes (22): execute_tool_policy(), RegisteredTool, ToolExecution, ToolRegistry, page_key(), ranked_tools_prefer_successful_workspace_tools(), StaticMemoryStore, tool_output_error_sets_error_flag() (+14 more)

### Community 13 - "Daemon Service"
Cohesion: 0.07
Nodes (46): daemon_create_session_uses_explicit_client_scope(), daemon_health_endpoint_responds_when_cloud_enabled(), daemon_info(), daemon_lists_session_previews(), daemon_log_path(), daemon_logs(), daemon_pid_path(), daemon_ping_create_and_shutdown_roundtrip() (+38 more)

### Community 14 - "Postgres Session Store"
Cohesion: 0.06
Nodes (18): checkpoint_view(), event_hand_id(), normalize_event_search_query(), PostgresSessionStore, qualified_name(), approval_rule_from_row(), pending_signal_from_row(), pending_signal_type_from_db() (+10 more)

### Community 15 - "Anthropic Provider"
Cohesion: 0.07
Nodes (38): anthropic_content_blocks(), anthropic_content_blocks_render_text_and_json_as_text_blocks(), anthropic_message(), anthropic_message_wraps_assistant_tool_calls_as_tool_use_blocks(), anthropic_message_wraps_tool_results_with_tool_use_id(), anthropic_tool_from_schema(), anthropic_tool_from_schema_moves_parameters_into_input_schema(), AnthropicProvider (+30 more)

### Community 16 - "Temporal Orchestrator"
Cohesion: 0.06
Nodes (27): activity_options(), apply_approval_decision(), ApprovalDecisionActivityInput, ApprovalSignalInput, brain_turn_activity_options(), CancelMode, connect_temporal_client(), event_to_runtime_event() (+19 more)

### Community 17 - "Turso Session Store"
Cohesion: 0.06
Nodes (17): optional_i64(), optional_text(), parse_timestamp(), platform_from_db(), session_meta_from_row(), session_status_from_db(), session_summary_from_row(), build_event_fts_query() (+9 more)

### Community 18 - "History Compilation Stage"
Cohesion: 0.08
Nodes (28): calculate_cost_cents(), CheckpointState, compaction_request(), event_summary_line(), latest_checkpoint_state(), maybe_compact(), maybe_compact_events(), non_checkpoint_events() (+20 more)

### Community 19 - "Skill Regression Testing"
Cohesion: 0.07
Nodes (41): build_distillation_prompt(), count_tool_calls(), extract_task_summary(), find_similar_skill(), maybe_distill_skill(), normalize_new_skill(), similarity_score(), tokenize() (+33 more)

### Community 20 - "Gemini Provider"
Cohesion: 0.09
Nodes (32): build_contents(), build_request_body(), canonical_model_id(), capabilities_for_model(), consume_sse_events(), content_message(), finish_reason_to_stop_reason(), flush_pending_parts() (+24 more)

### Community 21 - "Session Store Tests"
Cohesion: 0.06
Nodes (42): AppLayout (chrome orchestrator), ChatView (per-session transcript view), Active View Enum (chat/memory/settings), Chrome Orchestrator Pattern (layout owns all session mutations), Empty-State Prompt Suggestions, Hash History Routing Strategy, moa-theme localStorage Key, Hydration Mount Guard (SSR-safe theme toggle) (+34 more)

### Community 22 - "Neon Branch Manager"
Cohesion: 0.09
Nodes (25): checkpoint_branch_names_follow_moa_prefix(), checkpoint_info_from_branch(), checkpoint_label_from_name(), cleanup_expired_deletes_only_old_moa_branches(), create_checkpoint_refuses_to_exceed_capacity(), create_checkpoint_sends_expected_request_and_returns_handle(), discard_checkpoint_calls_delete_endpoint(), format_checkpoint_branch_name() (+17 more)

### Community 23 - "Skill Injection Stage"
Cohesion: 0.08
Nodes (27): allowed_tools(), budget_limit_skips_expensive_tests(), distills_skill_after_tool_heavy_session(), estimate_skill_tokens(), improvement_accepted_when_scores_better(), improvement_rejected_on_regression(), ImprovementAndEvalLlm, improves_existing_skill_when_better_flow_is_found() (+19 more)

### Community 24 - "Turn Streaming & Approval"
Cohesion: 0.06
Nodes (25): drain_signal_queue(), handle_stream_signal(), run_streamed_turn_with_tools_mode(), emit_tool_output_warning(), execute_pending_tool(), execute_tool(), format_tool_output(), handle_tool_call() (+17 more)

### Community 25 - "Memory Consolidation"
Cohesion: 0.08
Nodes (40): canonical_port_claims(), confidence_rank(), consolidation_due_for_scope(), consolidation_resolves_dates_prunes_and_refreshes_index(), ConsolidationReport, decay_confidence(), extract_port_claims(), inbound_reference_counts() (+32 more)

### Community 26 - "Provider Common Layer"
Cohesion: 0.08
Nodes (27): build_function_tool(), build_responses_request(), consume_responses_stream_once(), is_ignorable_openai_stream_error(), is_rate_limit_error(), is_rate_limit_message(), map_openai_error(), metadata_as_strings() (+19 more)

### Community 27 - "Adaptive Tool Stats"
Cohesion: 0.1
Nodes (36): annotate_schema(), annotation_warns_on_low_success(), apply_tool_rankings(), cache_stability_preserves_identical_ranked_output(), collect_session_tool_observations(), compare_f64_asc(), compare_f64_desc(), compare_failure_last() (+28 more)

### Community 28 - "Chat Transcript Types"
Cohesion: 0.11
Nodes (40): appendNoticeBlock(), appendTextDelta(), applyApprovalDecision(), approvalBlockFromEvent(), approvalDecisionFromEvent(), asBoolean(), asNumber(), asPayload() (+32 more)

### Community 29 - "Message Renderer"
Cohesion: 0.09
Nodes (23): append_piece(), discord_renderer_attaches_buttons_to_last_chunk_only(), discord_renderer_uses_embed_limit_for_long_text(), DiscordRenderChunk, DiscordRenderer, render_approval_request(), render_diff(), render_tool_card() (+15 more)

### Community 30 - "Temporal Orchestrator Tests"
Cohesion: 0.11
Nodes (28): build_temporal_helper_binary(), delayed_text_stream(), last_user_message(), mock_capabilities(), spawn_temporal_helper(), temporal_helper_binary(), temporal_orchestrator_live_anthropic_smoke(), temporal_orchestrator_processes_multiple_queued_messages_fifo() (+20 more)

### Community 31 - "Local Chat Runtime"
Cohesion: 0.07
Nodes (2): ChatRuntime, LocalChatRuntime

### Community 32 - "Discord Adapter"
Cohesion: 0.12
Nodes (18): approval_callback_maps_to_control_message(), attachments_from_message(), context_from_component(), discord_button(), discord_create_message(), discord_create_message_includes_buttons_for_last_chunk(), discord_edit_message(), discord_embed() (+10 more)

### Community 33 - "E2B Sandbox Provider"
Cohesion: 0.14
Nodes (16): build_url(), ConnectedSandbox, decode_stream_chunk(), default_headers(), E2BHandProvider, encode_connect_request(), encode_test_envelopes(), envd_headers() (+8 more)

### Community 34 - "Eval Execution Engine"
Cohesion: 0.12
Nodes (20): build_error_result(), cleanup_workspace(), dry_run_marks_results_skipped(), EngineOptions, EvalEngine, EvalRun, extract_trace_id(), fs_try_exists() (+12 more)

### Community 35 - "Daemon Chat Runtime"
Cohesion: 0.07
Nodes (1): DaemonChatRuntime

### Community 36 - "Slack Adapter"
Cohesion: 0.13
Nodes (19): handle_interaction_event(), handle_push_event(), inbound_from_app_mention(), inbound_from_interaction_event(), inbound_from_message_event(), inbound_from_push_event(), interaction_origin(), parses_approval_callback_into_control_message() (+11 more)

### Community 37 - "Eval Agent Setup"
Cohesion: 0.13
Nodes (27): AgentEnvironment, apply_skill_overrides(), build_agent_environment(), build_agent_environment_with_provider(), build_eval_policies(), build_pipeline(), build_skill_memory_path(), build_tool_router() (+19 more)

### Community 38 - "Wiki & Memory Branching"
Cohesion: 0.12
Nodes (28): append_change_manifest(), branch_dir(), branch_file_path(), branch_root(), ChangeOperation, ChangeRecord, list_branches(), merge_markdown() (+20 more)

### Community 39 - "Working Context Messages"
Cohesion: 0.09
Nodes (8): context_message_assistant_tool_call_preserves_invocation(), context_message_tool_result_preserves_text_and_blocks(), context_message_tool_still_defaults_to_text_only(), ContextMessage, estimate_text_tokens(), MessageRole, ProcessorOutput, WorkingContext

### Community 40 - "Tauri DTO Layer"
Cohesion: 0.1
Nodes (19): enum_label(), event_payload(), EventRecordDto, iso(), memory_scope_label(), MemorySearchResultDto, MoaConfigDto, ModelOptionDto (+11 more)

### Community 41 - "Local Tool Tests"
Cohesion: 0.12
Nodes (20): bash_captures_stdout_and_stderr(), bash_respects_timeout(), EmptyMemoryStore, file_operations_reject_path_traversal(), file_read_reads_written_content(), file_search_finds_files_by_glob(), local_bash_hard_cancel_kills_running_process(), memory_ingest_creates_source_page_and_related_pages() (+12 more)

### Community 42 - "MCP Client Discovery"
Cohesion: 0.14
Nodes (16): flatten_call_result(), flatten_tool_result_aggregates_text_items(), header_map_from_pairs(), http_client_sends_headers_and_parses_jsonrpc(), MCPClient, McpDiscoveredTool, McpTransport, parse_jsonrpc_result() (+8 more)

### Community 43 - "MCP Credential Proxy"
Cohesion: 0.12
Nodes (11): credential_from_env(), default_scope_for(), env_var(), environment_vault_loads_from_env_backed_server_config(), EnvironmentCredentialVault, headers_from_credential(), MCPCredentialProxy, McpSessionToken (+3 more)

### Community 44 - "Daytona Workspace Provider"
Cohesion: 0.17
Nodes (10): build_url(), DaytonaHandProvider, default_headers(), derive_toolbox_url(), expect_success(), expect_success_json(), extract_workspace_id(), http_error() (+2 more)

### Community 45 - "Telegram Adapter"
Cohesion: 0.16
Nodes (13): channel_from_chat_and_reply(), handle_callback_query(), handle_message(), inbound_from_callback_query(), inbound_from_message(), inline_keyboard(), parse_message_id(), parses_approval_callback_into_control_message() (+5 more)

### Community 46 - "Docker File Operations"
Cohesion: 0.11
Nodes (15): container_path_validation_accepts_workspace_absolute_paths(), container_path_validation_rejects_absolute_paths_outside_workspace(), container_path_validation_rejects_traversal(), docker_file_read(), docker_file_search(), docker_file_write(), docker_find_args(), docker_read_args() (+7 more)

### Community 47 - "Session State Store"
Cohesion: 0.09
Nodes (18): BufferedUserMessage, CheckpointHandle, CheckpointInfo, ObserveLevel, pending_signal_queue_message_round_trip(), PendingSignal, PendingSignalType, session_meta_default_builds_created_session() (+10 more)

### Community 48 - "CLI HTTP API Server"
Cohesion: 0.11
Nodes (14): ApiState, build_api_router(), health_endpoint_returns_ok(), runtime_event_stream(), session_stream(), session_stream_returns_not_found_when_runtime_is_unavailable(), session_stream_returns_sse_content_type(), start_api_server() (+6 more)

### Community 49 - "Session Blob Store"
Cohesion: 0.18
Nodes (12): claim_check_from_value(), collect_blob_refs(), collect_large_strings(), decode_event_from_storage(), encode_event_for_storage(), expand_local_path(), file_blob_store_deletes_session_directory(), file_blob_store_is_content_addressed() (+4 more)

### Community 50 - "Approval Request Types"
Cohesion: 0.12
Nodes (18): approval_buttons(), approval_request(), ApprovalCallbackAction, ApprovalDecision, ApprovalField, ApprovalFileDiff, ApprovalPrompt, ApprovalRequest (+10 more)

### Community 51 - "Session Database Interface"
Cohesion: 0.09
Nodes (3): create_session_store(), SessionDatabase, SessionStoreDispatch

### Community 52 - "LLM Span Instrumentation"
Cohesion: 0.15
Nodes (15): calculate_cost(), calculate_cost_with_cached(), cost_calculation_correct(), has_meaningful_output(), llm_span_name(), LLMSpanAttributes, LLMSpanRecorder, metadata_f64() (+7 more)

### Community 53 - "Completion API Types"
Cohesion: 0.13
Nodes (9): completion_stream_abort_stops_completion_task(), CompletionContent, CompletionRequest, CompletionResponse, CompletionStream, ProviderToolCallMetadata, StopReason, ToolCallContent (+1 more)

### Community 54 - "Event Stream Types"
Cohesion: 0.12
Nodes (8): ClaimCheck, event_stream_reports_lagged_broadcasts(), EventFilter, EventRange, EventRecord, EventStream, EventType, MaybeBlob

### Community 55 - "Tool Router Policy"
Cohesion: 0.11
Nodes (8): approval_diffs_for(), approval_fields_for(), normalized_input_for(), read_existing_text_file(), required_string_field(), single_approval_field(), PreparedToolInvocation, ToolRouter

### Community 56 - "Full Text Search"
Cohesion: 0.19
Nodes (10): build_fts_query(), delete_page_entries(), FtsIndex, insert_page(), migrate(), parse_confidence(), parse_page_type(), parse_scope_key() (+2 more)

### Community 57 - "Tool Approval Policies"
Cohesion: 0.22
Nodes (12): ApprovalRuleStore, glob_match(), parse_and_match_bash(), persistent_rule_matching_uses_glob_patterns(), PolicyCheck, read_tools_are_auto_approved_and_bash_requires_approval(), rule_matches(), rule_visible_to_workspace() (+4 more)

### Community 58 - "Memory Maintenance Tests"
Cohesion: 0.27
Nodes (13): branch_reconciliation_merges_conflicting_writes(), consolidation_decays_confidence_once_and_is_stable_on_repeat_runs(), consolidation_normalizes_dates_and_resolves_conflicts(), ingest_source_creates_summary_and_updates_related_pages(), ingest_source_truncates_large_content(), maintenance_operations_append_log_and_keep_results_searchable(), manual_seeded_memory_fuzz_preserves_core_invariants(), manual_stress_ingest_reconcile_and_consolidate_preserves_invariants() (+5 more)

### Community 59 - "OpenAI Provider Tests"
Cohesion: 0.22
Nodes (15): openai_provider_does_not_retry_after_partial_stream_output(), openai_provider_drops_oversized_metadata_values(), openai_provider_includes_native_web_search_when_enabled(), openai_provider_omits_native_web_search_when_disabled(), openai_provider_retries_after_rate_limit(), openai_provider_serializes_assistant_tool_calls_as_function_call_items(), openai_provider_serializes_tool_result_messages_as_function_call_output(), openai_provider_streams_parallel_tool_calls_in_order() (+7 more)

### Community 60 - "Daytona Memory Store Tests"
Cohesion: 0.18
Nodes (9): daytona_live_provider_handles_roundtrip_and_lifecycle(), daytona_live_router_lazy_provisions_reuses_and_isolates_sessions(), destroy_and_wait(), EmptyMemoryStore, live_config(), live_provider(), session(), wait_for_destroyed() (+1 more)

### Community 61 - "Settings View Components"
Cohesion: 0.16
Nodes (17): Approval Rules Read-Only Desktop Posture, ArkType-Validated Settings Forms Pattern, Daemon Auto-Connect Runtime Flag, Deferred Appearance Persistence (surface stability gate), Instruction Layer Hierarchy (workspace over user instructions), MCP Server Editing Desktop UI Gap, Memory Dir and Sandbox Dir as Separate Filesystem Roots, Observability Export with Environment Tag (+9 more)

### Community 62 - "Memory Bootstrap"
Cohesion: 0.24
Nodes (14): BootstrapReport, BootstrapSentinel, find_instruction_file(), index_page_with_instructions(), is_bootstrap_index(), minimal_index_page(), project_instructions_page(), run_bootstrap() (+6 more)

### Community 63 - "Encrypted Secret Vault"
Cohesion: 0.3
Nodes (4): decrypt_bytes(), encrypt_bytes(), file_vault_encrypts_and_decrypts_roundtrip(), FileVault

### Community 64 - "Tool Router Construction"
Cohesion: 0.18
Nodes (2): default_cloud_provider(), ToolRouter

### Community 65 - "Provider Retry Policy"
Cohesion: 0.27
Nodes (6): parse_retry_after(), response_text(), retries_on_rate_limit(), retry_after_delay(), retry_after_delay_from_message(), RetryPolicy

### Community 66 - "Prompt Injection Detection"
Cohesion: 0.26
Nodes (12): canary_detection_works(), check_canary(), classifier_flags_known_attack_patterns(), classify_input(), contains_canary_tokens(), inject_canary(), InputClassification, InputInspection (+4 more)

### Community 67 - "Session Search Tool"
Cohesion: 0.19
Nodes (6): event_snippet(), render_results(), SessionSearchEventType, SessionSearchInput, SessionSearchTool, truncate()

### Community 68 - "Provider Settings UI"
Cohesion: 0.19
Nodes (13): ArkType Resolver Integration with react-hook-form, Available Models Memo (provider-filtered), Backend Model Validation Constraint, Config Sync useEffect (reset on config change), Default Provider Fallback to OpenAI, New Session Provider and Model Selection Intent, Provider-to-Model Cascade Reset, ProvidersSettings Component (+5 more)

### Community 69 - "Memory Browser Components"
Cohesion: 0.26
Nodes (13): Memory Page Confidence Field (high/medium/low), Memory Page Type Taxonomy (topic/entity/decision/skill/source/schema/log/index), Source Component Hover-Card Pattern (internal vs external links), Wiki-Link Fuzzy Slug Resolution Algorithm, Wiki-Link Internal Navigation Scheme (memory: prefix), Workspace Wiki Pages (markdown knowledge store), MemoryEditor Component, MemoryPageViewer Component (+5 more)

### Community 70 - "Live Provider Roundtrip Tests"
Cohesion: 0.3
Nodes (11): available_live_providers(), google_live_provider(), live_google_provider_complete_tool_approval_roundtrip_when_available(), live_orchestrator_with_provider(), live_providers_complete_tool_approval_roundtrip_when_available(), LiveProvider, run_live_provider_tool_approval_roundtrip(), wait_for_approval_request() (+3 more)

### Community 71 - "Eval Terminal Reporter"
Cohesion: 0.3
Nodes (5): format_scores(), render_includes_case_names_and_summary(), render_verbose_case(), result_index(), TerminalReporter

### Community 72 - "Streaming Code Display"
Cohesion: 0.17
Nodes (12): Code Block (Chat), Markdown Per-Block Memoization, Reasoning Auto-Open on Streaming, Shiki Syntax Highlighting (lazy-loaded), Streaming Cursor (CSS pulse after-element), Prompt-Kit Chain of Thought, Prompt-Kit Markdown, Prompt-Kit Reasoning (+4 more)

### Community 73 - "Memory Store Tests"
Cohesion: 0.36
Nodes (8): create_read_update_and_delete_wiki_pages(), delete_page_removes_only_the_requested_scope(), fts_search_finds_ranked_results(), fts_search_handles_hyphenated_queries(), rebuild_search_index_from_files_restores_results(), sample_page(), user_and_workspace_scopes_are_separate(), write_page_creates_and_reads_pages_in_explicit_scopes()

### Community 74 - "Postgres Store Tests"
Cohesion: 0.36
Nodes (6): cleanup_schema(), create_test_store(), postgres_event_payloads_round_trip_as_jsonb(), postgres_session_ids_are_native_uuid_and_concurrent_emits_are_serialized(), postgres_shared_session_store_contract(), with_test_store()

### Community 75 - "Chat Message Rendering"
Cohesion: 0.22
Nodes (10): AssistantMessage Component, ContentBlockRenderer Component, FeedbackBar Post-Stream Pattern, Mixed Block Rendering Pattern, MOA Product Name, Prompt-Kit Adapter Pattern, Streaming Text Last-Block Heuristic, ToolGroup Component (+2 more)

### Community 76 - "Tailwind Merge Util"
Cohesion: 0.28
Nodes (4): formatAbsoluteDate(), formatRelativeTime(), formatUsd(), formatUsdFromCents()

### Community 77 - "Neon Branch Manager Tests"
Cohesion: 0.5
Nodes (7): live_neon_config(), live_neon_config_with_limit(), neon_branch_manager_create_list_get_rollback_and_discard_checkpoint(), neon_checkpoint_branch_connection_is_copy_on_write(), neon_checkpoint_capacity_limit_rejects_extra_branch(), neon_checkpoint_cleanup_without_expired_branches_returns_zero(), wait_for_workspace_session_count()

### Community 78 - "Temporal Worker Helper"
Cohesion: 0.32
Nodes (4): helper_config(), HelperProvider, main(), main_impl()

### Community 79 - "Instruction Stage"
Cohesion: 0.43
Nodes (2): instruction_processor_appends_config_backed_sections(), InstructionProcessor

### Community 80 - "Identity Stage"
Cohesion: 0.43
Nodes (2): identity_processor_appends_system_prompt(), IdentityProcessor

### Community 81 - "Trajectory Match Evaluator"
Cohesion: 0.43
Nodes (4): exact_match_scores_one(), lcs_len(), partial_match_scores_below_one(), TrajectoryMatchEvaluator

### Community 82 - "Output Match Evaluator"
Cohesion: 0.43
Nodes (4): contains_rules_pass_when_all_terms_match(), evaluate_output(), missing_contains_term_reduces_score(), OutputMatchEvaluator

### Community 83 - "Bash Tool"
Cohesion: 0.47
Nodes (3): BashToolInput, execute_docker(), execute_local()

### Community 84 - "Cache Optimizer Stage"
Cohesion: 0.4
Nodes (2): cache_optimizer_validates_cache_breakpoint(), CacheOptimizer

### Community 85 - "Threshold Evaluator"
Cohesion: 0.53
Nodes (3): cost_over_budget_fails_boolean_score(), limit_score(), ThresholdEvaluator

### Community 86 - "CLI Exec Mode"
Cohesion: 0.6
Nodes (4): exec_mode_formats_tool_updates_compactly(), format_tool_update(), resolve_exec_approval(), run_exec()

### Community 87 - "Anthropic Provider Tests"
Cohesion: 0.4
Nodes (0): 

### Community 88 - "File Search Tool"
Cohesion: 0.5
Nodes (3): collect_matches(), execute(), FileSearchInput

### Community 89 - "Overlay & Alert Components"
Cohesion: 0.4
Nodes (5): Alert Component, AlertDialog Component, Drawer Component, Sheet Component, Sonner Toaster Component

### Community 90 - "Approval Card Component"
Cohesion: 0.6
Nodes (5): ApprovalCard (inline tool approval), useApprovalStore (pending approval registry), Human-in-the-Loop Tool Approval Flow, Risk-Tone Visual Encoding (low/moderate/high), tauriClient (IPC bridge)

### Community 91 - "Gemini Live Tests"
Cohesion: 0.83
Nodes (3): gemini_live_completion_returns_expected_answer(), gemini_live_model(), gemini_live_web_search_returns_current_information()

### Community 92 - "Chat Harness Example"
Cohesion: 0.83
Nodes (3): main(), resolve_session_db_path(), run_prompt()

### Community 93 - "Cost Budget Enforcement"
Cohesion: 0.67
Nodes (2): enforce_workspace_budget(), format_budget_exhausted_message()

### Community 94 - "Tool Success Evaluator"
Cohesion: 0.5
Nodes (1): ToolSuccessEvaluator

### Community 95 - "Command Actions & Layout"
Cohesion: 0.5
Nodes (0): 

### Community 96 - "Menu Family Components"
Cohesion: 0.5
Nodes (4): ContextMenu, DropdownMenu, Menubar, Select

### Community 97 - "OpenAI Live Test"
Cohesion: 0.67
Nodes (0): 

### Community 98 - "Anthropic Live Test"
Cohesion: 0.67
Nodes (0): 

### Community 99 - "Input & Button Groups"
Cohesion: 0.67
Nodes (3): ButtonGroup Component, InputGroup Component, NativeSelect Component

### Community 100 - "Floating Card Components"
Cohesion: 1.0
Nodes (3): HoverCard Component, Popover Component, Tooltip Component

### Community 101 - "Form Label & Field"
Cohesion: 0.67
Nodes (3): Field Component, Label Component, Typography Component Collection

### Community 102 - "Tabs & Navigation"
Cohesion: 0.67
Nodes (3): Accordion Component, NavigationMenu Component, Tabs Component

### Community 103 - "Frontend Toolchain Assets"
Cohesion: 0.67
Nodes (3): Tauri Logo SVG, Tauri + Vite Frontend Toolchain, Vite Logo SVG

### Community 104 - "Turso Schema Migration"
Cohesion: 1.0
Nodes (0): 

### Community 105 - "Tauri Build Entry"
Cohesion: 1.0
Nodes (0): 

### Community 106 - "Docker Hardening Test"
Cohesion: 1.0
Nodes (0): 

### Community 107 - "Brain Live Harness Test"
Cohesion: 1.0
Nodes (0): 

### Community 108 - "Eval Live Tests"
Cohesion: 1.0
Nodes (0): 

### Community 109 - "moa-runtime"
Cohesion: 1.0
Nodes (1): ChatRuntime

### Community 110 - "Session Tab Store"
Cohesion: 1.0
Nodes (0): 

### Community 111 - "Mobile Breakpoint Hook"
Cohesion: 1.0
Nodes (0): 

### Community 112 - "Session Preview DTOs"
Cohesion: 1.0
Nodes (0): 

### Community 113 - "Desktop Shell Root"
Cohesion: 1.0
Nodes (2): App Root Component, Desktop Shell (Tauri-backed UI)

### Community 114 - "components"
Cohesion: 1.0
Nodes (2): Global Cmd+K Command Palette Pattern, CommandPalette Component

### Community 115 - "Slider Component"
Cohesion: 1.0
Nodes (2): Progress Component, Slider Component

### Community 116 - "Radio Group"
Cohesion: 1.0
Nodes (2): Checkbox, RadioGroup

### Community 117 - "Spinner"
Cohesion: 1.0
Nodes (2): Skeleton, Spinner

### Community 118 - "Textarea"
Cohesion: 1.0
Nodes (2): Input, Textarea

### Community 119 - "Diff Viewer Component"
Cohesion: 1.0
Nodes (2): Diff Unified/Split Toggle View, DiffViewer Component

### Community 120 - "Detail Panel Component"
Cohesion: 1.0
Nodes (2): Detail Panel as Future Per-Session Inspection Surface, DetailPanel Component

### Community 121 - "Context Window Bar"
Cohesion: 1.0
Nodes (2): Context Pressure Visualization (24-segment bar), ContextWindowBar Component

### Community 122 - "Session Tab Bar Component"
Cohesion: 1.0
Nodes (2): Drag-and-Drop Session Tab Reordering, SessionTabBar Component

### Community 123 - "Session Info Panel Component"
Cohesion: 1.0
Nodes (2): Context Window Pressure Visualization, SessionInfoPanel Component

### Community 124 - "Chat Prompt Input"
Cohesion: 1.0
Nodes (2): Prompt-Kit Prompt Suggestion, Prompt Input (Chat)

### Community 125 - "Vite Config"
Cohesion: 1.0
Nodes (0): 

### Community 126 - "Core Type Macros"
Cohesion: 1.0
Nodes (0): 

### Community 127 - "Vite Env Types"
Cohesion: 1.0
Nodes (0): 

### Community 128 - "Memory Search DTO"
Cohesion: 1.0
Nodes (0): 

### Community 129 - "App Error Type"
Cohesion: 1.0
Nodes (0): 

### Community 130 - "Wiki Page DTO"
Cohesion: 1.0
Nodes (0): 

### Community 131 - "Page Summary DTO"
Cohesion: 1.0
Nodes (0): 

### Community 132 - "Runtime Info DTO"
Cohesion: 1.0
Nodes (0): 

### Community 133 - "Session Meta DTO"
Cohesion: 1.0
Nodes (0): 

### Community 134 - "Event Record DTO"
Cohesion: 1.0
Nodes (0): 

### Community 135 - "App Config DTO"
Cohesion: 1.0
Nodes (0): 

### Community 136 - "Model Option DTO"
Cohesion: 1.0
Nodes (0): 

### Community 137 - "Aspect Ratio"
Cohesion: 1.0
Nodes (1): AspectRatio Component

### Community 138 - "Pagination"
Cohesion: 1.0
Nodes (1): Pagination Component

### Community 139 - "Direction Provider"
Cohesion: 1.0
Nodes (1): DirectionProvider Component

### Community 140 - "Card Component"
Cohesion: 1.0
Nodes (1): Card Component

### Community 141 - "OTP Input"
Cohesion: 1.0
Nodes (1): InputOTP Component

### Community 142 - "Chart Component"
Cohesion: 1.0
Nodes (1): Chart Component

### Community 143 - "Scroll Area"
Cohesion: 1.0
Nodes (1): ScrollArea Component

### Community 144 - "Empty State"
Cohesion: 1.0
Nodes (1): Empty State Component

### Community 145 - "Switch Component"
Cohesion: 1.0
Nodes (1): Switch Component

### Community 146 - "Calendar Picker"
Cohesion: 1.0
Nodes (1): Calendar Component

### Community 147 - "Breadcrumb Nav"
Cohesion: 1.0
Nodes (1): Breadcrumb

### Community 148 - "Command Component"
Cohesion: 1.0
Nodes (1): Command

### Community 149 - "Command Dialog"
Cohesion: 1.0
Nodes (1): CommandDialog

### Community 150 - "Command Input"
Cohesion: 1.0
Nodes (1): CommandInput

### Community 151 - "Command Shortcut"
Cohesion: 1.0
Nodes (1): CommandItem

### Community 152 - "Item Component"
Cohesion: 1.0
Nodes (1): Item

### Community 153 - "Toggle Group"
Cohesion: 1.0
Nodes (1): ToggleGroup

### Community 154 - "Avatar Component"
Cohesion: 1.0
Nodes (1): Avatar

### Community 155 - "Keyboard Badge"
Cohesion: 1.0
Nodes (1): Kbd

### Community 156 - "Dialog Component"
Cohesion: 1.0
Nodes (1): Dialog

### Community 157 - "Badge Component"
Cohesion: 1.0
Nodes (1): Badge

### Community 158 - "Sidebar Component"
Cohesion: 1.0
Nodes (1): Sidebar

### Community 159 - "Sidebar Provider"
Cohesion: 1.0
Nodes (1): SidebarProvider

### Community 160 - "Table Component"
Cohesion: 1.0
Nodes (1): Table

### Community 161 - "Separator"
Cohesion: 1.0
Nodes (1): Separator

### Community 162 - "Button Component"
Cohesion: 1.0
Nodes (1): Button

### Community 163 - "Toggle Component"
Cohesion: 1.0
Nodes (1): Toggle

### Community 164 - "Collapsible"
Cohesion: 1.0
Nodes (1): Collapsible

### Community 165 - "Carousel Component"
Cohesion: 1.0
Nodes (1): Carousel

### Community 166 - "User Message Component"
Cohesion: 1.0
Nodes (1): UserMessage Component

### Community 167 - "Prompt-Kit Chat Container"
Cohesion: 1.0
Nodes (1): Prompt-Kit Chat Container

### Community 168 - "Prompt-Kit Tool"
Cohesion: 1.0
Nodes (1): Prompt-Kit Tool

### Community 169 - "Prompt-Kit Loader"
Cohesion: 1.0
Nodes (1): Prompt-Kit Loader

### Community 170 - "Prompt-Kit System Message"
Cohesion: 1.0
Nodes (1): Prompt-Kit System Message

### Community 171 - "Prompt Input"
Cohesion: 1.0
Nodes (1): PromptInput

### Community 172 - "Prompt Input Textarea"
Cohesion: 1.0
Nodes (1): PromptInputTextarea

### Community 173 - "Text Shimmer Animation"
Cohesion: 1.0
Nodes (1): Text Shimmer CSS Animation

### Community 174 - "File Upload"
Cohesion: 1.0
Nodes (1): FileUpload

### Community 175 - "Code Block"
Cohesion: 1.0
Nodes (1): CodeBlock

### Community 176 - "Code Block Renderer"
Cohesion: 1.0
Nodes (1): CodeBlockCode

### Community 177 - "Prompt-Kit Message"
Cohesion: 1.0
Nodes (1): Prompt-Kit Message

### Community 178 - "Prompt Kit Image"
Cohesion: 1.0
Nodes (1): Image (prompt-kit)

### Community 179 - "Prompt-Kit Feedback Bar"
Cohesion: 1.0
Nodes (1): Prompt-Kit Feedback Bar

## Ambiguous Edges - Review These
- `Instruction Layer Hierarchy (workspace over user instructions)` → `Provider-Native Web Search (bypass MOA tools)`  [AMBIGUOUS]
  src/components/settings/general-settings.tsx · relation: semantically_similar_to
- `Provider-to-Model Cascade Reset` → `Backend Model Validation Constraint`  [AMBIGUOUS]
  src/components/settings/providers-settings.tsx · relation: semantically_similar_to
- `Stick-to-Bottom Scroll Behavior` → `Virtualization Threshold Pattern (>50 / >100 rows)`  [AMBIGUOUS]
  src/components/chat/message-list.tsx · relation: semantically_similar_to
- `Chrome Orchestrator Pattern (layout owns all session mutations)` → `Workspace Identity (workspaceId from runtimeInfo)`  [AMBIGUOUS]
  src/components/layout/top-bar.tsx · relation: conceptually_related_to
- `Prompt-Kit Markdown` → `Prompt-Kit Chain of Thought`  [AMBIGUOUS]
  src/components/prompt-kit/markdown.tsx · relation: references

## Knowledge Gaps
- **350 isolated node(s):** `LogChange`, `LogEntry`, `BootstrapReport`, `BootstrapSentinel`, `PageFrontmatter` (+345 more)
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
- **Thin community `moa-runtime`** (2 nodes): `ChatRuntime`, `.set_model()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Session Tab Store`** (2 nodes): `tabs.ts`, `moveItem()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Mobile Breakpoint Hook`** (2 nodes): `use-mobile.ts`, `useIsMobile()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Session Preview DTOs`** (2 nodes): `SessionPreviewDto.ts`, `SessionSummaryDto.ts`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Desktop Shell Root`** (2 nodes): `App Root Component`, `Desktop Shell (Tauri-backed UI)`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `components`** (2 nodes): `Global Cmd+K Command Palette Pattern`, `CommandPalette Component`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Slider Component`** (2 nodes): `Progress Component`, `Slider Component`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Radio Group`** (2 nodes): `Checkbox`, `RadioGroup`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Spinner`** (2 nodes): `Skeleton`, `Spinner`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Textarea`** (2 nodes): `Input`, `Textarea`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Diff Viewer Component`** (2 nodes): `Diff Unified/Split Toggle View`, `DiffViewer Component`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Detail Panel Component`** (2 nodes): `Detail Panel as Future Per-Session Inspection Surface`, `DetailPanel Component`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Context Window Bar`** (2 nodes): `Context Pressure Visualization (24-segment bar)`, `ContextWindowBar Component`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Session Tab Bar Component`** (2 nodes): `Drag-and-Drop Session Tab Reordering`, `SessionTabBar Component`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Session Info Panel Component`** (2 nodes): `Context Window Pressure Visualization`, `SessionInfoPanel Component`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Chat Prompt Input`** (2 nodes): `Prompt-Kit Prompt Suggestion`, `Prompt Input (Chat)`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Vite Config`** (1 nodes): `vite.config.ts`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Core Type Macros`** (1 nodes): `macros.rs`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Vite Env Types`** (1 nodes): `vite-env.d.ts`
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
- **Thin community `Aspect Ratio`** (1 nodes): `AspectRatio Component`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Pagination`** (1 nodes): `Pagination Component`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Direction Provider`** (1 nodes): `DirectionProvider Component`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Card Component`** (1 nodes): `Card Component`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `OTP Input`** (1 nodes): `InputOTP Component`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Chart Component`** (1 nodes): `Chart Component`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Scroll Area`** (1 nodes): `ScrollArea Component`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Empty State`** (1 nodes): `Empty State Component`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Switch Component`** (1 nodes): `Switch Component`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Calendar Picker`** (1 nodes): `Calendar Component`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Breadcrumb Nav`** (1 nodes): `Breadcrumb`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Command Component`** (1 nodes): `Command`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Command Dialog`** (1 nodes): `CommandDialog`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Command Input`** (1 nodes): `CommandInput`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Command Shortcut`** (1 nodes): `CommandItem`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Item Component`** (1 nodes): `Item`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Toggle Group`** (1 nodes): `ToggleGroup`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Avatar Component`** (1 nodes): `Avatar`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Keyboard Badge`** (1 nodes): `Kbd`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Dialog Component`** (1 nodes): `Dialog`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Badge Component`** (1 nodes): `Badge`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Sidebar Component`** (1 nodes): `Sidebar`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Sidebar Provider`** (1 nodes): `SidebarProvider`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Table Component`** (1 nodes): `Table`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Separator`** (1 nodes): `Separator`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Button Component`** (1 nodes): `Button`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Toggle Component`** (1 nodes): `Toggle`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Collapsible`** (1 nodes): `Collapsible`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Carousel Component`** (1 nodes): `Carousel`
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
- **Thin community `Prompt Input`** (1 nodes): `PromptInput`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Prompt Input Textarea`** (1 nodes): `PromptInputTextarea`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Text Shimmer Animation`** (1 nodes): `Text Shimmer CSS Animation`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `File Upload`** (1 nodes): `FileUpload`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Code Block`** (1 nodes): `CodeBlock`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Code Block Renderer`** (1 nodes): `CodeBlockCode`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Prompt-Kit Message`** (1 nodes): `Prompt-Kit Message`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Prompt Kit Image`** (1 nodes): `Image (prompt-kit)`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Prompt-Kit Feedback Bar`** (1 nodes): `Prompt-Kit Feedback Bar`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **What is the exact relationship between `Instruction Layer Hierarchy (workspace over user instructions)` and `Provider-Native Web Search (bypass MOA tools)`?**
  _Edge tagged AMBIGUOUS (relation: semantically_similar_to) - confidence is low._
- **What is the exact relationship between `Provider-to-Model Cascade Reset` and `Backend Model Validation Constraint`?**
  _Edge tagged AMBIGUOUS (relation: semantically_similar_to) - confidence is low._
- **What is the exact relationship between `Stick-to-Bottom Scroll Behavior` and `Virtualization Threshold Pattern (>50 / >100 rows)`?**
  _Edge tagged AMBIGUOUS (relation: semantically_similar_to) - confidence is low._
- **What is the exact relationship between `Chrome Orchestrator Pattern (layout owns all session mutations)` and `Workspace Identity (workspaceId from runtimeInfo)`?**
  _Edge tagged AMBIGUOUS (relation: conceptually_related_to) - confidence is low._
- **What is the exact relationship between `Prompt-Kit Markdown` and `Prompt-Kit Chain of Thought`?**
  _Edge tagged AMBIGUOUS (relation: references) - confidence is low._
- **Why does `ChatView (per-session transcript view)` connect `Session Store Tests` to `Tauri Session Commands`?**
  _High betweenness centrality (0.020) - this node is a cross-community bridge._
- **What connects `LogChange`, `LogEntry`, `BootstrapReport` to the rest of the system?**
  _350 weakly-connected nodes found - possible documentation gaps or missing edges._