//! Suite-agnostic evaluation kernel.
//!
//! Kernel modules may not import memory-suite or future suite modules. Suites
//! depend on this layer for common metrics, statistics, and report comparison.

pub mod compare;
pub mod core_metrics;
pub mod cost;
pub mod counting;
pub mod fixtures;
pub mod stats;

pub use core_metrics::{
    MetricSummary, PerLegRecall, PerLexicalBackendRecall, RetrievalCoreMetrics,
};
pub use cost::{CostError, CostLedger, ProviderProvenance};
pub use counting::{
    CountingEmbedder, CountingExtractor, CountingMergeVerifier, CountingReranker, SharedCostLedger,
};
pub use fixtures::{FixtureRecord, FixtureStore};
pub use stats::{
    BinaryProbeOutcome, BootstrapConfig, ClusterBootstrapReport, ClusterObservation,
    DEFAULT_BOOTSTRAP_RESAMPLES, PairedComparison, benjamini_hochberg,
    cluster_bootstrap_mean_by_user, mcnemar_paired_test,
};

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    #[test]
    fn kernel_sources_never_import_memory_eval() {
        // Pins: suite-agnostic kernel code must not depend on the memory suite.
        let kernel_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/kernel");
        let mut offenders = Vec::new();
        let forbidden = ["memory", "_eval"].concat();
        visit_rust_files(&kernel_dir, &mut |path| {
            let body = fs::read_to_string(path).expect("kernel source should be readable");
            let scanned = body
                .lines()
                .filter(|line| !line.contains("kernel_sources_never_import_memory_eval"))
                .collect::<Vec<_>>()
                .join("\n");
            if scanned.contains(&forbidden) {
                offenders.push(path.display().to_string());
            }
        });

        assert!(
            offenders.is_empty(),
            "kernel files must not import memory-suite modules: {}",
            offenders.join(", ")
        );
    }

    fn visit_rust_files(dir: &Path, visitor: &mut impl FnMut(&Path)) {
        for entry in fs::read_dir(dir).expect("kernel directory should be readable") {
            let path = entry
                .expect("kernel directory entry should be readable")
                .path();
            if path.is_dir() {
                visit_rust_files(&path, visitor);
            } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
                visitor(&path);
            }
        }
    }
}
