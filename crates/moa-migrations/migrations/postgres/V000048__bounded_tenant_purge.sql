-- Bounded, database-fenced tenant offboarding.
--
-- Purge progress and the complete residue catalog are database state so every
-- retry, replica, and final residue proof observes the same ordering.  Product
-- writers take shared advisory locks; purge admission takes the matching
-- exclusive lock before it installs the durable tenant-wide fence.

CREATE TABLE moa.tenant_purge_operations (
    tenant_id UUID PRIMARY KEY,
    operation_id TEXT NOT NULL UNIQUE,
    status TEXT NOT NULL DEFAULT 'in_progress'
        CHECK (status IN ('in_progress', 'relationally_committed')),
    current_stage TEXT NOT NULL DEFAULT 'authz',
    authz_cursor UUID,
    stage_deleted_count BIGINT NOT NULL DEFAULT 0,
    total_deleted_count BIGINT NOT NULL DEFAULT 0,
    batch_count BIGINT NOT NULL DEFAULT 0,
    started_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    relationally_committed_at TIMESTAMPTZ,
    CONSTRAINT tenant_purge_operations_operation_nonempty
        CHECK (btrim(operation_id) <> ''),
    CONSTRAINT tenant_purge_operations_stage_nonempty
        CHECK (btrim(current_stage) <> ''),
    CONSTRAINT tenant_purge_operations_counters_nonnegative
        CHECK (
            stage_deleted_count >= 0
            AND total_deleted_count >= 0
            AND batch_count >= 0
        ),
    CONSTRAINT tenant_purge_operations_status_consistent
        CHECK (
            (status = 'in_progress' AND relationally_committed_at IS NULL)
            OR
            (status = 'relationally_committed'
             AND relationally_committed_at IS NOT NULL
             AND current_stage = 'complete')
        )
);

GRANT SELECT ON moa.tenant_purge_operations TO moa_app, moa_promoter, moa_auditor;

CREATE TABLE moa.tenant_purge_catalog (
    stage_order SMALLINT PRIMARY KEY,
    stage_name TEXT NOT NULL UNIQUE,
    table_schema TEXT NOT NULL,
    table_name TEXT NOT NULL,
    scope_mode TEXT NOT NULL CHECK (scope_mode IN (
        'tenant_id',
        'storage_partition_id',
        'tenant_primary_key',
        'auth0_ciba_approval',
        'scim_group_member',
        'api_key_revocation',
        'session_event_dedupe',
        'control_residue',
        'external_owner'
    )),
    action_mode TEXT NOT NULL CHECK (action_mode IN (
        'delete',
        'clear_then_delete',
        'redact_kek',
        'redact_legal_hold',
        'retain_control',
        'assert_empty'
    )),
    UNIQUE (table_schema, table_name)
);

COMMENT ON TABLE moa.tenant_purge_catalog IS
    'Closed 127-table tenant-offboarding residue surface. The two nullable-scope simulator certification authority tables are intentionally global and absent.';

-- This order is the existing proven FK order, made durable.  One catalog row
-- owns one table; multi-phase pointer clearing is encoded by clear_then_delete.
INSERT INTO moa.tenant_purge_catalog
    (stage_order, stage_name, table_schema, table_name, scope_mode, action_mode)
VALUES
    (1, 'public.oauth_tokens', 'public', 'oauth_tokens', 'tenant_id', 'delete'),
    (2, 'public.oauth_authorization_codes', 'public', 'oauth_authorization_codes', 'tenant_id', 'delete'),
    (3, 'public.oauth_authorization_transactions', 'public', 'oauth_authorization_transactions', 'tenant_id', 'delete'),
    (5, 'moa.dual_control_request', 'moa', 'dual_control_request', 'tenant_id', 'delete'),
    (6, 'moa.audit_jti_used', 'moa', 'audit_jti_used', 'tenant_id', 'delete'),
    (7, 'moa.erasure_jobs', 'moa', 'erasure_jobs', 'tenant_id', 'delete'),
    (8, 'moa.knowledge_object_ingestion_claims', 'moa', 'knowledge_object_ingestion_claims', 'tenant_id', 'delete'),
    (9, 'moa.knowledge_semantic_graph_extractions', 'moa', 'knowledge_semantic_graph_extractions', 'tenant_id', 'delete'),
    (10, 'moa.knowledge_contact_group_memberships', 'moa', 'knowledge_contact_group_memberships', 'tenant_id', 'delete'),
    (11, 'moa.knowledge_contact_groups', 'moa', 'knowledge_contact_groups', 'tenant_id', 'delete'),
    (12, 'moa.knowledge_chunks', 'moa', 'knowledge_chunks', 'tenant_id', 'delete'),
    (13, 'moa.knowledge_blocks', 'moa', 'knowledge_blocks', 'tenant_id', 'delete'),
    (14, 'moa.knowledge_objects', 'moa', 'knowledge_objects', 'tenant_id', 'clear_then_delete'),
    (15, 'moa.knowledge_source_acl_entries', 'moa', 'knowledge_source_acl_entries', 'tenant_id', 'delete'),
    (16, 'moa.knowledge_source_acl_snapshots', 'moa', 'knowledge_source_acl_snapshots', 'tenant_id', 'delete'),
    (17, 'moa.knowledge_source_principal_group_bindings', 'moa', 'knowledge_source_principal_group_bindings', 'tenant_id', 'delete'),
    (18, 'moa.knowledge_source_principal_bindings', 'moa', 'knowledge_source_principal_bindings', 'tenant_id', 'delete'),
    (19, 'moa.knowledge_source_acl_epochs', 'moa', 'knowledge_source_acl_epochs', 'tenant_id', 'delete'),
    (20, 'moa.knowledge_source_acl_keys', 'moa', 'knowledge_source_acl_keys', 'tenant_id', 'delete'),
    (21, 'moa.knowledge_document_versions', 'moa', 'knowledge_document_versions', 'tenant_id', 'delete'),
    (22, 'moa.knowledge_provider_events', 'moa', 'knowledge_provider_events', 'tenant_id', 'delete'),
    (23, 'moa.knowledge_ingestion_steps', 'moa', 'knowledge_ingestion_steps', 'tenant_id', 'delete'),
    (24, 'moa.knowledge_sync_runs', 'moa', 'knowledge_sync_runs', 'tenant_id', 'delete'),
    (25, 'moa.knowledge_connections', 'moa', 'knowledge_connections', 'tenant_id', 'delete'),
    (26, 'public.security_events', 'public', 'security_events', 'tenant_id', 'delete'),
    (27, 'public.tenant_signing_keys', 'public', 'tenant_signing_keys', 'tenant_id', 'delete'),
    (28, 'public.tenant_action_reviews', 'public', 'tenant_action_reviews', 'tenant_id', 'delete'),
    (29, 'public.action_policy_rules', 'public', 'action_policy_rules', 'tenant_id', 'delete'),
    (30, 'public.builtin_pending_approvals', 'public', 'builtin_pending_approvals', 'tenant_id', 'delete'),
    (31, 'public.auth0_ciba_approvals', 'public', 'auth0_ciba_approvals', 'auth0_ciba_approval', 'delete'),
    (32, 'moa.privacy_erasure_record_decision', 'moa', 'privacy_erasure_record_decision', 'tenant_id', 'delete'),
    (33, 'moa.hand_leases', 'moa', 'hand_leases', 'tenant_id', 'delete'),
    (34, 'moa.tenant_sandbox_policy', 'moa', 'tenant_sandbox_policy', 'tenant_id', 'delete'),
    (35, 'moa.execution_node_materialization', 'moa', 'execution_node_materialization', 'tenant_id', 'delete'),
    (36, 'moa.execution_planner_call_audit', 'moa', 'execution_planner_call_audit', 'tenant_id', 'delete'),
    (37, 'moa.execution_compile_audit', 'moa', 'execution_compile_audit', 'tenant_id', 'delete'),
    (38, 'moa.execution_route_audit', 'moa', 'execution_route_audit', 'tenant_id', 'delete'),
    (39, 'moa.execution_action_review_outbox', 'moa', 'execution_action_review_outbox', 'tenant_id', 'delete'),
    (40, 'moa.execution_task', 'moa', 'execution_task', 'tenant_id', 'delete'),
    (41, 'moa.execution_template_admission', 'moa', 'execution_template_admission', 'tenant_id', 'delete'),
    (42, 'moa.execution_run', 'moa', 'execution_run', 'tenant_id', 'delete'),
    (43, 'moa.execution_planning_context', 'moa', 'execution_planning_context', 'tenant_id', 'delete'),
    (44, 'public.session_event_archives', 'public', 'session_event_archives', 'tenant_id', 'delete'),
    (45, 'public.session_agent_context', 'public', 'session_agent_context', 'tenant_id', 'delete'),
    (46, 'public.session_attachments', 'public', 'session_attachments', 'tenant_id', 'delete'),
    (47, 'public.session_blobs', 'public', 'session_blobs', 'tenant_id', 'delete'),
    (48, 'public.session_channel_bindings', 'public', 'session_channel_bindings', 'tenant_id', 'delete'),
    (49, 'public.contact_verification_challenges', 'public', 'contact_verification_challenges', 'tenant_id', 'delete'),
    (50, 'public.contact_token_grants', 'public', 'contact_token_grants', 'tenant_id', 'delete'),
    (51, 'public.contact_channel_accounts', 'public', 'contact_channel_accounts', 'tenant_id', 'delete'),
    (52, 'public.contact_points', 'public', 'contact_points', 'tenant_id', 'delete'),
    (53, 'public.contacts', 'public', 'contacts', 'tenant_id', 'delete'),
    (54, 'public.tenant_user_invitations', 'public', 'tenant_user_invitations', 'tenant_id', 'delete'),
    (55, 'public.password_reset_tokens', 'public', 'password_reset_tokens', 'tenant_id', 'delete'),
    (56, 'public.user_session_tokens', 'public', 'user_session_tokens', 'tenant_id', 'delete'),
    (57, 'public.local_user_credentials', 'public', 'local_user_credentials', 'tenant_id', 'delete'),
    (58, 'public.auth0_user_map', 'public', 'auth0_user_map', 'tenant_id', 'delete'),
    (60, 'public.scim_group_members', 'public', 'scim_group_members', 'scim_group_member', 'delete'),
    (61, 'public.scim_groups', 'public', 'scim_groups', 'tenant_id', 'delete'),
    (62, 'public.agents', 'public', 'agents', 'tenant_id', 'delete'),
    (63, 'public.api_key_revocations', 'public', 'api_key_revocations', 'api_key_revocation', 'delete'),
    (64, 'public.api_keys', 'public', 'api_keys', 'tenant_id', 'delete'),
    (65, 'public.users', 'public', 'users', 'tenant_id', 'delete'),
    (66, 'moa.agent_deployment', 'moa', 'agent_deployment', 'storage_partition_id', 'delete'),
    (67, 'moa.agent_installation', 'moa', 'agent_installation', 'storage_partition_id', 'delete'),
    (68, 'moa.experiment_score_provenance', 'moa', 'experiment_score_provenance', 'storage_partition_id', 'delete'),
    (69, 'moa.experiment_resource_reservation', 'moa', 'experiment_resource_reservation', 'storage_partition_id', 'delete'),
    (70, 'moa.experiment_trial', 'moa', 'experiment_trial', 'storage_partition_id', 'delete'),
    (71, 'moa.experiment_run_artifact_revision', 'moa', 'experiment_run_artifact_revision', 'storage_partition_id', 'delete'),
    (72, 'moa.experiment_run', 'moa', 'experiment_run', 'storage_partition_id', 'delete'),
    (73, 'moa.simulator_fidelity_study', 'moa', 'simulator_fidelity_study', 'storage_partition_id', 'delete'),
    (74, 'moa.simulator_policy', 'moa', 'simulator_policy', 'storage_partition_id', 'delete'),
    (75, 'analytics.score_run', 'analytics', 'score_run', 'storage_partition_id', 'delete'),
    (76, 'moa.artifact_suite_contribution', 'moa', 'artifact_suite_contribution', 'storage_partition_id', 'delete'),
    (77, 'moa.artifact_revision_contribution', 'moa', 'artifact_revision_contribution', 'storage_partition_id', 'delete'),
    (78, 'public.learning_log_source', 'public', 'learning_log_source', 'storage_partition_id', 'delete'),
    (79, 'public.learning_candidate_decision', 'public', 'learning_candidate_decision', 'storage_partition_id', 'delete'),
    (80, 'public.learning_candidate_source', 'public', 'learning_candidate_source', 'storage_partition_id', 'delete'),
    (81, 'moa.skill_embedding', 'moa', 'skill_embedding', 'storage_partition_id', 'delete'),
    (82, 'moa.artifact_file', 'moa', 'artifact_file', 'storage_partition_id', 'delete'),
    (83, 'moa.artifact_release_eval_overlay', 'moa', 'artifact_release_eval_overlay', 'storage_partition_id', 'delete'),
    (84, 'moa.artifact_release_attempt', 'moa', 'artifact_release_attempt', 'storage_partition_id', 'delete'),
    (85, 'moa.artifact_release_dispatch_outbox', 'moa', 'artifact_release_dispatch_outbox', 'storage_partition_id', 'delete'),
    (86, 'moa.artifact_release_case_pack', 'moa', 'artifact_release_case_pack', 'storage_partition_id', 'delete'),
    (87, 'moa.artifact_serving_pointer', 'moa', 'artifact_serving_pointer', 'storage_partition_id', 'delete'),
    (88, 'moa.artifact_activation_audit', 'moa', 'artifact_activation_audit', 'storage_partition_id', 'delete'),
    (89, 'moa.artifact_activation_attestation', 'moa', 'artifact_activation_attestation', 'storage_partition_id', 'delete'),
    (90, 'moa.artifact_release_candidate', 'moa', 'artifact_release_candidate', 'storage_partition_id', 'delete'),
    (91, 'moa.artifact_release_policy', 'moa', 'artifact_release_policy', 'storage_partition_id', 'delete'),
    (92, 'moa.artifact', 'moa', 'artifact', 'storage_partition_id', 'clear_then_delete'),
    (93, 'moa.artifact_revision', 'moa', 'artifact_revision', 'storage_partition_id', 'delete'),
    (94, 'public.learning_candidates', 'public', 'learning_candidates', 'storage_partition_id', 'delete'),
    (95, 'public.experience_attributions', 'public', 'experience_attributions', 'storage_partition_id', 'delete'),
    (96, 'public.experience_records', 'public', 'experience_records', 'storage_partition_id', 'delete'),
    (97, 'public.learning_log', 'public', 'learning_log', 'storage_partition_id', 'delete'),
    (98, 'public.task_segments', 'public', 'task_segments', 'storage_partition_id', 'delete'),
    (99, 'analytics.lineage_journal', 'analytics', 'lineage_journal', 'storage_partition_id', 'delete'),
    (100, 'analytics.turn_lineage', 'analytics', 'turn_lineage', 'storage_partition_id', 'delete'),
    (101, 'analytics.scores', 'analytics', 'scores', 'storage_partition_id', 'delete'),
    (102, 'analytics.audit_roots', 'analytics', 'audit_roots', 'storage_partition_id', 'delete'),
    (103, 'analytics.compliance_storage_partition_state', 'analytics', 'compliance_storage_partition_state', 'storage_partition_id', 'delete'),
    (104, 'analytics.compliance_tenants', 'analytics', 'compliance_tenants', 'storage_partition_id', 'delete'),
    (105, 'pii_vault.plaintext_side', 'pii_vault', 'plaintext_side', 'storage_partition_id', 'delete'),
    (106, 'pii_vault.subject_keys', 'pii_vault', 'subject_keys', 'storage_partition_id', 'delete'),
    (107, 'moa.retrieval_lineage', 'moa', 'retrieval_lineage', 'storage_partition_id', 'delete'),
    (108, 'moa.memory_digests', 'moa', 'memory_digests', 'storage_partition_id', 'delete'),
    (109, 'moa.ingest_dlq', 'moa', 'ingest_dlq', 'storage_partition_id', 'delete'),
    (110, 'moa.ingest_dedup', 'moa', 'ingest_dedup', 'storage_partition_id', 'delete'),
    (111, 'moa.vector_sync_outbox', 'moa', 'vector_sync_outbox', 'storage_partition_id', 'delete'),
    (112, 'moa.embeddings', 'moa', 'embeddings', 'tenant_id', 'delete'),
    (113, 'moa.graph_changelog', 'moa', 'graph_changelog', 'tenant_id', 'delete'),
    (114, 'moa.edge_index', 'moa', 'edge_index', 'tenant_id', 'delete'),
    (115, 'moa.node_index', 'moa', 'node_index', 'tenant_id', 'delete'),
    (116, 'moa.storage_partition_state', 'moa', 'storage_partition_state', 'tenant_id', 'delete'),
    (117, 'public.session_event_dedupe', 'public', 'session_event_dedupe', 'session_event_dedupe', 'delete'),
    (118, 'public.context_snapshots', 'public', 'context_snapshots', 'tenant_id', 'delete'),
    (119, 'public.events', 'public', 'events', 'tenant_id', 'delete'),
    (120, 'public.sessions', 'public', 'sessions', 'tenant_id', 'clear_then_delete'),
    (121, 'moa.kek', 'moa', 'kek', 'tenant_id', 'redact_kek'),
    (122, 'moa.legal_hold', 'moa', 'legal_hold', 'tenant_id', 'redact_legal_hold'),
    (123, 'public.tenants', 'public', 'tenants', 'tenant_primary_key', 'delete'),
    (124, 'public.tenant_credential_versions', 'public', 'tenant_credential_versions', 'external_owner', 'assert_empty'),
    (125, 'moa.knowledge_link_claims', 'moa', 'knowledge_link_claims', 'external_owner', 'assert_empty'),
    (126, 'public.tenant_credential_operations', 'public', 'tenant_credential_operations', 'control_residue', 'retain_control'),
    (127, 'public.authz_outbox', 'public', 'authz_outbox', 'control_residue', 'retain_control'),
    (128, 'moa.destruction_operation_fence', 'moa', 'destruction_operation_fence', 'control_residue', 'retain_control'),
    (129, 'moa.tenant_purge_operations', 'moa', 'tenant_purge_operations', 'control_residue', 'retain_control');

CREATE INDEX auth0_ciba_approvals_session_idx
    ON auth0_ciba_approvals (session_id);
CREATE INDEX auth0_ciba_approvals_deciding_user_idx
    ON auth0_ciba_approvals (deciding_user_id);

-- These are the only purge predicates whose existing keys are absent, partial
-- on an incompatible predicate, or led by another column.  They are installed
-- transactionally because V379 is a maintenance-window migration; normal
-- product writes may block while PostgreSQL builds them.
CREATE INDEX tenant_purge_dual_control_request_idx
    ON moa.dual_control_request (tenant_id);
CREATE INDEX tenant_purge_knowledge_contact_group_memberships_idx
    ON moa.knowledge_contact_group_memberships (tenant_id);
CREATE INDEX tenant_purge_knowledge_source_acl_entries_idx
    ON moa.knowledge_source_acl_entries (tenant_id);
CREATE INDEX tenant_purge_builtin_pending_approvals_idx
    ON public.builtin_pending_approvals (tenant_id);
CREATE INDEX tenant_purge_execution_action_review_outbox_idx
    ON moa.execution_action_review_outbox (tenant_id);
CREATE INDEX tenant_purge_contact_verification_challenges_idx
    ON public.contact_verification_challenges (tenant_id);
CREATE INDEX tenant_purge_password_reset_tokens_idx
    ON public.password_reset_tokens (tenant_id);
CREATE INDEX tenant_purge_user_session_tokens_idx
    ON public.user_session_tokens (tenant_id);
CREATE INDEX tenant_purge_auth0_user_map_idx
    ON public.auth0_user_map (tenant_id);
CREATE INDEX tenant_purge_artifact_suite_contribution_idx
    ON moa.artifact_suite_contribution (storage_partition_id);
CREATE INDEX tenant_purge_artifact_revision_contribution_idx
    ON moa.artifact_revision_contribution (storage_partition_id);
CREATE INDEX tenant_purge_artifact_release_eval_overlay_idx
    ON moa.artifact_release_eval_overlay (storage_partition_id);
CREATE INDEX tenant_purge_artifact_release_case_pack_idx
    ON moa.artifact_release_case_pack (storage_partition_id)
    WHERE storage_partition_id IS NOT NULL;
CREATE INDEX tenant_purge_artifact_activation_attestation_idx
    ON moa.artifact_activation_attestation (storage_partition_id);
CREATE INDEX tenant_purge_artifact_release_policy_idx
    ON moa.artifact_release_policy (storage_partition_id)
    WHERE storage_partition_id IS NOT NULL;
CREATE INDEX tenant_purge_artifact_idx
    ON moa.artifact (storage_partition_id)
    WHERE storage_partition_id IS NOT NULL;
CREATE INDEX tenant_purge_artifact_revision_idx
    ON moa.artifact_revision (storage_partition_id)
    WHERE storage_partition_id IS NOT NULL;
CREATE INDEX tenant_purge_embeddings_idx
    ON moa.embeddings (tenant_id);
CREATE INDEX tenant_purge_legal_hold_idx
    ON moa.legal_hold (tenant_id)
    WHERE released_at IS NOT NULL
      AND (
        subject_id IS NOT NULL
        OR reason <> '[REDACTED]'
        OR placed_by <> '[REDACTED]'
        OR released_by <> '[REDACTED]'
      );

GRANT SELECT ON moa.tenant_purge_catalog TO moa_app, moa_promoter, moa_auditor;

-- True only while an owner-run purge function is operating on the exact
-- in-progress progress row and tenant-wide destruction fence named by the
-- transaction-local operation id. A caller-set GUC alone is never authority.
CREATE FUNCTION moa.tenant_purge_bypass_valid(p_tenant_id UUID)
RETURNS BOOLEAN
LANGUAGE sql
STABLE
SECURITY INVOKER
SET search_path = pg_catalog, pg_temp
AS $$
    SELECT current_user = 'moa_owner'
       AND EXISTS (
            SELECT 1
            FROM moa.tenant_purge_operations AS progress
            JOIN moa.destruction_operation_fence AS fence
              ON fence.tenant_id = progress.tenant_id
             AND fence.subject_id IS NULL
             AND fence.operation_id = progress.operation_id
             AND fence.operation_kind = 'tenant.purge'
             AND fence.status = 'in_progress'
            WHERE progress.tenant_id = p_tenant_id
              AND progress.status = 'in_progress'
              AND progress.operation_id = NULLIF(
                    current_setting('moa.tenant_purge_operation_id', true),
                    ''
                  )
       );
$$;
ALTER FUNCTION moa.tenant_purge_bypass_valid(UUID) OWNER TO moa_owner;
REVOKE ALL ON FUNCTION moa.tenant_purge_bypass_valid(UUID) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION moa.tenant_purge_bypass_valid(UUID)
    TO moa_app, moa_promoter;

-- Activation decisions remain append-only during ordinary operation, but the
-- bounded purge must be able to remove them before their artifact parent. The
-- exception is tied to the same exact tenant, operation id, progress row, and
-- in-progress destruction fence as every other immutable purge surface.
CREATE OR REPLACE FUNCTION moa.artifact_activation_audit_guard()
RETURNS TRIGGER
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    touched_tenant UUID;
BEGIN
    IF TG_OP = 'DELETE' THEN
        BEGIN
            touched_tenant := OLD.storage_partition_id::UUID;
        EXCEPTION WHEN invalid_text_representation THEN
            touched_tenant := NULL;
        END;

        IF touched_tenant IS NOT NULL
           AND moa.tenant_purge_bypass_valid(touched_tenant)
        THEN
            RETURN OLD;
        END IF;
    END IF;

    RAISE EXCEPTION
        'activation audit % is append-only',
        OLD.audit_uid
        USING ERRCODE = 'P0001';
END;
$$;
ALTER FUNCTION moa.artifact_activation_audit_guard() OWNER TO moa_owner;
REVOKE ALL ON FUNCTION moa.artifact_activation_audit_guard() FROM PUBLIC;

COMMENT ON FUNCTION moa.artifact_activation_audit_guard() IS
    'Refuses UPDATE and ordinary DELETE on activation audit rows; permits only an exact validated tenant purge.';

-- Restricted SECURITY DEFINER writers cannot read the destruction-fence table
-- directly. Expose only the tenant-wide in-progress boolean that the invoker
-- statement guard needs; no control-row data crosses this least-privilege seam.
CREATE FUNCTION moa.tenant_write_fenced(p_tenant_id UUID)
RETURNS BOOLEAN
LANGUAGE sql
STABLE
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
    SELECT EXISTS (
        SELECT 1
        FROM moa.destruction_operation_fence AS fence
        WHERE fence.tenant_id = p_tenant_id
          AND fence.subject_id IS NULL
          AND fence.status = 'in_progress'
    );
$$;
ALTER FUNCTION moa.tenant_write_fenced(UUID) OWNER TO moa_owner;
REVOKE ALL ON FUNCTION moa.tenant_write_fenced(UUID) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION moa.tenant_write_fenced(UUID)
    TO moa_app, moa_promoter, moa_artifact_activator, moa_privacy_eraser;

-- Validated purge admission owns both durable control rows.  It takes the
-- exclusive tenant lock before checking holds, so every earlier shared writer
-- drains before the fence becomes visible.
CREATE FUNCTION moa.start_tenant_purge(
    p_tenant_id UUID,
    p_operation_id TEXT
) RETURNS VOID
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    existing_operation TEXT;
BEGIN
    IF p_tenant_id IS NULL OR p_operation_id IS NULL OR btrim(p_operation_id) = '' THEN
        RAISE EXCEPTION 'tenant purge requires tenant and operation ids'
            USING ERRCODE = '22023';
    END IF;

    PERFORM pg_advisory_xact_lock(
        hashtextextended('moa:destruction:tenant:' || p_tenant_id::TEXT, 0)
    );
    IF EXISTS (
        SELECT 1 FROM moa.legal_hold
        WHERE tenant_id = p_tenant_id AND released_at IS NULL
    ) THEN
        RAISE EXCEPTION 'tenant purge blocked by active legal hold'
            USING ERRCODE = '55000';
    END IF;

    SELECT operation_id INTO existing_operation
    FROM moa.destruction_operation_fence
    WHERE tenant_id = p_tenant_id AND subject_id IS NULL;
    IF existing_operation IS NOT NULL AND existing_operation <> p_operation_id THEN
        RAISE EXCEPTION 'tenant destruction fence belongs to another operation'
            USING ERRCODE = '55000';
    END IF;

    PERFORM set_config('moa.tenant_purge_operation_id', p_operation_id, true);
    INSERT INTO moa.destruction_operation_fence
        (tenant_id, subject_id, operation_id, operation_kind)
    VALUES (p_tenant_id, NULL, p_operation_id, 'tenant.purge')
    ON CONFLICT DO NOTHING;

    INSERT INTO moa.tenant_purge_operations
        (tenant_id, operation_id, status, current_stage)
    VALUES (p_tenant_id, p_operation_id, 'in_progress', 'authz')
    ON CONFLICT (tenant_id) DO NOTHING;

    SELECT operation_id INTO existing_operation
    FROM moa.tenant_purge_operations
    WHERE tenant_id = p_tenant_id
    FOR UPDATE;
    IF existing_operation <> p_operation_id THEN
        RAISE EXCEPTION 'tenant purge progress belongs to another operation'
            USING ERRCODE = '55000';
    END IF;
END;
$$;
ALTER FUNCTION moa.start_tenant_purge(UUID, TEXT) OWNER TO moa_owner;
REVOKE ALL ON FUNCTION moa.start_tenant_purge(UUID, TEXT) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION moa.start_tenant_purge(UUID, TEXT) TO moa_app, moa_promoter;

-- One statement trigger derives all tenant ids touched by the statement.  It
-- takes each shared lock once, in UUID order, then rejects a fenced write.  An
-- UPDATE derives both OLD and NEW tenants so moving a row cannot escape a
-- tenant whose fence already exists.
CREATE FUNCTION moa.guard_tenant_write_statement()
RETURNS TRIGGER
LANGUAGE plpgsql
SET search_path = pg_catalog, public, moa, analytics, pii_vault
AS $$
DECLARE
    source_sql TEXT;
    tenant_sql TEXT;
    touched_tenant UUID;
    maintenance_update BOOLEAN := FALSE;
    tenant_fenced BOOLEAN;
    purge_bypass BOOLEAN;
BEGIN
    IF TG_OP = 'INSERT' THEN
        source_sql := 'SELECT * FROM tenant_purge_new_rows';
    ELSIF TG_OP = 'UPDATE' THEN
        source_sql :=
            'SELECT * FROM tenant_purge_new_rows UNION ALL SELECT * FROM tenant_purge_old_rows';
    ELSE
        RAISE EXCEPTION 'unsupported tenant fence trigger operation %', TG_OP;
    END IF;

    IF TG_ARGV[0] = 'tenant_id' THEN
        tenant_sql := format(
            'SELECT DISTINCT tenant_id FROM (%s) AS changed WHERE tenant_id IS NOT NULL',
            source_sql
        );
    ELSIF TG_ARGV[0] = 'storage_partition_id' THEN
        tenant_sql := format(
            'SELECT DISTINCT storage_partition_id::UUID FROM (%s) AS changed WHERE storage_partition_id IS NOT NULL',
            source_sql
        );
    ELSIF TG_ARGV[0] = 'tenant_primary_key' THEN
        tenant_sql := format(
            'SELECT DISTINCT id FROM (%s) AS changed WHERE id IS NOT NULL',
            source_sql
        );
    ELSIF TG_ARGV[0] = 'auth0_ciba_approval' THEN
        tenant_sql := format(
            'SELECT DISTINCT tenant_id FROM ('
            'SELECT session_row.tenant_id FROM (%s) changed '
            'JOIN public.sessions session_row ON session_row.id = changed.session_id '
            'UNION ALL '
            'SELECT user_row.tenant_id FROM (%s) changed '
            'JOIN public.users user_row ON user_row.id = changed.deciding_user_id'
            ') tenants',
            source_sql,
            source_sql
        );
    ELSIF TG_ARGV[0] = 'scim_group_member' THEN
        tenant_sql := format(
            'SELECT DISTINCT tenant_id FROM ('
            'SELECT user_row.tenant_id FROM (%s) changed '
            'JOIN public.users user_row ON user_row.id = changed.user_id '
            'UNION ALL '
            'SELECT group_row.tenant_id FROM (%s) changed '
            'JOIN public.scim_groups group_row ON group_row.id = changed.group_id'
            ') tenants',
            source_sql,
            source_sql
        );
    ELSIF TG_ARGV[0] = 'api_key_revocation' THEN
        tenant_sql := format(
            'SELECT DISTINCT key_row.tenant_id FROM (%s) changed '
            'JOIN public.api_keys key_row ON key_row.id = changed.api_key_id',
            source_sql
        );
    ELSIF TG_ARGV[0] = 'session_event_dedupe' THEN
        tenant_sql := format(
            'SELECT DISTINCT session_row.tenant_id FROM (%s) changed '
            'JOIN public.sessions session_row ON session_row.id = changed.session_id',
            source_sql
        );
    ELSE
        RAISE EXCEPTION 'unknown tenant purge scope mode %', TG_ARGV[0];
    END IF;

    -- The lineage acceptance queue is immutable durable input plus lease/retry
    -- bookkeeping. A row accepted before fencing must still be claimable so the
    -- writer can suppress/dequeue the fenced row without stranding an unfenced
    -- neighbour from the same batch. Only updates that preserve every durable
    -- identity and payload field qualify; INSERTs and payload/scope rewrites
    -- remain ordinary writes and are rejected below.
    IF TG_OP = 'UPDATE'
       AND TG_TABLE_SCHEMA = 'analytics'
       AND TG_TABLE_NAME = 'lineage_journal'
    THEN
        SELECT NOT EXISTS (
            SELECT 1
            FROM tenant_purge_old_rows AS old_row
            FULL JOIN tenant_purge_new_rows AS new_row USING (journal_id)
            WHERE old_row.journal_id IS NULL
               OR new_row.journal_id IS NULL
               OR old_row.storage_partition_id IS DISTINCT FROM new_row.storage_partition_id
               OR old_row.user_id IS DISTINCT FROM new_row.user_id
               OR old_row.event_class IS DISTINCT FROM new_row.event_class
               OR old_row.payload IS DISTINCT FROM new_row.payload
               OR old_row.accepted_at IS DISTINCT FROM new_row.accepted_at
        ) INTO maintenance_update;
    END IF;

    FOR touched_tenant IN EXECUTE tenant_sql LOOP
        PERFORM pg_advisory_xact_lock_shared(
            hashtextextended('moa:destruction:tenant:' || touched_tenant::TEXT, 0)
        );
        tenant_fenced := moa.tenant_write_fenced(touched_tenant);
        purge_bypass := FALSE;
        IF tenant_fenced AND current_user = 'moa_owner' THEN
            purge_bypass := moa.tenant_purge_bypass_valid(touched_tenant);
        END IF;
        IF tenant_fenced
           AND NOT maintenance_update
           AND NOT purge_bypass
        THEN
            RAISE EXCEPTION 'tenant write refused: destruction is fenced for tenant %',
                touched_tenant
                USING ERRCODE = '55000';
        END IF;
    END LOOP;
    RETURN NULL;
END;
$$;

DO $$
DECLARE
    catalog_row RECORD;
BEGIN
    FOR catalog_row IN
        SELECT table_schema, table_name, scope_mode
        FROM moa.tenant_purge_catalog
        WHERE scope_mode IN (
            'tenant_id',
            'storage_partition_id',
            'tenant_primary_key',
            'auth0_ciba_approval',
            'scim_group_member',
            'api_key_revocation',
            'session_event_dedupe'
        )
          AND NOT (table_schema = 'public' AND table_name = 'authz_outbox')
        ORDER BY stage_order
    LOOP
        EXECUTE format(
            'CREATE TRIGGER moa_tenant_purge_fence_insert '
            'AFTER INSERT ON %I.%I '
            'REFERENCING NEW TABLE AS tenant_purge_new_rows '
            'FOR EACH STATEMENT EXECUTE FUNCTION moa.guard_tenant_write_statement(%L)',
            catalog_row.table_schema,
            catalog_row.table_name,
            catalog_row.scope_mode
        );
        EXECUTE format(
            'CREATE TRIGGER moa_tenant_purge_fence_update '
            'AFTER UPDATE ON %I.%I '
            'REFERENCING OLD TABLE AS tenant_purge_old_rows '
            'NEW TABLE AS tenant_purge_new_rows '
            'FOR EACH STATEMENT EXECUTE FUNCTION moa.guard_tenant_write_statement(%L)',
            catalog_row.table_schema,
            catalog_row.table_name,
            catalog_row.scope_mode
        );
    END LOOP;
END;
$$;

-- Outbox tuple identity and tenant attribution are immutable.  Once fenced,
-- ordinary desired writes are refused while delete delivery may continue to
-- lease and settle.  Only the validated purge function may invert write to
-- delete, even if an application connection spoofs the operation GUC.
CREATE FUNCTION moa.guard_authz_outbox_during_tenant_purge()
RETURNS TRIGGER
LANGUAGE plpgsql
SET search_path = pg_catalog, public, moa
AS $$
DECLARE
    touched_tenant UUID;
    bypass_valid BOOLEAN;
BEGIN
    IF TG_OP = 'UPDATE' AND (
        NEW.id <> OLD.id
        OR NEW.tuple_user <> OLD.tuple_user
        OR NEW.tuple_relation <> OLD.tuple_relation
        OR NEW.tuple_object <> OLD.tuple_object
        OR NEW.model_version <> OLD.model_version
        OR NEW.tenant_id IS DISTINCT FROM OLD.tenant_id
    ) THEN
        RAISE EXCEPTION 'authz outbox tuple identity and tenant attribution are immutable'
            USING ERRCODE = '55000';
    END IF;

    touched_tenant := COALESCE(NEW.tenant_id, CASE WHEN TG_OP = 'UPDATE' THEN OLD.tenant_id END);
    IF touched_tenant IS NULL THEN
        RETURN NEW;
    END IF;
    PERFORM pg_advisory_xact_lock_shared(
        hashtextextended('moa:destruction:tenant:' || touched_tenant::TEXT, 0)
    );
    IF NOT EXISTS (
        SELECT 1 FROM moa.destruction_operation_fence
        WHERE tenant_id = touched_tenant
          AND subject_id IS NULL
          AND status = 'in_progress'
    ) THEN
        RETURN NEW;
    END IF;

    bypass_valid := moa.tenant_purge_bypass_valid(touched_tenant);
    IF TG_OP = 'INSERT' AND NEW.op = 'write' THEN
        RAISE EXCEPTION 'authz desired write refused during tenant purge'
            USING ERRCODE = '55000';
    END IF;
    IF TG_OP = 'UPDATE' AND OLD.op = 'write' AND NEW.op = 'delete' AND NOT bypass_valid THEN
        RAISE EXCEPTION 'authz write-to-delete transition requires validated tenant purge'
            USING ERRCODE = '55000';
    END IF;
    IF NEW.op = 'write' AND NOT bypass_valid THEN
        RAISE EXCEPTION 'authz desired write refused during tenant purge'
            USING ERRCODE = '55000';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER authz_outbox_tenant_purge_guard
BEFORE INSERT OR UPDATE ON authz_outbox
FOR EACH ROW EXECUTE FUNCTION moa.guard_authz_outbox_during_tenant_purge();

-- Invert one actual tuple page.  The cursor advances across every tenant row,
-- not only changed rows, so work is O(actual tuples).  A later finalization
-- reset catches any delete that dead-letters behind the cursor.
CREATE FUNCTION moa.invert_tenant_authz_batch(
    p_tenant_id UUID,
    p_operation_id TEXT
) RETURNS TABLE (
    scanned INTEGER,
    inverted INTEGER,
    exhausted BOOLEAN,
    next_cursor UUID
)
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    p_limit CONSTANT INTEGER := 1000;
    progress_row moa.tenant_purge_operations%ROWTYPE;
BEGIN
    PERFORM pg_advisory_xact_lock(
        hashtextextended('moa:destruction:tenant:' || p_tenant_id::TEXT, 0)
    );
    SELECT * INTO progress_row
    FROM moa.tenant_purge_operations
    WHERE tenant_id = p_tenant_id
    FOR UPDATE;
    IF NOT FOUND
       OR progress_row.operation_id <> p_operation_id
       OR progress_row.status <> 'in_progress'
       OR progress_row.current_stage <> 'authz'
       OR NOT EXISTS (
            SELECT 1 FROM moa.destruction_operation_fence
            WHERE tenant_id = p_tenant_id
              AND subject_id IS NULL
              AND operation_id = p_operation_id
              AND operation_kind = 'tenant.purge'
              AND status = 'in_progress'
       )
    THEN
        RAISE EXCEPTION 'tenant purge authz batch does not own progress and fence'
            USING ERRCODE = '55000';
    END IF;
    PERFORM set_config('moa.tenant_purge_operation_id', p_operation_id, true);

    WITH batch AS MATERIALIZED (
        SELECT id
        FROM public.authz_outbox
        WHERE tenant_id = p_tenant_id
          AND (progress_row.authz_cursor IS NULL OR id > progress_row.authz_cursor)
        ORDER BY id
        LIMIT p_limit
        FOR UPDATE
    ),
    changed AS (
        UPDATE public.authz_outbox AS outbox
        SET op = 'delete',
            generation = outbox.generation + 1,
            status = 'pending',
            attempts = 0,
            last_error = NULL,
            lease_token = NULL,
            lease_expires_at = NULL,
            next_attempt_at = now(),
            updated_at = now()
        FROM batch
        WHERE outbox.id = batch.id
          AND (outbox.op = 'write' OR outbox.status = 'dead_letter')
        RETURNING outbox.id
    )
    SELECT count(*)::INTEGER,
           count(changed.id)::INTEGER,
           (array_agg(batch.id ORDER BY batch.id DESC))[1]
    INTO scanned, inverted, next_cursor
    FROM batch
    LEFT JOIN changed USING (id);

    exhausted := scanned = 0;
    IF exhausted THEN
        SELECT stage_name INTO progress_row.current_stage
        FROM moa.tenant_purge_catalog
        ORDER BY stage_order
        LIMIT 1;
        UPDATE moa.tenant_purge_operations
        SET current_stage = progress_row.current_stage,
            stage_deleted_count = 0,
            batch_count = batch_count + 1,
            updated_at = now()
        WHERE tenant_id = p_tenant_id;
    ELSE
        UPDATE moa.tenant_purge_operations
        SET authz_cursor = next_cursor,
            batch_count = batch_count + 1,
            updated_at = now()
        WHERE tenant_id = p_tenant_id;
    END IF;
    RETURN NEXT;
END;
$$;
ALTER FUNCTION moa.invert_tenant_authz_batch(UUID, TEXT) OWNER TO moa_owner;
REVOKE ALL ON FUNCTION moa.invert_tenant_authz_batch(UUID, TEXT) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION moa.invert_tenant_authz_batch(UUID, TEXT)
    TO moa_app, moa_promoter;

-- Executes exactly one catalog batch.  The function is the only relational
-- bypass and validates the progress row and destruction fence after taking the
-- exclusive tenant lock.  CTIDs are selected and consumed in this transaction;
-- they are never persisted or returned to Rust.
CREATE FUNCTION moa.run_tenant_purge_batch(
    p_tenant_id UUID,
    p_operation_id TEXT
) RETURNS TABLE (
    batch_state TEXT,
    stage TEXT,
    affected BIGINT
)
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    p_limit CONSTANT INTEGER := 1000;
    progress_row moa.tenant_purge_operations%ROWTYPE;
    catalog_row moa.tenant_purge_catalog%ROWTYPE;
    next_stage TEXT;
    qualified_table TEXT;
    predicate TEXT;
    statement TEXT;
    catalog_count INTEGER;
    drift TEXT[];
    residue BIGINT;
BEGIN
    PERFORM set_config('lock_timeout', '1s', true);
    PERFORM pg_advisory_xact_lock(
        hashtextextended('moa:destruction:tenant:' || p_tenant_id::TEXT, 0)
    );
    SELECT * INTO progress_row
    FROM moa.tenant_purge_operations
    WHERE tenant_id = p_tenant_id
    FOR UPDATE;
    IF NOT FOUND OR progress_row.operation_id <> p_operation_id THEN
        RAISE EXCEPTION 'tenant purge batch does not own progress'
            USING ERRCODE = '55000';
    END IF;
    IF progress_row.status = 'relationally_committed' THEN
        batch_state := 'already_committed';
        stage := 'complete';
        affected := 0;
        RETURN NEXT;
        RETURN;
    END IF;
    IF progress_row.current_stage = 'authz' THEN
        RAISE EXCEPTION 'tenant purge authz stage must be drained first'
            USING ERRCODE = '55000';
    END IF;
    IF NOT EXISTS (
        SELECT 1 FROM moa.destruction_operation_fence
        WHERE tenant_id = p_tenant_id
          AND subject_id IS NULL
          AND operation_id = p_operation_id
          AND operation_kind = 'tenant.purge'
          AND status = 'in_progress'
    ) THEN
        RAISE EXCEPTION 'tenant purge batch does not own destruction fence'
            USING ERRCODE = '55000';
    END IF;
    PERFORM set_config('moa.tenant_purge_operation_id', p_operation_id, true);
    PERFORM set_config('moa.events_maintenance', 'on', true);

    SELECT * INTO catalog_row
    FROM moa.tenant_purge_catalog
    WHERE stage_name = progress_row.current_stage;

    IF NOT FOUND THEN
        RAISE EXCEPTION 'unknown tenant purge stage %', progress_row.current_stage
            USING ERRCODE = '55000';
    END IF;

        qualified_table := format('%I.%I', catalog_row.table_schema, catalog_row.table_name);
        predicate := CASE catalog_row.scope_mode
            WHEN 'tenant_id' THEN 'target.tenant_id = $1'
            WHEN 'storage_partition_id' THEN 'target.storage_partition_id = $1::TEXT'
            WHEN 'tenant_primary_key' THEN 'target.id = $1'
            WHEN 'auth0_ciba_approval' THEN
                '(EXISTS (SELECT 1 FROM public.sessions session_row '
                'WHERE session_row.id = target.session_id AND session_row.tenant_id = $1) '
                'OR EXISTS (SELECT 1 FROM public.users user_row '
                'WHERE user_row.id = target.deciding_user_id AND user_row.tenant_id = $1))'
            WHEN 'scim_group_member' THEN
                '(EXISTS (SELECT 1 FROM public.users user_row '
                'WHERE user_row.id = target.user_id AND user_row.tenant_id = $1) '
                'OR EXISTS (SELECT 1 FROM public.scim_groups group_row '
                'WHERE group_row.id = target.group_id AND group_row.tenant_id = $1))'
            WHEN 'api_key_revocation' THEN
                'EXISTS (SELECT 1 FROM public.api_keys key_row '
                'WHERE key_row.id = target.api_key_id AND key_row.tenant_id = $1)'
            WHEN 'session_event_dedupe' THEN
                'EXISTS (SELECT 1 FROM public.sessions session_row '
                'WHERE session_row.id = target.session_id AND session_row.tenant_id = $1)'
            WHEN 'external_owner' THEN 'target.tenant_id = $1'
            ELSE NULL
        END;
        affected := 0;

        IF catalog_row.action_mode = 'clear_then_delete' THEN
            IF catalog_row.table_schema = 'moa' AND catalog_row.table_name = 'knowledge_objects' THEN
                statement := format(
                    'WITH batch AS ('
                    'SELECT target.tableoid AS row_tableoid, target.ctid AS row_ctid FROM %s target WHERE %s '
                    'AND target.current_acl_snapshot_id IS NOT NULL '
                    'LIMIT $2 FOR UPDATE'
                    ') UPDATE %s target SET acl_state = ''incomplete'', '
                    'acl_revision = NULL, current_acl_snapshot_id = NULL '
                    'FROM batch WHERE target.tableoid = batch.row_tableoid AND target.ctid = batch.row_ctid',
                    qualified_table,
                    predicate,
                    qualified_table
                );
            ELSIF catalog_row.table_schema = 'moa' AND catalog_row.table_name = 'artifact' THEN
                statement := format(
                    'WITH batch AS ('
                    'SELECT target.tableoid AS row_tableoid, target.ctid AS row_ctid FROM %s target WHERE %s '
                    'AND target.latest_revision_uid IS NOT NULL '
                    'LIMIT $2 FOR UPDATE'
                    ') UPDATE %s target SET latest_revision_uid = NULL '
                    'FROM batch WHERE target.tableoid = batch.row_tableoid AND target.ctid = batch.row_ctid',
                    qualified_table,
                    predicate,
                    qualified_table
                );
            ELSIF catalog_row.table_schema = 'public' AND catalog_row.table_name = 'sessions' THEN
                statement := format(
                    'WITH batch AS ('
                    'SELECT target.tableoid AS row_tableoid, target.ctid AS row_ctid FROM %s target WHERE %s '
                    'AND target.active_channel_binding_id IS NOT NULL '
                    'LIMIT $2 FOR UPDATE'
                    ') UPDATE %s target SET active_channel_binding_id = NULL '
                    'FROM batch WHERE target.tableoid = batch.row_tableoid AND target.ctid = batch.row_ctid',
                    qualified_table,
                    predicate,
                    qualified_table
                );
            ELSE
                RAISE EXCEPTION 'unknown clear-then-delete catalog stage %', catalog_row.stage_name;
            END IF;
            EXECUTE statement USING p_tenant_id, p_limit;
            GET DIAGNOSTICS affected = ROW_COUNT;

            -- A pointer-owning parent cannot be deleted until the child row
            -- that the pointer selected has drained. Keep all work under this
            -- one durable stage and one 1,000-row transaction: first clear the
            -- pointer, then drain the non-cascading child, then let the generic
            -- parent delete below run. The child catalog rows remain as an
            -- independent later zero-residue proof.
            IF affected = 0
               AND catalog_row.table_schema = 'moa'
               AND catalog_row.table_name = 'knowledge_objects'
            THEN
                WITH batch AS (
                    SELECT target.tableoid AS row_tableoid, target.ctid AS row_ctid
                    FROM moa.knowledge_source_acl_entries AS target
                    WHERE target.tenant_id = p_tenant_id
                    LIMIT p_limit
                    FOR UPDATE
                )
                DELETE FROM moa.knowledge_source_acl_entries AS target
                USING batch
                WHERE target.tableoid = batch.row_tableoid
                  AND target.ctid = batch.row_ctid;
                GET DIAGNOSTICS affected = ROW_COUNT;

                IF affected = 0 THEN
                    WITH batch AS (
                        SELECT target.tableoid AS row_tableoid, target.ctid AS row_ctid
                        FROM moa.knowledge_source_acl_snapshots AS target
                        WHERE target.tenant_id = p_tenant_id
                        LIMIT p_limit
                        FOR UPDATE
                    )
                    DELETE FROM moa.knowledge_source_acl_snapshots AS target
                    USING batch
                    WHERE target.tableoid = batch.row_tableoid
                      AND target.ctid = batch.row_ctid;
                    GET DIAGNOSTICS affected = ROW_COUNT;
                END IF;
            ELSIF affected = 0
               AND catalog_row.table_schema = 'moa'
               AND catalog_row.table_name = 'artifact'
            THEN
                WITH batch AS (
                    SELECT target.tableoid AS row_tableoid, target.ctid AS row_ctid
                    FROM moa.artifact_revision AS target
                    WHERE target.storage_partition_id = p_tenant_id::TEXT
                    LIMIT p_limit
                    FOR UPDATE
                )
                DELETE FROM moa.artifact_revision AS target
                USING batch
                WHERE target.tableoid = batch.row_tableoid
                  AND target.ctid = batch.row_ctid;
                GET DIAGNOSTICS affected = ROW_COUNT;
            END IF;
        ELSIF catalog_row.action_mode = 'redact_kek' THEN
            WITH batch AS (
                SELECT target.tableoid AS row_tableoid, target.ctid AS row_ctid
                FROM moa.kek AS target
                WHERE target.tenant_id = p_tenant_id
                  AND (target.wrapped_kek IS NOT NULL OR target.destroyed_at IS NULL)
                LIMIT p_limit
                FOR UPDATE
            )
            UPDATE moa.kek AS target
            SET wrapped_kek = NULL,
                destroyed_at = COALESCE(target.destroyed_at, now())
            FROM batch
            WHERE target.tableoid = batch.row_tableoid
              AND target.ctid = batch.row_ctid;
            GET DIAGNOSTICS affected = ROW_COUNT;
        ELSIF catalog_row.action_mode = 'redact_legal_hold' THEN
            WITH batch AS (
                SELECT target.tableoid AS row_tableoid, target.ctid AS row_ctid
                FROM moa.legal_hold AS target
                WHERE target.tenant_id = p_tenant_id
                  AND target.released_at IS NOT NULL
                  AND (
                    target.subject_id IS NOT NULL
                    OR target.reason <> '[REDACTED]'
                    OR target.placed_by <> '[REDACTED]'
                    OR target.released_by <> '[REDACTED]'
                  )
                LIMIT p_limit
                FOR UPDATE
            )
            UPDATE moa.legal_hold AS target
            SET subject_id = NULL,
                reason = '[REDACTED]',
                placed_by = '[REDACTED]',
                released_by = '[REDACTED]'
            FROM batch
            WHERE target.tableoid = batch.row_tableoid
              AND target.ctid = batch.row_ctid;
            GET DIAGNOSTICS affected = ROW_COUNT;
        ELSIF catalog_row.action_mode IN ('retain_control', 'assert_empty') THEN
            affected := 0;
            IF catalog_row.action_mode = 'assert_empty' THEN
                statement := format('SELECT count(*) FROM %s target WHERE %s', qualified_table, predicate);
                EXECUTE statement INTO residue USING p_tenant_id;
                IF residue <> 0 THEN
                    RAISE EXCEPTION 'external owner left % rows in %', residue, catalog_row.stage_name
                        USING ERRCODE = '55000';
                END IF;
            END IF;
        END IF;

        IF affected = 0 AND catalog_row.action_mode IN ('delete', 'clear_then_delete') THEN
            statement := format(
                'WITH batch AS ('
                'SELECT target.tableoid AS row_tableoid, target.ctid AS row_ctid FROM %s target WHERE %s '
                'LIMIT $2 FOR UPDATE'
                ') DELETE FROM %s target USING batch '
                'WHERE target.tableoid = batch.row_tableoid AND target.ctid = batch.row_ctid',
                qualified_table,
                predicate,
                qualified_table
            );
            EXECUTE statement USING p_tenant_id, p_limit;
            GET DIAGNOSTICS affected = ROW_COUNT;
        END IF;

        IF affected > 0 THEN
            UPDATE moa.tenant_purge_operations
            SET stage_deleted_count = stage_deleted_count + affected,
                total_deleted_count = total_deleted_count + affected,
                batch_count = batch_count + 1,
                updated_at = now()
            WHERE tenant_id = p_tenant_id;
            batch_state := 'in_progress';
            stage := catalog_row.stage_name;
            RETURN NEXT;
            RETURN;
        END IF;

        SELECT stage_name INTO next_stage
        FROM moa.tenant_purge_catalog
        WHERE stage_order > catalog_row.stage_order
        ORDER BY stage_order
        LIMIT 1;
        IF next_stage IS NOT NULL THEN
            UPDATE moa.tenant_purge_operations
            SET current_stage = next_stage,
                stage_deleted_count = 0,
                batch_count = batch_count + 1,
                updated_at = now()
            WHERE tenant_id = p_tenant_id;
            batch_state := 'in_progress';
            stage := next_stage;
            affected := 0;
            RETURN NEXT;
            RETURN;
        END IF;
    -- A dead-lettered delete can appear after its UUID was passed. Resetting the
    -- cursor is required; otherwise no future keyset page could see it.
    IF EXISTS (
        SELECT 1 FROM public.authz_outbox
        WHERE tenant_id = p_tenant_id
          AND (op = 'write' OR status = 'dead_letter')
    ) THEN
        UPDATE moa.tenant_purge_operations
        SET current_stage = 'authz',
            authz_cursor = NULL,
            stage_deleted_count = 0,
            batch_count = batch_count + 1,
            updated_at = now()
        WHERE tenant_id = p_tenant_id;
        batch_state := 'in_progress';
        stage := 'authz';
        affected := 0;
        RETURN NEXT;
        RETURN;
    END IF;

    SELECT count(*) INTO catalog_count FROM moa.tenant_purge_catalog;
    IF catalog_count <> 127 THEN
        RAISE EXCEPTION 'tenant purge catalog must contain exactly 127 tables, found %', catalog_count
            USING ERRCODE = '55000';
    END IF;
    SELECT array_agg(format('%I.%I', namespace.nspname, table_row.relname) ORDER BY 1)
    INTO drift
    FROM pg_class AS table_row
    JOIN pg_namespace AS namespace ON namespace.oid = table_row.relnamespace
    JOIN pg_attribute AS column_row ON column_row.attrelid = table_row.oid
    WHERE table_row.relkind IN ('r', 'p')
      AND NOT table_row.relispartition
      AND namespace.nspname IN ('public', 'moa', 'analytics', 'pii_vault')
      AND column_row.attnum > 0
      AND NOT column_row.attisdropped
      AND column_row.attname IN ('tenant_id', 'storage_partition_id')
      AND NOT (
        namespace.nspname = 'moa'
        AND table_row.relname IN (
            'simulator_certification_mandate',
            'simulator_certification_evidence_import'
        )
      )
      AND NOT EXISTS (
        SELECT 1 FROM moa.tenant_purge_catalog AS catalog
        WHERE catalog.table_schema = namespace.nspname
          AND catalog.table_name = table_row.relname
      );
    IF drift IS NOT NULL THEN
        RAISE EXCEPTION 'tenant purge catalog drift: %', drift
            USING ERRCODE = '55000';
    END IF;

    -- The stage sequence proved each table at zero while the fence prevented
    -- re-creation. Recheck control residue and the two intentional redactions in
    -- this same final transaction before marking relational completion.
    IF EXISTS (
        SELECT 1 FROM public.authz_outbox
        WHERE tenant_id = p_tenant_id
          AND (op <> 'delete' OR status NOT IN ('pending', 'in_flight', 'succeeded'))
    ) OR EXISTS (
        SELECT 1 FROM moa.kek
        WHERE tenant_id = p_tenant_id
          AND (wrapped_kek IS NOT NULL OR destroyed_at IS NULL)
    ) OR EXISTS (
        SELECT 1 FROM moa.legal_hold
        WHERE tenant_id = p_tenant_id
          AND (
            released_at IS NULL
            OR subject_id IS NOT NULL
            OR reason <> '[REDACTED]'
            OR placed_by <> '[REDACTED]'
            OR released_by <> '[REDACTED]'
          )
    ) THEN
        RAISE EXCEPTION 'tenant purge final residue proof failed'
            USING ERRCODE = '55000';
    END IF;

    UPDATE moa.tenant_purge_operations
    SET status = 'relationally_committed',
        current_stage = 'complete',
        stage_deleted_count = 0,
        batch_count = batch_count + 1,
        updated_at = now(),
        relationally_committed_at = now()
    WHERE tenant_id = p_tenant_id;
    batch_state := 'committed';
    stage := 'complete';
    affected := 0;
    RETURN NEXT;
END;
$$;
ALTER FUNCTION moa.run_tenant_purge_batch(UUID, TEXT) OWNER TO moa_owner;
REVOKE ALL ON FUNCTION moa.run_tenant_purge_batch(UUID, TEXT) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION moa.run_tenant_purge_batch(UUID, TEXT)
    TO moa_app, moa_promoter;
