use std::time::Duration;

use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

pub async fn migrated_ocsf_pool() -> sqlx::PgPool {
    let database_url = test_database_url();
    let schema_name = format!("moa_ocsf_test_{}", Uuid::new_v4().simple());
    let search_path = format!("{}, public", quote_identifier(&schema_name));
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_secs(5))
        .after_connect(move |conn, _meta| {
            let search_path = search_path.clone();
            Box::pin(async move {
                sqlx::query("SELECT pg_catalog.set_config('search_path', $1, false)")
                    .bind(search_path)
                    .execute(conn)
                    .await?;
                Ok(())
            })
        })
        .connect(&database_url)
        .await
        .expect("test Postgres should be reachable");
    moa_migrations::run_ocsf_schema(&pool, &schema_name)
        .await
        .expect("OCSF baseline should apply");
    pool
}

fn test_database_url() -> String {
    std::env::var("MOA_TEST_POSTGRES_URL")
        .or_else(|_| std::env::var("TEST_DATABASE_URL"))
        .or_else(|_| std::env::var("DATABASE_URL"))
        .unwrap_or_else(|_| "postgres://moa_owner:dev@localhost:10040/moa".to_string())
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}
