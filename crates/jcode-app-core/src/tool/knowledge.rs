//! Agent-facing tool for the verification-gated project knowledge model (K3).
//!
//! Thin by design: every rule lives in `jcode-base::knowledge` (model K1, gate
//! K2). This tool only parses input, resolves the project directory from the
//! call's working dir, and renders results. In particular it holds NO
//! verification logic: `verify` delegates to the K2 gate, and `confirm` is the
//! explicit user-authority path that the agent may only invoke after the user
//! actually confirmed the entry in conversation.

use super::{Tool, ToolContext, ToolOutput};
use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;

use crate::knowledge::verification::{self, VerifyError};
use crate::knowledge::{self, KnowledgeSection, KnowledgeStatus};

pub struct KnowledgeTool;

impl KnowledgeTool {
    pub fn new() -> Self {
        Self
    }

    /// The project the map belongs to: the call's working directory. No
    /// directory means no project, which callers see as a clear error rather
    /// than a silently global map.
    fn project_dir(ctx: &ToolContext) -> Result<PathBuf> {
        match ctx.working_dir.as_deref() {
            Some(dir) if !dir.as_os_str().is_empty() => Ok(dir.to_path_buf()),
            _ => Err(anyhow::anyhow!(
                "No working directory for this session, so there is no project to attach knowledge to."
            )),
        }
    }
}

#[derive(Debug, Deserialize)]
struct KnowledgeInput {
    action: String,
    #[serde(default)]
    section: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    id: Option<String>,
    /// For confirm: optional note describing the user's confirmation.
    #[serde(default)]
    note: Option<String>,
}

fn disabled_message() -> ToolOutput {
    ToolOutput::new("Project knowledge is disabled (agents.project_knowledge_enabled = false).")
}

fn status_label(status: KnowledgeStatus) -> &'static str {
    match status {
        KnowledgeStatus::Verified => "verified",
        KnowledgeStatus::Proposed => "proposed",
    }
}

#[async_trait]
impl Tool for KnowledgeTool {
    fn name(&self) -> &str {
        "knowledge"
    }

    fn description(&self) -> &str {
        "Maintain the project's living knowledge map (structure, decisions, rules, known problems, responsibilities). \
         Entries start as proposed; they become verified only after builds/tests pass (action=verify) or the user \
         explicitly confirms them in conversation (action=confirm)."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "intent": super::intent_schema_property(),
                "action": {
                    "type": "string",
                    "enum": ["show", "propose", "revise", "verify", "confirm", "remove", "history"],
                    "description": "Action. verify uses this session's build/test evidence; confirm records explicit user confirmation and must only be used after the user actually confirmed."
                },
                "section": {
                    "type": "string",
                    "enum": ["structure", "decision", "rule", "problem", "responsibility"],
                    "description": "Map section (propose action)."
                },
                "content": { "type": "string", "description": "Entry text (propose/revise)." },
                "id": { "type": "string", "description": "Entry id (revise/verify/confirm/remove)." },
                "note": { "type": "string", "description": "For confirm: what the user said, briefly." }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let input: KnowledgeInput = serde_json::from_value(input)?;
        if !knowledge::project_knowledge_enabled() {
            return Ok(disabled_message());
        }
        let project_dir = Self::project_dir(&ctx)?;

        match input.action.as_str() {
            "show" => {
                let map = knowledge::load(&project_dir);
                if map.is_empty() {
                    return Ok(ToolOutput::new(
                        "The project knowledge map is empty. Use action=propose to add entries.",
                    ));
                }
                let mut out = map.render_markdown();
                out.push_str("\n\nEntries with ids:\n");
                for entry in &map.entries {
                    out.push_str(&format!(
                        "- [{}] ({}) {} (id: {})\n",
                        entry.section,
                        status_label(entry.status),
                        entry.content,
                        entry.id
                    ));
                }
                Ok(ToolOutput::new(out))
            }
            "propose" => {
                let content = input
                    .content
                    .as_deref()
                    .map(str::trim)
                    .filter(|c| !c.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("content required"))?;
                let section = KnowledgeSection::parse(input.section.as_deref().unwrap_or(""));
                let mut map = knowledge::load(&project_dir);
                let id = map.propose(section, content);
                knowledge::save(&project_dir, &map);
                Ok(ToolOutput::new(format!(
                    "Proposed [{}]: \"{}\" (id: {}). It becomes verified after a successful build/test (action=verify) or explicit user confirmation (action=confirm).",
                    section, content, id
                )))
            }
            "revise" => {
                let id = input.id.ok_or_else(|| anyhow::anyhow!("id required"))?;
                let content = input
                    .content
                    .as_deref()
                    .map(str::trim)
                    .filter(|c| !c.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("content required"))?;
                let mut map = knowledge::load(&project_dir);
                if !map.revise(&id, content) {
                    return Ok(ToolOutput::new(format!("No knowledge entry with id {id}.")));
                }
                knowledge::save(&project_dir, &map);
                Ok(ToolOutput::new(format!(
                    "Revised {id}. The entry is back to proposed: changed claims need fresh verification."
                )))
            }
            "verify" => {
                let id = input.id.ok_or_else(|| anyhow::anyhow!("id required"))?;
                match verification::try_verify(&project_dir, &ctx.session_id, &id) {
                    Ok(evidence) => Ok(ToolOutput::new(format!(
                        "Verified {id} with evidence: {evidence}"
                    ))),
                    Err(err @ (VerifyError::Disabled | VerifyError::UnknownEntry)) => {
                        Ok(ToolOutput::new(format!("Cannot verify {id}: {err}.")))
                    }
                    Err(err) => Ok(ToolOutput::new(format!(
                        "Not verified: {err}. The entry stays proposed."
                    ))),
                }
            }
            "confirm" => {
                let id = input.id.ok_or_else(|| anyhow::anyhow!("id required"))?;
                match verification::verify_by_user(&project_dir, &id, input.note.as_deref()) {
                    Ok(provenance) => {
                        Ok(ToolOutput::new(format!("Confirmed {id} ({provenance}).")))
                    }
                    Err(err) => Ok(ToolOutput::new(format!("Cannot confirm {id}: {err}."))),
                }
            }
            "remove" => {
                let id = input.id.ok_or_else(|| anyhow::anyhow!("id required"))?;
                let mut map = knowledge::load(&project_dir);
                match map.remove(&id) {
                    Some(entry) => {
                        knowledge::save(&project_dir, &map);
                        Ok(ToolOutput::new(format!(
                            "Removed [{}] \"{}\".",
                            entry.section, entry.content
                        )))
                    }
                    None => Ok(ToolOutput::new(format!("No knowledge entry with id {id}."))),
                }
            }
            "history" => {
                let events = verification::session_events(&ctx.session_id);
                if events.is_empty() {
                    return Ok(ToolOutput::new(
                        "No verification events in this session yet. Run cargo build/check/clippy/test through the bash tool to produce evidence.",
                    ));
                }
                let mut out = format!("Verification events this session ({}):\n", events.len());
                for event in events {
                    out.push_str(&format!(
                        "- [{}] {} at {}\n",
                        if event.success { "ok" } else { "FAILED" },
                        event.evidence,
                        event.at.format("%H:%M:%S"),
                    ));
                }
                Ok(ToolOutput::new(out))
            }
            other => Err(anyhow::anyhow!(
                "Unknown action: {other}. Use show, propose, revise, verify, confirm, remove, or history."
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(working_dir: Option<&str>) -> ToolContext {
        ToolContext {
            session_id: "knowledge-tool-test".to_string(),
            message_id: "m1".to_string(),
            tool_call_id: "t1".to_string(),
            working_dir: working_dir.map(PathBuf::from),
            stdin_request_tx: None,
            graceful_shutdown_signal: None,
            execution_mode: crate::tool::ToolExecutionMode::AgentTurn(
                crate::tool::TurnExecutionContext::standalone("knowledge-test"),
            ),
        }
    }

    struct TestEnv {
        _home: tempfile::TempDir,
        _env: std::sync::MutexGuard<'static, ()>,
        prev_home: Option<std::ffi::OsString>,
        prev_flag: Option<std::ffi::OsString>,
    }

    /// Isolated home + feature flag ON via env override, restored on drop.
    fn setup_enabled() -> TestEnv {
        let env = crate::storage::lock_test_env();
        verification::clear_all();
        let home = tempfile::tempdir().expect("home");
        let prev_home = std::env::var_os("JCODE_HOME");
        let prev_flag = std::env::var_os("JCODE_PROJECT_KNOWLEDGE_ENABLED");
        crate::env::set_var("JCODE_HOME", home.path());
        crate::env::set_var("JCODE_PROJECT_KNOWLEDGE_ENABLED", "true");
        crate::config::invalidate_config_cache();
        TestEnv {
            _home: home,
            _env: env,
            prev_home,
            prev_flag,
        }
    }

    impl Drop for TestEnv {
        fn drop(&mut self) {
            verification::clear_all();
            match self.prev_home.take() {
                Some(v) => crate::env::set_var("JCODE_HOME", v),
                None => crate::env::remove_var("JCODE_HOME"),
            }
            match self.prev_flag.take() {
                Some(v) => crate::env::set_var("JCODE_PROJECT_KNOWLEDGE_ENABLED", v),
                None => crate::env::remove_var("JCODE_PROJECT_KNOWLEDGE_ENABLED"),
            }
            crate::config::invalidate_config_cache();
        }
    }

    #[tokio::test]
    async fn disabled_flag_short_circuits_every_action() {
        let _env = crate::storage::lock_test_env();
        // Default config: flag off. Every action must return the disabled
        // message without touching disk.
        let tool = KnowledgeTool::new();
        for action in ["show", "propose", "verify", "confirm", "history"] {
            let out = tool
                .execute(json!({ "action": action }), ctx(Some("C:/some/project")))
                .await
                .expect("disabled path must not error");
            assert!(
                out.output.contains("disabled"),
                "{action} must refuse while the flag is off, got: {}",
                out.output
            );
        }
    }

    #[tokio::test]
    async fn propose_show_confirm_round_trip() {
        let _env = setup_enabled();
        let tool = KnowledgeTool::new();
        let project = ctx(Some("C:/tool/project"));

        let out = tool
            .execute(
                json!({ "action": "propose", "section": "rule", "content": "use cargo only on Windows" }),
                project.clone(),
            )
            .await
            .expect("propose");
        assert!(out.output.contains("Proposed [rule]"));
        let id = out
            .output
            .split("(id: ")
            .nth(1)
            .and_then(|s| s.split(')').next())
            .expect("id in output")
            .to_string();

        let out = tool
            .execute(json!({ "action": "show" }), project.clone())
            .await
            .expect("show");
        assert!(out.output.contains("(proposed) use cargo only on Windows"));

        // Explicit user confirmation verifies without build evidence.
        let out = tool
            .execute(
                json!({ "action": "confirm", "id": id, "note": "user said yes in chat" }),
                project.clone(),
            )
            .await
            .expect("confirm");
        assert!(
            out.output
                .contains("user confirmation: user said yes in chat")
        );

        let out = tool
            .execute(json!({ "action": "show" }), project)
            .await
            .expect("show after confirm");
        assert!(
            !out.output.contains("(proposed) use cargo only"),
            "entry must no longer be proposed:\n{}",
            out.output
        );
    }

    #[tokio::test]
    async fn verify_uses_session_evidence_and_reports_gate_refusals() {
        let _env = setup_enabled();
        let tool = KnowledgeTool::new();
        let project = ctx(Some("C:/tool/gated"));

        let out = tool
            .execute(
                json!({ "action": "propose", "section": "decision", "content": "flags default off" }),
                project.clone(),
            )
            .await
            .expect("propose");
        let id = out
            .output
            .split("(id: ")
            .nth(1)
            .and_then(|s| s.split(')').next())
            .expect("id")
            .to_string();

        // No evidence yet: the gate must refuse and say why.
        let out = tool
            .execute(json!({ "action": "verify", "id": id }), project.clone())
            .await
            .expect("verify without evidence");
        assert!(
            out.output
                .contains("no successful build/test verification event")
        );

        // Simulate this session's cargo test passing (recorded after the
        // propose, so it is fresh).
        verification::record_command(&project.session_id, "cargo test -p demo", Some(0));

        let out = tool
            .execute(json!({ "action": "verify", "id": id }), project.clone())
            .await
            .expect("verify with evidence");
        assert!(
            out.output.contains("Verified") && out.output.contains("cargo test -p demo (exit 0)"),
            "gate should pass with fresh evidence, got: {}",
            out.output
        );

        // History shows the evidence trail.
        let out = tool
            .execute(json!({ "action": "history" }), project)
            .await
            .expect("history");
        assert!(out.output.contains("cargo test -p demo (exit 0)"));
    }

    #[tokio::test]
    async fn missing_working_dir_is_a_clear_error() {
        let _env = setup_enabled();
        let tool = KnowledgeTool::new();
        let err = tool
            .execute(json!({ "action": "show" }), ctx(None))
            .await
            .expect_err("no working dir must error");
        assert!(err.to_string().contains("no project"));
    }
}
