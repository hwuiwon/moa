-- Give the subject-access export's dedicated read-only role the complete typed
-- provenance surface. The export still narrows rows by its materialized subject
-- relation; these permissive policies only let that role execute the queries at
-- all under FORCE ROW LEVEL SECURITY.

GRANT SELECT ON public.contacts TO moa_auditor;
GRANT SELECT ON moa.edge_index TO moa_auditor;
GRANT SELECT ON public.sessions TO moa_auditor;
GRANT SELECT ON public.task_segments TO moa_auditor;
GRANT SELECT ON public.experience_records TO moa_auditor;
GRANT SELECT ON public.experience_attributions TO moa_auditor;
GRANT SELECT ON public.learning_candidates TO moa_auditor;
GRANT SELECT ON public.learning_candidate_source TO moa_auditor;
GRANT SELECT ON public.learning_candidate_decision TO moa_auditor;
GRANT SELECT ON public.learning_log TO moa_auditor;
GRANT SELECT ON public.learning_log_source TO moa_auditor;
GRANT SELECT ON moa.artifact_revision_contribution TO moa_auditor;
GRANT SELECT ON moa.artifact_suite_contribution TO moa_auditor;
GRANT SELECT ON moa.privacy_erasure_record_decision TO moa_auditor;

CREATE POLICY rd_auditor ON public.contacts
    AS PERMISSIVE FOR SELECT TO moa_auditor
    USING (true);

CREATE POLICY rd_auditor ON moa.edge_index
    AS PERMISSIVE FOR SELECT TO moa_auditor
    USING (true);

CREATE POLICY rd_auditor ON public.sessions
    AS PERMISSIVE FOR SELECT TO moa_auditor
    USING (true);

CREATE POLICY rd_auditor ON public.task_segments
    AS PERMISSIVE FOR SELECT TO moa_auditor
    USING (true);

CREATE POLICY rd_auditor ON public.experience_records
    AS PERMISSIVE FOR SELECT TO moa_auditor
    USING (true);

CREATE POLICY rd_auditor ON public.experience_attributions
    AS PERMISSIVE FOR SELECT TO moa_auditor
    USING (true);

CREATE POLICY rd_auditor ON public.learning_candidates
    AS PERMISSIVE FOR SELECT TO moa_auditor
    USING (true);

CREATE POLICY rd_auditor ON public.learning_candidate_source
    AS PERMISSIVE FOR SELECT TO moa_auditor
    USING (true);

CREATE POLICY rd_auditor ON public.learning_candidate_decision
    AS PERMISSIVE FOR SELECT TO moa_auditor
    USING (true);

CREATE POLICY rd_auditor ON public.learning_log
    AS PERMISSIVE FOR SELECT TO moa_auditor
    USING (true);

CREATE POLICY rd_auditor ON public.learning_log_source
    AS PERMISSIVE FOR SELECT TO moa_auditor
    USING (true);

CREATE POLICY rd_auditor ON moa.artifact_revision_contribution
    AS PERMISSIVE FOR SELECT TO moa_auditor
    USING (true);

CREATE POLICY rd_auditor ON moa.artifact_suite_contribution
    AS PERMISSIVE FOR SELECT TO moa_auditor
    USING (true);

CREATE POLICY rd_auditor ON moa.privacy_erasure_record_decision
    AS PERMISSIVE FOR SELECT TO moa_auditor
    USING (true);
