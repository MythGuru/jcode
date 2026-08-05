use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet};

/// Hard upper bound for one swarm's durable plan graph. A plan is coordination
/// state, not an append-only activity log; without a bound, repeated seed,
/// expand, inject, and approve calls can retain and broadcast thousands of
/// stale nodes forever. This is four times the live swarm-member cap and well
/// above normal deep graphs while bounding server, disk, and per-client state.
pub const MAX_PLAN_ITEMS: usize = 1024;

pub mod bridge;
pub mod dag;
pub mod lift;
pub mod mermaid;

/// A swarm plan item.
///
/// This is intentionally separate from session todos: plan data is shared at the
/// server/swarm level, while todos remain session-local.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlanItem {
    pub content: String,
    pub status: String,
    pub priority: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subsystem: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub file_scope: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_by: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assigned_to: Option<String>,
}

/// Durable progress associated with a swarm plan task.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SwarmTaskProgress {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assigned_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignment_summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assigned_at_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_heartbeat_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_checkpoint_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint_summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_since_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint_count: Option<u64>,
    /// How many times this node was re-queued because a deep-mode worker's turn
    /// ended without a `complete_node` artifact. Deep mode gives the node one
    /// fresh attempt, then fails it: there must be no path to "done" that skips
    /// artifact validation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_artifact_requeues: Option<u32>,
    /// How many times this node's assignment was reclaimed because its assignee
    /// session was dead (failed/stopped/crashed or gone). Caps automatic
    /// re-dispatch so a node whose workers keep dying cannot spawn workers
    /// forever; past the cap, explicit `retry`/`assign_task` remain the
    /// recovery paths.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dead_assignee_reclaims: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SwarmPlanItemSpec {
    pub id: String,
    pub content: String,
    pub priority: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subsystem: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub file_scope: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_by: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SwarmPlanDefinition {
    pub version: u64,
    pub participants: Vec<String>,
    pub items: Vec<SwarmPlanItemSpec>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SwarmExecutionItemState {
    pub task_id: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assigned_to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<SwarmTaskProgress>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SwarmExecutionState {
    pub items: Vec<SwarmExecutionItemState>,
}

/// Per-node task-DAG metadata, stored as a side map on `VersionedPlan` keyed by
/// plan item id. This mirrors the `task_progress` side-map pattern so existing
/// `PlanItem` construction sites stay unchanged while the DAG engine gains the
/// extra structure it needs (composite/gate mechanics + typed artifacts).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeMeta {
    /// Terminal-action kind: "explore" | "implement" | "verify" | "fix" |
    /// "synthesize" | "critique". Defaults to a plain task when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// The composite node this was decomposed from, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// True once decomposed into children (composite join/synthesis point).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub expanded: bool,
    /// True if this node is an auto-inserted critique/verify gate.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_gate: bool,
    /// The agent that planned this node's decomposition (composite owner). Kept
    /// separately from `PlanItem.assigned_to` so a re-queued composite can be
    /// auto-scheduled (assigned_to cleared) while still preferring its original
    /// planner for the synthesis step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planner: Option<String>,
    /// The typed handoff artifact, present once the node completes. Serialized as
    /// JSON text so the protocol/persistence layers need no extra types.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_json: Option<String>,
    /// Where the node came from: "seed" | "expand" | "gap" | "gate". Powers the
    /// growth accounting (seeded vs machinery-grown) on status surfaces. Absent
    /// on legacy plans, which count as seeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

/// Versioned shared swarm plan state.
#[derive(Clone, Debug)]
pub struct VersionedPlan {
    pub items: Vec<PlanItem>,
    pub version: u64,
    /// Session ids that should receive this plan's updates.
    pub participants: HashSet<String>,
    /// Durable runtime task progress keyed by plan item id.
    pub task_progress: HashMap<String, SwarmTaskProgress>,
    /// Engine mode: "deep" (comprehensive, gated) or "light" (fan-out). Defaults
    /// to light so legacy plans behave as before.
    pub mode: String,
    /// Per-node task-DAG metadata keyed by plan item id.
    pub node_meta: HashMap<String, NodeMeta>,
}

impl VersionedPlan {
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            version: 0,
            participants: HashSet::new(),
            task_progress: HashMap::new(),
            mode: "light".to_string(),
            node_meta: HashMap::new(),
        }
    }

    /// Replace the authoritative item set and discard side-map entries whose
    /// task ids no longer exist. Plan updates are snapshots, not an append-only
    /// log, so retaining progress or DAG metadata for removed items leaks state
    /// across every subsequent persistence and broadcast.
    pub fn replace_items(&mut self, items: Vec<PlanItem>) {
        self.items = items;
        self.prune_side_maps();
    }

    /// Remove task-scoped metadata that no longer belongs to a live plan item.
    pub fn prune_side_maps(&mut self) {
        let item_ids: HashSet<&str> = self.items.iter().map(|item| item.id.as_str()).collect();
        self.task_progress
            .retain(|task_id, _| item_ids.contains(task_id.as_str()));
        self.node_meta
            .retain(|task_id, _| item_ids.contains(task_id.as_str()));
    }

    /// Rewrite all durable references when a live client replaces its session
    /// id. This keeps ownership, task progress, and DAG planner affinity from
    /// accumulating dangling historical identities.
    pub fn rename_session(&mut self, old_session_id: &str, new_session_id: &str) {
        if self.participants.remove(old_session_id) {
            self.participants.insert(new_session_id.to_string());
        }
        for item in &mut self.items {
            if item.assigned_to.as_deref() == Some(old_session_id) {
                item.assigned_to = Some(new_session_id.to_string());
            }
        }
        for progress in self.task_progress.values_mut() {
            if progress.assigned_session_id.as_deref() == Some(old_session_id) {
                progress.assigned_session_id = Some(new_session_id.to_string());
            }
        }
        for meta in self.node_meta.values_mut() {
            if meta.planner.as_deref() == Some(old_session_id) {
                meta.planner = Some(new_session_id.to_string());
            }
        }
    }

    pub fn plan_definition(&self) -> SwarmPlanDefinition {
        let mut participants: Vec<String> = self.participants.iter().cloned().collect();
        participants.sort();
        SwarmPlanDefinition {
            version: self.version,
            participants,
            items: self
                .items
                .iter()
                .map(|item| SwarmPlanItemSpec {
                    id: item.id.clone(),
                    content: item.content.clone(),
                    priority: item.priority.clone(),
                    subsystem: item.subsystem.clone(),
                    file_scope: item.file_scope.clone(),
                    blocked_by: item.blocked_by.clone(),
                })
                .collect(),
        }
    }

    pub fn execution_state(&self) -> SwarmExecutionState {
        SwarmExecutionState {
            items: self
                .items
                .iter()
                .map(|item| SwarmExecutionItemState {
                    task_id: item.id.clone(),
                    status: item.status.clone(),
                    assigned_to: item.assigned_to.clone(),
                    progress: self.task_progress.get(&item.id).cloned(),
                })
                .collect(),
        }
    }
}

impl Default for VersionedPlan {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlanGraphSummary {
    pub ready_ids: Vec<String>,
    pub blocked_ids: Vec<String>,
    pub active_ids: Vec<String>,
    pub completed_ids: Vec<String>,
    /// Terminal without completing: failed, stopped, or crashed items. These are
    /// finished from the scheduler's perspective but must not read as success.
    pub failed_ids: Vec<String>,
    pub terminal_ids: Vec<String>,
    pub unresolved_dependency_ids: Vec<String>,
    pub cycle_ids: Vec<String>,
}

pub fn is_completed_status(status: &str) -> bool {
    matches!(status, "completed" | "done")
}

pub fn is_terminal_status(status: &str) -> bool {
    matches!(
        status,
        "completed" | "done" | "failed" | "stopped" | "crashed"
    )
}

pub fn is_active_status(status: &str) -> bool {
    matches!(status, "running" | "running_stale")
}

/// Terminal without completing: the item is finished from the scheduler's
/// perspective but did not succeed (failed, stopped, or crashed).
pub fn is_failed_status(status: &str) -> bool {
    is_terminal_status(status) && !is_completed_status(status)
}

pub fn is_runnable_status(status: &str) -> bool {
    matches!(status, "queued" | "ready" | "pending" | "todo")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskControlAction {
    Start,
    Wake,
    Resume,
    Retry,
    Reassign,
    Replace,
    Salvage,
}

impl TaskControlAction {
    pub fn parse(action: &str) -> Option<Self> {
        match action {
            "start" => Some(Self::Start),
            "wake" => Some(Self::Wake),
            "resume" => Some(Self::Resume),
            "retry" => Some(Self::Retry),
            "reassign" => Some(Self::Reassign),
            "replace" => Some(Self::Replace),
            "salvage" => Some(Self::Salvage),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Wake => "wake",
            Self::Resume => "resume",
            Self::Retry => "retry",
            Self::Reassign => "reassign",
            Self::Replace => "replace",
            Self::Salvage => "salvage",
        }
    }
}

pub fn combine_assignment_text(content: &str, message: Option<&str>) -> String {
    if let Some(extra) = message {
        format!(
            "{}\n\nAdditional coordinator instructions:\n{}",
            content, extra
        )
    } else {
        content.to_string()
    }
}

fn restart_instruction_prefix(action: TaskControlAction) -> Option<&'static str> {
    match action {
        TaskControlAction::Resume => Some(
            "Resume your assigned task from the current session context and continue the work.",
        ),
        TaskControlAction::Retry => {
            Some("Retry your assigned task. Fix any earlier issues and continue toward completion.")
        }
        _ => None,
    }
}

pub fn build_control_assignment_text(
    action: TaskControlAction,
    content: &str,
    message: Option<&str>,
) -> String {
    let mut parts = Vec::new();
    if let Some(prefix) = restart_instruction_prefix(action) {
        parts.push(prefix.to_string());
    }
    parts.push(content.to_string());
    if let Some(extra) = message {
        parts.push(format!("Additional coordinator instructions:\n{}", extra));
    }
    parts.join("\n\n")
}

pub fn task_control_action_allows_status(action: TaskControlAction, status: &str) -> bool {
    match action {
        TaskControlAction::Start | TaskControlAction::Wake => status == "queued",
        TaskControlAction::Resume => matches!(status, "queued" | "running" | "running_stale"),
        TaskControlAction::Retry => matches!(status, "failed" | "running_stale"),
        // A completed node must never be reopened by handoff actions: deep-mode
        // complete_node persists "completed" (not just "done"), and reassigning
        // it would re-queue finished work and clobber its artifact.
        TaskControlAction::Reassign | TaskControlAction::Replace | TaskControlAction::Salvage => {
            !is_completed_status(status)
        }
    }
}

pub fn task_control_status_error(action: TaskControlAction, status: &str, task_id: &str) -> String {
    match action {
        TaskControlAction::Start => format!(
            "Task '{}' is '{}' and cannot be started. Use start only for queued assignments.",
            task_id, status
        ),
        TaskControlAction::Wake => format!(
            "Task '{}' is '{}' and cannot be woken. Use wake only for queued assignments.",
            task_id, status
        ),
        TaskControlAction::Resume => format!(
            "Task '{}' is '{}' and cannot be resumed safely.",
            task_id, status
        ),
        TaskControlAction::Retry => format!(
            "Task '{}' is '{}' and cannot be retried. Retry is only for failed or stale work.",
            task_id, status
        ),
        TaskControlAction::Reassign => format!(
            "Task '{}' is already complete. Reassign unfinished work instead.",
            task_id
        ),
        TaskControlAction::Replace => format!(
            "Task '{}' is already complete. Replace is only for unfinished work.",
            task_id
        ),
        TaskControlAction::Salvage => format!(
            "Task '{}' is already complete. Salvage is only for unfinished or failed work.",
            task_id
        ),
    }
}

pub fn priority_rank(priority: &str) -> u8 {
    match priority {
        "high" | "urgent" | "p0" => 0,
        "medium" | "normal" | "p1" => 1,
        "low" | "p2" => 2,
        _ => 1,
    }
}

pub fn completed_item_ids(items: &[PlanItem]) -> HashSet<String> {
    items
        .iter()
        .filter(|item| is_completed_status(&item.status))
        .map(|item| item.id.clone())
        .collect()
}

pub fn unresolved_dependencies<'a>(
    item: &'a PlanItem,
    known_ids: &HashSet<&'a str>,
    completed_ids: &HashSet<&str>,
) -> Vec<String> {
    item.blocked_by
        .iter()
        .filter(|dep| known_ids.contains(dep.as_str()) && !completed_ids.contains(dep.as_str()))
        .cloned()
        .collect()
}

pub fn missing_dependencies<'a>(item: &'a PlanItem, known_ids: &HashSet<&'a str>) -> Vec<String> {
    item.blocked_by
        .iter()
        .filter(|dep| !known_ids.contains(dep.as_str()))
        .cloned()
        .collect()
}

pub fn is_unblocked<'a>(
    item: &'a PlanItem,
    known_ids: &HashSet<&'a str>,
    completed_ids: &HashSet<&str>,
) -> bool {
    missing_dependencies(item, known_ids).is_empty()
        && unresolved_dependencies(item, known_ids, completed_ids).is_empty()
}

pub fn cycle_item_ids(items: &[PlanItem]) -> Vec<String> {
    let item_ids: HashSet<&str> = items.iter().map(|item| item.id.as_str()).collect();
    let mut indegree: HashMap<&str, usize> = HashMap::new();
    let mut dependents: HashMap<&str, Vec<&str>> = HashMap::new();

    for item in items {
        indegree.entry(item.id.as_str()).or_insert(0);
    }

    for item in items {
        for dependency in item
            .blocked_by
            .iter()
            .filter(|dependency| item_ids.contains(dependency.as_str()))
        {
            *indegree.entry(item.id.as_str()).or_insert(0) += 1;
            dependents
                .entry(dependency.as_str())
                .or_default()
                .push(item.id.as_str());
        }
    }

    let mut queue: Vec<&str> = indegree
        .iter()
        .filter_map(|(id, degree)| (*degree == 0).then_some(*id))
        .collect();
    let mut visited = HashSet::new();

    while let Some(id) = queue.pop() {
        if !visited.insert(id) {
            continue;
        }
        if let Some(children) = dependents.get(id) {
            for child in children {
                if let Some(degree) = indegree.get_mut(child) {
                    *degree = degree.saturating_sub(1);
                    if *degree == 0 {
                        queue.push(child);
                    }
                }
            }
        }
    }

    let mut cycle_ids: Vec<String> = indegree
        .into_iter()
        .filter_map(|(id, degree)| (degree > 0 && !visited.contains(id)).then_some(id.to_string()))
        .collect();
    cycle_ids.sort();
    cycle_ids
}

pub fn summarize_plan_graph(items: &[PlanItem]) -> PlanGraphSummary {
    let known_ids: HashSet<&str> = items.iter().map(|item| item.id.as_str()).collect();
    let completed_ids = completed_item_ids(items);
    let completed_refs: HashSet<&str> = completed_ids.iter().map(String::as_str).collect();
    let cycle_ids = cycle_item_ids(items);
    let cycle_set: HashSet<&str> = cycle_ids.iter().map(String::as_str).collect();

    let mut ready_ids = Vec::new();
    let mut blocked_ids = Vec::new();
    let mut active_ids = Vec::new();
    let mut completed = BTreeSet::new();
    let mut failed = BTreeSet::new();
    let mut terminal = BTreeSet::new();
    let mut unresolved = BTreeSet::new();

    for item in items {
        let missing = missing_dependencies(item, &known_ids);
        let unresolved_for_item = unresolved_dependencies(item, &known_ids, &completed_refs);
        let is_cyclic = cycle_set.contains(item.id.as_str());

        unresolved.extend(missing.iter().cloned());

        if is_active_status(&item.status) {
            active_ids.push(item.id.clone());
        }
        if is_completed_status(&item.status) {
            completed.insert(item.id.clone());
        }
        if is_failed_status(&item.status) {
            failed.insert(item.id.clone());
        }
        if is_terminal_status(&item.status) {
            terminal.insert(item.id.clone());
        }

        let has_dependency_blocker = !unresolved_for_item.is_empty() || is_cyclic;
        if is_runnable_status(&item.status) && missing.is_empty() && !has_dependency_blocker {
            ready_ids.push(item.id.clone());
        } else if !is_terminal_status(&item.status)
            && !is_active_status(&item.status)
            && (!missing.is_empty() || has_dependency_blocker || item.status == "blocked")
        {
            blocked_ids.push(item.id.clone());
        }
    }

    ready_ids.sort();
    blocked_ids.sort();
    active_ids.sort();

    PlanGraphSummary {
        ready_ids,
        blocked_ids,
        active_ids,
        completed_ids: completed.into_iter().collect(),
        failed_ids: failed.into_iter().collect(),
        terminal_ids: terminal.into_iter().collect(),
        unresolved_dependency_ids: unresolved.into_iter().collect(),
        cycle_ids,
    }
}

pub fn next_runnable_item_ids(items: &[PlanItem], limit: Option<usize>) -> Vec<String> {
    let ready_ids: HashSet<String> = summarize_plan_graph(items).ready_ids.into_iter().collect();
    let mut ready_items: Vec<&PlanItem> = items
        .iter()
        .filter(|item| ready_ids.contains(&item.id))
        .collect();

    ready_items.sort_by(|left, right| {
        priority_rank(&left.priority)
            .cmp(&priority_rank(&right.priority))
            .then_with(|| left.id.cmp(&right.id))
    });

    let iter = ready_items.into_iter().map(|item| item.id.clone());
    match limit {
        Some(limit) => iter.take(limit).collect(),
        None => iter.collect(),
    }
}

pub fn assignment_loads(plan: &VersionedPlan) -> HashMap<String, usize> {
    let mut loads = HashMap::new();
    for item in &plan.items {
        if is_terminal_status(&item.status) {
            continue;
        }
        if let Some(assignee) = item.assigned_to.as_ref() {
            *loads.entry(assignee.clone()).or_default() += 1;
        }
    }
    loads
}

pub fn next_unassigned_runnable_item_id(plan: &VersionedPlan) -> Option<String> {
    next_runnable_item_ids(&plan.items, None)
        .into_iter()
        .find(|candidate_id| {
            plan.items
                .iter()
                .find(|item| item.id == *candidate_id)
                .map(|item| item.assigned_to.is_none())
                .unwrap_or(false)
        })
}

/// Cap on automatic reclaims of a node stranded on a dead assignee. Past this,
/// only explicit `retry`/`assign_task` can move the node, so a node whose
/// workers keep dying cannot spawn replacements forever.
pub const MAX_DEAD_ASSIGNEE_RECLAIMS: u32 = 3;

/// The highest-priority runnable (ready) item that is *stranded*: it carries an
/// assignment, but the assignee is dead per `assignee_is_dead` (terminal
/// lifecycle status or no longer a swarm member). Such items are invisible to
/// [`next_unassigned_runnable_item_id`] (which requires `assigned_to == None`),
/// which is how `task_control retry` against a dead worker used to strand a
/// Ready node: retry keeps the assignee, the re-dispatch dies with the session,
/// and automatic assignment skips the node forever. Items at or over
/// [`MAX_DEAD_ASSIGNEE_RECLAIMS`] are excluded.
pub fn next_stranded_runnable_item_id(
    plan: &VersionedPlan,
    assignee_is_dead: &dyn Fn(&str) -> bool,
) -> Option<String> {
    next_runnable_item_ids(&plan.items, None)
        .into_iter()
        .find(|candidate_id| {
            let Some(item) = plan.items.iter().find(|item| item.id == *candidate_id) else {
                return false;
            };
            let Some(assignee) = item.assigned_to.as_deref() else {
                return false;
            };
            if !assignee_is_dead(assignee) {
                return false;
            }
            plan.task_progress
                .get(candidate_id)
                .and_then(|progress| progress.dead_assignee_reclaims)
                .unwrap_or(0)
                < MAX_DEAD_ASSIGNEE_RECLAIMS
        })
}

/// Clear a stranded assignment so the node becomes eligible for normal
/// automatic dispatch again, bumping the per-node reclaim counter and the plan
/// version. Prior run history (heartbeats, checkpoints, details) is preserved;
/// only the assignment binding is released. Returns `false` when the item is
/// missing or not actually assigned.
pub fn reclaim_stranded_assignment(plan: &mut VersionedPlan, task_id: &str) -> bool {
    let Some(item) = plan.items.iter_mut().find(|item| item.id == task_id) else {
        return false;
    };
    if item.assigned_to.is_none() {
        return false;
    }
    let previous_assignee = item.assigned_to.take();
    let progress = plan.task_progress.entry(task_id.to_string()).or_default();
    progress.assigned_session_id = None;
    progress.dead_assignee_reclaims = Some(progress.dead_assignee_reclaims.unwrap_or(0) + 1);
    progress.checkpoint_summary = Some(format!(
        "assignment reclaimed: previous assignee {} is dead",
        previous_assignee.as_deref().unwrap_or("<unknown>")
    ));
    plan.version += 1;
    true
}

pub fn task_control_target_item_id(
    items: &[PlanItem],
    target_session: &str,
    action: TaskControlAction,
) -> Result<String, String> {
    let mut candidates: Vec<&PlanItem> = items
        .iter()
        .filter(|item| item.assigned_to.as_deref() == Some(target_session))
        .filter(|item| task_control_action_allows_status(action, &item.status))
        .collect();

    candidates.sort_by_key(|item| match item.status.as_str() {
        "running" | "running_stale" => 0,
        "queued" | "ready" | "pending" | "todo" => 1,
        "failed" | "stopped" | "crashed" => 2,
        "completed" | "done" => 3,
        _ => 4,
    });

    match candidates.as_slice() {
        [] => Err(format!(
            "No task assigned to '{}' can be {}. Provide task_id explicitly, or assign a task first.",
            target_session,
            action.as_str()
        )),
        [item] => Ok(item.id.clone()),
        [first, second, ..] if first.status != second.status => Ok(first.id.clone()),
        _ => Err(format!(
            "Multiple tasks assigned to '{}' can be {}: {}. Provide task_id explicitly.",
            target_session,
            action.as_str(),
            candidates
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

pub fn explicit_task_blocked_reason(plan: &VersionedPlan, task_id: &str) -> Option<String> {
    let known_ids: HashSet<&str> = plan.items.iter().map(|item| item.id.as_str()).collect();
    let completed_ids = completed_item_ids(&plan.items);
    let completed_refs: HashSet<&str> = completed_ids.iter().map(String::as_str).collect();
    let cycle_ids: HashSet<String> = cycle_item_ids(&plan.items).into_iter().collect();

    let item = plan.items.iter().find(|item| item.id == task_id)?;
    let missing = missing_dependencies(item, &known_ids);
    if !missing.is_empty() {
        return Some(format!(
            "Task '{}' has missing dependencies: {}",
            item.id,
            missing.join(", ")
        ));
    }

    let unresolved = unresolved_dependencies(item, &known_ids, &completed_refs);
    if !unresolved.is_empty() {
        return Some(format!(
            "Task '{}' is still blocked by: {}",
            item.id,
            unresolved.join(", ")
        ));
    }

    if cycle_ids.contains(&item.id) {
        return Some(format!(
            "Task '{}' is part of a dependency cycle and is not runnable",
            item.id
        ));
    }

    None
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AssignmentAffinities {
    pub loads: HashMap<String, usize>,
    pub dependency_carryover: HashMap<String, usize>,
    pub metadata_carryover: HashMap<String, usize>,
}

pub fn assignment_affinities_for_task(
    plan: &VersionedPlan,
    task_id: &str,
) -> Result<AssignmentAffinities, String> {
    let loads = assignment_loads(plan);

    let Some(task) = plan.items.iter().find(|item| item.id == task_id) else {
        return Err(format!("Task '{}' not found in swarm plan", task_id));
    };

    let mut dependency_carryover = HashMap::<String, usize>::new();
    let mut metadata_carryover = HashMap::<String, usize>::new();
    for dependency_id in &task.blocked_by {
        if let Some(dep_item) = plan.items.iter().find(|item| item.id == *dependency_id)
            && let Some(owner) = dep_item.assigned_to.as_ref()
        {
            *dependency_carryover.entry(owner.clone()).or_default() += 1;
        }
        if let Some(progress) = plan.task_progress.get(dependency_id)
            && let Some(owner) = progress.assigned_session_id.as_ref()
        {
            *dependency_carryover.entry(owner.clone()).or_default() += 1;
        }
    }

    for item in &plan.items {
        let Some(owner) = item.assigned_to.as_ref() else {
            continue;
        };
        if item.id == task.id {
            continue;
        }
        if task
            .subsystem
            .as_ref()
            .zip(item.subsystem.as_ref())
            .is_some_and(|(left, right)| left == right)
        {
            *metadata_carryover.entry(owner.clone()).or_default() += 2;
        }
        if !task.file_scope.is_empty() && !item.file_scope.is_empty() {
            let overlap = task
                .file_scope
                .iter()
                .filter(|path| item.file_scope.contains(*path))
                .count();
            if overlap > 0 {
                *metadata_carryover.entry(owner.clone()).or_default() += overlap;
            }
        }
    }

    Ok(AssignmentAffinities {
        loads,
        dependency_carryover,
        metadata_carryover,
    })
}

pub fn newly_ready_item_ids(before: &[PlanItem], after: &[PlanItem]) -> Vec<String> {
    let before_ready: HashSet<String> =
        summarize_plan_graph(before).ready_ids.into_iter().collect();
    let mut after_ready = summarize_plan_graph(after).ready_ids;
    after_ready.retain(|item_id| !before_ready.contains(item_id));
    after_ready
}

#[cfg(test)]
mod tests;
