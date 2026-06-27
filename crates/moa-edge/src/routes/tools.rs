//! Public tool route translation.

use axum::body::Bytes;
use axum::http::{Method, Uri};
use moa_core::TenantId;

use super::RouteTranslation;

pub(super) fn translate(
    _method: &Method,
    _uri: &Uri,
    _body: &Bytes,
    _tenant_id: TenantId,
) -> Option<RouteTranslation> {
    None
}
