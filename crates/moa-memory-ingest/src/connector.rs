//! Connector extension point for future external memory ingestion sources.

/// Marker trait for external ingestion connectors.
///
/// M20 fills this in with connector checkpointing and pull/push semantics. The stub keeps the
/// crate boundary stable without committing to the external CDC shape yet.
pub trait IngestConnector: Send + Sync {
    /// Returns the stable connector backend name.
    fn backend(&self) -> &'static str;
}
