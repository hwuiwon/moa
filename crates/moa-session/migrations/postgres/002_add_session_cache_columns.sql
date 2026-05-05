ALTER TABLE sessions
    ADD COLUMN IF NOT EXISTS total_input_tokens_uncached BIGINT DEFAULT 0,
    ADD COLUMN IF NOT EXISTS total_input_tokens_cache_write BIGINT DEFAULT 0,
    ADD COLUMN IF NOT EXISTS total_input_tokens_cache_read BIGINT DEFAULT 0;
