//! Graph retrieval policy selection and behavior switches.

use serde::{Deserialize, Serialize};

/// Graph retrieval policy selected for one retriever or diagnostic run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GraphRetrievalPolicy {
    /// Do not run graph expansion as a ranking leg.
    Off,
    /// Reserve graph structure for post-selection context organization.
    ContextOnly,
    /// Use graph only to rescue candidates from precise anchors.
    #[default]
    AnchoredRescue,
    /// Use graph evidence at source-object ranking time.
    SourceGraph,
    /// Use entity-local graph search for anchored queries.
    EntityLocalSearch,
    /// Use bounded graph propagation for multi-hop retrieval.
    Propagation,
    /// Use precomputed graph community evidence for broad queries.
    Community,
}

impl GraphRetrievalPolicy {
    /// Returns the stable report and CLI label for this policy.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::ContextOnly => "context-only",
            Self::AnchoredRescue => "anchored-rescue",
            Self::SourceGraph => "source-graph",
            Self::EntityLocalSearch => "entity-local-search",
            Self::Propagation => "propagation",
            Self::Community => "community",
        }
    }

    /// Parses the stable CLI label for this policy.
    #[must_use]
    pub fn from_str_label(value: &str) -> Option<Self> {
        match value {
            "off" => Some(Self::Off),
            "context-only" => Some(Self::ContextOnly),
            "anchored-rescue" => Some(Self::AnchoredRescue),
            "source-graph" => Some(Self::SourceGraph),
            "entity-local-search" => Some(Self::EntityLocalSearch),
            "propagation" => Some(Self::Propagation),
            "community" => Some(Self::Community),
            _ => None,
        }
    }

    /// Returns whether this policy disables graph ranking and fusion.
    #[must_use]
    pub(crate) const fn disables_graph_ranking(self) -> bool {
        matches!(self, Self::Off | Self::ContextOnly)
    }

    /// Returns whether this policy allows semantic entity seeds.
    #[must_use]
    pub(crate) const fn allows_semantic_entity_seeds(self) -> bool {
        matches!(
            self,
            Self::EntityLocalSearch | Self::Propagation | Self::Community
        )
    }

    /// Returns whether this policy performs source-object ranking.
    #[must_use]
    pub(crate) const fn uses_source_object_ranking(self) -> bool {
        matches!(self, Self::SourceGraph | Self::EntityLocalSearch)
    }

    /// Returns whether graph candidates join normal RRF candidate fusion.
    #[must_use]
    pub(crate) const fn uses_graph_candidate_fusion(self) -> bool {
        !matches!(self, Self::EntityLocalSearch)
    }
}

/// Applies request-local graph disablement to the retriever policy.
#[must_use]
pub(crate) const fn effective_graph_policy(
    retriever_policy: GraphRetrievalPolicy,
    disable_graph_expansion: bool,
) -> GraphRetrievalPolicy {
    if disable_graph_expansion {
        GraphRetrievalPolicy::Off
    } else {
        retriever_policy
    }
}
