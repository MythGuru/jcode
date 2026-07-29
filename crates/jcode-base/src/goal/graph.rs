//! Task-graph readiness over a goal's milestones and steps (T1).
//!
//! The durable hierarchy already exists (`Goal` -> `GoalMilestone` ->
//! `GoalStep`); this module adds the graph view: which steps are ready, which
//! are blocked and by what, and whether the declared dependencies contain
//! cycles. The graph math is deliberately not reimplemented: steps are adapted
//! to `jcode_plan::PlanItem` and summarized by the same
//! `summarize_plan_graph` engine the swarm planner uses.
//!
//! Scope rules:
//! - step `blocked_by` refers to step ids within the same goal (steps may
//!   depend across milestones),
//! - milestone `blocked_by` refers to milestone ids within the same goal; a
//!   blocked milestone's steps inherit those dependencies,
//! - everything here is read-only computation. Persistence stays in
//!   `goal.rs`, and nothing consults this module unless the
//!   `task_graph_enabled` flag is on (callers gate; the pure functions are
//!   flag-free so tests stay simple).

use jcode_plan::{PlanItem, summarize_plan_graph};
use jcode_task_types::{Goal, GoalStep};

/// Whether the persistent task graph is active. Read live (not cached) so
/// toggling the flag takes effect without a restart, like the STM and
/// project-knowledge flags.
pub fn task_graph_enabled() -> bool {
    crate::config::config().agents.task_graph_enabled
}

/// Whether ambient continuation of safe ready steps is allowed. Requires the
/// task graph itself to be enabled too: a graph nobody maintains must never
/// drive autonomous work.
pub fn task_graph_ambient_continuation_enabled() -> bool {
    let agents = &crate::config::config().agents;
    agents.task_graph_enabled && agents.task_graph_ambient_continuation
}

/// Prompt budget for the `# Active Plan` section (used in T5). Clamped so a
/// misconfigured value can neither erase the section nor flood the prompt.
pub fn task_graph_max_prompt_chars() -> usize {
    crate::config::config()
        .agents
        .task_graph_max_prompt_chars
        .clamp(256, 16_000)
}

/// Graph view of one goal's steps.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GoalGraphSummary {
    /// Steps whose dependencies are all complete and whose status is runnable.
    pub ready_step_ids: Vec<String>,
    /// Steps waiting on incomplete dependencies (or explicitly "blocked").
    pub blocked_step_ids: Vec<String>,
    /// Steps already completed.
    pub completed_step_ids: Vec<String>,
    /// Steps participating in a dependency cycle. A non-empty value is a plan
    /// authoring error surfaced to the model/user; cyclic steps never read as
    /// ready.
    pub cycle_step_ids: Vec<String>,
    /// Dependency ids that reference no existing step. Also an authoring
    /// error: the step stays blocked until the reference is fixed or removed.
    pub unknown_dependency_ids: Vec<String>,
}

/// Ready steps that are explicitly marked safe for ambient continuation.
/// The safety flag lives on the step and defaults to false, so this list is
/// empty unless someone deliberately opted steps in.
pub fn ambient_safe_ready_steps(goal: &Goal) -> Vec<&GoalStep> {
    let summary = summarize_goal_graph(goal);
    all_steps(goal)
        .filter(|step| step.safe_for_ambient && summary.ready_step_ids.contains(&step.id))
        .collect()
}

fn all_steps(goal: &Goal) -> impl Iterator<Item = &GoalStep> {
    goal.milestones
        .iter()
        .flat_map(|milestone| milestone.steps.iter())
}

/// Effective dependencies of one step: its own `blocked_by`, plus every step
/// of every milestone its milestone is blocked by. Milestone-level edges are
/// expanded to step-level edges so one engine handles both.
fn effective_dependencies(goal: &Goal, milestone_id: &str, step: &GoalStep) -> Vec<String> {
    let mut deps = step.blocked_by.clone();
    if let Some(milestone) = goal
        .milestones
        .iter()
        .find(|milestone| milestone.id == milestone_id)
    {
        for blocked_by_id in &milestone.blocked_by {
            if let Some(dependency) = goal
                .milestones
                .iter()
                .find(|candidate| candidate.id == *blocked_by_id)
            {
                deps.extend(dependency.steps.iter().map(|dep| dep.id.clone()));
            } else {
                // Unknown milestone reference: keep it as an (unknown) edge so
                // the summary reports it instead of silently unblocking.
                deps.push(format!("milestone:{blocked_by_id}"));
            }
        }
    }
    deps.sort();
    deps.dedup();
    deps
}

/// Map goal step statuses onto the plan engine's vocabulary. Goal steps use
/// "pending"/"in_progress"/"completed" (plus free text); the engine treats
/// unknown statuses as neither runnable nor terminal, which is the safe
/// reading for anything unexpected.
fn plan_status(step_status: &str) -> String {
    match step_status.trim().to_ascii_lowercase().as_str() {
        "in_progress" | "in-progress" | "active" => "running".to_string(),
        other => other.to_string(),
    }
}

/// Summarize the dependency graph of a goal's steps.
pub fn summarize_goal_graph(goal: &Goal) -> GoalGraphSummary {
    let items: Vec<PlanItem> = goal
        .milestones
        .iter()
        .flat_map(|milestone| {
            milestone.steps.iter().map(|step| PlanItem {
                id: step.id.clone(),
                content: step.content.clone(),
                status: plan_status(&step.status),
                priority: "medium".to_string(),
                subsystem: None,
                file_scope: Vec::new(),
                blocked_by: effective_dependencies(goal, &milestone.id, step),
                assigned_to: None,
            })
        })
        .collect();

    let summary = summarize_plan_graph(&items);
    GoalGraphSummary {
        ready_step_ids: summary.ready_ids,
        blocked_step_ids: summary.blocked_ids,
        completed_step_ids: summary.completed_ids,
        cycle_step_ids: summary.cycle_ids,
        unknown_dependency_ids: summary.unresolved_dependency_ids,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jcode_task_types::{GoalMilestone, GoalScope};

    fn step(id: &str, status: &str, blocked_by: &[&str]) -> GoalStep {
        GoalStep {
            id: id.to_string(),
            content: format!("step {id}"),
            status: status.to_string(),
            blocked_by: blocked_by.iter().map(|dep| dep.to_string()).collect(),
            ..Default::default()
        }
    }

    fn goal_with(milestones: Vec<GoalMilestone>) -> Goal {
        let mut goal = Goal::new("test goal", GoalScope::Project);
        goal.milestones = milestones;
        goal
    }

    fn milestone(id: &str, steps: Vec<GoalStep>, blocked_by: &[&str]) -> GoalMilestone {
        GoalMilestone {
            id: id.to_string(),
            title: format!("milestone {id}"),
            status: "pending".to_string(),
            steps,
            blocked_by: blocked_by.iter().map(|dep| dep.to_string()).collect(),
        }
    }

    /// Legacy goal JSON (no task-graph fields) must load with empty deps, no
    /// verification, and ambient opt-out. This is the downgrade/upgrade
    /// compatibility contract for every existing `~/.jcode/goals` file.
    #[test]
    fn legacy_goal_step_json_defaults_new_fields() {
        let legacy = r#"{"id":"s1","content":"old step","status":"pending"}"#;
        let step: GoalStep = serde_json::from_str(legacy).expect("legacy step should parse");
        assert!(step.blocked_by.is_empty());
        assert_eq!(step.verification, None);
        assert!(!step.safe_for_ambient);

        // And the new fields stay off the wire when unset, so files written by
        // this binary remain readable (and diff-clean) for older binaries.
        let serialized = serde_json::to_string(&step).expect("serialize");
        assert!(!serialized.contains("blocked_by"));
        assert!(!serialized.contains("verification"));
        assert!(!serialized.contains("safe_for_ambient"));
    }

    #[test]
    fn step_dependencies_gate_readiness() {
        let goal = goal_with(vec![milestone(
            "m1",
            vec![
                step("build", "completed", &[]),
                step("test", "pending", &["build"]),
                step("ship", "pending", &["test"]),
            ],
            &[],
        )]);

        let summary = summarize_goal_graph(&goal);
        assert_eq!(summary.ready_step_ids, vec!["test".to_string()]);
        assert_eq!(summary.blocked_step_ids, vec!["ship".to_string()]);
        assert_eq!(summary.completed_step_ids, vec!["build".to_string()]);
        assert!(summary.cycle_step_ids.is_empty());
        assert!(summary.unknown_dependency_ids.is_empty());
    }

    #[test]
    fn milestone_dependencies_expand_to_step_edges() {
        let goal = goal_with(vec![
            milestone("m1", vec![step("a", "pending", &[])], &[]),
            milestone("m2", vec![step("b", "pending", &[])], &["m1"]),
        ]);

        // m1's step is unfinished, so m2's step is blocked through the
        // milestone edge alone.
        let summary = summarize_goal_graph(&goal);
        assert_eq!(summary.ready_step_ids, vec!["a".to_string()]);
        assert_eq!(summary.blocked_step_ids, vec!["b".to_string()]);

        let mut done = goal.clone();
        done.milestones[0].steps[0].status = "completed".to_string();
        let summary = summarize_goal_graph(&done);
        assert_eq!(summary.ready_step_ids, vec!["b".to_string()]);
    }

    #[test]
    fn cycles_and_unknown_dependencies_are_reported_not_ready() {
        let goal = goal_with(vec![milestone(
            "m1",
            vec![
                step("x", "pending", &["y"]),
                step("y", "pending", &["x"]),
                step("z", "pending", &["ghost"]),
            ],
            &[],
        )]);

        let summary = summarize_goal_graph(&goal);
        assert!(summary.ready_step_ids.is_empty());
        assert_eq!(
            summary.cycle_step_ids,
            vec!["x".to_string(), "y".to_string()]
        );
        assert_eq!(summary.unknown_dependency_ids, vec!["ghost".to_string()]);
    }

    /// T2 contract: a step parked as done_pending_verification is not
    /// completed, so its dependents must stay blocked until it is verified.
    #[test]
    fn done_pending_verification_keeps_dependents_blocked() {
        let goal = goal_with(vec![milestone(
            "m1",
            vec![
                step("gated", "done_pending_verification", &[]),
                step("next", "pending", &["gated"]),
            ],
            &[],
        )]);
        let summary = summarize_goal_graph(&goal);
        assert!(summary.ready_step_ids.is_empty());
        assert_eq!(summary.blocked_step_ids, vec!["next".to_string()]);
        assert!(summary.completed_step_ids.is_empty());
    }

    #[test]
    fn in_progress_steps_are_neither_ready_nor_blocked() {
        let goal = goal_with(vec![milestone(
            "m1",
            vec![step("busy", "in_progress", &[])],
            &[],
        )]);
        let summary = summarize_goal_graph(&goal);
        assert!(summary.ready_step_ids.is_empty());
        assert!(summary.blocked_step_ids.is_empty());
    }

    #[test]
    fn ambient_safe_ready_steps_require_explicit_opt_in() {
        let mut safe = step("safe", "pending", &[]);
        safe.safe_for_ambient = true;
        let goal = goal_with(vec![milestone(
            "m1",
            vec![
                safe,
                step("unsafe", "pending", &[]),
                {
                    let mut blocked = step("later", "pending", &["unsafe"]);
                    blocked.safe_for_ambient = true;
                    blocked
                },
            ],
            &[],
        )]);

        let picked: Vec<&str> = ambient_safe_ready_steps(&goal)
            .iter()
            .map(|step| step.id.as_str())
            .collect();
        // "unsafe" is ready but not opted in; "later" is opted in but blocked.
        assert_eq!(picked, vec!["safe"]);
    }
}
