//! Integration-style coverage for graph-memory query planning.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use moa_brain::planning::{PlanningCtx, QueryPlanner, Strategy};
use moa_core::TenantId;
use moa_memory_graph::{
    EdgeLabel, EdgeWriteIntent, GraphError, GraphStore, NodeIndexRow, NodeLabel, NodeWriteIntent,
    PiiClass,
};
use moa_memory_types::MemoryScope;
use uuid::Uuid;

#[tokio::test]
async fn planner_classify_and_seed_dependency_query() {
    let auth_uid = Uuid::now_v7();
    let graph = std::sync::Arc::new(SeedGraph {
        auth_uid,
        deploy_uid: Uuid::now_v7(),
    });
    let scope = MemoryScope::Tenant {
        tenant_id: TenantId::new(),
    };
    let ctx = PlanningCtx::new(scope.clone(), graph).with_seed_limit_per_span(3);

    let planned = QueryPlanner::new()
        .plan("What depends on the auth service?", &ctx)
        .await
        .expect("planner should ground auth seed");

    assert_eq!(planned.strategy, Strategy::GraphFirst);
    assert_eq!(planned.seeds, vec![auth_uid]);
    assert_eq!(planned.scope, scope);
    assert!(planned.temporal_filter.is_none());
}

#[tokio::test]
async fn planner_classify_vector_query_and_builds_retrieval_request() {
    let graph = std::sync::Arc::new(SeedGraph {
        auth_uid: Uuid::now_v7(),
        deploy_uid: Uuid::now_v7(),
    });
    let scope = MemoryScope::Tenant {
        tenant_id: TenantId::new(),
    };
    let ctx = PlanningCtx::new(scope.clone(), graph);

    let planned = QueryPlanner::new()
        .plan("How often does the deploy fail?", &ctx)
        .await
        .expect("planner should handle vector-heavy query");

    assert_eq!(planned.strategy, Strategy::VectorFirst);
    let request = planned.into_retrieval_request(
        "How often does the deploy fail?",
        vec![0.0; 1024],
        PiiClass::Restricted,
        5,
        false,
    );
    assert_eq!(request.strategy, Some(Strategy::VectorFirst));
    assert_eq!(request.scope, scope);
    assert_eq!(request.k_final, 5);
}

#[tokio::test]
async fn planner_passes_temporal_filter_to_seed_lookup() {
    let historical_uid = Uuid::now_v7();
    let graph = std::sync::Arc::new(TemporalSeedGraph { historical_uid });
    let scope = MemoryScope::Tenant {
        tenant_id: TenantId::new(),
    };
    let ctx = PlanningCtx::new(scope, graph);

    let planned = QueryPlanner::new()
        .plan("What did auth service use as of March 2026?", &ctx)
        .await
        .expect("planner should pass parsed as_of into seed lookup");

    assert_eq!(planned.temporal_filter, Some(utc("2026-03-01T00:00:00Z")));
    assert_eq!(planned.seeds, vec![historical_uid]);
}

#[derive(Clone)]
struct SeedGraph {
    auth_uid: Uuid,
    deploy_uid: Uuid,
}

#[async_trait]
impl GraphStore for SeedGraph {
    async fn create_node(&self, _intent: NodeWriteIntent) -> Result<Uuid, GraphError> {
        unreachable!("query planner tests do not write graph nodes")
    }

    async fn supersede_node(
        &self,
        _old_uid: Uuid,
        _intent: NodeWriteIntent,
    ) -> Result<Uuid, GraphError> {
        unreachable!("query planner tests do not supersede graph nodes")
    }

    async fn invalidate_node(&self, _uid: Uuid, _reason: &str) -> Result<(), GraphError> {
        unreachable!("query planner tests do not invalidate graph nodes")
    }

    async fn hard_purge(&self, _uid: Uuid, _redaction_marker: &str) -> Result<(), GraphError> {
        unreachable!("query planner tests do not purge graph nodes")
    }

    async fn create_edge(&self, _intent: EdgeWriteIntent) -> Result<Uuid, GraphError> {
        unreachable!("query planner tests do not write graph edges")
    }

    async fn get_node(&self, _uid: Uuid) -> Result<Option<NodeIndexRow>, GraphError> {
        Ok(None)
    }

    async fn neighbors(
        &self,
        _seed: Uuid,
        _hops: u8,
        _edge_filter: Option<&[EdgeLabel]>,
        _as_of: Option<DateTime<Utc>>,
    ) -> Result<Vec<NodeIndexRow>, GraphError> {
        Ok(Vec::new())
    }

    async fn expand_seeds(
        &self,
        _seeds: &[Uuid],
        _max_hops: u8,
        _as_of: Option<DateTime<Utc>>,
        _scoring: &moa_memory_graph::GraphWalkScoring,
    ) -> Result<Vec<moa_memory_graph::GraphExpansionHit>, GraphError> {
        Ok(Vec::new())
    }

    async fn lookup_seeds(
        &self,
        name: &str,
        _limit: i64,
        _as_of: Option<DateTime<Utc>>,
    ) -> Result<Vec<NodeIndexRow>, GraphError> {
        let lower = name.to_ascii_lowercase();
        if lower.contains("auth") {
            return Ok(vec![row(self.auth_uid, "auth service")]);
        }
        if lower.contains("deploy") {
            return Ok(vec![row(self.deploy_uid, "deploy pipeline")]);
        }
        Ok(Vec::new())
    }
}

#[derive(Clone)]
struct TemporalSeedGraph {
    historical_uid: Uuid,
}

#[async_trait]
impl GraphStore for TemporalSeedGraph {
    async fn create_node(&self, _intent: NodeWriteIntent) -> Result<Uuid, GraphError> {
        unreachable!("query planner tests do not write graph nodes")
    }

    async fn supersede_node(
        &self,
        _old_uid: Uuid,
        _intent: NodeWriteIntent,
    ) -> Result<Uuid, GraphError> {
        unreachable!("query planner tests do not supersede graph nodes")
    }

    async fn invalidate_node(&self, _uid: Uuid, _reason: &str) -> Result<(), GraphError> {
        unreachable!("query planner tests do not invalidate graph nodes")
    }

    async fn hard_purge(&self, _uid: Uuid, _redaction_marker: &str) -> Result<(), GraphError> {
        unreachable!("query planner tests do not purge graph nodes")
    }

    async fn create_edge(&self, _intent: EdgeWriteIntent) -> Result<Uuid, GraphError> {
        unreachable!("query planner tests do not write graph edges")
    }

    async fn get_node(&self, _uid: Uuid) -> Result<Option<NodeIndexRow>, GraphError> {
        Ok(None)
    }

    async fn neighbors(
        &self,
        _seed: Uuid,
        _hops: u8,
        _edge_filter: Option<&[EdgeLabel]>,
        _as_of: Option<DateTime<Utc>>,
    ) -> Result<Vec<NodeIndexRow>, GraphError> {
        Ok(Vec::new())
    }

    async fn expand_seeds(
        &self,
        _seeds: &[Uuid],
        _max_hops: u8,
        _as_of: Option<DateTime<Utc>>,
        _scoring: &moa_memory_graph::GraphWalkScoring,
    ) -> Result<Vec<moa_memory_graph::GraphExpansionHit>, GraphError> {
        Ok(Vec::new())
    }

    async fn lookup_seeds(
        &self,
        name: &str,
        _limit: i64,
        as_of: Option<DateTime<Utc>>,
    ) -> Result<Vec<NodeIndexRow>, GraphError> {
        assert_eq!(as_of, Some(utc("2026-03-01T00:00:00Z")));
        if name.to_ascii_lowercase().contains("auth") {
            return Ok(vec![row(
                self.historical_uid,
                "auth service historical fact",
            )]);
        }
        Ok(Vec::new())
    }
}

fn row(uid: Uuid, name: &str) -> NodeIndexRow {
    NodeIndexRow {
        uid,
        label: NodeLabel::Entity,
        storage_partition_id: Some("planner-workspace".to_string()),
        contact_id: None,
        scope: "tenant".to_string(),
        name: name.to_string(),
        pii_class: PiiClass::None,
        valid_to: None,
        valid_from: Utc::now(),
        properties_summary: None,
        last_accessed_at: Utc::now(),
        quality_score: 0.5,
    }
}

fn utc(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .expect("test timestamp should be valid RFC3339")
        .with_timezone(&Utc)
}
