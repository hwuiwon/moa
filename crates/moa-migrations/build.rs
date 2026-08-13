//! Rebuild trigger for the embedded migration set.
//!
//! `refinery::embed_migrations!` reads `migrations/postgres` at compile time, but a macro
//! reading the filesystem is invisible to cargo's change detection: adding or editing a
//! `.sql` file does not touch any `.rs` file, so cargo considers the crate fresh and the
//! binary keeps the migration set it was last compiled with.
//!
//! The failure that produces is expensive to diagnose, because nothing reports a stale
//! embed. Migrations appear to run, the new migration is simply absent, and the first
//! symptom arrives much later as a runtime error from a query naming a column that the
//! migration file on disk plainly creates.
//!
//! Emitting the directory here makes the dependency explicit, so a new or edited migration
//! rebuilds the crate and everything that embeds it.
fn main() {
    println!("cargo:rerun-if-changed=migrations/postgres");
}
