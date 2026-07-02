//! Pure procedure graph interpreter for skill-backed procedure definitions.

use std::collections::{BTreeMap, BTreeSet};

use moa_artifacts::{
    procedure::{
        ProcedureCondition, ProcedureDefinition, ProcedureEdge, ProcedureNode, ProcedureNodeKind,
    },
    reference::ArtifactRef,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::procedure::error::{ProcedureError, Result};

/// Graph-renderable execution state for a procedure run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProcedureExecutionState {
    /// Durable procedure run identifier.
    pub run_uid: Uuid,
    /// Node currently being interpreted or blocked.
    pub current_node_id: Option<String>,
    /// Nodes that are ready or running.
    pub active_node_ids: BTreeSet<String>,
    /// Initial procedure input payload.
    pub input: Value,
    /// Persisted procedure state payload.
    pub state: Value,
    /// Nodes that completed successfully.
    pub completed_nodes: BTreeSet<String>,
    /// Nodes that failed during execution.
    pub failed_nodes: BTreeSet<String>,
    /// Nodes blocked on side effects.
    pub blocked_nodes: BTreeMap<String, ProcedureNodeRequest>,
    /// Parallel branch group state keyed by stable branch group ID.
    pub branch_groups: BTreeMap<String, BTreeSet<String>>,
    /// Loop traversal counts keyed by stable edge ID.
    pub loop_counts: BTreeMap<String, u32>,
    /// Maximum allowed traversals for a detected back-edge.
    pub max_loop_iterations: u32,
    /// Maximum number of branch requests one parallel node can create.
    pub max_parallel_branches: u32,
    /// Most recent traversed edge ID for UI path highlighting.
    pub last_edge_id: Option<String>,
    /// Traversed edge IDs in execution order for UI path highlighting.
    pub traversed_edge_ids: Vec<String>,
}

impl ProcedureExecutionState {
    /// Creates an execution state for a newly started procedure run.
    #[must_use]
    pub fn new(run_uid: Uuid, input: Value) -> Self {
        Self {
            run_uid,
            current_node_id: None,
            active_node_ids: BTreeSet::new(),
            input,
            state: Value::Object(Map::new()),
            completed_nodes: BTreeSet::new(),
            failed_nodes: BTreeSet::new(),
            blocked_nodes: BTreeMap::new(),
            branch_groups: BTreeMap::new(),
            loop_counts: BTreeMap::new(),
            max_loop_iterations: 16,
            max_parallel_branches: 8,
            last_edge_id: None,
            traversed_edge_ids: Vec::new(),
        }
    }
}

/// Pure interpreter over one procedure definition.
pub struct ProcedureInterpreter<'a> {
    /// Procedure graph to interpret.
    pub definition: &'a ProcedureDefinition,
}

impl<'a> ProcedureInterpreter<'a> {
    /// Creates an interpreter for a procedure definition.
    #[must_use]
    pub fn new(definition: &'a ProcedureDefinition) -> Self {
        Self { definition }
    }

    /// Advances the procedure until it completes or blocks on a side-effect node.
    pub fn advance(&self, mut state: ProcedureExecutionState) -> Result<ProcedureAdvance> {
        if let Some(advance) = advance_existing_blocked_state(&state) {
            return Ok(advance);
        }

        let mut node_id = self.next_node_id(&mut state)?;

        loop {
            state.current_node_id = Some(node_id.clone());
            state.active_node_ids.clear();
            state.active_node_ids.insert(node_id.clone());

            let node = self.node(&node_id)?;
            match &node.kind {
                ProcedureNodeKind::Start => {
                    state.completed_nodes.insert(node.id.clone());
                    let edge = self.select_edge(node, &state)?;
                    let next_node_id = edge.to.clone();
                    self.node(&next_node_id)?;
                    record_transition(&mut state, edge)?;
                    node_id = next_node_id;
                }
                ProcedureNodeKind::Condition => {
                    if let Some(condition) = &node.condition
                        && !evaluate_condition(condition, &state)?
                    {
                        return Err(ProcedureError::NoMatchingOutgoingEdge {
                            node_id: node.id.clone(),
                        });
                    }
                    state.completed_nodes.insert(node.id.clone());
                    let edge = self.select_edge(node, &state)?;
                    let next_node_id = edge.to.clone();
                    self.node(&next_node_id)?;
                    record_transition(&mut state, edge)?;
                    node_id = next_node_id;
                }
                ProcedureNodeKind::End => {
                    state.completed_nodes.insert(node.id.clone());
                    state.active_node_ids.clear();
                    let output = if is_empty_object(&node.input) {
                        state.state.clone()
                    } else {
                        node.input.clone()
                    };
                    return Ok(ProcedureAdvance::Completed { state, output });
                }
                ProcedureNodeKind::Action
                | ProcedureNodeKind::Tool
                | ProcedureNodeKind::SkillAction
                | ProcedureNodeKind::Agent
                | ProcedureNodeKind::Worker
                | ProcedureNodeKind::Review
                | ProcedureNodeKind::WaitSignal
                | ProcedureNodeKind::MemoryRead
                | ProcedureNodeKind::MemoryWrite => {
                    let request = ProcedureNodeRequest::from_node(node)?;
                    state.blocked_nodes.insert(node.id.clone(), request.clone());
                    return Ok(ProcedureAdvance::Blocked { state, request });
                }
                ProcedureNodeKind::Parallel => {
                    state.completed_nodes.insert(node.id.clone());
                    let edges = self.select_parallel_edges(node, &state)?;
                    if edges.len() > state.max_parallel_branches as usize {
                        return Err(ProcedureError::ParallelFanOutExceeded {
                            node_id: node.id.clone(),
                            branch_count: edges.len(),
                            max_branches: state.max_parallel_branches,
                        });
                    }

                    let mut branch_node_ids = BTreeSet::new();
                    for edge in &edges {
                        self.node(&edge.to)?;
                        branch_node_ids.insert(edge.to.clone());
                    }
                    let join_id = self.join_id_for_parallel_branches(node, &branch_node_ids)?;
                    reset_parallel_branch_state(&mut state, &join_id, &branch_node_ids);

                    let mut requests = Vec::with_capacity(edges.len());
                    for edge in &edges {
                        let branch = self.node(&edge.to)?;
                        let request = ProcedureNodeRequest::from_node(branch)?;
                        record_transition(&mut state, edge)?;
                        state
                            .blocked_nodes
                            .insert(branch.id.clone(), request.clone());
                        requests.push(request);
                    }
                    state.current_node_id = Some(node.id.clone());
                    state.active_node_ids = branch_node_ids.clone();
                    state.branch_groups.insert(join_id, branch_node_ids);
                    return Ok(ProcedureAdvance::Ready { state, requests });
                }
                ProcedureNodeKind::Join => {
                    if let Some(failed_node_ids) = self.failed_join_branch_ids(node, &state) {
                        return Err(ProcedureError::ParallelBranchFailed {
                            join_node_id: node.id.clone(),
                            failed_node_ids,
                        });
                    }
                    if !self.join_requirements_satisfied(node, &state) {
                        return Ok(ProcedureAdvance::Ready {
                            state,
                            requests: Vec::new(),
                        });
                    }
                    state.completed_nodes.insert(node.id.clone());
                    let edge = self.select_edge(node, &state)?;
                    let next_node_id = edge.to.clone();
                    self.node(&next_node_id)?;
                    record_transition(&mut state, edge)?;
                    node_id = next_node_id;
                }
            }
        }
    }

    fn next_node_id(&self, state: &mut ProcedureExecutionState) -> Result<String> {
        if let Some(node_id) = state.current_node_id.clone() {
            if state.active_node_ids.is_empty() {
                state.active_node_ids.insert(node_id.clone());
            } else if !state.active_node_ids.contains(&node_id) {
                return Err(ProcedureError::CurrentNodeNotActive { node_id });
            }
            if state.active_node_ids.len() > 1 {
                return Err(ProcedureError::MultipleActiveNodesUnsupported {
                    count: state.active_node_ids.len(),
                });
            }
            self.node(&node_id)?;
            return Ok(node_id);
        }

        match state.active_node_ids.len() {
            0 => self.start_node_id(),
            1 => Err(ProcedureError::MissingCurrentNodeForActiveState),
            count => Err(ProcedureError::MultipleActiveNodesUnsupported { count }),
        }
    }

    fn start_node_id(&self) -> Result<String> {
        let mut starts = self
            .definition
            .nodes
            .iter()
            .filter(|node| node.kind == ProcedureNodeKind::Start);
        let start = starts.next().ok_or(ProcedureError::MissingStartNode)?;
        if starts.next().is_some() {
            return Err(ProcedureError::MultipleStartNodes);
        }
        Ok(start.id.clone())
    }

    fn node(&self, node_id: &str) -> Result<&ProcedureNode> {
        self.definition
            .nodes
            .iter()
            .find(|node| node.id == node_id)
            .ok_or_else(|| ProcedureError::NodeNotFound {
                node_id: node_id.to_string(),
            })
    }

    fn select_edge(
        &self,
        node: &ProcedureNode,
        state: &ProcedureExecutionState,
    ) -> Result<&ProcedureEdge> {
        let mut conditional_matches = Vec::new();
        let mut default_edges = Vec::new();
        for edge in self
            .definition
            .edges
            .iter()
            .filter(|edge| edge.from == node.id)
        {
            match &edge.when {
                Some(condition) if evaluate_condition(condition, state)? => {
                    conditional_matches.push(edge);
                }
                Some(_) => {}
                None => default_edges.push(edge),
            }
        }

        let selected = if conditional_matches.is_empty() {
            match default_edges.as_slice() {
                [] => {
                    return Err(ProcedureError::NoMatchingOutgoingEdge {
                        node_id: node.id.clone(),
                    });
                }
                [selected] => *selected,
                defaults => {
                    return Err(ProcedureError::AmbiguousOutgoingEdges {
                        node_id: node.id.clone(),
                        matched_count: defaults.len(),
                    });
                }
            }
        } else {
            match conditional_matches.as_slice() {
                [selected] => *selected,
                matches => {
                    return Err(ProcedureError::AmbiguousOutgoingEdges {
                        node_id: node.id.clone(),
                        matched_count: matches.len(),
                    });
                }
            }
        };
        Ok(selected)
    }

    fn select_parallel_edges(
        &self,
        node: &ProcedureNode,
        state: &ProcedureExecutionState,
    ) -> Result<Vec<ProcedureEdge>> {
        let mut edges = Vec::new();
        for edge in self
            .definition
            .edges
            .iter()
            .filter(|edge| edge.from == node.id)
        {
            if let Some(condition) = &edge.when
                && !evaluate_condition(condition, state)?
            {
                continue;
            }
            edges.push(edge.clone());
        }
        if edges.is_empty() {
            return Err(ProcedureError::NoMatchingOutgoingEdge {
                node_id: node.id.clone(),
            });
        }
        Ok(edges)
    }

    fn join_requirements_satisfied(
        &self,
        node: &ProcedureNode,
        state: &ProcedureExecutionState,
    ) -> bool {
        self.required_join_branches(node, state)
            .is_some_and(|required| {
                required
                    .iter()
                    .all(|node_id| state.completed_nodes.contains(node_id))
            })
    }

    fn failed_join_branch_ids(
        &self,
        node: &ProcedureNode,
        state: &ProcedureExecutionState,
    ) -> Option<Vec<String>> {
        let failed = self
            .required_join_branches(node, state)?
            .iter()
            .filter(|node_id| state.failed_nodes.contains(*node_id))
            .cloned()
            .collect::<Vec<_>>();
        (!failed.is_empty()).then_some(failed)
    }

    fn required_join_branches<'state>(
        &self,
        node: &ProcedureNode,
        state: &'state ProcedureExecutionState,
    ) -> Option<&'state BTreeSet<String>> {
        state.branch_groups.get(&node.id)
    }

    fn join_id_for_parallel_branches(
        &self,
        node: &ProcedureNode,
        branch_node_ids: &BTreeSet<String>,
    ) -> Result<String> {
        let candidates = self
            .definition
            .nodes
            .iter()
            .filter(|candidate| candidate.kind == ProcedureNodeKind::Join)
            .filter(|candidate| {
                let incoming_sources = self
                    .definition
                    .edges
                    .iter()
                    .filter(|edge| edge.to == candidate.id)
                    .map(|edge| edge.from.as_str())
                    .collect::<BTreeSet<_>>();
                branch_node_ids
                    .iter()
                    .all(|branch_node_id| incoming_sources.contains(branch_node_id.as_str()))
            })
            .collect::<Vec<_>>();
        match candidates.as_slice() {
            [candidate] => Ok(candidate.id.clone()),
            [] => Err(ProcedureError::NoMatchingOutgoingEdge {
                node_id: node.id.clone(),
            }),
            candidates => Err(ProcedureError::AmbiguousOutgoingEdges {
                node_id: node.id.clone(),
                matched_count: candidates.len(),
            }),
        }
    }

    /// Completes one blocked side-effect node, records its output into state, and advances to the next graph node.
    pub fn complete_blocked_node(
        &self,
        mut state: ProcedureExecutionState,
        node_id: &str,
        output: Value,
    ) -> Result<ProcedureExecutionState> {
        state
            .blocked_nodes
            .remove(node_id)
            .ok_or_else(|| ProcedureError::BlockedNodeNotFound {
                node_id: node_id.to_string(),
            })?;
        let node = self.node(node_id)?;
        state.completed_nodes.insert(node.id.clone());
        match &mut state.state {
            Value::Object(map) => {
                map.insert(node.id.clone(), output);
            }
            _ => {
                let mut map = Map::new();
                map.insert(node.id.clone(), output);
                state.state = Value::Object(map);
            }
        }

        let edge = self.select_edge(node, &state)?;
        let next_node_id = edge.to.clone();
        self.node(&next_node_id)?;
        record_transition(&mut state, edge)?;
        state.current_node_id = Some(next_node_id.clone());
        state.active_node_ids = state.blocked_nodes.keys().cloned().collect();
        state.active_node_ids.insert(next_node_id);
        Ok(state)
    }
}

/// Result of one procedure interpreter advance.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ProcedureAdvance {
    /// The procedure reached an end node.
    Completed {
        /// Updated execution state.
        state: ProcedureExecutionState,
        /// Terminal procedure output.
        output: Value,
    },
    /// The procedure blocked on one side-effect node.
    Blocked {
        /// Updated execution state.
        state: ProcedureExecutionState,
        /// Request that must be satisfied by the orchestrator.
        request: ProcedureNodeRequest,
    },
    /// The procedure produced ready side-effect requests.
    Ready {
        /// Updated execution state.
        state: ProcedureExecutionState,
        /// Requests that can be run by the orchestrator.
        requests: Vec<ProcedureNodeRequest>,
    },
}

/// Side-effect request emitted by the pure interpreter.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ProcedureNodeRequest {
    /// Connector action or artifact action invocation.
    Action {
        /// Source procedure node ID.
        node_id: String,
        /// Optional action artifact reference.
        artifact_ref: Option<ArtifactRef>,
        /// Node input payload.
        input: Value,
    },
    /// Direct tool invocation.
    Tool {
        /// Source procedure node ID.
        node_id: String,
        /// Tool references allowed for this node.
        tool_refs: Vec<ArtifactRef>,
        /// Node input payload.
        input: Value,
    },
    /// Skill-declared action invocation.
    SkillAction {
        /// Source procedure node ID.
        node_id: String,
        /// Optional skill action reference.
        artifact_ref: Option<ArtifactRef>,
        /// Node input payload.
        input: Value,
    },
    /// Existing top-level agent loop invocation.
    Agent {
        /// Source procedure node ID.
        node_id: String,
        /// Skill references pinned for the agent.
        skill_refs: Vec<ArtifactRef>,
        /// Tool references pinned for the agent.
        tool_refs: Vec<ArtifactRef>,
        /// Node input payload.
        input: Value,
        /// Optional maximum autonomous turns.
        max_turns: Option<u32>,
    },
    /// Existing bounded worker invocation.
    Worker {
        /// Source procedure node ID.
        node_id: String,
        /// Skill references pinned for the worker.
        skill_refs: Vec<ArtifactRef>,
        /// Tool references pinned for the worker.
        tool_refs: Vec<ArtifactRef>,
        /// Node input payload.
        input: Value,
        /// Optional maximum autonomous turns.
        max_turns: Option<u32>,
    },
    /// Human or policy review pause.
    Review {
        /// Source procedure node ID.
        node_id: String,
        /// Node input payload.
        input: Value,
    },
    /// External signal wait.
    WaitSignal {
        /// Source procedure node ID.
        node_id: String,
        /// Node input payload.
        input: Value,
    },
    /// Graph memory retrieval request.
    MemoryRead {
        /// Source procedure node ID.
        node_id: String,
        /// Node input payload.
        input: Value,
    },
    /// Graph memory write request.
    MemoryWrite {
        /// Source procedure node ID.
        node_id: String,
        /// Node input payload.
        input: Value,
    },
}

impl ProcedureNodeRequest {
    fn from_node(node: &ProcedureNode) -> Result<Self> {
        Ok(match &node.kind {
            ProcedureNodeKind::Action => Self::Action {
                node_id: node.id.clone(),
                artifact_ref: node.artifact_ref.clone(),
                input: node.input.clone(),
            },
            ProcedureNodeKind::Tool => Self::Tool {
                node_id: node.id.clone(),
                tool_refs: node.tool_refs.clone(),
                input: node.input.clone(),
            },
            ProcedureNodeKind::SkillAction => Self::SkillAction {
                node_id: node.id.clone(),
                artifact_ref: node.artifact_ref.clone(),
                input: node.input.clone(),
            },
            ProcedureNodeKind::Agent => Self::Agent {
                node_id: node.id.clone(),
                skill_refs: node.skill_refs.clone(),
                tool_refs: node.tool_refs.clone(),
                input: node.input.clone(),
                max_turns: node.max_turns,
            },
            ProcedureNodeKind::Worker => Self::Worker {
                node_id: node.id.clone(),
                skill_refs: node.skill_refs.clone(),
                tool_refs: node.tool_refs.clone(),
                input: node.input.clone(),
                max_turns: node.max_turns,
            },
            ProcedureNodeKind::Review => Self::Review {
                node_id: node.id.clone(),
                input: node.input.clone(),
            },
            ProcedureNodeKind::WaitSignal => Self::WaitSignal {
                node_id: node.id.clone(),
                input: node.input.clone(),
            },
            ProcedureNodeKind::MemoryRead => Self::MemoryRead {
                node_id: node.id.clone(),
                input: node.input.clone(),
            },
            ProcedureNodeKind::MemoryWrite => Self::MemoryWrite {
                node_id: node.id.clone(),
                input: node.input.clone(),
            },
            ProcedureNodeKind::Start
            | ProcedureNodeKind::Condition
            | ProcedureNodeKind::End
            | ProcedureNodeKind::Parallel
            | ProcedureNodeKind::Join => {
                return Err(ProcedureError::UnsupportedNodeKind {
                    node_id: node.id.clone(),
                    kind: node_kind_label(&node.kind).to_string(),
                });
            }
        })
    }
}

fn advance_existing_blocked_state(state: &ProcedureExecutionState) -> Option<ProcedureAdvance> {
    let requests = state
        .blocked_nodes
        .values()
        .cloned()
        .collect::<Vec<ProcedureNodeRequest>>();
    match requests.as_slice() {
        [] => None,
        [request] => Some(ProcedureAdvance::Blocked {
            state: state.clone(),
            request: request.clone(),
        }),
        _ => Some(ProcedureAdvance::Ready {
            state: state.clone(),
            requests,
        }),
    }
}

fn reset_parallel_branch_state(
    state: &mut ProcedureExecutionState,
    join_id: &str,
    branch_node_ids: &BTreeSet<String>,
) {
    if let Some(previous_branch_node_ids) = state.branch_groups.get(join_id) {
        for node_id in previous_branch_node_ids {
            state.completed_nodes.remove(node_id);
            state.failed_nodes.remove(node_id);
            state.blocked_nodes.remove(node_id);
        }
    }
    for node_id in branch_node_ids {
        state.completed_nodes.remove(node_id);
        state.failed_nodes.remove(node_id);
        state.blocked_nodes.remove(node_id);
    }
    state.completed_nodes.remove(join_id);
    state.failed_nodes.remove(join_id);
    state.blocked_nodes.remove(join_id);
}

fn record_transition(state: &mut ProcedureExecutionState, edge: &ProcedureEdge) -> Result<()> {
    let edge_id = edge_id(edge);
    if state.completed_nodes.contains(&edge.to) {
        let count = state.loop_counts.entry(edge_id.clone()).or_insert(0);
        *count = count.saturating_add(1);
        if *count >= state.max_loop_iterations {
            return Err(ProcedureError::LoopIterationLimitExceeded {
                edge_id,
                attempted_iterations: *count,
                max_iterations: state.max_loop_iterations,
            });
        }
    }
    state.last_edge_id = Some(edge_id.clone());
    state.traversed_edge_ids.push(edge_id);
    Ok(())
}

fn edge_id(edge: &ProcedureEdge) -> String {
    edge.id
        .clone()
        .unwrap_or_else(|| format!("{}->{}", edge.from, edge.to))
}

fn evaluate_condition(
    condition: &ProcedureCondition,
    state: &ProcedureExecutionState,
) -> Result<bool> {
    match condition {
        ProcedureCondition::Equals { left, right } => Ok(resolve_path(left, state) == Some(right)),
        ProcedureCondition::Exists { path } => Ok(resolve_path(path, state).is_some()),
        ProcedureCondition::Expression { language, source } => {
            Err(ProcedureError::UnsupportedConditionExpression {
                language: language.clone(),
                expression: source.clone(),
            })
        }
    }
}

fn resolve_path<'a>(path: &str, state: &'a ProcedureExecutionState) -> Option<&'a Value> {
    let normalized = path
        .strip_prefix("$.")
        .or_else(|| path.strip_prefix('$'))
        .unwrap_or(path);
    let mut segments = normalized.split('.').filter(|segment| !segment.is_empty());
    match segments.next()? {
        "input" => descend(&state.input, segments),
        "state" => descend(&state.state, segments),
        first => {
            let mut state_segments = std::iter::once(first).chain(segments.clone());
            descend(&state.state, &mut state_segments)
                .or_else(|| descend(&state.input, std::iter::once(first).chain(segments)))
        }
    }
}

fn descend<I>(value: &Value, segments: I) -> Option<&Value>
where
    I: IntoIterator,
    I::Item: AsRef<str>,
{
    let mut current = value;
    for segment in segments {
        current = current.get(segment.as_ref())?;
    }
    Some(current)
}

fn is_empty_object(value: &Value) -> bool {
    matches!(value, Value::Object(map) if map.is_empty())
}

fn node_kind_label(kind: &ProcedureNodeKind) -> &'static str {
    match kind {
        ProcedureNodeKind::Start => "start",
        ProcedureNodeKind::Action => "action",
        ProcedureNodeKind::Condition => "condition",
        ProcedureNodeKind::Review => "review",
        ProcedureNodeKind::Agent => "agent",
        ProcedureNodeKind::End => "end",
        ProcedureNodeKind::Tool => "tool",
        ProcedureNodeKind::SkillAction => "skill_action",
        ProcedureNodeKind::Worker => "worker",
        ProcedureNodeKind::Parallel => "parallel",
        ProcedureNodeKind::Join => "join",
        ProcedureNodeKind::WaitSignal => "wait_signal",
        ProcedureNodeKind::MemoryRead => "memory_read",
        ProcedureNodeKind::MemoryWrite => "memory_write",
    }
}

#[cfg(test)]
mod tests {
    use moa_artifacts::procedure::{ProcedureEdge, ProcedureNode};
    use moa_artifacts::reference::ArtifactRef;
    use serde_json::json;

    use super::*;

    #[test]
    fn sequential_start_to_end_completes() {
        // Pins: deterministic sequential procedures execute entirely inside the pure interpreter.
        let definition = procedure(
            vec![
                node("start", ProcedureNodeKind::Start),
                node("end", ProcedureNodeKind::End).with_input(json!({ "done": true })),
            ],
            vec![edge("start-end", "start", "end")],
        );
        let run_uid = Uuid::now_v7();
        let result = ProcedureInterpreter::new(&definition)
            .advance(ProcedureExecutionState::new(
                run_uid,
                json!({ "ticket": "T-1" }),
            ))
            .expect("procedure should advance");

        let ProcedureAdvance::Completed { state, output } = result else {
            panic!("expected completed procedure");
        };
        assert_eq!(output, json!({ "done": true }));
        assert_eq!(state.current_node_id.as_deref(), Some("end"));
        assert_eq!(
            state.completed_nodes,
            BTreeSet::from(["end".to_string(), "start".to_string()])
        );
        assert_eq!(state.traversed_edge_ids, vec!["start-end"]);
        assert_eq!(state.active_node_ids, BTreeSet::new());
    }

    #[test]
    fn condition_edge_selects_matching_branch() {
        // Pins: branch choice is encoded by explicit graph edges and persisted edge IDs.
        let definition = procedure(
            vec![
                node("start", ProcedureNodeKind::Start),
                node("route", ProcedureNodeKind::Condition),
                node("vip", ProcedureNodeKind::End).with_input(json!({ "lane": "vip" })),
                node("standard", ProcedureNodeKind::End).with_input(json!({ "lane": "standard" })),
            ],
            vec![
                edge("start-route", "start", "route"),
                edge("route-vip", "route", "vip").when(ProcedureCondition::Equals {
                    left: "input.priority".to_string(),
                    right: json!("vip"),
                }),
                edge("route-standard", "route", "standard"),
            ],
        );

        let result = ProcedureInterpreter::new(&definition)
            .advance(ProcedureExecutionState::new(
                Uuid::now_v7(),
                json!({ "priority": "vip" }),
            ))
            .expect("procedure should advance");

        let ProcedureAdvance::Completed { state, output } = result else {
            panic!("expected completed procedure");
        };
        assert_eq!(output, json!({ "lane": "vip" }));
        assert_eq!(state.current_node_id.as_deref(), Some("vip"));
        assert_eq!(state.traversed_edge_ids, vec!["start-route", "route-vip"]);
    }

    #[test]
    fn effectful_tool_node_blocks_with_request() {
        // Pins: side-effect nodes return a typed request without hidden execution.
        let tool_ref = ArtifactRef::tool("orders.lookup");
        let definition = procedure(
            vec![
                node("start", ProcedureNodeKind::Start),
                node("lookup", ProcedureNodeKind::Tool)
                    .with_tool_refs(vec![tool_ref.clone()])
                    .with_input(json!({ "order_id": "O-1" })),
                node("end", ProcedureNodeKind::End),
            ],
            vec![
                edge("start-lookup", "start", "lookup"),
                edge("lookup-end", "lookup", "end"),
            ],
        );

        let result = ProcedureInterpreter::new(&definition)
            .advance(ProcedureExecutionState::new(Uuid::now_v7(), json!({})))
            .expect("procedure should advance");

        let ProcedureAdvance::Blocked { state, request } = result else {
            panic!("expected blocked procedure");
        };
        assert_eq!(
            request,
            ProcedureNodeRequest::Tool {
                node_id: "lookup".to_string(),
                tool_refs: vec![tool_ref],
                input: json!({ "order_id": "O-1" }),
            }
        );
        assert_eq!(state.current_node_id.as_deref(), Some("lookup"));
        assert_eq!(
            state.active_node_ids,
            BTreeSet::from(["lookup".to_string()])
        );
        assert_eq!(state.blocked_nodes.get("lookup"), Some(&request));
        assert_eq!(state.completed_nodes, BTreeSet::from(["start".to_string()]));
    }

    #[test]
    fn completing_blocked_node_records_output_and_continues() {
        // Pins: side-effect completion resumes through declared graph edges.
        let definition = procedure(
            vec![
                node("start", ProcedureNodeKind::Start),
                node("lookup", ProcedureNodeKind::Tool)
                    .with_tool_refs(vec![ArtifactRef::tool("orders.lookup")]),
                node("end", ProcedureNodeKind::End).with_input(json!({ "done": true })),
            ],
            vec![
                edge("start-lookup", "start", "lookup"),
                edge("lookup-end", "lookup", "end"),
            ],
        );
        let blocked = ProcedureInterpreter::new(&definition)
            .advance(ProcedureExecutionState::new(
                Uuid::now_v7(),
                json!({ "order_id": "O-1" }),
            ))
            .expect("procedure should block on tool");
        let ProcedureAdvance::Blocked { state, .. } = blocked else {
            panic!("expected blocked procedure");
        };

        let resumed = ProcedureInterpreter::new(&definition)
            .complete_blocked_node(state, "lookup", json!({ "status": "ok" }))
            .expect("blocked node should complete");
        assert_eq!(resumed.current_node_id.as_deref(), Some("end"));
        assert_eq!(resumed.state["lookup"], json!({ "status": "ok" }));
        assert_eq!(
            resumed.completed_nodes,
            BTreeSet::from(["lookup".to_string(), "start".to_string()])
        );

        let result = ProcedureInterpreter::new(&definition)
            .advance(resumed)
            .expect("procedure should finish after tool");
        let ProcedureAdvance::Completed { output, state } = result else {
            panic!("expected completed procedure");
        };
        assert_eq!(output, json!({ "done": true }));
        assert_eq!(state.current_node_id.as_deref(), Some("end"));
        assert!(state.completed_nodes.contains("end"));
    }

    #[test]
    fn loop_back_edge_stops_at_iteration_limit() {
        // Pins: explicit graph back-edges are bounded by persisted loop counters.
        let definition = procedure(
            vec![
                node("start", ProcedureNodeKind::Start),
                node("retry", ProcedureNodeKind::Condition),
                node("end", ProcedureNodeKind::End),
            ],
            vec![
                edge("start-retry", "start", "retry"),
                edge("retry-loop", "retry", "retry").when(ProcedureCondition::Exists {
                    path: "input.retry".to_string(),
                }),
                edge("retry-end", "retry", "end"),
            ],
        );
        let mut state = ProcedureExecutionState::new(Uuid::now_v7(), json!({ "retry": true }));
        state.max_loop_iterations = 1;

        let error = ProcedureInterpreter::new(&definition)
            .advance(state)
            .expect_err("loop should stop at limit");
        assert!(matches!(
            error,
            ProcedureError::LoopIterationLimitExceeded {
                edge_id,
                attempted_iterations: 1,
                max_iterations: 1
            } if edge_id == "retry-loop"
        ));
    }

    #[test]
    fn parallel_node_returns_branch_requests() {
        // Pins: parallel fan-out is explicit graph topology plus branch-group state.
        let definition = parallel_procedure();

        let result = ProcedureInterpreter::new(&definition)
            .advance(ProcedureExecutionState::new(Uuid::now_v7(), json!({})))
            .expect("parallel procedure should advance");

        let ProcedureAdvance::Ready { state, requests } = result else {
            panic!("expected ready branch requests");
        };
        assert_eq!(
            requests.iter().map(request_node_id).collect::<Vec<_>>(),
            vec!["left", "right"]
        );
        assert_eq!(
            state.active_node_ids,
            BTreeSet::from(["left".to_string(), "right".to_string()])
        );
        assert_eq!(
            state.branch_groups.get("join"),
            Some(&BTreeSet::from(["left".to_string(), "right".to_string()]))
        );
        assert_eq!(
            state.traversed_edge_ids,
            vec!["start-fanout", "fanout-left", "fanout-right"]
        );
    }

    #[test]
    fn join_waits_for_all_required_branches() {
        // Pins: join nodes do not transition until every required branch is terminal.
        let definition = parallel_procedure();
        let mut state = ProcedureExecutionState::new(Uuid::now_v7(), json!({}));
        state.current_node_id = Some("join".to_string());
        state.active_node_ids = BTreeSet::from(["join".to_string()]);
        state.completed_nodes = BTreeSet::from(["fanout".to_string(), "left".to_string()]);
        state.branch_groups.insert(
            "join".to_string(),
            BTreeSet::from(["left".to_string(), "right".to_string()]),
        );

        let result = ProcedureInterpreter::new(&definition)
            .advance(state)
            .expect("join should wait instead of failing");

        let ProcedureAdvance::Ready { state, requests } = result else {
            panic!("expected waiting join");
        };
        assert!(requests.is_empty());
        assert_eq!(state.current_node_id.as_deref(), Some("join"));
        assert!(!state.completed_nodes.contains("join"));
    }

    #[test]
    fn join_ignores_branch_group_keyed_to_another_join() {
        // Pins: a join only consumes the branch group keyed by its own node id.
        let definition = parallel_procedure();
        let mut state = ProcedureExecutionState::new(Uuid::now_v7(), json!({}));
        state.current_node_id = Some("join".to_string());
        state.active_node_ids = BTreeSet::from(["join".to_string()]);
        state.completed_nodes = BTreeSet::from([
            "fanout".to_string(),
            "left".to_string(),
            "right".to_string(),
        ]);
        state.branch_groups.insert(
            "other_join".to_string(),
            BTreeSet::from(["left".to_string(), "right".to_string()]),
        );

        let result = ProcedureInterpreter::new(&definition)
            .advance(state)
            .expect("join should wait without its own branch group");

        let ProcedureAdvance::Ready { state, requests } = result else {
            panic!("expected waiting join");
        };
        assert!(requests.is_empty());
        assert_eq!(state.current_node_id.as_deref(), Some("join"));
        assert!(!state.completed_nodes.contains("join"));
    }

    #[test]
    fn reentering_parallel_clears_stale_branch_completion() {
        // Pins: looped parallel sections require fresh branch completions before the join passes.
        let definition = parallel_procedure();
        let mut state = ProcedureExecutionState::new(Uuid::now_v7(), json!({}));
        state.current_node_id = Some("fanout".to_string());
        state.active_node_ids = BTreeSet::from(["fanout".to_string()]);
        state.completed_nodes = BTreeSet::from([
            "start".to_string(),
            "fanout".to_string(),
            "left".to_string(),
            "right".to_string(),
            "join".to_string(),
        ]);
        state.branch_groups.insert(
            "join".to_string(),
            BTreeSet::from(["left".to_string(), "right".to_string()]),
        );

        let result = ProcedureInterpreter::new(&definition)
            .advance(state)
            .expect("parallel re-entry should create fresh branch requests");

        let ProcedureAdvance::Ready { state, requests } = result else {
            panic!("expected fresh parallel branch requests");
        };
        assert_eq!(
            requests.iter().map(request_node_id).collect::<Vec<_>>(),
            vec!["left", "right"]
        );
        assert!(!state.completed_nodes.contains("left"));
        assert!(!state.completed_nodes.contains("right"));
        assert!(!state.completed_nodes.contains("join"));
        assert_eq!(
            state.branch_groups.get("join"),
            Some(&BTreeSet::from(["left".to_string(), "right".to_string()]))
        );
    }

    #[test]
    fn failed_branch_fails_join() {
        // Pins: failed required branches fail the join deterministically.
        let definition = parallel_procedure();
        let mut state = ProcedureExecutionState::new(Uuid::now_v7(), json!({}));
        state.current_node_id = Some("join".to_string());
        state.active_node_ids = BTreeSet::from(["join".to_string()]);
        state.completed_nodes = BTreeSet::from(["fanout".to_string(), "left".to_string()]);
        state.failed_nodes = BTreeSet::from(["right".to_string()]);
        state.branch_groups.insert(
            "join".to_string(),
            BTreeSet::from(["left".to_string(), "right".to_string()]),
        );

        let error = ProcedureInterpreter::new(&definition)
            .advance(state)
            .expect_err("failed branch should fail the join");
        assert!(matches!(
            error,
            ProcedureError::ParallelBranchFailed {
                join_node_id,
                failed_node_ids
            } if join_node_id == "join" && failed_node_ids == vec!["right".to_string()]
        ));
    }

    fn procedure(nodes: Vec<ProcedureNode>, edges: Vec<ProcedureEdge>) -> ProcedureDefinition {
        ProcedureDefinition {
            input_schema: json!({}),
            state_schema: json!({}),
            nodes,
            edges,
            ui: json!({}),
        }
    }

    fn parallel_procedure() -> ProcedureDefinition {
        procedure(
            vec![
                node("start", ProcedureNodeKind::Start),
                node("fanout", ProcedureNodeKind::Parallel),
                node("left", ProcedureNodeKind::Tool)
                    .with_tool_refs(vec![ArtifactRef::tool("left.tool")]),
                node("right", ProcedureNodeKind::Tool)
                    .with_tool_refs(vec![ArtifactRef::tool("right.tool")]),
                node("join", ProcedureNodeKind::Join),
                node("end", ProcedureNodeKind::End).with_input(json!({ "joined": true })),
            ],
            vec![
                edge("start-fanout", "start", "fanout"),
                edge("fanout-left", "fanout", "left"),
                edge("fanout-right", "fanout", "right"),
                edge("left-join", "left", "join"),
                edge("right-join", "right", "join"),
                edge("join-end", "join", "end"),
            ],
        )
    }

    fn request_node_id(request: &ProcedureNodeRequest) -> &str {
        match request {
            ProcedureNodeRequest::Action { node_id, .. }
            | ProcedureNodeRequest::Tool { node_id, .. }
            | ProcedureNodeRequest::SkillAction { node_id, .. }
            | ProcedureNodeRequest::Agent { node_id, .. }
            | ProcedureNodeRequest::Worker { node_id, .. }
            | ProcedureNodeRequest::Review { node_id, .. }
            | ProcedureNodeRequest::WaitSignal { node_id, .. }
            | ProcedureNodeRequest::MemoryRead { node_id, .. }
            | ProcedureNodeRequest::MemoryWrite { node_id, .. } => node_id,
        }
    }

    fn node(id: &str, kind: ProcedureNodeKind) -> ProcedureNode {
        ProcedureNode {
            id: id.to_string(),
            kind,
            artifact_ref: None,
            skill_refs: Vec::new(),
            tool_refs: Vec::new(),
            condition: None,
            input: json!({}),
            max_turns: None,
            ui: json!({}),
        }
    }

    trait NodeBuilder {
        fn with_input(self, input: Value) -> Self;
        fn with_tool_refs(self, tool_refs: Vec<ArtifactRef>) -> Self;
    }

    impl NodeBuilder for ProcedureNode {
        fn with_input(mut self, input: Value) -> Self {
            self.input = input;
            self
        }

        fn with_tool_refs(mut self, tool_refs: Vec<ArtifactRef>) -> Self {
            self.tool_refs = tool_refs;
            self
        }
    }

    fn edge(id: &str, from: &str, to: &str) -> ProcedureEdge {
        ProcedureEdge {
            id: Some(id.to_string()),
            from: from.to_string(),
            to: to.to_string(),
            when: None,
        }
    }

    trait EdgeBuilder {
        fn when(self, condition: ProcedureCondition) -> Self;
    }

    impl EdgeBuilder for ProcedureEdge {
        fn when(mut self, condition: ProcedureCondition) -> Self {
            self.when = Some(condition);
            self
        }
    }
}
