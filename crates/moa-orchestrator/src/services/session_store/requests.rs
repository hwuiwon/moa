//! Request payload re-exports for the Restate session-store service.

pub use moa_core::wire::{
    AppendEventRequest, CompleteSegmentRequest, CreateSegmentRequest, GetEventsRequest,
    GetSegmentBaselineRequest, InitSessionVoRequest, ListSessionsRequest,
    ListSkillResolutionRatesRequest, RecordSegmentSkillActivationRequest,
    RecordSegmentToolUseRequest, RecordSegmentTurnUsageRequest, SearchEventsRequest,
    UpdateSegmentResolutionRequest, UpdateSegmentResolutionScoreRequest, UpdateStatusRequest,
    WorkspaceCostSinceRequest,
};
