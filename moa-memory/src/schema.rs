//! Embedded `PostgreSQL` migrations for the wiki search index.

use moa_core::{MoaError, Result};
use sqlx::{PgPool, raw_sql};

const CREATE_WIKI_PAGES_MIGRATION: &str = include_str!("../migrations/001_wiki_pages.sql");
const CREATE_WIKI_EMBEDDINGS_MIGRATION: &str =
    include_str!("../migrations/002_wiki_embeddings.sql");

/// Runs the wiki search index migrations on the provided pool.
pub async fn migrate(pool: &PgPool, schema_name: Option<&str>) -> Result<()> {
    match schema_name {
        Some(schema_name) => migrate_in_schema(pool, schema_name).await,
        None => {
            execute_migration(pool, CREATE_WIKI_PAGES_MIGRATION).await?;
            execute_migration(pool, CREATE_WIKI_EMBEDDINGS_MIGRATION).await
        }
    }
}

fn migrate_in_schema_sql(schema_name: &str) -> String {
    let wiki_pages = qualified_name(schema_name, "wiki_pages");
    let wiki_pages_tsv_gin = quote_identifier("wiki_pages_tsv_gin");
    let wiki_pages_title_trgm = quote_identifier("wiki_pages_title_trgm");
    let wiki_pages_tags_gin = quote_identifier("wiki_pages_tags_gin");
    let wiki_pages_updated = quote_identifier("wiki_pages_updated");
    let wiki_pages_type = quote_identifier("wiki_pages_type");
    let wiki_pages_embedding_hnsw = quote_identifier("wiki_pages_embedding_hnsw");
    let wiki_embedding_queue = qualified_name(schema_name, "wiki_embedding_queue");
    let wiki_embedding_queue_enqueued = quote_identifier("wiki_embedding_queue_enqueued");

    format!(
        r#"
        CREATE EXTENSION IF NOT EXISTS pg_trgm;
        CREATE EXTENSION IF NOT EXISTS vector;

        CREATE TABLE IF NOT EXISTS {wiki_pages} (
            scope TEXT NOT NULL,
            path TEXT NOT NULL,
            title TEXT NOT NULL,
            page_type TEXT NOT NULL,
            confidence TEXT NOT NULL,
            created TIMESTAMPTZ NOT NULL,
            updated TIMESTAMPTZ NOT NULL,
            last_referenced TIMESTAMPTZ NOT NULL,
            reference_count INTEGER NOT NULL DEFAULT 0,
            tags TEXT[] NOT NULL DEFAULT '{{}}',
            content TEXT NOT NULL,
            search_tsv TSVECTOR GENERATED ALWAYS AS (
                setweight(to_tsvector('english', coalesce(title, '')), 'A') ||
                setweight(array_to_tsvector(coalesce(tags, ARRAY[]::text[])), 'B') ||
                setweight(to_tsvector('english', coalesce(content, '')), 'C')
            ) STORED,
            PRIMARY KEY (scope, path)
        );

        CREATE INDEX IF NOT EXISTS {wiki_pages_tsv_gin}
            ON {wiki_pages} USING GIN (search_tsv);
        CREATE INDEX IF NOT EXISTS {wiki_pages_title_trgm}
            ON {wiki_pages} USING GIN (title gin_trgm_ops);
        CREATE INDEX IF NOT EXISTS {wiki_pages_tags_gin}
            ON {wiki_pages} USING GIN (tags);
        CREATE INDEX IF NOT EXISTS {wiki_pages_updated}
            ON {wiki_pages} (scope, updated DESC);
        CREATE INDEX IF NOT EXISTS {wiki_pages_type}
            ON {wiki_pages} (scope, page_type);

        ALTER TABLE {wiki_pages}
            ADD COLUMN IF NOT EXISTS embedding vector(1536),
            ADD COLUMN IF NOT EXISTS embedding_model TEXT,
            ADD COLUMN IF NOT EXISTS embedding_updated TIMESTAMPTZ;

        CREATE INDEX IF NOT EXISTS {wiki_pages_embedding_hnsw}
            ON {wiki_pages} USING hnsw (embedding vector_cosine_ops)
            WITH (m = 16, ef_construction = 64);

        CREATE TABLE IF NOT EXISTS {wiki_embedding_queue} (
            scope TEXT NOT NULL,
            path TEXT NOT NULL,
            enqueued_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            attempt_count INTEGER NOT NULL DEFAULT 0,
            last_error TEXT,
            PRIMARY KEY (scope, path)
        );

        CREATE INDEX IF NOT EXISTS {wiki_embedding_queue_enqueued}
            ON {wiki_embedding_queue} (enqueued_at);
        "#
    )
}

async fn migrate_in_schema(pool: &PgPool, schema_name: &str) -> Result<()> {
    raw_sql(&migrate_in_schema_sql(schema_name))
        .execute(pool)
        .await
        .map(|_| ())
        .map_err(|error| {
            MoaError::StorageError(format!(
                "wiki search migration failed for `{schema_name}`: {error}"
            ))
        })
}

async fn execute_migration(pool: &PgPool, sql: &str) -> Result<()> {
    raw_sql(sql)
        .execute(pool)
        .await
        .map(|_| ())
        .map_err(|error| MoaError::StorageError(format!("wiki search migration failed: {error}")))
}

fn qualified_name(schema_name: &str, object_name: &str) -> String {
    format!(
        "{}.{}",
        quote_identifier(schema_name),
        quote_identifier(object_name)
    )
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}
