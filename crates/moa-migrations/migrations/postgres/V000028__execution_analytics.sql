-- Execution observability, audit, analytics, and export state.
--
-- The execution-runs migration owns the normalized execution schema. This
-- migration adds the validation helpers, immutable audits, analytics surfaces,
-- and export state that depend on it.

CREATE OR REPLACE FUNCTION moa.execution_canonical_json(candidate JSONB)
RETURNS TEXT
LANGUAGE plpgsql
IMMUTABLE
STRICT
AS $$
DECLARE
    rendered TEXT;
BEGIN
    CASE jsonb_typeof(candidate)
        WHEN 'object' THEN
            SELECT '{' || COALESCE(string_agg(
                       to_json(key)::TEXT || ':' || moa.execution_canonical_json(value),
                       ',' ORDER BY key COLLATE "C"
                   ), '') || '}'
            INTO rendered
            FROM jsonb_each(candidate);
        WHEN 'array' THEN
            SELECT '[' || COALESCE(string_agg(
                       moa.execution_canonical_json(value),
                       ',' ORDER BY ordinal
                   ), '') || ']'
            INTO rendered
            FROM jsonb_array_elements(candidate) WITH ORDINALITY item(value, ordinal);
        WHEN 'string' THEN
            rendered := to_json(candidate #>> '{}')::TEXT;
        WHEN 'number' THEN
            rendered := trim_scale((candidate #>> '{}')::NUMERIC)::TEXT;
            IF rendered = '-0' THEN
                rendered := '0';
            END IF;
        WHEN 'boolean' THEN
            rendered := candidate::TEXT;
        WHEN 'null' THEN
            rendered := 'null';
        ELSE
            RAISE EXCEPTION 'unsupported JSON kind';
    END CASE;
    RETURN rendered;
END;
$$;

CREATE OR REPLACE FUNCTION moa.execution_json_text_is_canonical(candidate TEXT)
RETURNS BOOLEAN
LANGUAGE plpgsql
IMMUTABLE
STRICT
AS $$
DECLARE
    parsed JSON;
BEGIN
    parsed := candidate::JSON;
    RETURN parsed::TEXT = moa.execution_canonical_json(parsed::JSONB);
EXCEPTION
    WHEN OTHERS THEN
        RETURN FALSE;
END;
$$;

CREATE OR REPLACE FUNCTION moa.execution_audit_report_is_valid(
    candidate TEXT,
    allow_oversized BOOLEAN
) RETURNS BOOLEAN
LANGUAGE plpgsql
IMMUTABLE
STRICT
AS $$
DECLARE
    report JSONB;
    report_kind TEXT;
    violation JSONB;
    previous_code TEXT;
    previous_path TEXT;
    previous_message TEXT;
    current_code TEXT;
    current_path TEXT;
    current_message TEXT;
BEGIN
    IF octet_length(candidate) > 262144
       OR NOT moa.execution_json_text_is_canonical(candidate) THEN
        RETURN FALSE;
    END IF;
    report := candidate::JSONB;
    report_kind := report ->> 'kind';
    IF report_kind IN ('schema', 'compiler') THEN
        IF NOT moa.execution_json_object_has_exact_keys(
            report,
            ARRAY['kind','violations','omitted_violations','full_report_hash']
        )
           OR jsonb_typeof(report -> 'violations') <> 'array'
           OR jsonb_array_length(report -> 'violations') > 256
           OR jsonb_typeof(report -> 'omitted_violations') <> 'number'
           OR (report ->> 'omitted_violations') !~ '^[0-9]+$'
           OR (report ->> 'omitted_violations')::NUMERIC > 4294967295
           OR COALESCE(report ->> 'full_report_hash', '') !~ '^[0-9a-f]{64}$' THEN
            RETURN FALSE;
        END IF;
        FOR violation IN
            SELECT value
            FROM jsonb_array_elements(report -> 'violations') item(value)
        LOOP
            IF NOT moa.execution_json_object_has_exact_keys(
                violation, ARRAY['code','path','message']
            )
               OR jsonb_typeof(violation -> 'code') <> 'string'
               OR jsonb_typeof(violation -> 'path') <> 'string'
               OR jsonb_typeof(violation -> 'message') <> 'string'
               OR octet_length(violation ->> 'code') > 64
               OR octet_length(violation ->> 'path') > 512
               OR octet_length(violation ->> 'message') > 512 THEN
                RETURN FALSE;
            END IF;
            current_code := violation ->> 'code';
            current_path := violation ->> 'path';
            current_message := violation ->> 'message';
            IF previous_code IS NOT NULL
               AND ROW(
                   current_code COLLATE "C",
                   current_path COLLATE "C",
                   current_message COLLATE "C"
               ) < ROW(
                   previous_code COLLATE "C",
                   previous_path COLLATE "C",
                   previous_message COLLATE "C"
               ) THEN
                RETURN FALSE;
            END IF;
            previous_code := current_code;
            previous_path := current_path;
            previous_message := current_message;
        END LOOP;
        RETURN TRUE;
    END IF;
    IF report_kind = 'oversized' THEN
        RETURN allow_oversized
           AND moa.execution_json_object_has_exact_keys(
               report,
               ARRAY['kind','field','limit_bytes','observed_bytes','content_hash']
           )
           AND report ->> 'field' = 'candidate'
           AND jsonb_typeof(report -> 'limit_bytes') = 'number'
           AND jsonb_typeof(report -> 'observed_bytes') = 'number'
           AND (report ->> 'limit_bytes') ~ '^[0-9]+$'
           AND (report ->> 'observed_bytes') ~ '^[0-9]+$'
           AND (report ->> 'limit_bytes')::NUMERIC = 1048576
           AND (report ->> 'observed_bytes')::NUMERIC > 1048576
           AND COALESCE(report ->> 'content_hash', '') ~ '^[0-9a-f]{64}$';
    END IF;
    RETURN FALSE;
EXCEPTION
    WHEN OTHERS THEN
        RETURN FALSE;
END;
$$;

CREATE OR REPLACE FUNCTION moa.execution_traceparent_is_valid(candidate TEXT)
RETURNS BOOLEAN
LANGUAGE plpgsql
IMMUTABLE
STRICT
AS $$
DECLARE
    flags INTEGER;
BEGIN
    IF octet_length(candidate) <> 55
       OR candidate !~ '^00-[0-9a-f]{32}-[0-9a-f]{16}-[0-9a-f]{2}$'
       OR substring(candidate FROM 4 FOR 32) = repeat('0', 32)
       OR substring(candidate FROM 37 FOR 16) = repeat('0', 16) THEN
        RETURN FALSE;
    END IF;
    flags := get_byte(decode(substring(candidate FROM 54 FOR 2), 'hex'), 0);
    RETURN (flags & 252) = 0;
EXCEPTION
    WHEN OTHERS THEN
        RETURN FALSE;
END;
$$;

CREATE OR REPLACE FUNCTION moa.execution_tracestate_is_valid(candidate TEXT)
RETURNS BOOLEAN
LANGUAGE plpgsql
IMMUTABLE
STRICT
AS $$
DECLARE
    members TEXT[];
    member TEXT;
    trimmed TEXT;
    key_text TEXT;
    value_text TEXT;
    equals_at INTEGER;
    byte_value INTEGER;
    byte_index INTEGER;
    seen_keys TEXT[] := ARRAY[]::TEXT[];
BEGIN
    IF octet_length(candidate) > 512
       OR candidate <> convert_from(convert_to(candidate, 'UTF8'), 'UTF8') THEN
        RETURN FALSE;
    END IF;
    members := string_to_array(candidate, ',');
    IF cardinality(members) > 32 THEN
        RETURN FALSE;
    END IF;
    FOREACH member IN ARRAY members LOOP
        trimmed := regexp_replace(
            regexp_replace(member, '^[ \t]+', ''),
            '[ \t]+$', ''
        );
        IF trimmed = '' THEN
            CONTINUE;
        END IF;
        equals_at := strpos(trimmed, '=');
        IF equals_at <= 1 OR strpos(substring(trimmed FROM equals_at + 1), '=') > 0 THEN
            RETURN FALSE;
        END IF;
        key_text := substring(trimmed FROM 1 FOR equals_at - 1);
        value_text := substring(trimmed FROM equals_at + 1);
        IF octet_length(key_text) NOT BETWEEN 1 AND 256
           OR key_text !~ '^[a-z0-9][a-z0-9_\-*/@]*$'
           OR key_text = ANY(seen_keys)
           OR octet_length(value_text) NOT BETWEEN 1 AND 256 THEN
            RETURN FALSE;
        END IF;
        FOR byte_index IN 0..octet_length(value_text) - 1 LOOP
            byte_value := get_byte(convert_to(value_text, 'UTF8'), byte_index);
            IF byte_value < 32 OR byte_value > 126 OR byte_value IN (44, 61) THEN
                RETURN FALSE;
            END IF;
        END LOOP;
        IF get_byte(convert_to(value_text, 'UTF8'), octet_length(value_text) - 1) = 32 THEN
            RETURN FALSE;
        END IF;
        seen_keys := array_append(seen_keys, key_text);
    END LOOP;
    RETURN TRUE;
EXCEPTION
    WHEN OTHERS THEN
        RETURN FALSE;
END;
$$;

CREATE OR REPLACE FUNCTION moa.execution_tracestate_is_normalized(candidate TEXT)
RETURNS BOOLEAN
LANGUAGE sql
IMMUTABLE
STRICT
AS $$
    SELECT moa.execution_tracestate_is_valid(candidate)
       AND EXISTS (
           SELECT 1
           FROM unnest(string_to_array(candidate, ',')) member
           WHERE btrim(member, E' \t') <> ''
       )
$$;

CREATE OR REPLACE FUNCTION moa.execution_uuid_v5(namespace UUID, name BYTEA)
RETURNS UUID
LANGUAGE plpgsql
IMMUTABLE
STRICT
AS $$
DECLARE
    bytes BYTEA;
    hex TEXT;
BEGIN
    bytes := public.digest(uuid_send(namespace) || name, 'sha1');
    bytes := set_byte(bytes, 6, (get_byte(bytes, 6) & 15) | 80);
    bytes := set_byte(bytes, 8, (get_byte(bytes, 8) & 63) | 128);
    hex := encode(substring(bytes FROM 1 FOR 16), 'hex');
    RETURN (
        substring(hex, 1, 8) || '-' ||
        substring(hex, 9, 4) || '-' ||
        substring(hex, 13, 4) || '-' ||
        substring(hex, 17, 4) || '-' ||
        substring(hex, 21, 12)
    )::UUID;
END;
$$;

CREATE OR REPLACE FUNCTION moa.execution_audit_preimage(
    domain_name TEXT,
    fields TEXT[]
) RETURNS BYTEA
LANGUAGE plpgsql
IMMUTABLE
STRICT
AS $$
DECLARE
    field_value TEXT;
    bytes BYTEA := convert_to(domain_name, 'UTF8');
    encoded BYTEA;
BEGIN
    FOREACH field_value IN ARRAY fields LOOP
        IF field_value IS NULL THEN
            bytes := bytes || decode('00', 'hex');
        ELSE
            encoded := convert_to(field_value, 'UTF8');
            bytes := bytes || decode('01', 'hex') ||
                int4send(octet_length(encoded)) || encoded;
        END IF;
    END LOOP;
    RETURN bytes;
END;
$$;

CREATE OR REPLACE FUNCTION moa.execution_route_audit_uid(
    tenant_id UUID,
    contact_id UUID,
    session_id UUID,
    originating_sequence BIGINT,
    stage TEXT
) RETURNS UUID
LANGUAGE sql
IMMUTABLE
AS $$
    SELECT moa.execution_uuid_v5(
        '7b83c5c2-5cf7-5fa0-8eb6-2d7c6e0f1d11'::UUID,
        moa.execution_audit_preimage(
            'moa.execution.route-audit',
            ARRAY[
                lower(tenant_id::TEXT),
                lower(contact_id::TEXT),
                lower(session_id::TEXT),
                originating_sequence::TEXT,
                stage
            ]
        )
    )
$$;

CREATE OR REPLACE FUNCTION moa.execution_planner_audit_uid(
    tenant_id UUID,
    contact_id UUID,
    session_id UUID,
    originating_sequence BIGINT,
    run_uid UUID,
    plan_revision BIGINT,
    call_kind TEXT,
    call_ordinal SMALLINT
) RETURNS UUID
LANGUAGE sql
IMMUTABLE
AS $$
    SELECT moa.execution_uuid_v5(
        '7b83c5c2-5cf7-5fa0-8eb6-2d7c6e0f1d11'::UUID,
        moa.execution_audit_preimage(
            'moa.execution.planner-audit',
            ARRAY[
                lower(tenant_id::TEXT),
                lower(contact_id::TEXT),
                lower(session_id::TEXT),
                originating_sequence::TEXT,
                lower(run_uid::TEXT),
                plan_revision::TEXT,
                call_kind,
                call_ordinal::TEXT
            ]
        )
    )
$$;

CREATE OR REPLACE FUNCTION moa.execution_compile_audit_uid(
    tenant_id UUID,
    contact_id UUID,
    source TEXT,
    operation_key TEXT
) RETURNS UUID
LANGUAGE sql
IMMUTABLE
AS $$
    SELECT moa.execution_uuid_v5(
        '7b83c5c2-5cf7-5fa0-8eb6-2d7c6e0f1d11'::UUID,
        moa.execution_audit_preimage(
            'moa.execution.compile-audit',
            ARRAY[
                lower(tenant_id::TEXT),
                lower(contact_id::TEXT),
                source,
                operation_key
            ]
        )
    )
$$;

CREATE OR REPLACE FUNCTION moa.execution_route_provenance_is_valid(
    stage TEXT,
    decision TEXT,
    strategy TEXT,
    provenance JSONB
) RETURNS BOOLEAN
LANGUAGE plpgsql
IMMUTABLE
AS $$
DECLARE
    source_kind TEXT;
    classifier_outcome TEXT;
    usage JSONB;
    collected BOOLEAN;
    parsed BOOLEAN;
    route_valid BOOLEAN;
BEGIN
    IF jsonb_typeof(provenance) <> 'object'
       OR NOT moa.execution_json_object_has_exact_keys(
           provenance,
           ARRAY[
               'source','classifier_outcome','provider_model','prompt_version',
               'objective_hash','response_hash','confidence_bps',
               'missing_input_count','usage','cost_microusd','duration_micros'
           ]
       )
       OR COALESCE(provenance ->> 'objective_hash', '') !~ '^[0-9a-f]{64}$'
       OR jsonb_typeof(provenance -> 'missing_input_count') <> 'number'
       OR (provenance ->> 'missing_input_count') !~ '^[0-9]+$'
       OR (provenance ->> 'missing_input_count')::NUMERIC > 8
       OR jsonb_typeof(provenance -> 'cost_microusd') <> 'number'
       OR (provenance ->> 'cost_microusd') !~ '^[0-9]+$'
       OR (provenance ->> 'cost_microusd')::NUMERIC > 9223372036854775807
       OR jsonb_typeof(provenance -> 'duration_micros') <> 'number'
       OR (provenance ->> 'duration_micros') !~ '^[0-9]+$'
       OR (provenance ->> 'duration_micros')::NUMERIC > 9223372036854775807
       OR (
           provenance -> 'confidence_bps' <> 'null'::JSONB
           AND (
               jsonb_typeof(provenance -> 'confidence_bps') <> 'number'
               OR (provenance ->> 'confidence_bps') !~ '^[0-9]+$'
               OR (provenance ->> 'confidence_bps')::NUMERIC > 10000
           )
       ) THEN
        RETURN FALSE;
    END IF;
    usage := provenance -> 'usage';
    IF jsonb_typeof(usage) <> 'object'
       OR NOT moa.execution_json_object_has_exact_keys(
           usage,
           ARRAY[
               'input_tokens_uncached','input_tokens_cache_write',
               'input_tokens_cache_read','output_tokens'
           ]
       )
       OR EXISTS (
           SELECT 1
           FROM jsonb_each(usage) item(key, value)
           WHERE jsonb_typeof(value) <> 'number'
              OR value #>> '{}' !~ '^[0-9]+$'
              OR (value #>> '{}')::NUMERIC > 9223372036854775807
       ) THEN
        RETURN FALSE;
    END IF;
    IF (decision = 'needs_input') <>
       ((provenance ->> 'missing_input_count')::INTEGER BETWEEN 1 AND 8) THEN
        RETURN FALSE;
    END IF;

    source_kind := provenance ->> 'source';
    classifier_outcome := provenance ->> 'classifier_outcome';
    route_valid := CASE source_kind
        WHEN 'classifier' THEN
            stage = 'initial' AND (
                (decision = 'needs_input' AND strategy IS NULL)
                OR (decision = 'respond' AND strategy IS NULL)
                OR (decision = 'execute' AND strategy IN ('inline','durable'))
            )
        WHEN 'blank_objective' THEN
            stage = 'initial' AND decision = 'needs_input' AND strategy IS NULL
        WHEN 'selected_execution_template' THEN
            stage = 'initial' AND decision = 'execute' AND strategy = 'durable'
        WHEN 'durable_upgrade' THEN
            stage = 'durable_upgrade' AND decision = 'execute' AND strategy = 'durable'
        ELSE FALSE
    END;
    IF route_valid IS NOT TRUE THEN
        RETURN FALSE;
    END IF;

    IF source_kind <> 'classifier' THEN
        RETURN classifier_outcome = 'not_called'
           AND provenance -> 'provider_model' = 'null'::JSONB
           AND provenance -> 'prompt_version' = 'null'::JSONB
           AND provenance -> 'response_hash' = 'null'::JSONB
           AND provenance -> 'confidence_bps' = 'null'::JSONB
           AND usage = '{
               "input_tokens_uncached": 0,
               "input_tokens_cache_write": 0,
               "input_tokens_cache_read": 0,
               "output_tokens": 0
           }'::JSONB
           AND provenance ->> 'cost_microusd' = '0'
           AND provenance ->> 'duration_micros' = '0';
    END IF;

    IF classifier_outcome NOT IN (
           'accepted','provider_error','stream_error','oversized',
           'schema_rejected','invalid_decision','low_confidence',
           'context_forced_inline'
       )
       OR jsonb_typeof(provenance -> 'provider_model') <> 'string'
       OR octet_length(provenance ->> 'provider_model') NOT BETWEEN 1 AND 128
       OR jsonb_typeof(provenance -> 'prompt_version') <> 'string'
       OR octet_length(provenance ->> 'prompt_version') NOT BETWEEN 1 AND 64 THEN
        RETURN FALSE;
    END IF;
    collected := classifier_outcome IN (
        'accepted','oversized','schema_rejected','invalid_decision',
        'low_confidence','context_forced_inline'
    );
    parsed := classifier_outcome IN ('accepted','low_confidence','context_forced_inline');
    IF collected <> (provenance ->> 'response_hash' IS NOT NULL)
       OR (
           provenance ->> 'response_hash' IS NOT NULL
           AND provenance ->> 'response_hash' !~ '^[0-9a-f]{64}$'
       )
       OR parsed <> (provenance ->> 'confidence_bps' IS NOT NULL)
       OR (
           NOT collected
           AND (
               usage <> '{
                   "input_tokens_uncached": 0,
                   "input_tokens_cache_write": 0,
                   "input_tokens_cache_read": 0,
                   "output_tokens": 0
               }'::JSONB
               OR provenance ->> 'cost_microusd' <> '0'
           )
       )
       OR (
           classifier_outcome <> 'accepted'
           AND NOT (
               decision = 'execute' AND strategy = 'inline'
           )
       ) THEN
        RETURN FALSE;
    END IF;
    RETURN TRUE;
EXCEPTION
    WHEN OTHERS THEN
        RETURN FALSE;
END;
$$;

CREATE OR REPLACE FUNCTION moa.execution_planning_audit_envelope_is_valid(
    envelope JSONB
) RETURNS BOOLEAN
LANGUAGE plpgsql
IMMUTABLE
STRICT
AS $$
DECLARE
    payload JSONB;
    payload_kind TEXT;
    source_kind TEXT;
    outcome TEXT;
    call_kind TEXT;
    ordinal BIGINT;
    run_uid_text TEXT;
    revision_text TEXT;
    session_text TEXT;
    origin_text TEXT;
    candidate_text TEXT;
    report_text TEXT;
    operation_key TEXT;
BEGIN
    IF NOT moa.execution_json_object_has_exact_keys(
        envelope,
        ARRAY[
            'schema_version','tenant_id','contact_id','session_id',
            'originating_sequence','payload'
        ]
    )
       OR envelope ->> 'schema_version' <> '1'
       OR COALESCE(envelope ->> 'tenant_id', '') !~
          '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
       OR envelope ->> 'tenant_id' = '00000000-0000-0000-0000-000000000000'
       OR (
           envelope -> 'contact_id' <> 'null'::JSONB
           AND (
               COALESCE(envelope ->> 'contact_id', '') !~
               '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
               OR envelope ->> 'contact_id' =
                  '00000000-0000-0000-0000-000000000000'
           )
       )
       OR jsonb_typeof(envelope -> 'payload') <> 'object' THEN
        RETURN FALSE;
    END IF;
    session_text := envelope ->> 'session_id';
    origin_text := envelope ->> 'originating_sequence';
    IF (session_text IS NULL) <> (origin_text IS NULL)
       OR (
           session_text IS NOT NULL
           AND session_text !~
               '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
       )
       OR (
           origin_text IS NOT NULL
           AND (
               jsonb_typeof(envelope -> 'originating_sequence') <> 'number'
               OR origin_text !~ '^[0-9]+$'
               OR origin_text::NUMERIC > 9223372036854775807
           )
       ) THEN
        RETURN FALSE;
    END IF;
    payload := envelope -> 'payload';
    payload_kind := payload ->> 'kind';
    IF payload_kind = 'route' THEN
        IF session_text IS NULL
           OR NOT moa.execution_json_object_has_exact_keys(
               payload,
               ARRAY[
                   'kind','stage','decision','strategy','provenance','accepted_at'
               ]
           )
           OR payload ->> 'stage' NOT IN ('initial','durable_upgrade')
           OR payload ->> 'decision' NOT IN ('respond','execute','needs_input')
           OR jsonb_typeof(payload -> 'provenance') <> 'object'
           OR jsonb_typeof(payload -> 'accepted_at') <> 'string' THEN
            RETURN FALSE;
        END IF;
        PERFORM (payload ->> 'accepted_at')::TIMESTAMPTZ;
        RETURN moa.execution_route_provenance_is_valid(
            payload ->> 'stage',
            payload ->> 'decision',
            payload ->> 'strategy',
            payload -> 'provenance'
        );
    END IF;
    IF payload_kind = 'planner_call' THEN
        IF session_text IS NULL
           OR NOT moa.execution_json_object_has_exact_keys(
               payload,
               ARRAY[
                   'kind','call_kind','call_ordinal','run_uid','plan_revision',
                   'outcome','provider_model','prompt_version','candidate_hash',
                   'candidate_json','compiler_report','duration_micros','created_at'
               ]
           ) THEN
            RETURN FALSE;
        END IF;
        call_kind := payload ->> 'call_kind';
        outcome := payload ->> 'outcome';
        ordinal := (payload ->> 'call_ordinal')::BIGINT;
        run_uid_text := payload ->> 'run_uid';
        revision_text := payload ->> 'plan_revision';
        candidate_text := payload ->> 'candidate_json';
        report_text := payload ->> 'compiler_report';
        IF call_kind NOT IN (
               'initial_plan','initial_repair','amendment','amendment_repair'
           )
           OR outcome NOT IN (
               'accepted','needs_input','unsupported','schema_rejected',
               'immutable_goal_changed','compiler_rejected','oversized','provider_error'
           )
           OR jsonb_typeof(payload -> 'call_ordinal') <> 'number'
           OR (payload ->> 'call_ordinal') !~ '^[0-9]+$'
           OR ordinal NOT IN (0, 1)
           OR (
               (call_kind IN ('initial_plan','amendment') AND ordinal <> 0)
               OR (call_kind IN ('initial_repair','amendment_repair') AND ordinal <> 1)
           )
           OR (
               call_kind IN ('initial_plan','initial_repair')
               AND (run_uid_text IS NOT NULL OR revision_text IS NOT NULL)
           )
           OR (
               call_kind IN ('amendment','amendment_repair')
               AND (
                   run_uid_text !~
                       '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
                   OR revision_text !~ '^[0-9]+$'
                   OR revision_text::NUMERIC > 9223372036854775807
               )
           )
           OR jsonb_typeof(payload -> 'provider_model') <> 'string'
           OR octet_length(payload ->> 'provider_model') NOT BETWEEN 1 AND 128
           OR jsonb_typeof(payload -> 'prompt_version') <> 'string'
           OR octet_length(payload ->> 'prompt_version') NOT BETWEEN 1 AND 64
           OR jsonb_typeof(payload -> 'duration_micros') <> 'number'
           OR (payload ->> 'duration_micros') !~ '^[0-9]+$'
           OR (payload ->> 'duration_micros')::NUMERIC > 9223372036854775807
           OR jsonb_typeof(payload -> 'created_at') <> 'string' THEN
            RETURN FALSE;
        END IF;
        PERFORM (payload ->> 'created_at')::TIMESTAMPTZ;
        IF outcome = 'provider_error' THEN
            RETURN payload -> 'candidate_hash' = 'null'::JSONB
               AND payload -> 'candidate_json' = 'null'::JSONB
               AND payload -> 'compiler_report' = 'null'::JSONB;
        END IF;
        IF COALESCE(payload ->> 'candidate_hash', '') !~ '^[0-9a-f]{64}$'
           OR report_text IS NULL
           OR jsonb_typeof(payload -> 'compiler_report') <> 'string' THEN
            RETURN FALSE;
        END IF;
        IF outcome = 'oversized' THEN
            RETURN payload -> 'candidate_json' = 'null'::JSONB
               AND moa.execution_audit_report_is_valid(report_text, TRUE)
               AND (report_text::JSONB ->> 'kind') = 'oversized';
        END IF;
        IF outcome = 'schema_rejected' THEN
            RETURN payload -> 'candidate_json' = 'null'::JSONB
               AND moa.execution_audit_report_is_valid(report_text, FALSE)
               AND (report_text::JSONB ->> 'kind') = 'schema';
        END IF;
        IF candidate_text IS NULL
           OR jsonb_typeof(payload -> 'candidate_json') <> 'string'
           OR octet_length(candidate_text) > 1048576
           OR NOT moa.execution_json_text_is_canonical(candidate_text)
           OR NOT moa.execution_audit_report_is_valid(report_text, FALSE) THEN
            RETURN FALSE;
        END IF;
        IF outcome = 'immutable_goal_changed' THEN
            RETURN call_kind = 'initial_repair'
               AND report_text::JSONB ->> 'kind' = 'schema'
               AND report_text::JSONB ->> 'omitted_violations' = '0'
               AND report_text::JSONB -> 'violations' = jsonb_build_array(
                   jsonb_build_object(
                       'code', 'immutable_goal_changed',
                       'path', '/goal',
                       'message',
                           'repair must preserve the complete immutable goal contract'
                   )
               );
        END IF;
        RETURN outcome IN ('accepted','needs_input','unsupported','compiler_rejected')
           AND report_text::JSONB ->> 'kind' = 'compiler';
    END IF;
    IF payload_kind = 'compile' THEN
        IF NOT moa.execution_json_object_has_exact_keys(
               payload,
               ARRAY[
                   'kind','source','operation_key','run_uid','plan_revision',
                   'outcome','candidate_hash','final_plan_hash','validation_report',
                   'duration_micros','created_at'
               ]
           ) THEN
            RETURN FALSE;
        END IF;
        source_kind := payload ->> 'source';
        outcome := payload ->> 'outcome';
        operation_key := payload ->> 'operation_key';
        run_uid_text := payload ->> 'run_uid';
        revision_text := payload ->> 'plan_revision';
        report_text := payload ->> 'validation_report';
        IF source_kind NOT IN (
               'generated_plan','skill_template','experiment_template',
               'amendment','skill_regression'
           )
           OR outcome NOT IN ('accepted','needs_input','unsupported','rejected')
           OR jsonb_typeof(payload -> 'operation_key') <> 'string'
           OR octet_length(operation_key) NOT BETWEEN 1 AND 512
           OR COALESCE(payload ->> 'candidate_hash', '') !~ '^[0-9a-f]{64}$'
           OR (
               (outcome = 'accepted')
               <> (payload ->> 'final_plan_hash' IS NOT NULL)
           )
           OR (
               payload ->> 'final_plan_hash' IS NOT NULL
               AND payload ->> 'final_plan_hash' !~ '^[0-9a-f]{64}$'
           )
           OR jsonb_typeof(payload -> 'validation_report') <> 'string'
           OR NOT moa.execution_audit_report_is_valid(report_text, FALSE)
           OR report_text::JSONB ->> 'kind' <> 'compiler'
           OR jsonb_typeof(payload -> 'duration_micros') <> 'number'
           OR (payload ->> 'duration_micros') !~ '^[0-9]+$'
           OR (payload ->> 'duration_micros')::NUMERIC > 9223372036854775807
           OR jsonb_typeof(payload -> 'created_at') <> 'string' THEN
            RETURN FALSE;
        END IF;
        PERFORM (payload ->> 'created_at')::TIMESTAMPTZ;
        IF source_kind IN ('generated_plan','skill_template') THEN
            IF session_text IS NULL OR run_uid_text IS NOT NULL OR revision_text IS NOT NULL THEN
                RETURN FALSE;
            END IF;
        ELSIF source_kind = 'experiment_template' THEN
            IF session_text IS NULL
               OR run_uid_text IS NOT NULL
               OR revision_text IS NOT NULL THEN
                RETURN FALSE;
            END IF;
        ELSIF source_kind = 'amendment' THEN
            IF session_text IS NULL
               OR run_uid_text !~
                   '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
               OR revision_text !~ '^[0-9]+$'
               OR revision_text::NUMERIC > 9223372036854775807 THEN
                RETURN FALSE;
            END IF;
        ELSE
            IF session_text IS NOT NULL OR run_uid_text IS NOT NULL OR revision_text IS NOT NULL THEN
                RETURN FALSE;
            END IF;
        END IF;
        RETURN CASE source_kind
            WHEN 'generated_plan' THEN
                operation_key ~ (
                    '^session:' || session_text || ':' || origin_text ||
                    ':generated:[01]$'
                )
            WHEN 'skill_template' THEN
                operation_key ~ (
                    '^session:' || session_text || ':' || origin_text ||
                    ':skill:[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
                )
            WHEN 'experiment_template' THEN
                operation_key ~
                    '^experiment:[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}:[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}:(none|[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})$'
            WHEN 'amendment' THEN
                operation_key = (
                    'run:' || run_uid_text || ':' || revision_text ||
                    ':amendment:' || (payload ->> 'candidate_hash')
                )
            WHEN 'skill_regression' THEN
                operation_key ~
                    '^skill_regression:[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}:[0-9a-f]{64}$'
            ELSE FALSE
        END;
    END IF;
    RETURN FALSE;
EXCEPTION
    WHEN OTHERS THEN
        RETURN FALSE;
END;
$$;

CREATE OR REPLACE FUNCTION moa.execution_source_provenance_is_valid(
    provenance JSONB,
    expected_tenant UUID,
    expected_contact UUID,
    expected_run UUID,
    expected_active_plan_hash TEXT
) RETURNS BOOLEAN
LANGUAGE plpgsql
IMMUTABLE
AS $$
DECLARE
    kind TEXT;
    planner JSONB;
BEGIN
    IF jsonb_typeof(provenance) <> 'object' THEN
        RETURN FALSE;
    END IF;
    kind := provenance ->> 'kind';
    IF kind = 'generated_plan' THEN
        IF NOT moa.execution_json_object_has_exact_keys(
               provenance, ARRAY['kind','planner']
           )
           OR jsonb_typeof(provenance -> 'planner') <> 'object' THEN
            RETURN FALSE;
        END IF;
        planner := provenance -> 'planner';
        RETURN moa.execution_json_object_has_exact_keys(
                   planner,
                   ARRAY[
                       'model','prompt_version','candidate_hash',
                       'compiler_report_hash','final_plan_hash','repair_attempts'
                   ]
               )
           AND jsonb_typeof(planner -> 'model') = 'string'
           AND octet_length(planner ->> 'model') BETWEEN 1 AND 128
           AND jsonb_typeof(planner -> 'prompt_version') = 'string'
           AND octet_length(planner ->> 'prompt_version') BETWEEN 1 AND 64
           AND jsonb_typeof(planner -> 'candidate_hash') = 'string'
           AND planner ->> 'candidate_hash' ~ '^[0-9a-f]{64}$'
           AND jsonb_typeof(planner -> 'compiler_report_hash') = 'string'
           AND planner ->> 'compiler_report_hash' ~ '^[0-9a-f]{64}$'
           AND jsonb_typeof(planner -> 'final_plan_hash') = 'string'
           AND planner ->> 'final_plan_hash' ~ '^[0-9a-f]{64}$'
           AND planner ->> 'final_plan_hash' = expected_active_plan_hash
           AND jsonb_typeof(planner -> 'repair_attempts') = 'number'
           AND planner ->> 'repair_attempts' IN ('0','1');
    END IF;
    IF kind = 'skill_template' THEN
        RETURN moa.execution_json_object_has_exact_keys(
                   provenance,
                   ARRAY[
                       'kind','skill_template_ref',
                       'skill_template_revision_uid'
                   ]
               )
           AND moa.execution_skill_ref_is_canonical(
               provenance ->> 'skill_template_ref'
           )
           AND provenance ->> 'skill_template_revision_uid' ~
               '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$';
    END IF;
    IF kind = 'experiment_template' THEN
        RETURN moa.execution_json_object_has_exact_keys(
                   provenance,
                   ARRAY[
                       'kind','skill_template_ref',
                       'skill_template_revision_uid','experiment_run_uid',
                       'score_run_id','trial_uid'
                   ]
               )
           AND moa.execution_skill_ref_is_canonical(
               provenance ->> 'skill_template_ref'
           )
           AND provenance ->> 'skill_template_revision_uid' ~
               '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
           AND provenance ->> 'experiment_run_uid' ~
               '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
           AND provenance ->> 'score_run_id' ~
               '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
           AND (
               provenance -> 'trial_uid' = 'null'::JSONB
               OR provenance ->> 'trial_uid' ~
                  '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
           )
           AND provenance ->> 'experiment_run_uid'
               <> provenance ->> 'score_run_id'
           AND provenance ->> 'skill_template_revision_uid'
               NOT IN (
                   provenance ->> 'experiment_run_uid',
                   provenance ->> 'score_run_id'
               )
           AND (
               provenance -> 'trial_uid' = 'null'::JSONB
               OR (
                   provenance ->> 'trial_uid'
                       <> provenance ->> 'experiment_run_uid'
                   AND provenance ->> 'trial_uid'
                       <> provenance ->> 'score_run_id'
                   AND provenance ->> 'trial_uid'
                       <> provenance ->> 'skill_template_revision_uid'
               )
           );
    END IF;
    RETURN FALSE;
EXCEPTION
    WHEN OTHERS THEN
        RETURN FALSE;
END;
$$;

CREATE OR REPLACE FUNCTION moa.execution_route_audit_row_is_valid(
    stage TEXT,
    decision TEXT,
    strategy TEXT,
    source TEXT,
    classifier_outcome TEXT,
    provider_model TEXT,
    prompt_version TEXT,
    objective_hash TEXT,
    response_hash TEXT,
    confidence_bps SMALLINT,
    missing_input_count SMALLINT,
    input_tokens_uncached BIGINT,
    input_tokens_cache_write BIGINT,
    input_tokens_cache_read BIGINT,
    output_tokens BIGINT,
    cost_microusd BIGINT,
    duration_micros BIGINT
) RETURNS BOOLEAN
LANGUAGE sql
IMMUTABLE
AS $$
    SELECT moa.execution_route_provenance_is_valid(
        stage,
        decision,
        strategy,
        jsonb_build_object(
            'source', source,
            'classifier_outcome', classifier_outcome,
            'provider_model', provider_model,
            'prompt_version', prompt_version,
            'objective_hash', objective_hash,
            'response_hash', response_hash,
            'confidence_bps', confidence_bps,
            'missing_input_count', missing_input_count,
            'usage', jsonb_build_object(
                'input_tokens_uncached', input_tokens_uncached,
                'input_tokens_cache_write', input_tokens_cache_write,
                'input_tokens_cache_read', input_tokens_cache_read,
                'output_tokens', output_tokens
            ),
            'cost_microusd', cost_microusd,
            'duration_micros', duration_micros
        )
    )
$$;

CREATE OR REPLACE FUNCTION moa.execution_planner_audit_row_is_valid(
    call_kind TEXT,
    call_ordinal SMALLINT,
    run_uid UUID,
    plan_revision BIGINT,
    outcome TEXT,
    provider_model TEXT,
    prompt_version TEXT,
    candidate_hash TEXT,
    candidate_json JSON,
    compiler_report JSON,
    duration_micros BIGINT
) RETURNS BOOLEAN
LANGUAGE sql
IMMUTABLE
AS $$
    SELECT moa.execution_planning_audit_envelope_is_valid(jsonb_build_object(
        'schema_version', 1,
        'tenant_id', '00000000-0000-0000-0000-000000000001',
        'contact_id', NULL,
        'session_id', '00000000-0000-0000-0000-000000000002',
        'originating_sequence', 0,
        'payload', jsonb_build_object(
            'kind', 'planner_call',
            'call_kind', call_kind,
            'call_ordinal', call_ordinal,
            'run_uid', run_uid,
            'plan_revision', plan_revision,
            'outcome', outcome,
            'provider_model', provider_model,
            'prompt_version', prompt_version,
            'candidate_hash', candidate_hash,
            'candidate_json', candidate_json::TEXT,
            'compiler_report', compiler_report::TEXT,
            'duration_micros', duration_micros,
            'created_at', '2026-01-01T00:00:00.000000Z'
        )
    ))
$$;

CREATE OR REPLACE FUNCTION moa.execution_compile_audit_row_is_valid(
    session_id UUID,
    originating_sequence BIGINT,
    run_uid UUID,
    plan_revision BIGINT,
    source TEXT,
    operation_key TEXT,
    outcome TEXT,
    candidate_hash TEXT,
    final_plan_hash TEXT,
    validation_report JSON,
    duration_micros BIGINT
) RETURNS BOOLEAN
LANGUAGE sql
IMMUTABLE
AS $$
    SELECT moa.execution_planning_audit_envelope_is_valid(jsonb_build_object(
        'schema_version', 1,
        'tenant_id', '00000000-0000-0000-0000-000000000001',
        'contact_id', NULL,
        'session_id', session_id,
        'originating_sequence', originating_sequence,
        'payload', jsonb_build_object(
            'kind', 'compile',
            'source', source,
            'operation_key', operation_key,
            'run_uid', run_uid,
            'plan_revision', plan_revision,
            'outcome', outcome,
            'candidate_hash', candidate_hash,
            'final_plan_hash', final_plan_hash,
            'validation_report', validation_report::TEXT,
            'duration_micros', duration_micros,
            'created_at', '2026-01-01T00:00:00.000000Z'
        )
    ))
$$;

CREATE TABLE moa.execution_route_audit (
    audit_uid UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    contact_id UUID,
    contact_scope_id UUID GENERATED ALWAYS AS (
        COALESCE(
            contact_id,
            '00000000-0000-0000-0000-000000000000'::UUID
        )
    ) STORED,
    session_id UUID NOT NULL,
    originating_sequence BIGINT NOT NULL CHECK (originating_sequence >= 0),
    stage TEXT NOT NULL,
    decision TEXT NOT NULL,
    strategy TEXT,
    source TEXT NOT NULL,
    classifier_outcome TEXT NOT NULL,
    provider_model VARCHAR(128),
    prompt_version VARCHAR(64),
    objective_hash TEXT NOT NULL,
    response_hash TEXT,
    confidence_bps SMALLINT,
    missing_input_count SMALLINT NOT NULL,
    input_tokens_uncached BIGINT NOT NULL,
    input_tokens_cache_write BIGINT NOT NULL,
    input_tokens_cache_read BIGINT NOT NULL,
    output_tokens BIGINT NOT NULL,
    cost_microusd BIGINT NOT NULL,
    duration_micros BIGINT NOT NULL,
    accepted_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL,
    CONSTRAINT execution_route_audit_contact_not_nil CHECK (
        contact_id IS NULL
        OR contact_id <> '00000000-0000-0000-0000-000000000000'::UUID
    ),
    CONSTRAINT execution_route_audit_uid_check CHECK (
        audit_uid = moa.execution_route_audit_uid(
            tenant_id, contact_id, session_id, originating_sequence, stage
        )
    ),
    CONSTRAINT execution_route_audit_matrix_check CHECK (
        moa.execution_route_audit_row_is_valid(
            stage, decision, strategy, source, classifier_outcome,
            provider_model, prompt_version, objective_hash, response_hash,
            confidence_bps, missing_input_count, input_tokens_uncached,
            input_tokens_cache_write, input_tokens_cache_read, output_tokens,
            cost_microusd, duration_micros
        )
    ),
    CONSTRAINT execution_route_audit_created_at_check CHECK (
        created_at = accepted_at
    ),
    CONSTRAINT execution_route_audit_logical_key UNIQUE NULLS NOT DISTINCT (
        tenant_id, contact_id, session_id, originating_sequence, stage
    )
);

CREATE TABLE moa.execution_planner_call_audit (
    audit_uid UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    contact_id UUID,
    contact_scope_id UUID GENERATED ALWAYS AS (
        COALESCE(
            contact_id,
            '00000000-0000-0000-0000-000000000000'::UUID
        )
    ) STORED,
    session_id UUID NOT NULL,
    originating_sequence BIGINT NOT NULL CHECK (originating_sequence >= 0),
    run_uid UUID,
    plan_revision BIGINT,
    call_kind TEXT NOT NULL,
    call_ordinal SMALLINT NOT NULL,
    outcome TEXT NOT NULL,
    provider_model VARCHAR(128) NOT NULL,
    prompt_version VARCHAR(64) NOT NULL,
    candidate_hash TEXT,
    candidate_json JSON,
    compiler_report JSON,
    duration_micros BIGINT NOT NULL CHECK (duration_micros >= 0),
    created_at TIMESTAMPTZ NOT NULL,
    CONSTRAINT execution_planner_call_audit_contact_not_nil CHECK (
        contact_id IS NULL
        OR contact_id <> '00000000-0000-0000-0000-000000000000'::UUID
    ),
    CONSTRAINT execution_planner_call_audit_uid_check CHECK (
        audit_uid = moa.execution_planner_audit_uid(
            tenant_id, contact_id, session_id, originating_sequence,
            run_uid, plan_revision, call_kind, call_ordinal
        )
    ),
    CONSTRAINT execution_planner_call_audit_candidate_bytes CHECK (
        candidate_json IS NULL
        OR octet_length(candidate_json::TEXT) <= 1048576
    ),
    CONSTRAINT execution_planner_call_audit_report_bytes CHECK (
        compiler_report IS NULL
        OR octet_length(compiler_report::TEXT) <= 262144
    ),
    CONSTRAINT execution_planner_call_audit_row_check CHECK (
        moa.execution_planner_audit_row_is_valid(
            call_kind, call_ordinal, run_uid, plan_revision, outcome,
            provider_model, prompt_version, candidate_hash, candidate_json,
            compiler_report, duration_micros
        )
    ),
    CONSTRAINT execution_planner_call_audit_run_fkey
        FOREIGN KEY (run_uid, tenant_id, contact_scope_id)
        REFERENCES moa.execution_run (
            run_uid, tenant_id, contact_scope_id
        ),
    CONSTRAINT execution_planner_call_audit_logical_key
        UNIQUE NULLS NOT DISTINCT (
            tenant_id, contact_id, session_id, originating_sequence,
            run_uid, plan_revision, call_kind, call_ordinal
        )
);

CREATE TABLE moa.execution_compile_audit (
    audit_uid UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    contact_id UUID,
    contact_scope_id UUID GENERATED ALWAYS AS (
        COALESCE(
            contact_id,
            '00000000-0000-0000-0000-000000000000'::UUID
        )
    ) STORED,
    session_id UUID,
    originating_sequence BIGINT,
    run_uid UUID,
    plan_revision BIGINT,
    source TEXT NOT NULL,
    operation_key VARCHAR(512) NOT NULL,
    outcome TEXT NOT NULL,
    candidate_hash TEXT NOT NULL,
    final_plan_hash TEXT,
    validation_report JSON NOT NULL,
    duration_micros BIGINT NOT NULL CHECK (duration_micros >= 0),
    created_at TIMESTAMPTZ NOT NULL,
    CONSTRAINT execution_compile_audit_contact_not_nil CHECK (
        contact_id IS NULL
        OR contact_id <> '00000000-0000-0000-0000-000000000000'::UUID
    ),
    CONSTRAINT execution_compile_audit_uid_check CHECK (
        audit_uid = moa.execution_compile_audit_uid(
            tenant_id, contact_id, source, operation_key
        )
    ),
    CONSTRAINT execution_compile_audit_report_bytes CHECK (
        octet_length(validation_report::TEXT) <= 262144
    ),
    CONSTRAINT execution_compile_audit_row_check CHECK (
        moa.execution_compile_audit_row_is_valid(
            session_id, originating_sequence, run_uid, plan_revision,
            source, operation_key, outcome, candidate_hash, final_plan_hash,
            validation_report, duration_micros
        )
    ),
    CONSTRAINT execution_compile_audit_run_fkey
        FOREIGN KEY (run_uid, tenant_id, contact_scope_id)
        REFERENCES moa.execution_run (
            run_uid, tenant_id, contact_scope_id
        ),
    CONSTRAINT execution_compile_audit_logical_key
        UNIQUE NULLS NOT DISTINCT (
            tenant_id, contact_id, source, operation_key
        )
);

CREATE TABLE moa.execution_node_materialization (
    run_uid UUID NOT NULL,
    plan_revision BIGINT NOT NULL CHECK (plan_revision >= 1),
    node_id TEXT NOT NULL,
    tenant_id UUID NOT NULL,
    contact_id UUID,
    contact_scope_id UUID GENERATED ALWAYS AS (
        COALESCE(
            contact_id,
            '00000000-0000-0000-0000-000000000000'::UUID
        )
    ) STORED,
    kind TEXT NOT NULL CHECK (kind IN ('map','reduce')),
    fanout_items BIGINT,
    reducer_depth BIGINT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT execution_node_materialization_pkey
        PRIMARY KEY (run_uid, plan_revision, node_id),
    CONSTRAINT execution_node_materialization_contact_not_nil CHECK (
        contact_id IS NULL
        OR contact_id <> '00000000-0000-0000-0000-000000000000'::UUID
    ),
    CONSTRAINT execution_node_materialization_payload_check CHECK (
        CASE kind
            WHEN 'map' THEN
                fanout_items IS NOT NULL
                AND fanout_items >= 0
                AND reducer_depth IS NULL
            WHEN 'reduce' THEN
                fanout_items IS NULL
                AND reducer_depth IS NOT NULL
                AND reducer_depth >= 0
            ELSE FALSE
        END
    ),
    CONSTRAINT execution_node_materialization_run_fkey
        FOREIGN KEY (run_uid, tenant_id, contact_scope_id)
        REFERENCES moa.execution_run (
            run_uid, tenant_id, contact_scope_id
        )
        ON DELETE CASCADE
);

CREATE INDEX execution_route_audit_scope_created_idx
    ON moa.execution_route_audit (
        tenant_id, contact_scope_id, created_at, audit_uid
    );
CREATE INDEX execution_planner_call_audit_scope_created_idx
    ON moa.execution_planner_call_audit (
        tenant_id, contact_scope_id, created_at, audit_uid
    );
CREATE INDEX execution_compile_audit_scope_created_idx
    ON moa.execution_compile_audit (
        tenant_id, contact_scope_id, created_at, audit_uid
    );
CREATE INDEX execution_node_materialization_scope_idx
    ON moa.execution_node_materialization (
        tenant_id, contact_scope_id, run_uid, plan_revision, node_id
    );

CREATE OR REPLACE FUNCTION moa.reject_execution_immutable_payload()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    IF TG_OP = 'DELETE' AND EXISTS (
        SELECT 1
        FROM moa.destruction_operation_fence
        WHERE tenant_id = OLD.tenant_id
          AND subject_id IS NULL
    ) THEN
        RETURN OLD;
    END IF;
    RAISE EXCEPTION 'execution analytics rows are immutable';
END;
$$;

CREATE TRIGGER execution_route_audit_immutable_guard
BEFORE UPDATE OR DELETE ON moa.execution_route_audit
FOR EACH ROW EXECUTE FUNCTION moa.reject_execution_immutable_payload();
CREATE TRIGGER execution_planner_call_audit_immutable_guard
BEFORE UPDATE OR DELETE ON moa.execution_planner_call_audit
FOR EACH ROW EXECUTE FUNCTION moa.reject_execution_immutable_payload();
CREATE TRIGGER execution_compile_audit_immutable_guard
BEFORE UPDATE OR DELETE ON moa.execution_compile_audit
FOR EACH ROW EXECUTE FUNCTION moa.reject_execution_immutable_payload();
CREATE TRIGGER execution_node_materialization_immutable_guard
BEFORE UPDATE OR DELETE ON moa.execution_node_materialization
FOR EACH ROW EXECUTE FUNCTION moa.reject_execution_immutable_payload();

SELECT moa.apply_contact_rls('moa.execution_route_audit'::REGCLASS);
SELECT moa.apply_contact_rls('moa.execution_planner_call_audit'::REGCLASS);
SELECT moa.apply_contact_rls('moa.execution_compile_audit'::REGCLASS);
SELECT moa.apply_contact_rls('moa.execution_node_materialization'::REGCLASS);

ALTER TABLE tenant_action_reviews
    ADD COLUMN execution_task_traceparent TEXT,
    ADD COLUMN execution_task_tracestate TEXT,
    ADD CONSTRAINT tenant_action_reviews_execution_task_trace_check CHECK (
        (
            execution_task_traceparent IS NULL
            AND execution_task_tracestate IS NULL
        )
        OR (
            moa.execution_traceparent_is_valid(execution_task_traceparent)
            AND (
                execution_task_tracestate IS NULL
                OR moa.execution_tracestate_is_normalized(
                    execution_task_tracestate
                )
            )
        )
    );

ALTER TABLE moa.execution_action_review_outbox
    ADD COLUMN traceparent TEXT,
    ADD COLUMN tracestate TEXT,
    ADD COLUMN task_traceparent TEXT,
    ADD COLUMN task_tracestate TEXT,
    ADD CONSTRAINT execution_action_review_outbox_resolution_trace_check CHECK (
        (
            traceparent IS NULL
            AND tracestate IS NULL
        )
        OR (
            moa.execution_traceparent_is_valid(traceparent)
            AND (
                tracestate IS NULL
                OR moa.execution_tracestate_is_normalized(tracestate)
            )
        )
    ),
    ADD CONSTRAINT execution_action_review_outbox_task_trace_check CHECK (
        (
            task_traceparent IS NULL
            AND task_tracestate IS NULL
        )
        OR (
            moa.execution_traceparent_is_valid(task_traceparent)
            AND (
                task_tracestate IS NULL
                OR moa.execution_tracestate_is_normalized(task_tracestate)
            )
        )
    );

CREATE OR REPLACE FUNCTION moa.enforce_action_review_trace_immutability()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    IF TG_TABLE_NAME = 'tenant_action_reviews' THEN
        IF NEW.execution_task_traceparent IS DISTINCT FROM
               OLD.execution_task_traceparent
           OR NEW.execution_task_tracestate IS DISTINCT FROM
               OLD.execution_task_tracestate THEN
            RAISE EXCEPTION
                'tenant action review execution-task trace context is immutable';
        END IF;
    ELSIF NEW.traceparent IS DISTINCT FROM OLD.traceparent
       OR NEW.tracestate IS DISTINCT FROM OLD.tracestate
       OR NEW.task_traceparent IS DISTINCT FROM OLD.task_traceparent
       OR NEW.task_tracestate IS DISTINCT FROM OLD.task_tracestate THEN
        RAISE EXCEPTION
            'execution action review outbox trace contexts are immutable';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER tenant_action_reviews_execution_task_trace_immutable_guard
BEFORE UPDATE ON tenant_action_reviews
FOR EACH ROW EXECUTE FUNCTION moa.enforce_action_review_trace_immutability();

CREATE TRIGGER execution_action_review_outbox_trace_immutable_guard
BEFORE UPDATE ON moa.execution_action_review_outbox
FOR EACH ROW EXECUTE FUNCTION moa.enforce_action_review_trace_immutability();

CREATE SEQUENCE moa.execution_analytics_change_seq
    AS BIGINT
    MINVALUE 1
    NO CYCLE;

GRANT USAGE, SELECT ON SEQUENCE moa.execution_analytics_change_seq
    TO moa_app, moa_promoter;

ALTER TABLE moa.execution_run
    ADD COLUMN analytics_change_seq BIGINT NOT NULL,
    ADD CONSTRAINT execution_run_analytics_change_seq_check
        CHECK (analytics_change_seq > 0);
ALTER TABLE moa.execution_task
    ADD COLUMN analytics_change_seq BIGINT NOT NULL,
    ADD CONSTRAINT execution_task_analytics_change_seq_check
        CHECK (analytics_change_seq > 0);

CREATE INDEX execution_run_analytics_change_idx
    ON moa.execution_run (analytics_change_seq, run_uid);
CREATE INDEX execution_task_analytics_change_idx
    ON moa.execution_task (analytics_change_seq, task_id);

CREATE OR REPLACE FUNCTION moa.assign_execution_analytics_change_seq()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM pg_advisory_xact_lock_shared(1297047877, 337);
    NEW.analytics_change_seq :=
        nextval('moa.execution_analytics_change_seq');
    RETURN NEW;
END;
$$;

CREATE TRIGGER execution_run_analytics_change_guard
BEFORE INSERT OR UPDATE ON moa.execution_run
FOR EACH ROW EXECUTE FUNCTION moa.assign_execution_analytics_change_seq();

CREATE TRIGGER execution_task_analytics_change_guard
BEFORE INSERT OR UPDATE ON moa.execution_task
FOR EACH ROW EXECUTE FUNCTION moa.assign_execution_analytics_change_seq();

CREATE MATERIALIZED VIEW analytics.execution_run_fact AS
SELECT
    r.run_uid,
    r.tenant_id,
    r.contact_id,
    r.session_id,
    sac.agent_id,
    r.initial_plan_hash,
    r.active_plan_hash,
    r.plan_revision,
    r.source_kind,
    r.skill_template_ref,
    r.skill_template_revision_uid,
    r.status,
    r.terminal_reason,
    COALESCE(
        r.terminal_requirement_count,
        CASE
            WHEN jsonb_typeof(r.goal_contract -> 'requirements') = 'array'
            THEN jsonb_array_length(r.goal_contract -> 'requirements')::BIGINT
            ELSE 0
        END
    ) AS requirement_count,
    COALESCE(
        r.terminal_satisfied_requirement_count,
        0
    ) AS satisfied_requirement_count,
    CASE
        WHEN jsonb_typeof(r.goal_contract -> 'completion_checks') = 'array'
        THEN jsonb_array_length(r.goal_contract -> 'completion_checks')::BIGINT
        ELSE 0
    END AS completion_check_count,
    r.progress_total_tasks AS logical_task_count,
    r.queued_at,
    r.started_at,
    CASE
        WHEN r.queued_at IS NULL OR r.started_at IS NULL THEN NULL::DOUBLE PRECISION
        ELSE GREATEST(
            EXTRACT(EPOCH FROM (r.started_at - r.queued_at)) * 1000.0,
            0.0
        )
    END AS queue_to_start_ms,
    r.completed_at,
    CASE
        WHEN r.started_at IS NULL OR r.completed_at IS NULL
            THEN NULL::DOUBLE PRECISION
        ELSE GREATEST(
            EXTRACT(EPOCH FROM (r.completed_at - r.started_at)) * 1000.0,
            0.0
        )
    END AS duration_ms,
    r.reserved_cost_microusd,
    r.consumed_cost_microusd AS actual_cost_microusd,
    r.reserved_tokens,
    r.consumed_tokens AS actual_tokens,
    r.reserved_tasks,
    r.consumed_tasks AS actual_tasks,
    r.reserved_tool_calls,
    r.consumed_tool_calls AS actual_tool_calls,
    r.reserved_retrieved_bytes,
    r.consumed_retrieved_bytes AS actual_retrieved_bytes,
    r.created_at,
    r.updated_at
FROM moa.execution_run r
LEFT JOIN session_agent_context sac
    ON sac.session_id = r.session_id;

CREATE UNIQUE INDEX analytics_execution_run_fact_run_uidx
    ON analytics.execution_run_fact (run_uid);
CREATE INDEX analytics_execution_run_fact_tenant_started_idx
    ON analytics.execution_run_fact (tenant_id, started_at DESC, run_uid);
CREATE INDEX analytics_execution_run_fact_tenant_plan_idx
    ON analytics.execution_run_fact (
        tenant_id, active_plan_hash, started_at DESC, run_uid
    );
CREATE INDEX analytics_execution_run_fact_tenant_template_idx
    ON analytics.execution_run_fact (
        tenant_id, skill_template_revision_uid, started_at DESC, run_uid
    )
    WHERE skill_template_revision_uid IS NOT NULL;

CREATE MATERIALIZED VIEW analytics.execution_task_fact AS
SELECT
    t.task_id AS task_id,
    t.run_uid,
    t.tenant_id,
    t.node_id,
    t.item_key,
    t.task_kind ->> 'kind' AS task_kind,
    CASE
        WHEN t.task_kind ->> 'kind' = 'capability'
        THEN t.task_kind #>> '{reference,name}'
        ELSE NULL
    END AS capability_name,
    CASE
        WHEN t.task_kind ->> 'kind' = 'capability'
        THEN t.task_kind #>> '{reference,version}'
        ELSE NULL
    END AS capability_version,
    t.plan_revision,
    t.status,
    CASE
        WHEN t.status = 'failed'
        THEN COALESCE(
            t.current_outcome ->> 'class',
            t.error ->> 'class'
        )
        ELSE NULL
    END AS failure_class,
    t.attempt,
    t.generation,
    jsonb_array_length(t.citations)::BIGINT AS citation_count,
    CASE
        WHEN t.started_at IS NULL THEN NULL::DOUBLE PRECISION
        ELSE GREATEST(
            EXTRACT(EPOCH FROM (t.started_at - t.created_at)) * 1000.0,
            0.0
        )
    END AS queue_latency_ms,
    CASE
        WHEN t.completed_at IS NULL THEN NULL::DOUBLE PRECISION
        ELSE GREATEST(
            EXTRACT(
                EPOCH FROM (
                    t.completed_at - COALESCE(t.started_at, t.created_at)
                )
            ) * 1000.0,
            0.0
        )
    END AS duration_ms,
    t.reserved_cost_microusd,
    t.actual_cost_microusd,
    t.reserved_tokens,
    t.actual_tokens,
    t.reserved_tasks,
    t.actual_tasks,
    t.reserved_tool_calls,
    t.actual_tool_calls,
    t.reserved_retrieved_bytes,
    t.actual_retrieved_bytes,
    t.started_at,
    t.completed_at,
    t.created_at,
    t.updated_at
FROM moa.execution_task t;

CREATE UNIQUE INDEX analytics_execution_task_fact_task_id_uidx
    ON analytics.execution_task_fact (task_id);
CREATE INDEX analytics_execution_task_fact_tenant_started_idx
    ON analytics.execution_task_fact (
        tenant_id, started_at DESC, task_id
    );
CREATE INDEX analytics_execution_task_fact_tenant_capability_idx
    ON analytics.execution_task_fact (
        tenant_id, capability_name, capability_version, started_at DESC, task_id
    )
    WHERE capability_name IS NOT NULL;

CREATE TABLE analytics.clickhouse_schema_upgrade_state (
    upgrade_key TEXT NOT NULL,
    generation BIGINT NOT NULL DEFAULT 1,
    database_uuid UUID NOT NULL,
    run_table_uuid UUID NOT NULL,
    task_table_uuid UUID NOT NULL,
    stage TEXT NOT NULL DEFAULT 'pending',
    upgrade_version TIMESTAMPTZ NOT NULL,
    export_version_floor TIMESTAMPTZ NOT NULL,
    run_high_water_seq BIGINT NOT NULL,
    run_high_water_id UUID NOT NULL,
    task_high_water_seq BIGINT NOT NULL,
    task_high_water_id UUID NOT NULL,
    run_page_seq BIGINT NOT NULL,
    run_page_id UUID NOT NULL,
    task_page_seq BIGINT NOT NULL,
    task_page_id UUID NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    completed_at TIMESTAMPTZ,
    CONSTRAINT clickhouse_schema_upgrade_state_pkey PRIMARY KEY (
        upgrade_key, generation
    ),
    CONSTRAINT clickhouse_schema_upgrade_table_generation_key UNIQUE (
        upgrade_key, run_table_uuid, task_table_uuid
    ),
    CONSTRAINT clickhouse_schema_upgrade_key_check CHECK (
        upgrade_key = 'execution_dimensions'
    ),
    CONSTRAINT clickhouse_schema_upgrade_generation_check CHECK (
        generation > 0
    ),
    CONSTRAINT clickhouse_schema_upgrade_identity_check CHECK (
        database_uuid <> '00000000-0000-0000-0000-000000000000'
        AND run_table_uuid <> '00000000-0000-0000-0000-000000000000'
        AND task_table_uuid <> '00000000-0000-0000-0000-000000000000'
    ),
    CONSTRAINT clickhouse_schema_upgrade_stage_check CHECK (
        stage IN (
            'pending','schema_upgraded','cursors_reset',
            'runs_exported','tasks_exported','complete'
        )
    ),
    CONSTRAINT clickhouse_schema_upgrade_versions_check CHECK (
        export_version_floor >= upgrade_version
    ),
    CONSTRAINT clickhouse_schema_upgrade_run_positions_check CHECK (
        run_high_water_seq >= 0
        AND run_page_seq >= 0
        AND ROW(run_page_seq, run_page_id)
            <= ROW(run_high_water_seq, run_high_water_id)
    ),
    CONSTRAINT clickhouse_schema_upgrade_task_positions_check CHECK (
        task_high_water_seq >= 0
        AND task_page_seq >= 0
        AND ROW(task_page_seq, task_page_id)
            <= ROW(task_high_water_seq, task_high_water_id)
    ),
    CONSTRAINT clickhouse_schema_upgrade_completion_check CHECK (
        (stage = 'complete') = (completed_at IS NOT NULL)
    ),
    CONSTRAINT clickhouse_schema_upgrade_timestamps_check CHECK (
        updated_at >= created_at
        AND (
            completed_at IS NULL
            OR completed_at >= created_at
        )
    )
);

CREATE OR REPLACE FUNCTION analytics.execution_upgrade_stage_rank(
    stage_value TEXT
) RETURNS SMALLINT
LANGUAGE sql
IMMUTABLE
STRICT
AS $$
    SELECT CASE stage_value
        WHEN 'pending' THEN 0
        WHEN 'schema_upgraded' THEN 1
        WHEN 'cursors_reset' THEN 2
        WHEN 'runs_exported' THEN 3
        WHEN 'tasks_exported' THEN 4
        WHEN 'complete' THEN 5
    END::SMALLINT
$$;

CREATE OR REPLACE FUNCTION analytics.enforce_execution_upgrade_monotonicity()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
DECLARE
    previous_generation BIGINT;
    previous_database_uuid UUID;
    previous_run_table_uuid UUID;
    previous_task_table_uuid UUID;
    previous_export_version_floor TIMESTAMPTZ;
    old_rank SMALLINT;
    new_rank SMALLINT;
BEGIN
    IF TG_OP = 'INSERT' THEN
        SELECT generation, database_uuid, run_table_uuid, task_table_uuid,
               export_version_floor
        INTO previous_generation, previous_database_uuid, previous_run_table_uuid,
             previous_task_table_uuid, previous_export_version_floor
        FROM analytics.clickhouse_schema_upgrade_state
        WHERE upgrade_key = NEW.upgrade_key
        ORDER BY generation DESC
        LIMIT 1;

        IF NOT FOUND THEN
            IF NEW.generation <> 1 THEN
                RAISE EXCEPTION
                    'first execution analytics bootstrap generation must be 1';
            END IF;
        ELSE
            IF NEW.generation <> previous_generation + 1 THEN
                RAISE EXCEPTION
                    'execution analytics bootstrap generations must be contiguous';
            END IF;
            IF NEW.database_uuid <> previous_database_uuid THEN
                RAISE EXCEPTION
                    'execution analytics ClickHouse database identity is immutable across generations';
            END IF;
            IF NEW.run_table_uuid = previous_run_table_uuid
               OR NEW.task_table_uuid = previous_task_table_uuid THEN
                RAISE EXCEPTION
                    'execution analytics bootstrap generation requires both ClickHouse table identities to change';
            END IF;
            IF NEW.upgrade_version <= previous_export_version_floor
               OR NEW.export_version_floor < previous_export_version_floor THEN
                RAISE EXCEPTION
                    'execution analytics bootstrap versions must advance across generations';
            END IF;
        END IF;
        RETURN NEW;
    END IF;

    old_rank := analytics.execution_upgrade_stage_rank(OLD.stage);
    new_rank := analytics.execution_upgrade_stage_rank(NEW.stage);

    IF NEW.upgrade_key IS DISTINCT FROM OLD.upgrade_key
       OR NEW.generation IS DISTINCT FROM OLD.generation
       OR NEW.database_uuid IS DISTINCT FROM OLD.database_uuid
       OR NEW.run_table_uuid IS DISTINCT FROM OLD.run_table_uuid
       OR NEW.task_table_uuid IS DISTINCT FROM OLD.task_table_uuid
       OR NEW.upgrade_version IS DISTINCT FROM OLD.upgrade_version
       OR NEW.run_high_water_seq IS DISTINCT FROM OLD.run_high_water_seq
       OR NEW.run_high_water_id IS DISTINCT FROM OLD.run_high_water_id
       OR NEW.task_high_water_seq IS DISTINCT FROM OLD.task_high_water_seq
       OR NEW.task_high_water_id IS DISTINCT FROM OLD.task_high_water_id
       OR NEW.created_at IS DISTINCT FROM OLD.created_at THEN
        RAISE EXCEPTION
            'execution analytics upgrade identity and high waters are immutable';
    END IF;
    IF new_rank < old_rank OR new_rank > old_rank + 1 THEN
        RAISE EXCEPTION
            'execution analytics upgrade stages must advance one step at a time';
    END IF;
    IF NEW.export_version_floor < OLD.export_version_floor THEN
        RAISE EXCEPTION
            'execution analytics export version floor cannot decrease';
    END IF;
    IF ROW(NEW.run_page_seq, NEW.run_page_id)
           < ROW(OLD.run_page_seq, OLD.run_page_id)
       OR ROW(NEW.task_page_seq, NEW.task_page_id)
           < ROW(OLD.task_page_seq, OLD.task_page_id) THEN
        RAISE EXCEPTION
            'execution analytics upgrade page cursors cannot move backwards';
    END IF;
    IF NEW.updated_at < OLD.updated_at THEN
        RAISE EXCEPTION
            'execution analytics upgrade updated_at cannot move backwards';
    END IF;
    IF OLD.completed_at IS NOT NULL
       AND NEW.completed_at IS DISTINCT FROM OLD.completed_at THEN
        RAISE EXCEPTION
            'execution analytics upgrade completion timestamp is immutable';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER clickhouse_schema_upgrade_state_monotonic_guard
BEFORE INSERT OR UPDATE ON analytics.clickhouse_schema_upgrade_state
FOR EACH ROW
EXECUTE FUNCTION analytics.enforce_execution_upgrade_monotonicity();
