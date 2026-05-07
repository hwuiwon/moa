//! Cache-breakpoint handling for the stable skill-manifest prefix.

use moa_core::{CacheTtl, WorkingContext};

pub(super) fn mark_stable_prefix_breakpoint(ctx: &mut WorkingContext) {
    ctx.mark_cache_breakpoint_with_ttl(CacheTtl::OneHour);
}
