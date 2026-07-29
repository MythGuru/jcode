//! Links from the task graph to the project knowledge map (T4).
//!
//! A plan step may declare, up front, the lesson its completion will teach
//! (`propose_knowledge`). When the step actually completes, that lesson is
//! **proposed** into the project knowledge map. Proposed, never verified: the
//! knowledge gate (K2) stays the only path to `Verified`, so a task graph can
//! seed the map but can never manufacture trusted knowledge. Verified entries
//! then reach long-term memory through the existing K4 bridge, which this
//! module deliberately does not duplicate.
//!
//! The reverse link is `knowledge_ids` on a step: entries the step's work
//! should respect. They are carried in the `ready` listing so the agent can
//! consult the map before starting the step.

use crate::knowledge::{self, KnowledgeSection};
use jcode_task_types::GoalMilestone;
use std::path::Path;

/// Parse a `propose_knowledge` spec. An explicit `section:` prefix (one of
/// structure/decision/rule/problem/responsibility) selects the section;
/// anything else is a Decision, the safest default for "we did it this way".
pub fn parse_knowledge_spec(spec: &str) -> (KnowledgeSection, String) {
    if let Some((prefix, rest)) = spec.split_once(':') {
        let name = prefix.trim().to_lowercase();
        if matches!(
            name.as_str(),
            "structure" | "decision" | "rule" | "problem" | "responsibility"
        ) {
            return (KnowledgeSection::parse(&name), rest.trim().to_string());
        }
    }
    (KnowledgeSection::Decision, spec.trim().to_string())
}

fn is_completion_status(status: &str) -> bool {
    status.eq_ignore_ascii_case("completed") || status.eq_ignore_ascii_case("done")
}

/// Lessons taught by steps that this write completed: the step carries a
/// non-empty `propose_knowledge`, is complete in `current`, and was not
/// already complete in `previous`. A step parked as
/// `done_pending_verification` teaches nothing yet, by construction.
pub fn newly_completed_knowledge_proposals(
    previous: &[GoalMilestone],
    current: &[GoalMilestone],
) -> Vec<(KnowledgeSection, String)> {
    let mut proposals = Vec::new();
    for milestone in current {
        for step in &milestone.steps {
            let Some(spec) = step.propose_knowledge.as_deref() else {
                continue;
            };
            if spec.trim().is_empty() || !is_completion_status(&step.status) {
                continue;
            }
            let was_complete = previous
                .iter()
                .flat_map(|milestone| milestone.steps.iter())
                .find(|prev| prev.id == step.id)
                .is_some_and(|prev| is_completion_status(&prev.status));
            if was_complete {
                continue;
            }
            proposals.push(parse_knowledge_spec(spec));
        }
    }
    proposals
}

/// Propose lessons for the steps this write completed. Best-effort and
/// deliberately narrow:
/// - requires the knowledge model to be enabled (callers already sit behind
///   the task-graph flag), otherwise the map must not accrue entries,
/// - requires a project directory, since knowledge is per-project,
/// - proposes only; verification stays with the K2 gate and the user.
///
/// Returns the proposed entry ids.
pub fn propose_for_completed_steps(
    project_dir: Option<&Path>,
    previous: &[GoalMilestone],
    current: &[GoalMilestone],
) -> Vec<String> {
    if !knowledge::project_knowledge_enabled() {
        return Vec::new();
    }
    let Some(project_dir) = project_dir else {
        return Vec::new();
    };
    let proposals = newly_completed_knowledge_proposals(previous, current);
    if proposals.is_empty() {
        return Vec::new();
    }
    let mut map = knowledge::load(project_dir);
    let ids: Vec<String> = proposals
        .into_iter()
        .map(|(section, content)| map.propose(section, &content))
        .collect();
    knowledge::save(project_dir, &map);
    ids
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge::KnowledgeStatus;
    use jcode_task_types::GoalStep;

    fn step(id: &str, status: &str, propose: Option<&str>) -> GoalStep {
        GoalStep {
            id: id.to_string(),
            content: format!("step {id}"),
            status: status.to_string(),
            propose_knowledge: propose.map(str::to_string),
            ..Default::default()
        }
    }

    fn milestone(steps: Vec<GoalStep>) -> GoalMilestone {
        GoalMilestone {
            id: "m1".to_string(),
            title: "milestone".to_string(),
            status: "pending".to_string(),
            steps,
            ..Default::default()
        }
    }

    #[test]
    fn spec_prefix_selects_section_and_defaults_to_decision() {
        assert_eq!(
            parse_knowledge_spec("rule: use cargo only"),
            (KnowledgeSection::Rule, "use cargo only".to_string())
        );
        assert_eq!(
            parse_knowledge_spec("problem: flaky test in ci"),
            (KnowledgeSection::Problem, "flaky test in ci".to_string())
        );
        assert_eq!(
            parse_knowledge_spec("we ship behind flags"),
            (
                KnowledgeSection::Decision,
                "we ship behind flags".to_string()
            )
        );
        // An unknown prefix is content, not a section.
        assert_eq!(
            parse_knowledge_spec("note: keep this"),
            (KnowledgeSection::Decision, "note: keep this".to_string())
        );
    }

    #[test]
    fn only_newly_completed_steps_with_specs_propose() {
        let previous = vec![milestone(vec![
            step("old", "completed", Some("rule: already known")),
            step("fresh", "pending", Some("decision: new lesson")),
            step("parked", "pending", Some("rule: unverified lesson")),
        ])];
        let current = vec![milestone(vec![
            step("old", "completed", Some("rule: already known")),
            step("fresh", "completed", Some("decision: new lesson")),
            step(
                "parked",
                "done_pending_verification",
                Some("rule: unverified lesson"),
            ),
            step("plain", "completed", None),
        ])];

        let proposals = newly_completed_knowledge_proposals(&previous, &current);
        assert_eq!(
            proposals,
            vec![(KnowledgeSection::Decision, "new lesson".to_string())]
        );
    }

    /// End to end through storage: completing a verified step proposes (never
    /// verifies) its lesson into the project knowledge map.
    #[test]
    fn verified_step_completion_proposes_knowledge_entry() {
        let _guard = crate::storage::lock_test_env();
        crate::knowledge::verification::clear_all();
        let previous_home = std::env::var_os("JCODE_HOME");
        let previous_flag = std::env::var_os("JCODE_PROJECT_KNOWLEDGE_ENABLED");
        let dir = tempfile::TempDir::new().expect("tempdir");
        crate::env::set_var("JCODE_HOME", dir.path());
        crate::env::set_var("JCODE_PROJECT_KNOWLEDGE_ENABLED", "true");
        crate::config::invalidate_config_cache();

        let project = dir.path().join("repo");
        std::fs::create_dir_all(&project).expect("mkdir");

        let goal = crate::goal::create_goal(
            crate::goal::GoalCreateInput {
                title: "lesson demo".to_string(),
                scope: crate::goal::GoalScope::Project,
                milestones: vec![milestone(vec![GoalStep {
                    id: "tested".to_string(),
                    content: "prove it".to_string(),
                    status: "done_pending_verification".to_string(),
                    verification: Some("cargo test passes".to_string()),
                    propose_knowledge: Some("rule: prove steps with cargo test".to_string()),
                    ..Default::default()
                }])],
                ..Default::default()
            },
            Some(&project),
        )
        .expect("create goal");

        let events = vec![crate::knowledge::verification::VerificationEvent {
            kind: crate::knowledge::verification::VerificationKind::TestsPassed,
            success: true,
            evidence: "cargo test -p demo (exit 0)".to_string(),
            at: chrono::Utc::now(),
        }];
        crate::goal::verification::verify_goal_step_with_events(
            &goal.id,
            Some(crate::goal::GoalScope::Project),
            Some(&project),
            "tested",
            &events,
        )
        .expect("verify");

        let map = crate::knowledge::load(&project);
        let lesson = map
            .entries
            .iter()
            .find(|entry| entry.content == "prove steps with cargo test")
            .expect("lesson proposed");
        assert_eq!(lesson.section, KnowledgeSection::Rule);
        assert_eq!(
            lesson.status,
            KnowledgeStatus::Proposed,
            "task graph must never mint verified knowledge"
        );

        match previous_home {
            Some(value) => crate::env::set_var("JCODE_HOME", value),
            None => crate::env::remove_var("JCODE_HOME"),
        }
        match previous_flag {
            Some(value) => crate::env::set_var("JCODE_PROJECT_KNOWLEDGE_ENABLED", value),
            None => crate::env::remove_var("JCODE_PROJECT_KNOWLEDGE_ENABLED"),
        }
        crate::config::invalidate_config_cache();
    }
}
