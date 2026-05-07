//! Approval wait loops and resumed approval processing for the harness.

mod pending;
mod resolved;
mod signal_wait;

pub(super) use pending::wait_for_approval;
pub(super) use resolved::process_resolved_approval;
pub(super) use signal_wait::wait_for_signal_approval;
