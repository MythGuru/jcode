//! Verification-gated step completion for the task graph (T2).
//!
//! A [`GoalStep`] that declares a `verification` requirement is a claim about
//! observable evidence ("tests pass", "the command exits 0"). This module is
//! the only path that lets such a step reach `completed`:
//!
//! - the **gate** ([`gate_step_completions`]) runs at the goal-persistence
//!   chokepoints (`create_goal` / `update_goal`). A verification-carrying step
//!   arriving as completed keeps that status only when the session holds
//!   qualifying build/test evidence (a success with no failure after it, from
//!   the same event store the project-knowledge gate uses). Otherwise it is
//!   downgraded to [`STATUS_DONE_PENDING_VERIFICATION`]: honest, visible, and
//!   it keeps dependents blocked in the readiness graph,
//! - **explicit verification** ([`verify_goal_step`] /
//!   [`verify_goal_step_by_user`]) later upgrades a pending step with fresh
//!   evidence or an explicit user confirmation.
//!
//! Deliberate conservatism, mirroring `knowledge::verification`:
//! - evidence is session-scoped and never persisted, so nothing verifies
//!   against yesterday's build,
//! - steps completed before this feature (or completed in a previous update)
//!   are grandfathered: the gate only judges *newly arriving* completions,
//! - steps without a `verification` requirement are untouched, so plain goals
//!   behave exactly as before,
//! - everything here is flag-free pure logic except the thin persistence
//!   wrappers, which check `task_graph_enabled` and refuse when off.

use anyhow::Result;
use chrono::Utc;
use std::path::Path;

use crate::knowledge::verification::{
    VerificationEvent, VerifyError, latest_qualifying_evidence, session_events,
};
use jcode_task_types::{Goal, GoalMilestone, GoalScope};

/// Status for a step whose work is done but whose verification requirement has
/// not been backed by evidence yet. Deliberately not "completed": the
/// readiness graph treats it as neither runnable nor terminal, so dependents
/// stay blocked until the step is actually verified.
pub const STATUS_DONE_PENDING_VERIFICATION: &str = "done_pending_verification";

/// What the gate did to one persistence write.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StepGateReport {
    /// Steps whose completion was accepted, with the evidence that backed it.
    pub verified: Vec<(String, String)>,
    /// Steps downgraded to [`STATUS_DONE_PENDING_VERIFICATION`].
    pub deferred: Vec<String>,
}

impl StepGateReport {
    pub fn is_empty(&self) -> bool {
        self.verified.is_empty() && self.deferred.is_empty()
    }
}

/// Why a step could not be verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepVerifyError {
    /// The task-graph feature flag is off.
    Disabled,
    /// No goal with that id.
    UnknownGoal,
    /// No step with that id in the goal.
    UnknownStep,
    /// The step declares no verification requirement, so there is nothing to
    /// verify; complete it normally instead.
    NoVerificationRequirement,
    /// The step is not awaiting verification (it is pending, in progress, or
    /// already completed).
    NotAwaitingVerification,
    /// The evidence rule failed (no evidence, or invalidated by a failure).
    Evidence(VerifyError),
    /// The goal could not be loaded or saved.
    Storage(String),
}

impl std::fmt::Display for StepVerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disabled => f.write_str("the task graph is disabled"),
            Self::UnknownGoal => f.write_str("no such goal"),
            Self::UnknownStep => f.write_str("no such step in this goal"),
            Self::NoVerificationRequirement => {
                f.write_str("this step has no verification requirement; complete it normally")
            }
            Self::NotAwaitingVerification => f.write_str(
                "this step is not awaiting verification (only done_pending_verification steps can be verified)",
            ),
            Self::Evidence(err) => write!(f, "{err}"),
            Self::Storage(err) => write!(f, "storage error: {err}"),
        }
    }
}

fn is_completion_status(status: &str) -> bool {
    status.eq_ignore_ascii_case("completed") || status.eq_ignore_ascii_case("done")
}

/// Previous state of one step, for the grandfather rule.
fn previous_step<'previous>(
    previous: &'previous [GoalMilestone],
    step_id: &str,
) -> Option<&'previous jcode_task_types::GoalStep> {
    previous
        .iter()
        .flat_map(|milestone| milestone.steps.iter())
        .find(|step| step.id == step_id)
}

/// The completion gate. Scans `incoming` for verification-carrying steps that
/// newly arrive as completed and either accepts them (fresh evidence; evidence
/// string recorded on the step) or downgrades them to
/// [`STATUS_DONE_PENDING_VERIFICATION`].
///
/// Pure and flag-free: callers decide when the gate runs. `previous` is the
/// last persisted state (empty for a brand-new goal).
pub fn gate_step_completions(
    previous: &[GoalMilestone],
    incoming: &mut [GoalMilestone],
    events: &[VerificationEvent],
) -> StepGateReport {
    let mut report = StepGateReport::default();
    for milestone in incoming.iter_mut() {
        for step in milestone.steps.iter_mut() {
            if step.verification.is_none() {
                continue;
            }
            let before = previous_step(previous, &step.id);
            if !is_completion_status(&step.status) {
                // Not claiming completion. Preserve previously recorded
                // evidence over anything the caller may have echoed/forged.
                step.verification_evidence =
                    before.and_then(|step| step.verification_evidence.clone());
                continue;
            }
            if before.is_some_and(|step| is_completion_status(&step.status)) {
                // Grandfathered: this write does not newly complete the step.
                // Restore the previously recorded evidence so an echoing (or
                // forging) caller cannot rewrite provenance.
                step.verification_evidence =
                    before.and_then(|step| step.verification_evidence.clone());
                continue;
            }
            match latest_qualifying_evidence(events) {
                Ok(evidence) => {
                    step.status = "completed".to_string();
                    step.verification_evidence = Some(evidence.clone());
                    report.verified.push((step.id.clone(), evidence));
                }
                Err(_) => {
                    step.status = STATUS_DONE_PENDING_VERIFICATION.to_string();
                    step.verification_evidence = None;
                    report.deferred.push(step.id.clone());
                }
            }
        }
    }
    report
}

/// Upgrade one pending step using the given evidence events. Flag-free core
/// used by tests and the persistence wrapper.
pub fn verify_step_with_events(
    goal: &mut Goal,
    step_id: &str,
    events: &[VerificationEvent],
) -> Result<String, StepVerifyError> {
    let step = goal
        .milestones
        .iter_mut()
        .flat_map(|milestone| milestone.steps.iter_mut())
        .find(|step| step.id == step_id)
        .ok_or(StepVerifyError::UnknownStep)?;
    if step.verification.is_none() {
        return Err(StepVerifyError::NoVerificationRequirement);
    }
    if !step
        .status
        .eq_ignore_ascii_case(STATUS_DONE_PENDING_VERIFICATION)
    {
        return Err(StepVerifyError::NotAwaitingVerification);
    }
    let evidence = latest_qualifying_evidence(events).map_err(StepVerifyError::Evidence)?;
    step.status = "completed".to_string();
    step.verification_evidence = Some(evidence.clone());
    Ok(evidence)
}

/// Upgrade one pending step on explicit user confirmation. The user is the
/// authority, so no build/test evidence is consulted; provenance records the
/// human decision. Flag-free core.
pub fn verify_step_by_user(
    goal: &mut Goal,
    step_id: &str,
    note: Option<&str>,
) -> Result<String, StepVerifyError> {
    let step = goal
        .milestones
        .iter_mut()
        .flat_map(|milestone| milestone.steps.iter_mut())
        .find(|step| step.id == step_id)
        .ok_or(StepVerifyError::UnknownStep)?;
    if step.verification.is_none() {
        return Err(StepVerifyError::NoVerificationRequirement);
    }
    if !step
        .status
        .eq_ignore_ascii_case(STATUS_DONE_PENDING_VERIFICATION)
    {
        return Err(StepVerifyError::NotAwaitingVerification);
    }
    let provenance = match note.map(str::trim).filter(|note| !note.is_empty()) {
        Some(note) => format!("user confirmation: {note}"),
        None => "user confirmation".to_string(),
    };
    step.status = "completed".to_string();
    step.verification_evidence = Some(provenance.clone());
    Ok(provenance)
}

/// Load, verify one step with this session's evidence, save, and sync memory.
/// This is the path the initiative tool wires up in T3.
pub fn verify_goal_step(
    goal_id: &str,
    scope_hint: Option<GoalScope>,
    working_dir: Option<&Path>,
    step_id: &str,
    session_id: &str,
) -> Result<(Goal, String), StepVerifyError> {
    if !super::graph::task_graph_enabled() {
        return Err(StepVerifyError::Disabled);
    }
    let events = session_events(session_id);
    verify_goal_step_with_events(goal_id, scope_hint, working_dir, step_id, &events)
}

/// Event-injected form of [`verify_goal_step`] so tests can drive persistence
/// without global state or config.
pub fn verify_goal_step_with_events(
    goal_id: &str,
    scope_hint: Option<GoalScope>,
    working_dir: Option<&Path>,
    step_id: &str,
    events: &[VerificationEvent],
) -> Result<(Goal, String), StepVerifyError> {
    let mut goal = super::load_goal(goal_id, scope_hint, working_dir)
        .map_err(|err| StepVerifyError::Storage(err.to_string()))?
        .ok_or(StepVerifyError::UnknownGoal)?;
    let previous = goal.milestones.clone();
    let evidence = verify_step_with_events(&mut goal, step_id, events)?;
    // T4: a step verified into completion may teach its declared lesson.
    super::knowledge_link::propose_for_completed_steps(
        working_dir,
        &previous,
        &goal.milestones,
    );
    finish_step_verification(&mut goal, working_dir)?;
    Ok((goal, evidence))
}

/// Like [`verify_goal_step`], but on explicit user confirmation.
pub fn verify_goal_step_by_user(
    goal_id: &str,
    scope_hint: Option<GoalScope>,
    working_dir: Option<&Path>,
    step_id: &str,
    note: Option<&str>,
) -> Result<(Goal, String), StepVerifyError> {
    if !super::graph::task_graph_enabled() {
        return Err(StepVerifyError::Disabled);
    }
    let mut goal = super::load_goal(goal_id, scope_hint, working_dir)
        .map_err(|err| StepVerifyError::Storage(err.to_string()))?
        .ok_or(StepVerifyError::UnknownGoal)?;
    let previous = goal.milestones.clone();
    let provenance = verify_step_by_user(&mut goal, step_id, note)?;
    super::knowledge_link::propose_for_completed_steps(
        working_dir,
        &previous,
        &goal.milestones,
    );
    finish_step_verification(&mut goal, working_dir)?;
    Ok((goal, provenance))
}

/// Mark one step complete on behalf of the todo bridge (T3): the session
/// finished the todo group linked to this step, so the durable graph should
/// record that progress. Completion still passes through the T2 gate: with
/// qualifying evidence the step completes with provenance; without it the
/// step parks as done_pending_verification. Steps without a verification
/// requirement complete directly.
///
/// Returns the resulting step status. Refuses when the flag is off.
pub fn checkpoint_goal_step(
    goal_id: &str,
    working_dir: Option<&Path>,
    step_id: &str,
    session_id: &str,
) -> Result<String, StepVerifyError> {
    if !super::graph::task_graph_enabled() {
        return Err(StepVerifyError::Disabled);
    }
    let mut goal = super::load_goal(goal_id, None, working_dir)
        .map_err(|err| StepVerifyError::Storage(err.to_string()))?
        .ok_or(StepVerifyError::UnknownGoal)?;
    let previous = goal.milestones.clone();
    let step = goal
        .milestones
        .iter_mut()
        .flat_map(|milestone| milestone.steps.iter_mut())
        .find(|step| step.id == step_id)
        .ok_or(StepVerifyError::UnknownStep)?;
    if is_completion_status(&step.status) {
        return Ok(step.status.clone());
    }
    step.status = "completed".to_string();
    let events = session_events(session_id);
    gate_step_completions(&previous, &mut goal.milestones, &events);
    let status = goal
        .milestones
        .iter()
        .flat_map(|milestone| milestone.steps.iter())
        .find(|step| step.id == step_id)
        .map(|step| step.status.clone())
        .unwrap_or_default();
    super::knowledge_link::propose_for_completed_steps(
        working_dir,
        &previous,
        &goal.milestones,
    );
    finish_step_verification(&mut goal, working_dir)?;
    Ok(status)
}

fn finish_step_verification(
    goal: &mut Goal,
    working_dir: Option<&Path>,
) -> Result<(), StepVerifyError> {
    goal.updated_at = Utc::now();
    super::save_goal(goal, working_dir).map_err(|err| StepVerifyError::Storage(err.to_string()))?;
    // Memory sync is best-effort, exactly as in create/update: losing a memory
    // must never fail the verification itself.
    let _ = super::sync_goal_memory(goal, working_dir);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge::verification::VerificationKind;
    use chrono::Duration;
    use jcode_task_types::GoalStep;

    fn step(id: &str, status: &str, verification: Option<&str>) -> GoalStep {
        GoalStep {
            id: id.to_string(),
            content: format!("step {id}"),
            status: status.to_string(),
            verification: verification.map(str::to_string),
            ..Default::default()
        }
    }

    fn milestone(id: &str, steps: Vec<GoalStep>) -> GoalMilestone {
        GoalMilestone {
            id: id.to_string(),
            title: format!("milestone {id}"),
            status: "pending".to_string(),
            steps,
            blocked_by: Vec::new(),
        }
    }

    fn event(success: bool, minutes_ago: i64) -> VerificationEvent {
        VerificationEvent {
            kind: VerificationKind::TestsPassed,
            success,
            evidence: if success {
                "cargo test -p jcode-base (exit 0)".to_string()
            } else {
                "cargo test -p jcode-base (exit 101)".to_string()
            },
            at: Utc::now() - Duration::minutes(minutes_ago),
        }
    }

    #[test]
    fn gate_ignores_steps_without_verification_requirement() {
        let mut incoming = vec![milestone("m1", vec![step("plain", "completed", None)])];
        let report = gate_step_completions(&[], &mut incoming, &[]);
        assert!(report.is_empty());
        assert_eq!(incoming[0].steps[0].status, "completed");
        assert_eq!(incoming[0].steps[0].verification_evidence, None);
    }

    #[test]
    fn gate_accepts_completion_with_fresh_evidence() {
        let mut incoming = vec![milestone(
            "m1",
            vec![step("tested", "completed", Some("cargo test passes"))],
        )];
        let events = vec![event(true, 1)];
        let report = gate_step_completions(&[], &mut incoming, &events);

        assert_eq!(incoming[0].steps[0].status, "completed");
        assert_eq!(
            incoming[0].steps[0].verification_evidence.as_deref(),
            Some("cargo test -p jcode-base (exit 0)")
        );
        assert_eq!(report.verified.len(), 1);
        assert!(report.deferred.is_empty());
    }

    #[test]
    fn gate_defers_completion_without_evidence() {
        let mut incoming = vec![milestone(
            "m1",
            vec![step("tested", "completed", Some("cargo test passes"))],
        )];
        let report = gate_step_completions(&[], &mut incoming, &[]);

        assert_eq!(incoming[0].steps[0].status, STATUS_DONE_PENDING_VERIFICATION);
        assert_eq!(incoming[0].steps[0].verification_evidence, None);
        assert_eq!(report.deferred, vec!["tested".to_string()]);
    }

    #[test]
    fn gate_defers_completion_invalidated_by_later_failure() {
        let mut incoming = vec![milestone(
            "m1",
            vec![step("tested", "done", Some("cargo test passes"))],
        )];
        // Success 10 minutes ago, failure 2 minutes ago: evidence invalidated.
        let events = vec![event(true, 10), event(false, 2)];
        let report = gate_step_completions(&[], &mut incoming, &events);

        assert_eq!(incoming[0].steps[0].status, STATUS_DONE_PENDING_VERIFICATION);
        assert_eq!(report.deferred, vec!["tested".to_string()]);
    }

    #[test]
    fn gate_grandfathers_previous_completions_and_protects_their_evidence() {
        let mut already_done = step("tested", "completed", Some("cargo test passes"));
        already_done.verification_evidence = Some("cargo test (exit 0)".to_string());
        let previous = vec![milestone("m1", vec![already_done])];

        // The caller echoes the step back as completed but with forged evidence.
        let mut forged = step("tested", "completed", Some("cargo test passes"));
        forged.verification_evidence = Some("trust me".to_string());
        let mut incoming = vec![milestone("m1", vec![forged])];

        let report = gate_step_completions(&previous, &mut incoming, &[]);
        assert!(report.is_empty(), "no newly completed steps to judge");
        assert_eq!(incoming[0].steps[0].status, "completed");
        assert_eq!(
            incoming[0].steps[0].verification_evidence.as_deref(),
            Some("cargo test (exit 0)"),
            "recorded provenance wins over the echoed value"
        );
    }

    #[test]
    fn gate_clears_forged_evidence_on_incomplete_steps() {
        let mut forged = step("tested", "in_progress", Some("cargo test passes"));
        forged.verification_evidence = Some("trust me".to_string());
        let mut incoming = vec![milestone("m1", vec![forged])];

        gate_step_completions(&[], &mut incoming, &[]);
        assert_eq!(incoming[0].steps[0].verification_evidence, None);
    }

    #[test]
    fn verify_step_upgrades_pending_step_with_evidence() {
        let mut goal = Goal::new("goal", GoalScope::Project);
        goal.milestones = vec![milestone(
            "m1",
            vec![step(
                "tested",
                STATUS_DONE_PENDING_VERIFICATION,
                Some("cargo test passes"),
            )],
        )];

        let err = verify_step_with_events(&mut goal, "tested", &[]).unwrap_err();
        assert_eq!(err, StepVerifyError::Evidence(VerifyError::NoEvidence));
        assert_eq!(
            goal.milestones[0].steps[0].status,
            STATUS_DONE_PENDING_VERIFICATION
        );

        let evidence =
            verify_step_with_events(&mut goal, "tested", &[event(true, 1)]).expect("verify");
        assert_eq!(evidence, "cargo test -p jcode-base (exit 0)");
        assert_eq!(goal.milestones[0].steps[0].status, "completed");
        assert_eq!(
            goal.milestones[0].steps[0].verification_evidence.as_deref(),
            Some("cargo test -p jcode-base (exit 0)")
        );
    }

    #[test]
    fn verify_step_rejects_wrong_targets() {
        let mut goal = Goal::new("goal", GoalScope::Project);
        goal.milestones = vec![milestone(
            "m1",
            vec![
                step("plain", STATUS_DONE_PENDING_VERIFICATION, None),
                step("open", "pending", Some("cargo test passes")),
            ],
        )];

        assert_eq!(
            verify_step_with_events(&mut goal, "missing", &[event(true, 1)]).unwrap_err(),
            StepVerifyError::UnknownStep
        );
        assert_eq!(
            verify_step_with_events(&mut goal, "plain", &[event(true, 1)]).unwrap_err(),
            StepVerifyError::NoVerificationRequirement
        );
        assert_eq!(
            verify_step_with_events(&mut goal, "open", &[event(true, 1)]).unwrap_err(),
            StepVerifyError::NotAwaitingVerification
        );
    }

    #[test]
    fn verify_step_by_user_records_provenance() {
        let mut goal = Goal::new("goal", GoalScope::Project);
        goal.milestones = vec![milestone(
            "m1",
            vec![step(
                "tested",
                STATUS_DONE_PENDING_VERIFICATION,
                Some("user checks the layout"),
            )],
        )];

        let provenance =
            verify_step_by_user(&mut goal, "tested", Some("looks right")).expect("verify");
        assert_eq!(provenance, "user confirmation: looks right");
        assert_eq!(goal.milestones[0].steps[0].status, "completed");
    }

    #[test]
    fn verify_goal_step_round_trips_through_storage() {
        let _guard = crate::storage::lock_test_env();
        let previous_home = std::env::var_os("JCODE_HOME");
        let dir = tempfile::TempDir::new().expect("tempdir");
        crate::env::set_var("JCODE_HOME", dir.path());

        let working_dir = dir.path().join("project");
        std::fs::create_dir_all(&working_dir).expect("mkdir");

        // Create a goal whose step is already awaiting verification. The
        // create path itself is exercised flag-off (gate skipped), which is
        // exactly the default-config behavior.
        let goal = super::super::create_goal(
            super::super::GoalCreateInput {
                title: "ship the feature".to_string(),
                scope: GoalScope::Project,
                milestones: vec![milestone(
                    "m1",
                    vec![step(
                        "tested",
                        STATUS_DONE_PENDING_VERIFICATION,
                        Some("cargo test passes"),
                    )],
                )],
                ..Default::default()
            },
            Some(&working_dir),
        )
        .expect("create goal");

        let (verified, evidence) = verify_goal_step_with_events(
            &goal.id,
            Some(GoalScope::Project),
            Some(&working_dir),
            "tested",
            &[event(true, 1)],
        )
        .expect("verify");
        assert_eq!(evidence, "cargo test -p jcode-base (exit 0)");
        assert_eq!(verified.milestones[0].steps[0].status, "completed");

        // Reload from disk: the verified status and evidence survived.
        let reloaded =
            super::super::load_goal(&goal.id, Some(GoalScope::Project), Some(&working_dir))
                .expect("load")
                .expect("goal exists");
        assert_eq!(reloaded.milestones[0].steps[0].status, "completed");
        assert_eq!(
            reloaded.milestones[0].steps[0]
                .verification_evidence
                .as_deref(),
            Some("cargo test -p jcode-base (exit 0)")
        );

        match previous_home {
            Some(value) => crate::env::set_var("JCODE_HOME", value),
            None => crate::env::remove_var("JCODE_HOME"),
        }
    }
}
