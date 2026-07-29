//! Memory tool for storing and recalling information across sessions

use super::{Tool, ToolContext, ToolOutput};
use crate::memory::{MemoryCategory, MemoryEntry, MemoryManager, MemoryScope};
use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

pub struct MemoryTool {
    manager: MemoryManager,
}

impl MemoryTool {
    pub fn new() -> Self {
        Self {
            manager: MemoryManager::new(),
        }
    }

    /// Create a memory tool in test mode (isolated storage)
    pub fn new_test() -> Self {
        Self {
            manager: MemoryManager::new_test(),
        }
    }

    fn parse_scope(scope: Option<&str>, default: MemoryScope) -> Result<MemoryScope> {
        match scope.unwrap_or(match default {
            MemoryScope::Project => "project",
            MemoryScope::Global => "global",
            MemoryScope::All => "all",
        }) {
            "project" => Ok(MemoryScope::Project),
            "global" => Ok(MemoryScope::Global),
            "all" => Ok(MemoryScope::All),
            other => Err(anyhow::anyhow!(
                "Unknown scope: {}. Use project, global, or all",
                other
            )),
        }
    }

    /// Scope the manager to the per-call working directory so project-scoped
    /// memories resolve to the right `projects/<hash>.json` store. The base
    /// manager is built once in `new()` with `project_dir: None`, which made
    /// project writes silently no-op and reads come back empty (issue #491).
    fn scoped_manager(&self, ctx: &ToolContext) -> MemoryManager {
        match ctx.working_dir.as_deref() {
            Some(dir) if !dir.as_os_str().is_empty() => self.manager.clone().with_project_dir(dir),
            _ => self.manager.clone(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct MemoryInput {
    action: String,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default)]
    scope: Option<String>,
    /// For link action: source memory ID
    #[serde(default)]
    from_id: Option<String>,
    /// For link action: target memory ID
    #[serde(default)]
    to_id: Option<String>,
    /// For link action: relationship weight (0.0-1.0)
    #[serde(default)]
    weight: Option<f32>,
    /// For related action: traversal depth (default: 2)
    #[serde(default)]
    depth: Option<usize>,
    /// For recall action: max results (default: 10)
    #[serde(default)]
    limit: Option<usize>,
    /// For recall action: retrieval mode
    #[serde(default)]
    mode: Option<String>,
    /// For note action: working-memory kind (goal, constraint, fact, decision, open)
    #[serde(default)]
    kind: Option<String>,
    /// For set_importance action: explicit importance (0.0-1.0)
    #[serde(default)]
    importance: Option<f32>,
}

#[async_trait]
impl Tool for MemoryTool {
    fn name(&self) -> &str {
        "memory"
    }

    fn description(&self) -> &str {
        "Manage memory."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "intent": super::intent_schema_property(),
                "action": {
                    "type": "string",
                    "enum": ["remember", "recall", "search", "list", "forget", "tag", "link", "related", "working", "note", "rehearse", "promote", "set_importance"],
                    "description": "Action."
                },
                "content": { "type": "string" },
                "category": {
                    "type": "string",
                    "enum": ["fact", "preference", "entity", "correction"]
                },
                "query": { "type": "string" },
                "id": { "type": "string" },
                "tags": { "type": "array", "items": { "type": "string" } },
                "scope": { "type": "string", "enum": ["project", "global", "all"] },
                "from_id": { "type": "string" },
                "to_id": { "type": "string" },
                "limit": { "type": "integer", "description": "Max results." },
                "kind": {
                    "type": "string",
                    "enum": ["goal", "constraint", "fact", "decision", "open"],
                    "description": "Working-memory item kind (note action)."
                },
                "importance": {
                    "type": "number",
                    "description": "Importance 0.0-1.0 (set_importance action)."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        use crate::memory;
        use crate::memory_types::{MemoryEventKind, MemoryState};

        let input: MemoryInput = serde_json::from_value(input)?;
        let action_label = input.action.clone();
        let session_id = ctx.session_id.clone();
        let manager = self.scoped_manager(&ctx);

        match input.action.as_str() {
            "remember" => {
                let content = input
                    .content
                    .ok_or_else(|| anyhow::anyhow!("content required"))?;
                let category: MemoryCategory = input
                    .category
                    .as_deref()
                    .unwrap_or("fact")
                    .parse()
                    .map_err(|err| anyhow::anyhow!("invalid memory category: {}", err))?;
                let scope = input.scope.as_deref().unwrap_or("project");
                memory::set_state(MemoryState::ToolAction {
                    action: "remember".into(),
                    detail: truncate_for_widget(&content, 40),
                });
                let mut entry =
                    MemoryEntry::new(category.clone(), &content).with_source(ctx.session_id);
                if let Some(tags) = input.tags {
                    entry = entry.with_tags(tags);
                }
                let id = if scope == "global" {
                    manager.remember_global(entry)?
                } else {
                    manager.remember_project(entry)?
                };
                // The agent just wrote this memory itself; the content is in
                // the transcript (tool call + result), so auto-recall should
                // not inject it back into this session.
                memory::mark_memories_known(
                    &session_id,
                    std::slice::from_ref(&id),
                    "stored via memory tool in this session",
                );
                memory::add_event(MemoryEventKind::ToolRemembered {
                    content: truncate_for_widget(&content, 60),
                    scope: scope.to_string(),
                    category: category.to_string(),
                });
                memory::set_state(MemoryState::Idle);
                Ok(ToolOutput::new(format!(
                    "Remembered {} ({}): \"{}\" [id: {}]",
                    category, scope, content, id
                )))
            }
            "recall" => {
                let limit = input.limit.unwrap_or(10);
                let scope = Self::parse_scope(input.scope.as_deref(), MemoryScope::All)?;
                let mode = input.mode.as_deref().unwrap_or_else(|| {
                    if input.query.is_some() {
                        "cascade"
                    } else {
                        "recent"
                    }
                });

                match mode {
                    "recent" => {
                        memory::set_state(MemoryState::ToolAction {
                            action: "recall".into(),
                            detail: "recent".into(),
                        });
                        let result = match manager.get_prompt_memories_scoped(limit, scope) {
                            Some(memories) => {
                                let count =
                                    memories.lines().filter(|l| l.starts_with("- ")).count();
                                memory::add_event(MemoryEventKind::ToolRecalled {
                                    query: "(recent)".into(),
                                    count,
                                });
                                Ok(ToolOutput::new(format!("Recent memories:\n{}", memories)))
                            }
                            None => {
                                memory::add_event(MemoryEventKind::ToolRecalled {
                                    query: "(recent)".into(),
                                    count: 0,
                                });
                                Ok(ToolOutput::new("No memories stored yet."))
                            }
                        };
                        memory::set_state(MemoryState::Idle);
                        result
                    }
                    "semantic" | "cascade" => {
                        let query = match &input.query {
                            Some(q) => q.clone(),
                            None => {
                                return Err(anyhow::anyhow!(
                                    "query required for semantic/cascade mode"
                                ));
                            }
                        };
                        memory::set_state(MemoryState::ToolAction {
                            action: "recall".into(),
                            detail: truncate_for_widget(&query, 40),
                        });

                        let results = if mode == "cascade" {
                            manager
                                .find_similar_with_cascade_scoped(&query, 0.5, limit, scope)?
                        } else {
                            manager
                                .find_similar_scoped(&query, 0.5, limit, scope)?
                        };

                        memory::add_event(MemoryEventKind::ToolRecalled {
                            query: truncate_for_widget(&query, 40),
                            count: results.len(),
                        });
                        memory::set_state(MemoryState::Idle);

                        if results.is_empty() {
                            Ok(ToolOutput::new(format!(
                                "No memories found matching '{}'. Try recall without query to see recent memories.",
                                query
                            )))
                        } else {
                            let mut out = format!(
                                "Found {} relevant memories for '{}':\n\n",
                                results.len(),
                                query
                            );
                            for (entry, score) in results {
                                let tags_str = if entry.tags.is_empty() {
                                    String::new()
                                } else {
                                    format!(" [{}]", entry.tags.join(", "))
                                };
                                out.push_str(&format!(
                                    "- [{}] {}{}\n  id: {} (relevance: {:.0}%)\n\n",
                                    entry.category,
                                    entry.content,
                                    tags_str,
                                    entry.id,
                                    score * 100.0
                                ));
                            }
                            Ok(ToolOutput::new(out))
                        }
                    }
                    other => Err(anyhow::anyhow!(
                        "Unknown mode: {}. Use recent, semantic, or cascade",
                        other
                    )),
                }
            }
            "search" => {
                let query = input
                    .query
                    .ok_or_else(|| anyhow::anyhow!("query required"))?;
                let scope = Self::parse_scope(input.scope.as_deref(), MemoryScope::All)?;
                memory::set_state(MemoryState::ToolAction {
                    action: "search".into(),
                    detail: truncate_for_widget(&query, 40),
                });
                let results = manager.search_scoped(&query, scope)?;
                memory::add_event(MemoryEventKind::ToolRecalled {
                    query: truncate_for_widget(&query, 40),
                    count: results.len(),
                });
                memory::set_state(MemoryState::Idle);
                if results.is_empty() {
                    Ok(ToolOutput::new(format!("No memories matching '{}'", query)))
                } else {
                    let mut out = format!("Found {} memories:\n\n", results.len());
                    for e in results {
                        out.push_str(&format!(
                            "- [{}] {}\n  id: {}\n\n",
                            e.category, e.content, e.id
                        ));
                    }
                    Ok(ToolOutput::new(out))
                }
            }
            "list" => {
                let scope = Self::parse_scope(input.scope.as_deref(), MemoryScope::All)?;
                memory::set_state(MemoryState::ToolAction {
                    action: "list".into(),
                    detail: String::new(),
                });
                let all = manager.list_all_scoped(scope)?;
                memory::add_event(MemoryEventKind::ToolListed { count: all.len() });
                memory::set_state(MemoryState::Idle);
                if all.is_empty() {
                    Ok(ToolOutput::new("No memories stored."))
                } else {
                    let mut out = format!("All memories ({}):\n\n", all.len());
                    for e in all {
                        out.push_str(&format!(
                            "- [{}] {}\n  id: {}\n\n",
                            e.category, e.content, e.id
                        ));
                    }
                    Ok(ToolOutput::new(out))
                }
            }
            "forget" => {
                let id = input.id.ok_or_else(|| anyhow::anyhow!("id required"))?;
                memory::set_state(MemoryState::ToolAction {
                    action: "forget".into(),
                    detail: truncate_for_widget(&id, 30),
                });
                let found = manager.forget(&id)?;
                memory::add_event(MemoryEventKind::ToolForgot { id: id.clone() });
                memory::set_state(MemoryState::Idle);
                if found {
                    Ok(ToolOutput::new(format!("Forgot: {}", id)))
                } else {
                    Ok(ToolOutput::new(format!("Not found: {}", id)))
                }
            }
            "tag" => {
                let id = input.id.ok_or_else(|| anyhow::anyhow!("id required"))?;
                let tags = input.tags.ok_or_else(|| anyhow::anyhow!("tags required"))?;

                if tags.is_empty() {
                    return Err(anyhow::anyhow!("At least one tag required"));
                }

                memory::set_state(MemoryState::ToolAction {
                    action: "tag".into(),
                    detail: format!("{} +{}", truncate_for_widget(&id, 20), tags.join(",")),
                });
                for tag in &tags {
                    manager.tag_memory(&id, tag)?;
                }
                let tags_str = tags.join(", ");
                memory::add_event(MemoryEventKind::ToolTagged {
                    id: id.clone(),
                    tags: tags_str.clone(),
                });
                memory::set_state(MemoryState::Idle);

                Ok(ToolOutput::new(format!(
                    "Tagged memory {} with: {}",
                    id, tags_str
                )))
            }
            "link" => {
                let from_id = input
                    .from_id
                    .ok_or_else(|| anyhow::anyhow!("from_id required"))?;
                let to_id = input
                    .to_id
                    .ok_or_else(|| anyhow::anyhow!("to_id required"))?;
                let weight = input.weight.unwrap_or(0.5);

                memory::set_state(MemoryState::ToolAction {
                    action: "link".into(),
                    detail: format!(
                        "{} -> {}",
                        truncate_for_widget(&from_id, 15),
                        truncate_for_widget(&to_id, 15)
                    ),
                });
                manager.link_memories(&from_id, &to_id, weight)?;
                memory::add_event(MemoryEventKind::ToolLinked {
                    from: from_id.clone(),
                    to: to_id.clone(),
                });
                memory::set_state(MemoryState::Idle);
                Ok(ToolOutput::new(format!(
                    "Linked memories {} -> {} (weight {:.2})",
                    from_id, to_id, weight
                )))
            }
            "related" => {
                let id = input.id.ok_or_else(|| anyhow::anyhow!("id required"))?;
                let depth = input.depth.unwrap_or(2);

                memory::set_state(MemoryState::ToolAction {
                    action: "related".into(),
                    detail: truncate_for_widget(&id, 30),
                });
                let related = manager.get_related(&id, depth)?;
                memory::add_event(MemoryEventKind::ToolRecalled {
                    query: format!("related:{}", truncate_for_widget(&id, 20)),
                    count: related.len(),
                });
                memory::set_state(MemoryState::Idle);

                if related.is_empty() {
                    Ok(ToolOutput::new(format!(
                        "No related memories found for {}",
                        id
                    )))
                } else {
                    let mut out = format!(
                        "Found {} memories related to {} (depth {}):\n\n",
                        related.len(),
                        id,
                        depth
                    );
                    for e in related {
                        out.push_str(&format!(
                            "- [{}] {}\n  id: {}\n\n",
                            e.category, e.content, e.id
                        ));
                    }
                    Ok(ToolOutput::new(out))
                }
            }
            "working" => {
                if !memory::working_memory_enabled() {
                    return Ok(ToolOutput::new(
                        "Working memory is disabled (agents.working_memory_enabled = false).",
                    ));
                }
                let items = memory::list_working_memory(&session_id);
                memory::set_state(MemoryState::ToolAction {
                    action: "working".into(),
                    detail: format!("{} items", items.len()),
                });
                memory::set_state(MemoryState::Idle);
                if items.is_empty() {
                    Ok(ToolOutput::new(
                        "Working memory is empty. Use action=note to add items.",
                    ))
                } else {
                    let mut out = format!("Working memory ({} items):\n\n", items.len());
                    for item in items {
                        out.push_str(&format!(
                            "- [{}] {} (rehearsals: {}, id: {})\n",
                            item.kind, item.content, item.rehearsals, item.id
                        ));
                    }
                    Ok(ToolOutput::new(out))
                }
            }
            "note" => {
                if !memory::working_memory_enabled() {
                    return Ok(ToolOutput::new(
                        "Working memory is disabled (agents.working_memory_enabled = false).",
                    ));
                }
                let content = input
                    .content
                    .ok_or_else(|| anyhow::anyhow!("content required"))?;
                let kind = memory::WorkingMemoryKind::parse(input.kind.as_deref().unwrap_or(""));
                memory::set_state(MemoryState::ToolAction {
                    action: "note".into(),
                    detail: truncate_for_widget(&content, 40),
                });
                let (item, evicted) = memory::push_working_memory(&session_id, &content, kind);
                // Items squeezed out under pressure still earn promotion when
                // they were rehearsed enough (P4 exit rule).
                let promoted = memory::promote_exiting_items(&manager, &evicted);
                memory::set_state(MemoryState::Idle);
                let mut out = format!("Noted [{}]: \"{}\" (id: {})", item.kind, item.content, item.id);
                if !evicted.is_empty() {
                    out.push_str(&format!(
                        "\nEvicted {} item(s) to make room ({} promoted to long-term).",
                        evicted.len(),
                        promoted
                    ));
                }
                Ok(ToolOutput::new(out))
            }
            "rehearse" => {
                if !memory::working_memory_enabled() {
                    return Ok(ToolOutput::new(
                        "Working memory is disabled (agents.working_memory_enabled = false).",
                    ));
                }
                let id = input.id.ok_or_else(|| anyhow::anyhow!("id required"))?;
                memory::set_state(MemoryState::ToolAction {
                    action: "rehearse".into(),
                    detail: truncate_for_widget(&id, 30),
                });
                let result = memory::rehearse_with_promotion(&manager, &session_id, &id);
                memory::set_state(MemoryState::Idle);
                match result {
                    Some((item, Some(outcome))) => Ok(ToolOutput::new(format!(
                        "Rehearsed \"{}\" (rehearsals: {}). Promoted to long-term memory [id: {}].",
                        item.content,
                        item.rehearsals,
                        outcome.memory_id()
                    ))),
                    Some((item, None)) => Ok(ToolOutput::new(format!(
                        "Rehearsed \"{}\" (rehearsals: {}).",
                        item.content, item.rehearsals
                    ))),
                    None => Ok(ToolOutput::new(format!(
                        "No working-memory item with id {} in this session.",
                        id
                    ))),
                }
            }
            "promote" => {
                if !memory::working_memory_enabled() {
                    return Ok(ToolOutput::new(
                        "Working memory is disabled (agents.working_memory_enabled = false).",
                    ));
                }
                let id = input.id.ok_or_else(|| anyhow::anyhow!("id required"))?;
                memory::set_state(MemoryState::ToolAction {
                    action: "promote".into(),
                    detail: truncate_for_widget(&id, 30),
                });
                let item = memory::list_working_memory(&session_id)
                    .into_iter()
                    .find(|item| item.id == id);
                let result = match item {
                    Some(item) => {
                        let outcome = memory::promote_item(&manager, &item)?;
                        memory::mark_memories_known(
                            &session_id,
                            std::slice::from_ref(&outcome.memory_id().to_string()),
                            "promoted via memory tool in this session",
                        );
                        Ok(ToolOutput::new(format!(
                            "Promoted \"{}\" to long-term memory [id: {}].",
                            item.content,
                            outcome.memory_id()
                        )))
                    }
                    None => Ok(ToolOutput::new(format!(
                        "No working-memory item with id {} in this session.",
                        id
                    ))),
                };
                memory::set_state(MemoryState::Idle);
                result
            }
            "set_importance" => {
                let id = input.id.ok_or_else(|| anyhow::anyhow!("id required"))?;
                let importance = input
                    .importance
                    .ok_or_else(|| anyhow::anyhow!("importance required (0.0-1.0)"))?;
                memory::set_state(MemoryState::ToolAction {
                    action: "set_importance".into(),
                    detail: format!("{} -> {:.2}", truncate_for_widget(&id, 20), importance),
                });
                let stored = manager.set_memory_importance(&id, importance)?;
                memory::set_state(MemoryState::Idle);
                let note = if stored >= 0.8 {
                    " This memory is now protected from pruning."
                } else {
                    ""
                };
                Ok(ToolOutput::new(format!(
                    "Set importance of {} to {:.2}.{}",
                    id, stored, note
                )))
            }
            other => Err(anyhow::anyhow!("Unknown action: {}", other)),
        }
        .map_err(|err| {
            crate::logging::warn(&format!(
                "[tool:memory] action failed action={} session_id={} error={}",
                action_label, session_id, err
            ));
            err
        })
    }
}

fn truncate_for_widget(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        let truncated: String = s.chars().take(max).collect();
        format!("{}â€¦", truncated)
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_only_advertises_core_memory_fields() {
        let schema = MemoryTool::new().parameters_schema();
        let props = schema["properties"]
            .as_object()
            .expect("memory schema should have properties");

        assert!(props.contains_key("action"));
        assert!(props.contains_key("content"));
        assert!(props.contains_key("category"));
        assert!(props.contains_key("query"));
        assert!(props.contains_key("id"));
        assert!(props.contains_key("tags"));
        assert!(props.contains_key("scope"));
        assert!(props.contains_key("from_id"));
        assert!(props.contains_key("to_id"));
        assert!(props.contains_key("limit"));
        assert!(props.contains_key("kind"));
        assert!(props.contains_key("importance"));
        assert!(!props.contains_key("weight"));
        assert!(!props.contains_key("depth"));
        assert!(!props.contains_key("mode"));

        let actions: Vec<&str> = schema["properties"]["action"]["enum"]
            .as_array()
            .expect("action enum")
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        for action in ["working", "note", "rehearse", "promote", "set_importance"] {
            assert!(
                actions.contains(&action),
                "schema must advertise the {action} action"
            );
        }
    }

    fn test_ctx(working_dir: Option<std::path::PathBuf>) -> ToolContext {
        ToolContext {
            session_id: "test-session".to_string(),
            message_id: "test-message".to_string(),
            tool_call_id: "test-tool-call".to_string(),
            working_dir,
            stdin_request_tx: None,
            graceful_shutdown_signal: None,
            execution_mode: crate::tool::ToolExecutionMode::Direct,
        }
    }

    /// Issue #491 regression: project-scoped remember followed by list must
    /// round-trip through the real (non-test-mode) manager when the tool
    /// context carries a working dir.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn project_scope_round_trips_with_working_dir() {
        let _guard = crate::storage::lock_test_env();
        let home = tempfile::tempdir().expect("home");
        let project = tempfile::tempdir().expect("project");
        let prev_home = std::env::var_os("JCODE_HOME");
        crate::env::set_var("JCODE_HOME", home.path());

        let tool = MemoryTool::new();
        let remember = tool
            .execute(
                json!({
                    "action": "remember",
                    "content": "issue-491-probe",
                    "scope": "project"
                }),
                test_ctx(Some(project.path().to_path_buf())),
            )
            .await
            .expect("remember should succeed");
        assert!(remember.output.contains("issue-491-probe"));

        let list = tool
            .execute(
                json!({ "action": "list", "scope": "project" }),
                test_ctx(Some(project.path().to_path_buf())),
            )
            .await
            .expect("list should succeed");
        assert!(
            list.output.contains("issue-491-probe"),
            "project-scoped memory must persist and be listed, got: {}",
            list.output
        );

        if let Some(prev_home) = prev_home {
            crate::env::set_var("JCODE_HOME", prev_home);
        } else {
            crate::env::remove_var("JCODE_HOME");
        }
    }

    /// The five working-memory/importance actions added in P7. Runs with the
    /// flag toggled via env override (config is read live), then verifies the
    /// disabled path returns the friendly refusal instead of erroring.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn working_memory_actions_round_trip_when_enabled() {
        let _guard = crate::storage::lock_test_env();
        let home = tempfile::tempdir().expect("home");
        let project = tempfile::tempdir().expect("project");
        let prev_home = std::env::var_os("JCODE_HOME");
        let prev_flag = std::env::var_os("JCODE_WORKING_MEMORY_ENABLED");
        crate::env::set_var("JCODE_HOME", home.path());
        crate::env::set_var("JCODE_WORKING_MEMORY_ENABLED", "1");
        crate::config::invalidate_config_cache();
        crate::memory::clear_working_memory("test-session");

        let tool = MemoryTool::new();
        let ctx = || test_ctx(Some(project.path().to_path_buf()));

        // note: adds an item and reports its id.
        let noted = tool
            .execute(
                json!({ "action": "note", "content": "ship the P7 actions", "kind": "goal" }),
                ctx(),
            )
            .await
            .expect("note should succeed");
        assert!(noted.output.contains("Noted [goal]"), "{}", noted.output);
        let id = noted
            .output
            .split("(id: ")
            .nth(1)
            .and_then(|s| s.split(')').next())
            .expect("note output should contain the item id")
            .to_string();

        // working: lists the item.
        let listing = tool
            .execute(json!({ "action": "working" }), ctx())
            .await
            .expect("working should succeed");
        assert!(
            listing.output.contains("ship the P7 actions"),
            "{}",
            listing.output
        );

        // rehearse below threshold: no promotion yet.
        let rehearsed = tool
            .execute(json!({ "action": "rehearse", "id": id }), ctx())
            .await
            .expect("rehearse should succeed");
        assert!(
            rehearsed.output.contains("rehearsals: 1") && !rehearsed.output.contains("Promoted"),
            "{}",
            rehearsed.output
        );

        // promote: explicit promotion to long-term.
        let promoted = tool
            .execute(json!({ "action": "promote", "id": id }), ctx())
            .await
            .expect("promote should succeed");
        assert!(promoted.output.contains("Promoted"), "{}", promoted.output);
        let ltm_id = promoted
            .output
            .split("[id: ")
            .nth(1)
            .and_then(|s| s.split(']').next())
            .expect("promote output should contain the long-term id")
            .to_string();

        // set_importance: valid id updates and reports protection at >= 0.8.
        let set = tool
            .execute(
                json!({ "action": "set_importance", "id": ltm_id, "importance": 0.9 }),
                ctx(),
            )
            .await
            .expect("set_importance should succeed");
        assert!(
            set.output.contains("0.90") && set.output.contains("protected from pruning"),
            "{}",
            set.output
        );

        // set_importance on a missing id must error.
        let missing = tool
            .execute(
                json!({ "action": "set_importance", "id": "mem_missing", "importance": 0.5 }),
                ctx(),
            )
            .await;
        assert!(missing.is_err(), "missing id must be an error");

        // Disabled path: flag off returns the refusal, not an error.
        crate::env::set_var("JCODE_WORKING_MEMORY_ENABLED", "0");
        crate::config::invalidate_config_cache();
        let refused = tool
            .execute(json!({ "action": "note", "content": "x" }), ctx())
            .await
            .expect("disabled note should not error");
        assert!(refused.output.contains("disabled"), "{}", refused.output);

        crate::memory::clear_working_memory("test-session");
        match prev_flag {
            Some(v) => crate::env::set_var("JCODE_WORKING_MEMORY_ENABLED", v),
            None => crate::env::remove_var("JCODE_WORKING_MEMORY_ENABLED"),
        }
        crate::config::invalidate_config_cache();
        match prev_home {
            Some(v) => crate::env::set_var("JCODE_HOME", v),
            None => crate::env::remove_var("JCODE_HOME"),
        }
    }
}
