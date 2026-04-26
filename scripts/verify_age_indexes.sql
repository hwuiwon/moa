WITH vertex_labels(label) AS (
    SELECT unnest(moa.age_vertex_labels())
),
edge_labels(label) AS (
    SELECT unnest(moa.age_edge_labels())
),
base_vertex_labels(label) AS (
    VALUES ('_ag_label_vertex')
),
base_edge_labels(label) AS (
    VALUES ('_ag_label_edge')
),
expected_labels(label) AS (
    SELECT label FROM vertex_labels
    UNION ALL
    SELECT label FROM edge_labels
    UNION ALL
    SELECT label FROM base_vertex_labels
    UNION ALL
    SELECT label FROM base_edge_labels
),
expected_indexes(indexname) AS (
    SELECT label || suffix
    FROM (
        SELECT label FROM vertex_labels
        UNION ALL
        SELECT label FROM base_vertex_labels
    ) labels
    CROSS JOIN (
        VALUES
            ('_id_idx'),
            ('_uid_idx'),
            ('_workspace_idx'),
            ('_scope_idx'),
            ('_validto_partial_idx'),
            ('_props_gin')
    ) AS suffixes(suffix)
    UNION ALL
    SELECT label || suffix
    FROM (
        SELECT label FROM edge_labels
        UNION ALL
        SELECT label FROM base_edge_labels
    ) labels
    CROSS JOIN (
        VALUES
            ('_start_idx'),
            ('_end_idx'),
            ('_workspace_idx')
    ) AS suffixes(suffix)
),
missing_labels AS (
    SELECT label
    FROM expected_labels
    WHERE to_regclass(format('%I.%I', 'moa_graph', label)) IS NULL
),
missing_indexes AS (
    SELECT indexname
    FROM expected_indexes
    EXCEPT
    SELECT indexname
    FROM pg_indexes
    WHERE schemaname = 'moa_graph'
)
SELECT 'missing_label' AS kind, label AS name FROM missing_labels
UNION ALL
SELECT 'missing_index' AS kind, indexname AS name FROM missing_indexes
ORDER BY kind, name;
