//! Consolidated `_db` integration harness for `moa-kms`.
//!
//! These tests require a live migrated Postgres (Docker Compose default at
//! `127.0.0.1:10040`) with the `moa.kek` table (migration V000340). Each test
//! runs against its own isolated database and uses fresh tenant/subject UUIDs, so
//! the harness is parallel-safe.

#[path = "kms_db/postgres_kms_db.rs"]
mod postgres_kms_db;
