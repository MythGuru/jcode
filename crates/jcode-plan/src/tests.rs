//! Tests for the plan scheduling helpers in [`crate`].
//!
//! Split out of `lib.rs` to keep the production module within the
//! repository code-size budget; the contents are unchanged.

use super::*;

fn item(id: &str, status: &str, blocked_by: &[&str]) -> PlanItem {
    PlanItem {
        id: id.to_string(),
        content: id.to_string(),
        status: status.to_string(),
        priority: "high".to_string(),
        subsystem: None,
        file_scope: Vec::new(),
        blocked_by: blocked_by.iter().map(|value| value.to_string()).collect(),
        assigned_to: None,
    }
}

#[test]
fn summarize_plan_graph_reports_ready_and_blocked_items() {
    let items = vec![
        item("a", "completed", &[]),
        item("b", "queued", &["a"]),
        item("c", "queued", &["b"]),
    ];

    let summary = summarize_plan_graph(&items);
    assert_eq!(summary.ready_ids, vec!["b".to_string()]);
    assert_eq!(summary.blocked_ids, vec!["c".to_string()]);
    assert_eq!(summary.completed_ids, vec!["a".to_string()]);
    assert_eq!(summary.cycle_ids, Vec::<String>::new());
}

#[test]
fn summarize_plan_graph_reports_missing_dependencies() {
    let items = vec![
        item("a", "queued", &["missing-task"]),
        item("b", "running", &[]),
    ];

    let summary = summarize_plan_graph(&items);
    assert_eq!(summary.ready_ids, Vec::<String>::new());
    assert_eq!(summary.blocked_ids, vec!["a".to_string()]);
    assert_eq!(summary.active_ids, vec!["b".to_string()]);
    assert_eq!(
        summary.unresolved_dependency_ids,
        vec!["missing-task".to_string()]
    );
}

#[test]
fn newly_ready_item_ids_reports_tasks_unblocked_by_completion() {
    let before = vec![
        item("setup", "running", &[]),
        item("follow-up", "queued", &["setup"]),
        item("later", "queued", &["follow-up"]),
    ];
    let after = vec![
        item("setup", "completed", &[]),
        item("follow-up", "queued", &["setup"]),
        item("later", "queued", &["follow-up"]),
    ];

    assert_eq!(newly_ready_item_ids(&before, &after), vec!["follow-up"]);
}

#[test]
fn summarize_plan_graph_reports_failed_items_separately_from_completed() {
    let items = vec![
        item("ok", "completed", &[]),
        item("boom", "failed", &[]),
        item("halted", "stopped", &[]),
        item("crashed-task", "crashed", &[]),
        item("pending-task", "queued", &[]),
    ];

    let summary = summarize_plan_graph(&items);
    assert_eq!(summary.completed_ids, vec!["ok".to_string()]);
    assert_eq!(
        summary.failed_ids,
        vec![
            "boom".to_string(),
            "crashed-task".to_string(),
            "halted".to_string()
        ]
    );
    // Terminal covers both success and failure; failed is the non-success subset.
    assert_eq!(
        summary.terminal_ids,
        vec![
            "boom".to_string(),
            "crashed-task".to_string(),
            "halted".to_string(),
            "ok".to_string()
        ]
    );
    assert_eq!(summary.ready_ids, vec!["pending-task".to_string()]);
}

#[test]
fn summarize_plan_graph_reports_cycles() {
    let items = vec![
        item("a", "queued", &["c"]),
        item("b", "queued", &["a"]),
        item("c", "queued", &["b"]),
    ];

    let summary = summarize_plan_graph(&items);
    assert_eq!(summary.ready_ids, Vec::<String>::new());
    assert_eq!(
        summary.blocked_ids,
        vec!["a".to_string(), "b".to_string(), "c".to_string()]
    );
    assert_eq!(
        summary.cycle_ids,
        vec!["a".to_string(), "b".to_string(), "c".to_string()]
    );
}

#[test]
fn status_helpers_match_runtime_expectations() {
    assert!(is_completed_status("completed"));
    assert!(is_terminal_status("failed"));
    assert!(is_active_status("running_stale"));
    assert!(is_runnable_status("queued"));
    assert!(!is_terminal_status("queued"));
}

#[test]
fn next_runnable_items_prefers_higher_priority() {
    let items = vec![
        item("done", "completed", &[]),
        item("b", "queued", &["done"]),
        PlanItem {
            priority: "low".to_string(),
            ..item("c", "queued", &["done"])
        },
        PlanItem {
            priority: "high".to_string(),
            ..item("a", "queued", &["done"])
        },
    ];

    assert_eq!(next_runnable_item_ids(&items, None), vec!["a", "b", "c"]);
    assert_eq!(next_runnable_item_ids(&items, Some(2)), vec!["a", "b"]);
}

#[test]
fn assignment_loads_ignore_terminal_tasks() {
    let plan = VersionedPlan {
        items: vec![
            PlanItem {
                assigned_to: Some("agent-a".to_string()),
                ..item("active", "queued", &[])
            },
            PlanItem {
                assigned_to: Some("agent-a".to_string()),
                ..item("done", "completed", &[])
            },
            PlanItem {
                assigned_to: Some("agent-b".to_string()),
                ..item("running", "running", &[])
            },
        ],
        ..VersionedPlan::new()
    };

    assert_eq!(assignment_loads(&plan).get("agent-a"), Some(&1));
    assert_eq!(assignment_loads(&plan).get("agent-b"), Some(&1));
}

#[test]
fn task_control_target_prefers_active_assignment_and_rejects_ambiguous_matches() {
    let items = vec![
        PlanItem {
            assigned_to: Some("agent-a".to_string()),
            ..item("queued", "queued", &[])
        },
        PlanItem {
            assigned_to: Some("agent-a".to_string()),
            ..item("running", "running", &[])
        },
    ];

    assert_eq!(
        task_control_target_item_id(&items, "agent-a", TaskControlAction::Resume),
        Ok("running".to_string())
    );

    let ambiguous = vec![
        PlanItem {
            assigned_to: Some("agent-a".to_string()),
            ..item("one", "queued", &[])
        },
        PlanItem {
            assigned_to: Some("agent-a".to_string()),
            ..item("two", "queued", &[])
        },
    ];
    assert!(
        task_control_target_item_id(&ambiguous, "agent-a", TaskControlAction::Start)
            .unwrap_err()
            .contains("Multiple tasks")
    );
}

#[test]
fn assignment_helpers_report_blocked_and_next_unassigned_tasks() {
    let plan = VersionedPlan {
        items: vec![
            item("done", "completed", &[]),
            PlanItem {
                assigned_to: Some("agent-a".to_string()),
                ..item("assigned", "queued", &["done"])
            },
            item("ready", "queued", &["done"]),
            item("blocked", "queued", &["ready"]),
        ],
        ..VersionedPlan::new()
    };

    assert_eq!(
        next_unassigned_runnable_item_id(&plan),
        Some("ready".to_string())
    );
    assert_eq!(
        explicit_task_blocked_reason(&plan, "blocked"),
        Some("Task 'blocked' is still blocked by: ready".to_string())
    );
}

#[test]
fn assignment_affinities_count_dependency_and_metadata_carryover() {
    let mut plan = VersionedPlan {
        items: vec![
            PlanItem {
                assigned_to: Some("agent-a".to_string()),
                subsystem: Some("ui".to_string()),
                file_scope: vec!["src/tui.rs".to_string()],
                ..item("dep", "completed", &[])
            },
            PlanItem {
                assigned_to: Some("agent-b".to_string()),
                subsystem: Some("ui".to_string()),
                file_scope: vec!["src/tui.rs".to_string()],
                ..item("sibling", "queued", &[])
            },
            PlanItem {
                subsystem: Some("ui".to_string()),
                file_scope: vec!["src/tui.rs".to_string()],
                ..item("target", "queued", &["dep"])
            },
        ],
        ..VersionedPlan::new()
    };
    plan.task_progress.insert(
        "dep".to_string(),
        SwarmTaskProgress {
            assigned_session_id: Some("agent-a".to_string()),
            ..SwarmTaskProgress::default()
        },
    );

    let affinities = assignment_affinities_for_task(&plan, "target").unwrap();
    assert_eq!(affinities.dependency_carryover.get("agent-a"), Some(&2));
    assert_eq!(affinities.metadata_carryover.get("agent-b"), Some(&3));
    assert_eq!(affinities.loads.get("agent-b"), Some(&1));
}

#[test]
fn stranded_runnable_item_requires_dead_assignee_and_respects_reclaim_cap() {
    let dead = |session: &str| session == "dead-session";

    // Ready but unassigned: not stranded (normal path handles it).
    let mut plan = VersionedPlan {
        items: vec![item("a", "queued", &[])],
        ..VersionedPlan::new()
    };
    assert_eq!(next_stranded_runnable_item_id(&plan, &dead), None);

    // Assigned to a live session: not stranded.
    plan.items[0].assigned_to = Some("live-session".to_string());
    assert_eq!(next_stranded_runnable_item_id(&plan, &dead), None);

    // Assigned to a dead session: stranded.
    plan.items[0].assigned_to = Some("dead-session".to_string());
    assert_eq!(
        next_stranded_runnable_item_id(&plan, &dead),
        Some("a".to_string())
    );

    // Blocked items never count even with a dead assignee.
    let blocked_plan = VersionedPlan {
        items: vec![item("gate", "queued", &[]), {
            let mut blocked = item("b", "queued", &["gate"]);
            blocked.assigned_to = Some("dead-session".to_string());
            blocked
        }],
        ..VersionedPlan::new()
    };
    assert_eq!(next_stranded_runnable_item_id(&blocked_plan, &dead), None);

    // At the reclaim cap: excluded, so repeat failures cannot loop forever.
    plan.task_progress.insert(
        "a".to_string(),
        SwarmTaskProgress {
            dead_assignee_reclaims: Some(MAX_DEAD_ASSIGNEE_RECLAIMS),
            ..SwarmTaskProgress::default()
        },
    );
    assert_eq!(next_stranded_runnable_item_id(&plan, &dead), None);
}

#[test]
fn reclaim_stranded_assignment_releases_owner_and_counts_reclaims() {
    let mut plan = VersionedPlan {
        items: vec![{
            let mut stranded = item("a", "queued", &[]);
            stranded.assigned_to = Some("dead-session".to_string());
            stranded
        }],
        ..VersionedPlan::new()
    };
    plan.task_progress.insert(
        "a".to_string(),
        SwarmTaskProgress {
            assigned_session_id: Some("dead-session".to_string()),
            last_heartbeat_unix_ms: Some(42),
            ..SwarmTaskProgress::default()
        },
    );
    let version_before = plan.version;

    assert!(reclaim_stranded_assignment(&mut plan, "a"));

    let item = &plan.items[0];
    assert_eq!(item.assigned_to, None, "assignment binding released");
    assert_eq!(item.status, "queued", "lifecycle status untouched");
    let progress = plan.task_progress.get("a").unwrap();
    assert_eq!(progress.assigned_session_id, None);
    assert_eq!(progress.dead_assignee_reclaims, Some(1));
    assert_eq!(
        progress.last_heartbeat_unix_ms,
        Some(42),
        "prior run history preserved"
    );
    assert_eq!(plan.version, version_before + 1, "version bump for pollers");

    // The reclaimed item is now visible to the normal unassigned picker.
    assert_eq!(
        next_unassigned_runnable_item_id(&plan),
        Some("a".to_string())
    );

    // Reclaiming an unassigned item is a no-op failure, not a counter bump.
    assert!(!reclaim_stranded_assignment(&mut plan, "a"));
    assert_eq!(
        plan.task_progress.get("a").unwrap().dead_assignee_reclaims,
        Some(1)
    );
    assert!(!reclaim_stranded_assignment(&mut plan, "missing"));
}
