//! Contact identity domain and persistence helpers.

pub mod domain;
pub mod error;
pub mod repository;
pub mod verification_service;

pub use error::{ContactError, Result};
