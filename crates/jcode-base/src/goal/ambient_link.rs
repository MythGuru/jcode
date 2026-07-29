//! Ambient continuation of safe ready steps (T6).
//!
//! When the user is away, ambient mode may continue a durable plan, but only
//! within an aggressively narrow envelope. A step qualifies only when ALL of:
//! - the task graph AND the separate ambient-continuation flag are on,
//! - the goal is active (resumable) and knows its project directory,
//! - the step is ready (dependencies complete, T1 graph),
//! - the step was explicitly marked `safe_for_ambient` by a human-approved
//!   plan (default false, per step, never inferred).
//!
//! And even then, this module only *surfaces* the step to the ambient cycle
//! prompt. Execution stays inside the existing ambient safety envelope:
//! code changes still require `request_permission`, and step completion still
//! passes the T2 verification gate, so unverified work parks as
//! `done_pending_verification` for a human session to confirm. Ambient can
//! propose progress; it cannot mint verified completion.

use jcode_task_types::{Goal, GoalStatus};
use std::path::{Path, PathBuf};

/// One ambient-eligible step, with enough context to work on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AmbientContinuation {
    pub goal_id: String,
    pub goal_title: String,
    pub working_dir: String,
    pub step_id: String,
    pub step_content: String,
    /// The step's declared verification requirement, if any. Ambient should
    /// run it, but the completion still goes through the gate.
    pub verification: Option<String>,
}

/// Upper bound on surfaced continuations per cycle. Ambient cycles are budget
/// bound; a huge plan must not flood the prompt.
pub const MAX_AMBIENT_CONTINUATIONS: usize = 5;

/// Scan every project's goals for ambient-safe ready steps. Returns an empty
/// list unless both feature flags are on. Unreadable files are skipped, never
/// fatal (mirrors `gather_knowledge_health`).
pub fn gather_ambient_continuations() -> Vec<AmbientContinuation> {
    if !super::graph::task_graph_ambient_continuation_enabled() {
        return Vec::new();
    }
    let Ok(projects_dir) = crate::storage::jcode_dir().map(|d| d.join("goals").join("projects"))
    else {
        return Vec::new();
    };
    let mut continuations = Vec::new();
    let Ok(project_dirs) = std::fs::read_dir(&projects_dir) else {
        return Vec::new();
    };
    for project_dir in project_dirs.flatten() {
        let path = project_dir.path();
        if !path.is_dir() {
            continue;
        }
        collect_from_goal_dir(&path, &mut continuations);
        if continuations.len() >= MAX_AMBIENT_CONTINUATIONS {
            break;
        }
    }
    continuations.truncate(MAX_AMBIENT_CONTINUATIONS);
    continuations
}

fn collect_from_goal_dir(dir: &Path, continuations: &mut Vec<AmbientContinuation>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
        .collect();
    files.sort();
    for path in files {
        let Ok(goal) = crate::storage::read_json::<Goal>(&path) else {
            continue;
        };
        collect_from_goal(&goal, continuations);
        if continuations.len() >= MAX_AMBIENT_CONTINUATIONS {
            return;
        }
    }
}

fn collect_from_goal(goal: &Goal, continuations: &mut Vec<AmbientContinuation>) {
    // Only active plans: paused/blocked/completed goals must stay untouched.
    if goal.status != GoalStatus::Active {
        return;
    }
    // No project directory means ambient cannot know where to run the work.
    let Some(working_dir) = goal
        .working_dir
        .as_deref()
        .map(str::trim)
        .filter(|dir| !dir.is_empty())
    else {
        return;
    };
    // The directory must still exist; a moved project silently disqualifies.
    if !Path::new(working_dir).is_dir() {
        return;
    }
    for step in super::graph::ambient_safe_ready_steps(goal) {
        continuations.push(AmbientContinuation {
            goal_id: goal.id.clone(),
            goal_title: goal.title.clone(),
            working_dir: working_dir.to_string(),
            step_id: step.id.clone(),
            step_content: step.content.clone(),
            verification: step.verification.clone(),
        });
        if continuations.len() >= MAX_AMBIENT_CONTINUATIONS {
            return;
        }
    }
}

/// Render the ambient-prompt section for these continuations. Returns `None`
/// when there is nothing to surface, keeping pre-T6 prompts byte-identical.
pub fn render_ambient_section(continuations: &[AmbientContinuation]) -> Option<String> {
    if continuations.is_empty() {
        return None;
    }
    let mut out = String::from("## Task Graph Continuation\n");
    out.push_str(
        "These plan steps are ready and were explicitly marked safe for \
         ambient work by the user's plan. You may continue them, under the \
         normal safety rules: all code changes still require \
         request_permission, and work must stay inside the step's own project \
         directory.\n",
    );
    for continuation in continuations {
        out.push_str(&format!(
            "- `{}` of initiative `{}` ({}): {}\n",
            continuation.step_id,
            continuation.goal_id,
            continuation.goal_title,
            continuation.step_content,
        ));
        out.push_str(&format!("  Working dir: {}\n", continuation.working_dir));
        if let Some(verification) = &continuation.verification {
            out.push_str(&format!(
                "  Verification: {} (run it; completion is still gated)\n",
                verification
            ));
        }
    }
    out.push_str(
        "After working a step, record progress with the initiative tool \
         (action=update). Do NOT mark steps completed without their \
         verification passing in this session; the completion gate will park \
         them as done_pending_verification for the user to confirm, which is \
         the correct outcome for ambient work.\n",
    );
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use jcode_task_types::{GoalMilestone, GoalScope, GoalStep};

    struct AmbientEnv {
        home: tempfile::TempDir,
        _guard: std::sync::MutexGuard<'static, ()>,
        prev: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    fn setup(task_graph: bool, ambient: bool) -> AmbientEnv {
        let guard = crate::storage::lock_test_env();
        let home = tempfile::TempDir::new().expect("tempdir");
        let keys = [
            ("JCODE_HOME", Some(home.path().as_os_str().to_os_string())),
            (
                "JCODE_TASK_GRAPH_ENABLED",
                task_graph.then(|| std::ffi::OsString::from("true")),
            ),
            (
                "JCODE_TASK_GRAPH_AMBIENT_CONTINUATION",
                ambient.then(|| std::ffi::OsString::from("true")),
            ),
        ];
        let mut prev = Vec::new();
        for (key, value) in keys {
            prev.push((key, std::env::var_os(key)));
            match value {
                Some(value) => crate::env::set_var(key, value),
                None => crate::env::remove_var(key),
            }
        }
        crate::config::invalidate_config_cache();
        AmbientEnv {
            home,
            _guard: guard,
            prev,
        }
    }

    impl Drop for AmbientEnv {
        fn drop(&mut self) {
            for (key, value) in self.prev.drain(..) {
                match value {
                    Some(value) => crate::env::set_var(key, value),
                    None => crate::env::remove_var(key),
                }
            }
            crate::config::invalidate_config_cache();
        }
    }

    fn seeded_goal(env: &AmbientEnv, title: &str, safe: bool, status: GoalStatus) -> Goal {
        let project = env.home.path().join("repo");
        std::fs::create_dir_all(&project).expect("mkdir");
        let mut goal = super::super::create_goal(
            super::super::GoalCreateInput {
                title: title.to_string(),
                scope: GoalScope::Project,
                milestones: vec![GoalMilestone {
                    id: "m1".to_string(),
                    title: "milestone".to_string(),
                    status: "pending".to_string(),
                    steps: vec![
                        GoalStep {
                            id: "safe-step".to_string(),
                            content: "safe work".to_string(),
                            status: "pending".to_string(),
                            safe_for_ambient: safe,
                            verification: Some("cargo test passes".to_string()),
                            ..Default::default()
                        },
                        GoalStep {
                            id: "gated".to_string(),
                            content: "later work".to_string(),
                            status: "pending".to_string(),
                            blocked_by: vec!["safe-step".to_string()],
                            safe_for_ambient: true,
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                }],
                ..Default::default()
            },
            Some(&project),
        )
        .expect("create goal");
        if status != GoalStatus::Active {
            goal = super::super::update_goal(
                &goal.id,
                Some(GoalScope::Project),
                Some(&project),
                super::super::GoalUpdateInput {
                    status: Some(status),
                    ..Default::default()
                },
            )
            .expect("update")
            .expect("goal exists");
        }
        goal
    }

    #[test]
    fn gathering_requires_both_flags() {
        // Task graph on, ambient continuation off: nothing surfaces.
        {
            let env = setup(true, false);
            seeded_goal(&env, "flag demo", true, GoalStatus::Active);
            assert!(gather_ambient_continuations().is_empty());
        }
        // Both on: the safe ready step surfaces; the blocked one does not.
        let env = setup(true, true);
        seeded_goal(&env, "flag demo", true, GoalStatus::Active);
        let found = gather_ambient_continuations();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].step_id, "safe-step");
        assert_eq!(found[0].verification.as_deref(), Some("cargo test passes"));
        assert!(found[0].working_dir.ends_with("repo"));
    }

    #[test]
    fn unsafe_steps_and_inactive_goals_never_surface() {
        let env = setup(true, true);
        // Steps not opted in are invisible even when ready.
        seeded_goal(&env, "unsafe demo", false, GoalStatus::Active);
        assert!(gather_ambient_continuations().is_empty());

        // A paused goal's opted-in steps are invisible too.
        seeded_goal(&env, "paused demo", true, GoalStatus::Paused);
        assert!(gather_ambient_continuations().is_empty());
    }

    #[test]
    fn rendered_section_carries_safety_instructions() {
        let continuation = AmbientContinuation {
            goal_id: "demo".to_string(),
            goal_title: "Demo".to_string(),
            working_dir: "C:/repo".to_string(),
            step_id: "safe-step".to_string(),
            step_content: "safe work".to_string(),
            verification: Some("cargo test passes".to_string()),
        };
        let section = render_ambient_section(std::slice::from_ref(&continuation)).expect("section");
        assert!(section.starts_with("## Task Graph Continuation"));
        assert!(section.contains("request_permission"));
        assert!(section.contains("done_pending_verification"));
        assert!(section.contains("`safe-step` of initiative `demo`"));
        assert!(section.contains("Working dir: C:/repo"));
        assert!(section.contains("Verification: cargo test passes"));
        assert_eq!(render_ambient_section(&[]), None);
    }
}
