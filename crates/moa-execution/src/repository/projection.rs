//! Durable budget and scheduler projection reconstruction.

use super::*;

pub(super) fn budget_ledger(run: &ExecutionRunRecord) -> BudgetLedger {
    BudgetLedger {
        limit: run.approved_budget.clone(),
        reserved: run.reserved,
        consumed: run.consumed,
        overrun: run.budget_overrun,
    }
}

pub(super) fn terminal_projection_output(projection: &TerminalProjection) -> Option<Value> {
    match projection {
        TerminalProjection::Completed { output } => Some(output.clone()),
        TerminalProjection::Partial { output, .. } | TerminalProjection::Blocked { output, .. } => {
            output.clone()
        }
        TerminalProjection::Unsupported { .. }
        | TerminalProjection::Failed { .. }
        | TerminalProjection::Cancelled { .. } => None,
    }
}
