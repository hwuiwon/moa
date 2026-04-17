//! Embedded `PostgreSQL` migrations for the wiki search index.

use moa_core::{MoaError, Result};
use sqlx::{PgPool, raw_sql};

const DEFAULT_MIGRATION: &str = include_str!("../migrations/001_wiki_pages.sql");

/// Runs the wiki search index migrations on the provided pool.
pub async fn migrate(pool: &PgPool, schema_name: Option<&str>) -> Result<()> {
    match schema_name {
        Some(schema_name) => migrate_in_schema(pool, schema_name).await,
        None => raw_sql(DEFAULT_MIGRATION)
            .execute(pool)
            .await
            .map(|_| ())
            .map_err(|error| {
                MoaError::StorageError(format!("wiki search migration failed: {error}"))
            }),
    }
}

fn migrate_in_schema_sql(schema_name: &str) -> String {
    let wiki_pages = qualified_name(schema_name, "wiki_pages");
    let wiki_pages_tsv_gin = quote_identifier("wiki_pages_tsv_gin");
    let wiki_pages_title_trgm = quote_identifier("wiki_pages_title_trgm");
    let wiki_pages_tags_gin = quote_identifier("wiki_pages_tags_gin");
    let wiki_pages_updated = quote_identifier("wiki_pages_updated");
    let wiki_pages_type = quote_identifier("wiki_pages_type");

    format!(
        r#"
        CREATE EXTENSION IF NOT EXISTS pg_trgm;

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
