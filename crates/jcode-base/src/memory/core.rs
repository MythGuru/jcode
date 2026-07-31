use super::{MemoryCategory, MemoryEntry, MemoryManager};
use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::PathBuf;

const CORE_TAG: &str = "core";
const CORE_MEMORY_HEADING: &str = "# Core Memory\n";
const CORE_PROPOSALS_FILE: &str = "core_proposals.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoreProposalPayload {
    pub content: String,
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoreProposal {
    pub proposal_id: String,
    pub timestamp: DateTime<Utc>,
    payload: CoreProposalPayload,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreConfirmation {
    pub entry_id: String,
    pub updated: bool,
}

/// Return active core memories from the global memory graph.
///
/// Project memories are deliberately not consulted: core memory is durable
/// user-level context shared across projects.
pub fn list_core_memories() -> Vec<MemoryEntry> {
    let manager = MemoryManager::new();
    let graph = match manager.load_global_graph() {
        Ok(graph) => graph,
        Err(err) => {
            crate::logging::info(&format!("Failed to load core memories: {err}"));
            return Vec::new();
        }
    };

    let mut entries: Vec<_> = graph
        .active_memories()
        .filter(|entry| entry.tags.iter().any(|tag| tag == CORE_TAG))
        .cloned()
        .collect();
    entries.sort_by(|left, right| {
        core_tag_rank(left)
            .cmp(&core_tag_rank(right))
            .then_with(|| left.created_at.cmp(&right.created_at))
            .then_with(|| left.id.cmp(&right.id))
    });
    entries
}

/// Stage a core-memory change without mutating the live memory graph.
pub fn stage_core_proposal(
    content: &str,
    tags: Option<Vec<String>>,
    target_id: Option<String>,
) -> Result<CoreProposal> {
    let content = content.trim();
    if content.is_empty() {
        bail!("content required");
    }

    if let Some(target_id) = target_id.as_deref() {
        let graph = MemoryManager::new().load_global_graph()?;
        if !graph.memories.contains_key(target_id) {
            bail!("Global memory not found: {target_id}");
        }
    }

    let proposal = CoreProposal {
        proposal_id: format!("core-proposal-{}", uuid::Uuid::new_v4().simple()),
        timestamp: Utc::now(),
        payload: CoreProposalPayload {
            content: content.to_string(),
            tags: normalize_core_tags(tags.unwrap_or_default()),
            id: target_id,
        },
    };
    let mut proposals = load_core_proposals()?;
    proposals.push(proposal.clone());
    save_core_proposals(&proposals)?;
    Ok(proposal)
}

/// Apply one explicitly selected staged proposal to the global graph.
pub fn confirm_core_proposal(proposal_id: &str) -> Result<CoreConfirmation> {
    let proposal_id = proposal_id.trim();
    if proposal_id.is_empty() {
        bail!("proposal id required");
    }

    let mut proposals = load_core_proposals()?;
    let Some(index) = proposals
        .iter()
        .position(|proposal| proposal.proposal_id == proposal_id)
    else {
        bail!("Core proposal not found: {proposal_id}");
    };
    let proposal = proposals[index].clone();

    let manager = MemoryManager::new();
    let mut graph = manager.load_global_graph()?;
    let (entry_id, updated) = if let Some(target_id) = proposal.payload.id.as_deref() {
        let existing_tags: BTreeSet<String> = graph
            .get_memory(target_id)
            .ok_or_else(|| anyhow::anyhow!("Global memory not found: {target_id}"))?
            .tags
            .iter()
            .cloned()
            .collect();
        let proposed_tags: BTreeSet<String> = proposal.payload.tags.iter().cloned().collect();
        for tag in existing_tags.difference(&proposed_tags) {
            graph.untag_memory(target_id, tag);
        }
        for tag in proposed_tags.difference(&existing_tags) {
            graph.tag_memory(target_id, tag);
        }
        let Some(entry) = graph.get_memory_mut(target_id) else {
            bail!("Global memory not found: {target_id}");
        };
        let content_changed = entry.content != proposal.payload.content;
        entry.content = proposal.payload.content.clone();
        entry.tags = proposal.payload.tags.clone();
        entry.active = true;
        entry.superseded_by = None;
        if content_changed {
            entry.embedding = None;
            entry.embedding_model = None;
        }
        entry.set_importance(1.0);
        entry.refresh_search_text();
        (target_id.to_string(), true)
    } else {
        let mut entry = MemoryEntry::new(MemoryCategory::Fact, proposal.payload.content.clone())
            .with_tags(proposal.payload.tags.clone());
        entry.set_importance(1.0);
        let entry_id = graph.add_memory(entry);
        (entry_id, false)
    };

    manager.save_global_graph(&graph)?;
    proposals.remove(index);
    save_core_proposals(&proposals)?;
    Ok(CoreConfirmation { entry_id, updated })
}

pub fn is_core_memory(entry: &MemoryEntry) -> bool {
    entry.tags.iter().any(|tag| tag == CORE_TAG)
}

/// The `# Core Memory` section to inject into this turn's prompt, if any.
///
/// Returns `None` when core memory is disabled or the global graph contains no
/// active core-tagged entries. The complete rendered section, including its
/// heading, is capped at the configured character budget.
pub fn core_memory_prompt_section() -> Option<String> {
    let config = crate::config::config();
    if !config.agents.core_memory_enabled {
        return None;
    }

    let entries = list_core_memories();
    if entries.is_empty() {
        return None;
    }

    let mut section = String::from(CORE_MEMORY_HEADING);
    for entry in entries {
        section.push_str("- ");
        section.push_str(&entry.content);
        section.push('\n');
    }

    Some(
        section
            .chars()
            .take(config.agents.core_memory_budget_chars)
            .collect(),
    )
}

fn core_tag_rank(entry: &MemoryEntry) -> u8 {
    const ORDERED_TAGS: [&str; 4] = ["core-identity", "core-style", "core-rules", "core-history"];
    ORDERED_TAGS
        .iter()
        .position(|ordered| entry.tags.iter().any(|tag| tag == ordered))
        .map_or(ORDERED_TAGS.len() as u8, |rank| rank as u8)
}

fn normalize_core_tags(tags: Vec<String>) -> Vec<String> {
    let mut normalized: BTreeSet<String> = tags
        .into_iter()
        .map(|tag| tag.trim().to_string())
        .filter(|tag| !tag.is_empty())
        .collect();
    normalized.insert(CORE_TAG.to_string());
    normalized.into_iter().collect()
}

fn core_proposals_path() -> Result<PathBuf> {
    Ok(crate::storage::jcode_dir()?
        .join("memory")
        .join(CORE_PROPOSALS_FILE))
}

fn load_core_proposals() -> Result<Vec<CoreProposal>> {
    let path = core_proposals_path()?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    crate::storage::read_json(&path)
}

fn save_core_proposals(proposals: &[CoreProposal]) -> Result<()> {
    crate::storage::write_json(&core_proposals_path()?, &proposals)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{MemoryCategory, MemoryEntry, MemoryManager};
    use chrono::{TimeZone, Utc};

    struct TestHome {
        _home: tempfile::TempDir,
        _env: std::sync::MutexGuard<'static, ()>,
        prev_home: Option<std::ffi::OsString>,
        prev_enabled: Option<std::ffi::OsString>,
        prev_budget: Option<std::ffi::OsString>,
    }

    fn setup(enabled: bool, budget: usize) -> (MemoryManager, TestHome) {
        let env = crate::storage::lock_test_env();
        let home = tempfile::tempdir().expect("home");
        let prev_home = std::env::var_os("JCODE_HOME");
        let prev_enabled = std::env::var_os("JCODE_CORE_MEMORY_ENABLED");
        let prev_budget = std::env::var_os("JCODE_CORE_MEMORY_BUDGET_CHARS");
        crate::env::set_var("JCODE_HOME", home.path());
        crate::env::set_var(
            "JCODE_CORE_MEMORY_ENABLED",
            if enabled { "true" } else { "false" },
        );
        crate::env::set_var("JCODE_CORE_MEMORY_BUDGET_CHARS", budget.to_string());
        crate::config::invalidate_config_cache();
        (
            MemoryManager::new().with_project_dir(home.path().join("project")),
            TestHome {
                _home: home,
                _env: env,
                prev_home,
                prev_enabled,
                prev_budget,
            },
        )
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            restore_env("JCODE_HOME", self.prev_home.take());
            restore_env("JCODE_CORE_MEMORY_ENABLED", self.prev_enabled.take());
            restore_env("JCODE_CORE_MEMORY_BUDGET_CHARS", self.prev_budget.take());
            crate::config::invalidate_config_cache();
        }
    }

    fn restore_env(key: &str, value: Option<std::ffi::OsString>) {
        match value {
            Some(value) => crate::env::set_var(key, value),
            None => crate::env::remove_var(key),
        }
    }

    fn entry(content: &str, tags: &[&str], created_at: i64) -> MemoryEntry {
        let mut entry = MemoryEntry::new(MemoryCategory::Fact, content);
        entry.tags = tags.iter().map(|tag| (*tag).to_string()).collect();
        entry.created_at = Utc
            .timestamp_opt(created_at, 0)
            .single()
            .expect("timestamp");
        entry.updated_at = entry.created_at;
        entry
    }

    #[test]
    fn renders_core_memories_in_category_then_creation_order() {
        let (manager, _home) = setup(true, 2000);
        let mut graph = manager.load_global_graph().expect("global graph");
        graph.add_memory(entry("other older", &["core", "misc"], 10));
        graph.add_memory(entry("history", &["core", "core-history"], 40));
        graph.add_memory(entry("rules", &["core", "core-rules"], 30));
        graph.add_memory(entry("style", &["core", "core-style"], 20));
        graph.add_memory(entry("identity", &["core", "core-identity"], 50));
        graph.add_memory(entry("other newer", &["core"], 60));
        let mut inactive = entry("inactive", &["core", "core-identity"], 1);
        inactive.active = false;
        graph.add_memory(inactive);
        graph.add_memory(entry("not core", &["core-style"], 1));
        manager
            .save_global_graph(&graph)
            .expect("save global graph");

        assert_eq!(
            core_memory_prompt_section().as_deref(),
            Some(
                "# Core Memory\n- identity\n- style\n- rules\n- history\n- other older\n- other newer\n"
            )
        );
    }

    #[test]
    fn prompt_respects_unicode_character_budget() {
        let (manager, _home) = setup(true, 19);
        let mut graph = manager.load_global_graph().expect("global graph");
        graph.add_memory(entry("éééééé", &["core"], 1));
        manager
            .save_global_graph(&graph)
            .expect("save global graph");

        let prompt = core_memory_prompt_section().expect("core prompt");
        assert_eq!(prompt, "# Core Memory\n- ééé");
        assert_eq!(prompt.chars().count(), 19);
    }

    #[test]
    fn prompt_is_none_when_flag_is_off() {
        let (manager, _home) = setup(false, 2000);
        let mut graph = manager.load_global_graph().expect("global graph");
        graph.add_memory(entry("identity", &["core"], 1));
        manager
            .save_global_graph(&graph)
            .expect("save global graph");

        assert!(core_memory_prompt_section().is_none());
    }

    #[test]
    fn prompt_is_none_when_global_core_memory_is_empty() {
        let (_manager, _home) = setup(true, 2000);
        assert!(core_memory_prompt_section().is_none());
    }

    #[test]
    fn list_reads_only_the_global_graph() {
        let (manager, _home) = setup(true, 2000);
        let mut project = manager.load_project_graph().expect("project graph");
        project.add_memory(entry("project core", &["core", "core-identity"], 1));
        manager
            .save_project_graph(&project)
            .expect("save project graph");

        assert!(list_core_memories().is_empty());
        assert!(core_memory_prompt_section().is_none());
    }
}
