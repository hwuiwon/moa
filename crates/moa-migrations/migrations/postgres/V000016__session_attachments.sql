-- Durable user-visible attachments linked from session messages.

CREATE TABLE IF NOT EXISTS public.session_attachments (
    id UUID PRIMARY KEY,
    session_id UUID NOT NULL,
    tenant_id UUID NOT NULL,
    contact_id UUID,
    name TEXT NOT NULL,
    mime_type TEXT NOT NULL,
    sha256 TEXT NOT NULL CHECK (sha256 ~ '^[0-9a-f]{64}$'),
    size_bytes BIGINT NOT NULL CHECK (size_bytes >= 0),
    object_key TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT session_attachments_session_tenant_fkey
        FOREIGN KEY (session_id, tenant_id)
        REFERENCES public.sessions(id, tenant_id)
        ON DELETE RESTRICT
);

CREATE INDEX IF NOT EXISTS idx_session_attachments_tenant_session_created
    ON public.session_attachments (tenant_id, session_id, created_at, id);

SELECT moa.apply_tenant_rls('public.session_attachments'::REGCLASS);
