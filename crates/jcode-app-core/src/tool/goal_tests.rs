use super::*;
use tokio::time::{Duration, timeout};

#[tokio::test]
async fn initiative_tool_create_and_resume_round_trip() {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let project = temp.path().join("repo");
    std::fs::create_dir_all(&project).expect("project dir");
    let prev_home = std::env::var_os("JCODE_HOME");
    crate::env::set_var("JCODE_HOME", temp.path());

    let tool = InitiativeTool::new();
    let ctx = ToolContext {
        session_id: "ses_goal_tool".to_string(),
        message_id: "msg1".to_string(),
        tool_call_id: "tool1".to_string(),
        working_dir: Some(project.clone()),
        stdin_request_tx: None,
        graceful_shutdown_signal: None,
        execution_mode: crate::tool::ToolExecutionMode::AgentTurn,
    };

    let mut bus_rx = Bus::global().subscribe();

    let create = tool
        .execute(
            json!({
                "action": "create",
                "title": "Ship mobile MVP",
                "scope": "project",
                "next_steps": ["finish reconnect flow"]
            }),
            ctx.clone(),
        )
        .await
        .expect("create goal");
    assert!(create.output.contains("Created initiative"));
    assert!(!create.output.contains("side panel"));

    // Creating an initiative must not spawn the side panel.
    let update = timeout(Duration::from_millis(200), bus_rx.recv()).await;
    if let Ok(Ok(event)) = update {
        assert!(
            !matches!(event, BusEvent::SidePanelUpdated(_)),
            "create must not publish a side panel update, got {:?}",
            event
        );
    }

    let persisted =
        crate::side_panel::snapshot_for_session("ses_goal_tool").expect("side panel snapshot");
    assert!(
        !persisted
            .pages
            .iter()
            .any(|page| page.id == "goal.ship-mobile-mvp"),
        "create must not write a goal page to the side panel"
    );

    let resume = tool
        .execute(json!({"action": "resume"}), ctx)
        .await
        .expect("resume goal");
    assert!(resume.output.contains("Resumed initiative"));
    assert!(resume.output.contains("finish reconnect flow"));

    if let Some(prev_home) = prev_home {
        crate::env::set_var("JCODE_HOME", prev_home);
    } else {
        crate::env::remove_var("JCODE_HOME");
    }
}

#[tokio::test]
async fn initiative_tool_list_does_not_open_side_panel_by_default() {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let project = temp.path().join("repo");
    std::fs::create_dir_all(&project).expect("project dir");
    let prev_home = std::env::var_os("JCODE_HOME");
    crate::env::set_var("JCODE_HOME", temp.path());

    crate::goal::create_goal(
        crate::goal::GoalCreateInput {
            title: "Ship mobile MVP".to_string(),
            scope: crate::goal::GoalScope::Project,
            ..crate::goal::GoalCreateInput::default()
        },
        Some(&project),
    )
    .expect("create goal");

    let tool = InitiativeTool::new();
    let ctx = ToolContext {
        session_id: "ses_goal_list".to_string(),
        message_id: "msg1".to_string(),
        tool_call_id: "tool1".to_string(),
        working_dir: Some(project.clone()),
        stdin_request_tx: None,
        graceful_shutdown_signal: None,
        execution_mode: crate::tool::ToolExecutionMode::AgentTurn,
    };

    let list = tool
        .execute(json!({"action": "list"}), ctx)
        .await
        .expect("list goals");

    assert!(list.output.contains("# Goals"));
    let snapshot =
        crate::side_panel::snapshot_for_session("ses_goal_list").expect("side panel snapshot");
    assert!(
        !snapshot.pages.iter().any(|page| page.id == "goals"),
        "list must not open the goals overview in the side panel"
    );
    assert_eq!(snapshot.focused_page_id, None);

    if let Some(prev_home) = prev_home {
        crate::env::set_var("JCODE_HOME", prev_home);
    } else {
        crate::env::remove_var("JCODE_HOME");
    }
}

#[tokio::test]
async fn initiative_tool_update_refreshes_open_overview_without_stealing_focus() {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let project = temp.path().join("repo");
    std::fs::create_dir_all(&project).expect("project dir");
    let prev_home = std::env::var_os("JCODE_HOME");
    crate::env::set_var("JCODE_HOME", temp.path());

    let goal = crate::goal::create_goal(
        crate::goal::GoalCreateInput {
            title: "Ship mobile MVP".to_string(),
            scope: crate::goal::GoalScope::Project,
            next_steps: vec!["finish reconnect flow".to_string()],
            ..crate::goal::GoalCreateInput::default()
        },
        Some(&project),
    )
    .expect("create goal");

    let tool = InitiativeTool::new();
    let ctx = ToolContext {
        session_id: "ses_goal_update".to_string(),
        message_id: "msg1".to_string(),
        tool_call_id: "tool1".to_string(),
        working_dir: Some(project.clone()),
        stdin_request_tx: None,
        graceful_shutdown_signal: None,
        execution_mode: crate::tool::ToolExecutionMode::AgentTurn,
    };

    // The user opens the overview explicitly (e.g. via /goals); the tool
    // itself never spawns the side panel.
    crate::goal::open_goals_overview_for_session("ses_goal_update", Some(&project), true)
        .expect("open goals overview");

    tool.execute(
        json!({
            "action": "update",
            "id": goal.id,
            "next_steps": ["ship reconnect flow"]
        }),
        ctx,
    )
    .await
    .expect("update goal");

    let snapshot =
        crate::side_panel::snapshot_for_session("ses_goal_update").expect("side panel snapshot");
    assert_eq!(snapshot.focused_page_id.as_deref(), Some("goals"));
    let goals_page = snapshot
        .pages
        .iter()
        .find(|page| page.id == "goals")
        .expect("goals page");
    assert!(goals_page.content.contains("ship reconnect flow"));

    if let Some(prev_home) = prev_home {
        crate::env::set_var("JCODE_HOME", prev_home);
    } else {
        crate::env::remove_var("JCODE_HOME");
    }
}

#[test]
fn test_initiative_schema_milestones_define_items() {
    let schema = InitiativeTool::new().parameters_schema();
    let milestone_items = &schema["properties"]["milestones"]["items"];

    assert_eq!(milestone_items["type"], "object");
    assert_eq!(milestone_items["additionalProperties"], json!(true));
    assert_eq!(milestone_items["properties"]["steps"]["type"], "array");
    assert_eq!(
        milestone_items["properties"]["steps"]["items"]["additionalProperties"],
        json!(true)
    );
}

#[test]
fn test_initiative_schema_omits_display_override() {
    let schema = InitiativeTool::new().parameters_schema();
    assert!(schema["properties"]["display"].is_null());
}

#[test]
fn test_initiative_schema_omits_public_enums_for_scope_and_status() {
    let schema = InitiativeTool::new().parameters_schema();
    assert!(schema["properties"]["scope"]["enum"].is_null());
    assert!(schema["properties"]["status"]["enum"].is_null());
}


struct TaskGraphEnv {
    _home: tempfile::TempDir,
    _env: std::sync::MutexGuard<'static, ()>,
    prev_home: Option<std::ffi::OsString>,
    prev_flag: Option<std::ffi::OsString>,
}

/// Isolated home + task-graph flag ON via env override, restored on drop.
fn setup_task_graph_enabled() -> TaskGraphEnv {
    let env = crate::storage::lock_test_env();
    crate::knowledge::verification::clear_all();
    let home = tempfile::tempdir().expect("home");
    let prev_home = std::env::var_os("JCODE_HOME");
    let prev_flag = std::env::var_os("JCODE_TASK_GRAPH_ENABLED");
    crate::env::set_var("JCODE_HOME", home.path());
    crate::env::set_var("JCODE_TASK_GRAPH_ENABLED", "true");
    crate::config::invalidate_config_cache();
    TaskGraphEnv {
        _home: home,
        _env: env,
        prev_home,
        prev_flag,
    }
}

impl Drop for TaskGraphEnv {
    fn drop(&mut self) {
        crate::knowledge::verification::clear_all();
        match self.prev_home.take() {
            Some(v) => crate::env::set_var("JCODE_HOME", v),
            None => crate::env::remove_var("JCODE_HOME"),
        }
        match self.prev_flag.take() {
            Some(v) => crate::env::set_var("JCODE_TASK_GRAPH_ENABLED", v),
            None => crate::env::remove_var("JCODE_TASK_GRAPH_ENABLED"),
        }
        crate::config::invalidate_config_cache();
    }
}

fn graph_ctx(session: &str, project: &std::path::Path) -> ToolContext {
    ToolContext {
        session_id: session.to_string(),
        message_id: "msg1".to_string(),
        tool_call_id: "tool1".to_string(),
        working_dir: Some(project.to_path_buf()),
        stdin_request_tx: None,
        graceful_shutdown_signal: None,
        execution_mode: crate::tool::ToolExecutionMode::AgentTurn,
    }
}

fn graph_milestone() -> crate::goal::GoalMilestone {
    serde_json::from_value(json!({
        "id": "m1",
        "title": "milestone",
        "steps": [
            {"id": "build", "content": "build it", "status": "completed"},
            {"id": "test", "content": "test it", "status": "pending",
             "blocked_by": ["build"], "verification": "cargo test passes"},
            {"id": "ship", "content": "ship it", "status": "pending",
             "blocked_by": ["test"], "safe_for_ambient": true}
        ]
    }))
    .expect("milestone json")
}

#[tokio::test]
async fn ready_action_is_flag_gated_and_lists_frontier() {
    // Flag off: refused with a clear message.
    {
        let _env = crate::storage::lock_test_env();
        let tool = InitiativeTool::new();
        let temp = tempfile::tempdir().expect("tempdir");
        let out = tool
            .execute(json!({"action": "ready"}), graph_ctx("ses_tg_off", temp.path()))
            .await
            .expect("ready while disabled");
        assert!(out.output.contains("disabled"));
    }

    let env = setup_task_graph_enabled();
    let project = env._home.path().join("repo");
    std::fs::create_dir_all(&project).expect("project dir");
    let tool = InitiativeTool::new();
    let ctx = graph_ctx("ses_tg_ready", &project);

    let create = tool
        .execute(
            json!({
                "action": "create",
                "title": "Graph demo",
                "scope": "project",
                "milestones": [serde_json::to_value(graph_milestone()).unwrap()]
            }),
            ctx.clone(),
        )
        .await
        .expect("create");
    assert!(create.output.contains("Created initiative"));

    let ready = tool
        .execute(json!({"action": "ready", "id": "graph-demo"}), ctx)
        .await
        .expect("ready");
    assert!(ready.output.contains("`test` test it"), "test step ready: {}", ready.output);
    assert!(ready.output.contains("`ship` ship it (blocked by test)"));
    let meta = ready.metadata.expect("metadata");
    assert_eq!(meta["ready"], json!(["test"]));
    assert_eq!(meta["blocked"], json!(["ship"]));
}

#[tokio::test]
async fn verify_step_upgrades_gated_completion_with_evidence() {
    let env = setup_task_graph_enabled();
    let project = env._home.path().join("repo");
    std::fs::create_dir_all(&project).expect("project dir");
    let tool = InitiativeTool::new();
    let ctx = graph_ctx("ses_tg_verify", &project);

    tool.execute(
        json!({
            "action": "create",
            "title": "Graph demo",
            "scope": "project",
            "milestones": [serde_json::to_value(graph_milestone()).unwrap()]
        }),
        ctx.clone(),
    )
    .await
    .expect("create");

    // Marking the gated step completed without evidence parks it.
    let update = tool
        .execute(
            json!({
                "action": "update",
                "id": "graph-demo",
                "milestones": [serde_json::from_str::<serde_json::Value>(
                    &serde_json::to_string(&graph_milestone()).unwrap().replace(
                        "\"id\":\"test\",\"content\":\"test it\",\"status\":\"pending\"",
                        "\"id\":\"test\",\"content\":\"test it\",\"status\":\"completed\"",
                    )
                ).unwrap()]
            }),
            ctx.clone(),
        )
        .await
        .expect("update");
    assert!(update.output.contains("Updated initiative"));
    let stored = crate::goal::load_goal("graph-demo", None, Some(&project))
        .expect("load")
        .expect("goal");
    assert_eq!(stored.milestones[0].steps[1].status, "done_pending_verification");

    // verify_step without evidence: friendly refusal, not an error.
    let refused = tool
        .execute(
            json!({"action": "verify_step", "id": "graph-demo", "step_id": "test"}),
            ctx.clone(),
        )
        .await
        .expect("verify without evidence");
    assert!(refused.output.contains("Could not verify"), "{}", refused.output);

    // With fresh session evidence the step completes with provenance.
    crate::knowledge::verification::record_command(
        "ses_tg_verify",
        "cargo test -p demo",
        Some(0),
    );
    let verified = tool
        .execute(
            json!({"action": "verify_step", "id": "graph-demo", "step_id": "test"}),
            ctx.clone(),
        )
        .await
        .expect("verify with evidence");
    assert!(
        verified.output.contains("Verified step `test`")
            && verified.output.contains("cargo test -p demo (exit 0)"),
        "{}",
        verified.output
    );

    // The dependent step is now ready, and it is marked ambient-safe.
    let ready = tool
        .execute(json!({"action": "ready", "id": "graph-demo"}), ctx)
        .await
        .expect("ready after verify");
    assert!(ready.output.contains("`ship` ship it [ambient-safe]"), "{}", ready.output);
}
