//! Session store backends and backend selection for MOA.

#[cfg(not(any(feature = "turso", feature = "postgres")))]
compile_error!("At least one session store backend must be enabled: `turso` or `postgres`");
#[cfg(any(feature = "turso", feature = "postgres"))]
mod backend;

pub mod blob;
pub mod neon;
#[cfg(feature = "postgres")]
pub mod postgres;
#[cfg(feature = "postgres")]
pub mod queries_postgres;
#[cfg(feature = "turso")]
pub mod queries_turso;
#[cfg(feature = "postgres")]
pub mod schema_postgres;
#[cfg(feature = "turso")]
pub mod schema_turso;
#[cfg(feature = "turso")]
pub mod turso;

#[cfg(any(feature = "turso", feature = "postgres"))]
pub use backend::{SessionDatabase, create_session_store};
pub use blob::FileBlobStore;
pub use neon::NeonBranchManager;
#[cfg(feature = "postgres")]
pub use postgres::PostgresSessionStore;
#[cfg(feature = "turso")]
pub use turso::TursoSessionStore;
