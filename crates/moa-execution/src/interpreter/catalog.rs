//! Capability-catalog validation for materialization inputs.

use super::*;

pub(super) fn validate_scheduler_catalog(catalog: &ExecutionCapabilityCatalog) -> Result<()> {
    let mut previous = None;
    for capability in &catalog.capabilities {
        if capability.estimate.tasks != 1 {
            return Err(Error::InvalidProjection {
                message: format!(
                    "capability {}@{} must reserve exactly one logical task",
                    capability.reference.name, capability.reference.version
                ),
            });
        }
        let key = canonical_sort_key(&capability.reference)?;
        if previous.as_ref().is_some_and(|previous| key <= *previous) {
            return Err(Error::InvalidProjection {
                message: "scheduler capability catalog must be sorted and duplicate-free"
                    .to_string(),
            });
        }
        previous = Some(key);
    }
    Ok(())
}
