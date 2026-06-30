//! Pure workflow graph interpreter for artifact-backed workflow definitions.

use std::collections::{BTreeMap, BTreeSet};

use moa_artifacts::{
    reference::ArtifactRef,
    workflow::{
        WorkflowCondition, WorkflowDefinition, WorkflowEdge, WorkflowNode, WorkflowNodeKind,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::error::{Result, WorkflowError};

/// Graph-renderable execution state for a workflow run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkflowExecutionState {
    /// Durable workflow run identifier.
    pub run_uid: Uuid,
    /// Node currently being interpreted or blocked.
    pub current_node_id: Option<String>,
    /// Nodes that are ready or running.
    pub active_node_ids: BTreeSet<String>,
    /// Initial workflow input payload.
    pub input: Value,
    /// Persisted workflow state payload.
    pub state: Value,
    /// Nodes that completed successfully.
    pub completed_nodes: BTreeSet<String>,
    /// Nodes that failed during execution.
    pub failed_nodes: BTreeSet<String>,
    /// Nodes blocked on side effects.
    pub blocked_nodes: BTreeMap<String, WorkflowNodeRequest>,
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

impl WorkflowExecutionState {
    /// Creates an execution state for a newly started workflow run.
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

/// Pure interpreter over one workflow definition.
pub struct WorkflowInterpreter<'a> {
    /// Workflow graph to interpret.
    pub definition: &'a WorkflowDefinition,
}

impl<'a> WorkflowInterpreter<'a> {
    /// Creates an interpreter for a workflow definition.
    #[must_use]
    pub fn new(definition: &'a WorkflowDefinition) -> Self {
        Self { definition }
    }

    /// Advances the workflow until it completes or blocks on a side-effect node.
    pub fn advance(&self, mut state: WorkflowExecutionState) -> Result<WorkflowAdvance> {
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
                WorkflowNodeKind::Start => {
                    state.completed_nodes.insert(node.id.clone());
                    let edge = self.select_edge(node, &state)?;
                    let next_node_id = edge.to.clone();
                    self.node(&next_node_id)?;
                    record_transition(&mut state, edge)?;
                    node_id = next_node_id;
                }
                WorkflowNodeKind::Condition => {
                    if let Some(condition) = &node.condition
                        && !evaluate_condition(condition, &state)?
                    {
                        return Err(WorkflowError::NoMatchingOutgoingEdge {
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
                WorkflowNodeKind::End => {
                    state.completed_nodes.insert(node.id.clone());
                    state.active_node_ids.clear();
                    let output = if is_empty_object(&node.input) {
                        state.state.clone()
                    } else {
                        node.input.clone()
                    };
                    return Ok(WorkflowAdvance::Completed { state, output });
                }
                WorkflowNodeKind::Action
                | WorkflowNodeKind::Tool
                | WorkflowNodeKind::SkillAction
                | WorkflowNodeKind::Agent
                | WorkflowNodeKind::Worker
                | WorkflowNodeKind::Review
                | WorkflowNodeKind::WaitSignal
                | WorkflowNodeKind::MemoryRead
                | WorkflowNodeKind::MemoryWrite => {
                    let request = WorkflowNodeRequest::from_node(node)?;
                    state.blocked_nodes.insert(node.id.clone(), request.clone());
                    return Ok(WorkflowAdvance::Blocked { state, request });
                }
                WorkflowNodeKind::Parallel => {
                    state.completed_nodes.insert(node.id.clone());
                    let edges = self.select_parallel_edges(node, &state)?;
                    if edges.len() > state.max_parallel_branches as usize {
                        return Err(WorkflowError::ParallelFanOutExceeded {
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
                        let request = WorkflowNodeRequest::from_node(branch)?;
                        record_transition(&mut state, edge)?;
                        state
                            .blocked_nodes
                            .insert(branch.id.clone(), request.clone());
                        requests.push(request);
                    }
                    state.current_node_id = Some(node.id.clone());
                    state.active_node_ids = branch_node_ids.clone();
                    state.branch_groups.insert(join_id, branch_node_ids);
                    return Ok(WorkflowAdvance::Ready { state, requests });
                }
                WorkflowNodeKind::Join => {
                    if let Some(failed_node_ids) = self.failed_join_branch_ids(node, &state) {
                        return Err(WorkflowError::ParallelBranchFailed {
                            join_node_id: node.id.clone(),
                            failed_node_ids,
                        });
                    }
                    if !self.join_requirements_satisfied(node, &state) {
                        return Ok(WorkflowAdvance::Ready {
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

    fn next_node_id(&self, state: &mut WorkflowExecutionState) -> Result<String> {
        if let Some(node_id) = state.current_node_id.clone() {
            if state.active_node_ids.is_empty() {
                state.active_node_ids.insert(node_id.clone());
            } else if !state.active_node_ids.contains(&node_id) {
                return Err(WorkflowError::CurrentNodeNotActive { node_id });
            }
            if state.active_node_ids.len() > 1 {
                return Err(WorkflowError::MultipleActiveNodesUnsupported {
                    count: state.active_node_ids.len(),
                });
            }
            self.node(&node_id)?;
            return Ok(node_id);
        }

        match state.active_node_ids.len() {
            0 => self.start_node_id(),
            1 => Err(WorkflowError::MissingCurrentNodeForActiveState),
            count => Err(WorkflowError::MultipleActiveNodesUnsupported { count }),
        }
    }

    fn start_node_id(&self) -> Result<String> {
        let mut starts = self
            .definition
            .nodes
            .iter()
            .filter(|node| node.kind == WorkflowNodeKind::Start);
        let start = starts.next().ok_or(WorkflowError::MissingStartNode)?;
        if starts.next().is_some() {
            return Err(WorkflowError::MultipleStartNodes);
        }
        Ok(start.id.clone())
    }

    fn node(&self, node_id: &str) -> Result<&WorkflowNode> {
        self.definition
            .nodes
            .iter()
            .find(|node| node.id == node_id)
            .ok_or_else(|| WorkflowError::NodeNotFound {
                node_id: node_id.to_string(),
            })
    }

    fn select_edge(
        &self,
        node: &WorkflowNode,
        state: &WorkflowExecutionState,
    ) -> Result<&WorkflowEdge> {
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
                    return Err(WorkflowError::NoMatchingOutgoingEdge {
                        node_id: node.id.clone(),
                    });
                }
                [selected] => *selected,
                defaults => {
                    return Err(WorkflowError::AmbiguousOutgoingEdges {
                        node_id: node.id.clone(),
                        matched_count: defaults.len(),
                    });
                }
            }
        } else {
            match conditional_matches.as_slice() {
                [selected] => *selected,
                matches => {
                    return Err(WorkflowError::AmbiguousOutgoingEdges {
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
        node: &WorkflowNode,
        state: &WorkflowExecutionState,
    ) -> Result<Vec<WorkflowEdge>> {
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
            return Err(WorkflowError::NoMatchingOutgoingEdge {
                node_id: node.id.clone(),
            });
        }
        Ok(edges)
    }

    fn join_requirements_satisfied(
        &self,
        node: &WorkflowNode,
        state: &WorkflowExecutionState,
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
        node: &WorkflowNode,
        state: &WorkflowExecutionState,
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
        node: &WorkflowNode,
        state: &'state WorkflowExecutionState,
    ) -> Option<&'state BTreeSet<String>> {
        state.branch_groups.get(&node.id)
    }

    fn join_id_for_parallel_branches(
        &self,
        node: &WorkflowNode,
        branch_node_ids: &BTreeSet<String>,
    ) -> Result<String> {
        let candidates = self
            .definition
            .nodes
            .iter()
            .filter(|candidate| candidate.kind == WorkflowNodeKind::Join)
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
            [] => Err(WorkflowError::NoMatchingOutgoingEdge {
                node_id: node.id.clone(),
            }),
            candidates => Err(WorkflowError::AmbiguousOutgoingEdges {
                node_id: node.id.clone(),
                matched_count: candidates.len(),
            }),
        }
    }

    /// Completes one blocked side-effect node, records its output into state, and advances to the next graph node.
    pub fn complete_blocked_node(
        &self,
        mut state: WorkflowExecutionState,
        node_id: &str,
        output: Value,
    ) -> Result<WorkflowExecutionState> {
        state
            .blocked_nodes
            .remove(node_id)
            .ok_or_else(|| WorkflowError::BlockedNodeNotFound {
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

/// Result of one workflow interpreter advance.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum WorkflowAdvance {
    /// The workflow reached an end node.
    Completed {
        /// Updated execution state.
        state: WorkflowExecutionState,
        /// Terminal workflow output.
        output: Value,
    },
    /// The workflow blocked on one side-effect node.
    Blocked {
        /// Updated execution state.
        state: WorkflowExecutionState,
        /// Request that must be satisfied by the orchestrator.
        request: WorkflowNodeRequest,
    },
    /// The workflow produced ready side-effect requests.
    Ready {
        /// Updated execution state.
        state: WorkflowExecutionState,
        /// Requests that can be run by the orchestrator.
        requests: Vec<WorkflowNodeRequest>,
    },
}

/// Side-effect request emitted by the pure interpreter.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum WorkflowNodeRequest {
    /// Connector action or artifact action invocation.
    Action {
        /// Source workflow node ID.
        node_id: String,
        /// Optional action artifact reference.
        artifact_ref: Option<ArtifactRef>,
        /// Node input payload.
        input: Value,
    },
    /// Direct tool invocation.
    Tool {
        /// Source workflow node ID.
        node_id: String,
        /// Tool references allowed for this node.
        tool_refs: Vec<ArtifactRef>,
        /// Node input payload.
        input: Value,
    },
    /// Skill-declared action invocation.
    SkillAction {
        /// Source workflow node ID.
        node_id: String,
        /// Optional skill action reference.
        artifact_ref: Option<ArtifactRef>,
        /// Node input payload.
        input: Value,
    },
    /// Existing top-level agent loop invocation.
    Agent {
        /// Source workflow node ID.
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
        /// Source workflow node ID.
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
        /// Source workflow node ID.
        node_id: String,
        /// Node input payload.
        input: Value,
    },
    /// External signal wait.
    WaitSignal {
        /// Source workflow node ID.
        node_id: String,
        /// Node input payload.
        input: Value,
    },
    /// Graph memory retrieval request.
    MemoryRead {
        /// Source workflow node ID.
        node_id: String,
        /// Node input payload.
        input: Value,
    },
    /// Graph memory write request.
    MemoryWrite {
        /// Source workflow node ID.
        node_id: String,
        /// Node input payload.
        input: Value,
    },
}

impl WorkflowNodeRequest {
    fn from_node(node: &WorkflowNode) -> Result<Self> {
        Ok(match &node.kind {
            WorkflowNodeKind::Action => Self::Action {
                node_id: node.id.clone(),
                artifact_ref: node.artifact_ref.clone(),
                input: node.input.clone(),
            },
            WorkflowNodeKind::Tool => Self::Tool {
                node_id: node.id.clone(),
                tool_refs: node.tool_refs.clone(),
                input: node.input.clone(),
            },
            WorkflowNodeKind::SkillAction => Self::SkillAction {
                node_id: node.id.clone(),
                artifact_ref: node.artifact_ref.clone(),
                input: node.input.clone(),
            },
            WorkflowNodeKind::Agent => Self::Agent {
                node_id: node.id.clone(),
                skill_refs: node.skill_refs.clone(),
                tool_refs: node.tool_refs.clone(),
                input: node.input.clone(),
                max_turns: node.max_turns,
            },
            WorkflowNodeKind::Worker => Self::Worker {
                node_id: node.id.clone(),
                skill_refs: node.skill_refs.clone(),
                tool_refs: node.tool_refs.clone(),
                input: node.input.clone(),
                max_turns: node.max_turns,
            },
            WorkflowNodeKind::Review => Self::Review {
                node_id: node.id.clone(),
                input: node.input.clone(),
            },
            WorkflowNodeKind::WaitSignal => Self::WaitSignal {
                node_id: node.id.clone(),
                input: node.input.clone(),
            },
            WorkflowNodeKind::MemoryRead => Self::MemoryRead {
                node_id: node.id.clone(),
                input: node.input.clone(),
            },
            WorkflowNodeKind::MemoryWrite => Self::MemoryWrite {
                node_id: node.id.clone(),
                input: node.input.clone(),
            },
            WorkflowNodeKind::Start
            | WorkflowNodeKind::Condition
            | WorkflowNodeKind::End
            | WorkflowNodeKind::Parallel
            | WorkflowNodeKind::Join => {
                return Err(WorkflowError::UnsupportedNodeKind {
                    node_id: node.id.clone(),
                    kind: node_kind_label(&node.kind).to_string(),
                });
            }
        })
    }
}

fn advance_existing_blocked_state(state: &WorkflowExecutionState) -> Option<WorkflowAdvance> {
    let requests = state
        .blocked_nodes
        .values()
        .cloned()
        .collect::<Vec<WorkflowNodeRequest>>();
    match requests.as_slice() {
        [] => None,
        [request] => Some(WorkflowAdvance::Blocked {
            state: state.clone(),
            request: request.clone(),
        }),
        _ => Some(WorkflowAdvance::Ready {
            state: state.clone(),
            requests,
        }),
    }
}

fn reset_parallel_branch_state(
    state: &mut WorkflowExecutionState,
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

fn record_transition(state: &mut WorkflowExecutionState, edge: &WorkflowEdge) -> Result<()> {
    let edge_id = edge_id(edge);
    if state.completed_nodes.contains(&edge.to) {
        let count = state.loop_counts.entry(edge_id.clone()).or_insert(0);
        *count = count.saturating_add(1);
        if *count >= state.max_loop_iterations {
            return Err(WorkflowError::LoopIterationLimitExceeded {
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

fn edge_id(edge: &WorkflowEdge) -> String {
    edge.id
        .clone()
        .unwrap_or_else(|| format!("{}->{}", edge.from, edge.to))
}

fn evaluate_condition(
    condition: &WorkflowCondition,
    state: &WorkflowExecutionState,
) -> Result<bool> {
    match condition {
        WorkflowCondition::Equals { left, right } => Ok(resolve_path(left, state) == Some(right)),
        WorkflowCondition::Exists { path } => Ok(resolve_path(path, state).is_some()),
        WorkflowCondition::Expression { language, source } => {
            Err(WorkflowError::UnsupportedConditionExpression {
                language: language.clone(),
                expression: source.clone(),
            })
        }
    }
}

fn resolve_path<'a>(path: &str, state: &'a WorkflowExecutionState) -> Option<&'a Value> {
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

fn node_kind_label(kind: &WorkflowNodeKind) -> &'static str {
    match kind {
        WorkflowNodeKind::Start => "start",
        WorkflowNodeKind::Action => "action",
        WorkflowNodeKind::Condition => "condition",
        WorkflowNodeKind::Review => "review",
        WorkflowNodeKind::Agent => "agent",
        WorkflowNodeKind::End => "end",
        WorkflowNodeKind::Tool => "tool",
        WorkflowNodeKind::SkillAction => "skill_action",
        WorkflowNodeKind::Worker => "worker",
        WorkflowNodeKind::Parallel => "parallel",
        WorkflowNodeKind::Join => "join",
        WorkflowNodeKind::WaitSignal => "wait_signal",
        WorkflowNodeKind::MemoryRead => "memory_read",
        WorkflowNodeKind::MemoryWrite => "memory_write",
    }
}

#[cfg(test)]
mod tests {
    use moa_artifacts::reference::ArtifactRef;
    use moa_artifacts::workflow::{WorkflowEdge, WorkflowNode};
    use serde_json::json;

    use super::*;

    #[test]
    fn sequential_start_to_end_completes() {
        // Pins: deterministic sequential workflows execute entirely inside the pure interpreter.
        let definition = workflow(
            vec![
                node("start", WorkflowNodeKind::Start),
                node("end", WorkflowNodeKind::End).with_input(json!({ "done": true })),
            ],
            vec![edge("start-end", "start", "end")],
        );
        let run_uid = Uuid::now_v7();
        let result = WorkflowInterpreter::new(&definition)
            .advance(WorkflowExecutionState::new(
                run_uid,
                json!({ "ticket": "T-1" }),
            ))
            .expect("workflow should advance");

        let WorkflowAdvance::Completed { state, output } = result else {
            panic!("expected completed workflow");
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
        let definition = workflow(
            vec![
                node("start", WorkflowNodeKind::Start),
                node("route", WorkflowNodeKind::Condition),
                node("vip", WorkflowNodeKind::End).with_input(json!({ "lane": "vip" })),
                node("standard", WorkflowNodeKind::End).with_input(json!({ "lane": "standard" })),
            ],
            vec![
                edge("start-route", "start", "route"),
                edge("route-vip", "route", "vip").when(WorkflowCondition::Equals {
                    left: "input.priority".to_string(),
                    right: json!("vip"),
                }),
                edge("route-standard", "route", "standard"),
            ],
        );

        let result = WorkflowInterpreter::new(&definition)
            .advance(WorkflowExecutionState::new(
                Uuid::now_v7(),
                json!({ "priority": "vip" }),
            ))
            .expect("workflow should advance");

        let WorkflowAdvance::Completed { state, output } = result else {
            panic!("expected completed workflow");
        };
        assert_eq!(output, json!({ "lane": "vip" }));
        assert_eq!(state.current_node_id.as_deref(), Some("vip"));
        assert_eq!(state.traversed_edge_ids, vec!["start-route", "route-vip"]);
    }

    #[test]
    fn effectful_tool_node_blocks_with_request() {
        // Pins: side-effect nodes return a typed request without hidden execution.
        let tool_ref = ArtifactRef::tool("orders.lookup");
        let definition = workflow(
            vec![
                node("start", WorkflowNodeKind::Start),
                node("lookup", WorkflowNodeKind::Tool)
                    .with_tool_refs(vec![tool_ref.clone()])
                    .with_input(json!({ "order_id": "O-1" })),
                node("end", WorkflowNodeKind::End),
            ],
            vec![
                edge("start-lookup", "start", "lookup"),
                edge("lookup-end", "lookup", "end"),
            ],
        );

        let result = WorkflowInterpreter::new(&definition)
            .advance(WorkflowExecutionState::new(Uuid::now_v7(), json!({})))
            .expect("workflow should advance");

        let WorkflowAdvance::Blocked { state, request } = result else {
            panic!("expected blocked workflow");
        };
        assert_eq!(
            request,
            WorkflowNodeRequest::Tool {
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
        let definition = workflow(
            vec![
                node("start", WorkflowNodeKind::Start),
                node("lookup", WorkflowNodeKind::Tool)
                    .with_tool_refs(vec![ArtifactRef::tool("orders.lookup")]),
                node("end", WorkflowNodeKind::End).with_input(json!({ "done": true })),
            ],
            vec![
                edge("start-lookup", "start", "lookup"),
                edge("lookup-end", "lookup", "end"),
            ],
        );
        let blocked = WorkflowInterpreter::new(&definition)
            .advance(WorkflowExecutionState::new(
                Uuid::now_v7(),
                json!({ "order_id": "O-1" }),
            ))
            .expect("workflow should block on tool");
        let WorkflowAdvance::Blocked { state, .. } = blocked else {
            panic!("expected blocked workflow");
        };

        let resumed = WorkflowInterpreter::new(&definition)
            .complete_blocked_node(state, "lookup", json!({ "status": "ok" }))
            .expect("blocked node should complete");
        assert_eq!(resumed.current_node_id.as_deref(), Some("end"));
        assert_eq!(resumed.state["lookup"], json!({ "status": "ok" }));
        assert_eq!(
            resumed.completed_nodes,
            BTreeSet::from(["lookup".to_string(), "start".to_string()])
        );

        let result = WorkflowInterpreter::new(&definition)
            .advance(resumed)
            .expect("workflow should finish after tool");
        let WorkflowAdvance::Completed { output, state } = result else {
            panic!("expected completed workflow");
        };
        assert_eq!(output, json!({ "done": true }));
        assert_eq!(state.current_node_id.as_deref(), Some("end"));
        assert!(state.completed_nodes.contains("end"));
    }

    #[test]
    fn loop_back_edge_stops_at_iteration_limit() {
        // Pins: explicit graph back-edges are bounded by persisted loop counters.
        let definition = workflow(
            vec![
                node("start", WorkflowNodeKind::Start),
                node("retry", WorkflowNodeKind::Condition),
                node("end", WorkflowNodeKind::End),
            ],
            vec![
                edge("start-retry", "start", "retry"),
                edge("retry-loop", "retry", "retry").when(WorkflowCondition::Exists {
                    path: "input.retry".to_string(),
                }),
                edge("retry-end", "retry", "end"),
            ],
        );
        let mut state = WorkflowExecutionState::new(Uuid::now_v7(), json!({ "retry": true }));
        state.max_loop_iterations = 1;

        let error = WorkflowInterpreter::new(&definition)
            .advance(state)
            .expect_err("loop should stop at limit");
        assert!(matches!(
            error,
            WorkflowError::LoopIterationLimitExceeded {
                edge_id,
                attempted_iterations: 1,
                max_iterations: 1
            } if edge_id == "retry-loop"
        ));
    }

    #[test]
    fn parallel_node_returns_branch_requests() {
        // Pins: parallel fan-out is explicit graph topology plus branch-group state.
        let definition = parallel_workflow();

        let result = WorkflowInterpreter::new(&definition)
            .advance(WorkflowExecutionState::new(Uuid::now_v7(), json!({})))
            .expect("parallel workflow should advance");

        let WorkflowAdvance::Ready { state, requests } = result else {
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
        let definition = parallel_workflow();
        let mut state = WorkflowExecutionState::new(Uuid::now_v7(), json!({}));
        state.current_node_id = Some("join".to_string());
        state.active_node_ids = BTreeSet::from(["join".to_string()]);
        state.completed_nodes = BTreeSet::from(["fanout".to_string(), "left".to_string()]);
        state.branch_groups.insert(
            "join".to_string(),
            BTreeSet::from(["left".to_string(), "right".to_string()]),
        );

        let result = WorkflowInterpreter::new(&definition)
            .advance(state)
            .expect("join should wait instead of failing");

        let WorkflowAdvance::Ready { state, requests } = result else {
            panic!("expected waiting join");
        };
        assert!(requests.is_empty());
        assert_eq!(state.current_node_id.as_deref(), Some("join"));
        assert!(!state.completed_nodes.contains("join"));
    }

    #[test]
    fn join_ignores_branch_group_keyed_to_another_join() {
        // Pins: a join only consumes the branch group keyed by its own node id.
        let definition = parallel_workflow();
        let mut state = WorkflowExecutionState::new(Uuid::now_v7(), json!({}));
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

        let result = WorkflowInterpreter::new(&definition)
            .advance(state)
            .expect("join should wait without its own branch group");

        let WorkflowAdvance::Ready { state, requests } = result else {
            panic!("expected waiting join");
        };
        assert!(requests.is_empty());
        assert_eq!(state.current_node_id.as_deref(), Some("join"));
        assert!(!state.completed_nodes.contains("join"));
    }

    #[test]
    fn reentering_parallel_clears_stale_branch_completion() {
        // Pins: looped parallel sections require fresh branch completions before the join passes.
        let definition = parallel_workflow();
        let mut state = WorkflowExecutionState::new(Uuid::now_v7(), json!({}));
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

        let result = WorkflowInterpreter::new(&definition)
            .advance(state)
            .expect("parallel re-entry should create fresh branch requests");

        let WorkflowAdvance::Ready { state, requests } = result else {
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
        let definition = parallel_workflow();
        let mut state = WorkflowExecutionState::new(Uuid::now_v7(), json!({}));
        state.current_node_id = Some("join".to_string());
        state.active_node_ids = BTreeSet::from(["join".to_string()]);
        state.completed_nodes = BTreeSet::from(["fanout".to_string(), "left".to_string()]);
        state.failed_nodes = BTreeSet::from(["right".to_string()]);
        state.branch_groups.insert(
            "join".to_string(),
            BTreeSet::from(["left".to_string(), "right".to_string()]),
        );

        let error = WorkflowInterpreter::new(&definition)
            .advance(state)
            .expect_err("failed branch should fail the join");
        assert!(matches!(
            error,
            WorkflowError::ParallelBranchFailed {
                join_node_id,
                failed_node_ids
            } if join_node_id == "join" && failed_node_ids == vec!["right".to_string()]
        ));
    }

    fn workflow(nodes: Vec<WorkflowNode>, edges: Vec<WorkflowEdge>) -> WorkflowDefinition {
        WorkflowDefinition {
            input_schema: json!({}),
            state_schema: json!({}),
            nodes,
            edges,
            ui: json!({}),
        }
    }

    fn parallel_workflow() -> WorkflowDefinition {
        workflow(
            vec![
                node("start", WorkflowNodeKind::Start),
                node("fanout", WorkflowNodeKind::Parallel),
                node("left", WorkflowNodeKind::Tool)
                    .with_tool_refs(vec![ArtifactRef::tool("left.tool")]),
                node("right", WorkflowNodeKind::Tool)
                    .with_tool_refs(vec![ArtifactRef::tool("right.tool")]),
                node("join", WorkflowNodeKind::Join),
                node("end", WorkflowNodeKind::End).with_input(json!({ "joined": true })),
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

    fn request_node_id(request: &WorkflowNodeRequest) -> &str {
        match request {
            WorkflowNodeRequest::Action { node_id, .. }
            | WorkflowNodeRequest::Tool { node_id, .. }
            | WorkflowNodeRequest::SkillAction { node_id, .. }
            | WorkflowNodeRequest::Agent { node_id, .. }
            | WorkflowNodeRequest::Worker { node_id, .. }
            | WorkflowNodeRequest::Review { node_id, .. }
            | WorkflowNodeRequest::WaitSignal { node_id, .. }
            | WorkflowNodeRequest::MemoryRead { node_id, .. }
            | WorkflowNodeRequest::MemoryWrite { node_id, .. } => node_id,
        }
    }

    fn node(id: &str, kind: WorkflowNodeKind) -> WorkflowNode {
        WorkflowNode {
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

    impl NodeBuilder for WorkflowNode {
        fn with_input(mut self, input: Value) -> Self {
            self.input = input;
            self
        }

        fn with_tool_refs(mut self, tool_refs: Vec<ArtifactRef>) -> Self {
            self.tool_refs = tool_refs;
            self
        }
    }

    fn edge(id: &str, from: &str, to: &str) -> WorkflowEdge {
        WorkflowEdge {
            id: Some(id.to_string()),
            from: from.to_string(),
            to: to.to_string(),
            when: None,
        }
    }

    trait EdgeBuilder {
        fn when(self, condition: WorkflowCondition) -> Self;
    }

    impl EdgeBuilder for WorkflowEdge {
        fn when(mut self, condition: WorkflowCondition) -> Self {
            self.when = Some(condition);
            self
        }
    }
}
