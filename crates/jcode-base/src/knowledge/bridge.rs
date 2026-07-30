//! Bridge from verified project knowledge to long-term memory (K4).
//!
//! When an entry passes the verification gate it is, by construction, the most
//! trustworthy kind of knowledge jcode holds: a claim backed by a passing
//! build/test run or an explicit user confirmation. This module turns that
//! claim into a long-term memory so it survives beyond the project map file
//! and participates in normal retrieval.
//!
//! Rules:
//! - importance is seeded at [`VERIFIED_LESSON_IMPORTANCE`] (0.85): above the
//!   prune-protection floor (0.8), below the explicit-user band (0.9+), so a
//!   verified lesson can never outrank something the user marked important,
//! - each knowledge entry maps to at most ONE long-term memory, keyed by a
//!   `pk-id:<entry-id>` tag. Re-verification (e.g. after revise) updates and
//!   reinforces that memory instead of minting duplicates,
//! - bridging is best-effort: a failure logs and the verification itself
//!   stands. Losing a lesson must never break the gate.

use super::{KnowledgeEntry, KnowledgeStatus};
use crate::memory::{MemoryCategory, MemoryEntry, MemoryManager};
use anyhow::Result;
use std::path::Path;

/// Importance seeded on a bridged lesson. Prune-protected, but deliberately
/// below the explicit-user-request range (0.9+).
pub const VERIFIED_LESSON_IMPORTANCE: f32 = 0.85;

/// Tag shared by every bridged lesson, for listing/curation.
pub const VERIFIED_LESSON_TAG: &str = "knowledge-verified";

/// The identity tag linking a lesson back to its knowledge entry.
fn identity_tag(entry_id: &str) -> String {
    format!("pk-id:{entry_id}")
}

/// Render the lesson content. The section gives retrieval context the raw
/// entry text lacks (e.g. "rule:" reads very differently from "problem:").
fn lesson_content(entry: &KnowledgeEntry) -> String {
    format!("[project {}] {}", entry.section, entry.content)
}

/// Create or update the long-term memory for a verified knowledge entry.
/// Returns the memory id.
///
/// Callers pass the same `project_dir` the knowledge map belongs to, so the
/// lesson lands in that project's graph.
pub fn bridge_verified_entry(project_dir: &Path, entry: &KnowledgeEntry) -> Result<String> {
    debug_assert_eq!(
        entry.status,
        KnowledgeStatus::Verified,
        "only verified entries may be bridged"
    );
    let manager = MemoryManager::new().with_project_dir(project_dir);
    let tag = identity_tag(&entry.id);

    // One lesson per knowledge entry: find the existing one by identity tag.
    let mut graph = manager.load_project_graph()?;
    let existing_id = graph
        .memories
        .values()
        .find(|memory| memory.tags.contains(&tag))
        .map(|memory| memory.id.clone());

    if let Some(id) = existing_id {
        if let Some(memory) = graph.get_memory_mut(&id) {
            memory.content = lesson_content(entry);
            memory.refresh_search_text();
            // Content changed, so any old embedding no longer describes it.
            memory.embedding = None;
            memory.embedding_model = None;
            memory.reinforce("knowledge_verification", 0);
            memory.set_importance(VERIFIED_LESSON_IMPORTANCE.max(memory.importance));
            memory.active = true;
        }
        manager.save_project_graph(&graph)?;
        return Ok(id);
    }
    drop(graph);

    let mut lesson = MemoryEntry::new(MemoryCategory::Fact, lesson_content(entry))
        .with_source("project_knowledge")
        .with_tags(vec![VERIFIED_LESSON_TAG.to_string(), tag]);
    lesson.set_importance(VERIFIED_LESSON_IMPORTANCE);
    if let Some(provenance) = entry.provenance.last() {
        lesson.reinforce(provenance, 0);
    }
    manager.remember_project(lesson)
}

/// Best-effort wrapper used by the gate: bridge, and on failure log instead of
/// propagating. The verification result must never depend on the bridge.
pub fn bridge_best_effort(project_dir: &Path, entry: &KnowledgeEntry) -> Option<String> {
    match bridge_verified_entry(project_dir, entry) {
        Ok(id) => Some(id),
        Err(err) => {
            crate::logging::info(&format!(
                "Failed to bridge verified knowledge entry {} into long-term memory: {err}",
                entry.id
            ));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{KnowledgeSection, ProjectKnowledge};
    use super::*;

    struct TestHome {
        _home: tempfile::TempDir,
        _env: std::sync::MutexGuard<'static, ()>,
        prev: Option<std::ffi::OsString>,
    }

    fn setup() -> TestHome {
        let env = crate::storage::lock_test_env();
        let home = tempfile::tempdir().expect("home");
        let prev = std::env::var_os("JCODE_HOME");
        crate::env::set_var("JCODE_HOME", home.path());
        crate::config::invalidate_config_cache();
        TestHome {
            _home: home,
            _env: env,
            prev,
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            match self.prev.take() {
                Some(v) => crate::env::set_var("JCODE_HOME", v),
                None => crate::env::remove_var("JCODE_HOME"),
            }
        }
    }

    fn verified_entry(section: KnowledgeSection, content: &str) -> KnowledgeEntry {
        let mut knowledge = ProjectKnowledge::default();
        let id = knowledge.propose(section, content);
        knowledge.mark_verified(&id, "cargo test (exit 0)");
        knowledge.get(&id).unwrap().clone()
    }

    fn project_memories(project_dir: &Path) -> Vec<MemoryEntry> {
        let manager = MemoryManager::new().with_project_dir(project_dir);
        let graph = manager.load_project_graph().expect("graph");
        graph.memories.values().cloned().collect()
    }

    #[test]
    fn bridging_creates_a_prune_protected_lesson() {
        let _home = setup();
        let project = Path::new("C:/bridge/create");
        let entry = verified_entry(KnowledgeSection::Rule, "flags default off");

        let id = bridge_verified_entry(project, &entry).expect("bridge");
        let memories = project_memories(project);
        assert_eq!(memories.len(), 1);
        let lesson = &memories[0];
        assert_eq!(lesson.id, id);
        assert_eq!(lesson.content, "[project rule] flags default off");
        assert!(
            (lesson.importance - VERIFIED_LESSON_IMPORTANCE).abs() < 1e-6,
            "importance must be prune-protected but below the user band"
        );
        assert!(lesson.tags.iter().any(|t| t == VERIFIED_LESSON_TAG));
        assert!(lesson.tags.iter().any(|t| t == &identity_tag(&entry.id)));
        assert_eq!(lesson.source.as_deref(), Some("project_knowledge"));
    }

    #[test]
    fn reverification_updates_the_same_lesson_instead_of_duplicating() {
        let _home = setup();
        let project = Path::new("C:/bridge/update");

        let mut knowledge = ProjectKnowledge::default();
        let entry_id = knowledge.propose(KnowledgeSection::Decision, "initial claim");
        knowledge.mark_verified(&entry_id, "cargo test (exit 0)");
        let first = knowledge.get(&entry_id).unwrap().clone();
        let first_mem = bridge_verified_entry(project, &first).expect("first bridge");

        // Revise (demotes) and re-verify: same knowledge id, new content.
        knowledge.revise(&entry_id, "revised claim");
        knowledge.mark_verified(&entry_id, "cargo test rerun (exit 0)");
        let second = knowledge.get(&entry_id).unwrap().clone();
        let second_mem = bridge_verified_entry(project, &second).expect("second bridge");

        assert_eq!(first_mem, second_mem, "one lesson per knowledge entry");
        let memories = project_memories(project);
        assert_eq!(memories.len(), 1, "re-verification must not duplicate");
        let lesson = &memories[0];
        assert_eq!(lesson.content, "[project decision] revised claim");
        assert!(lesson.strength > 1, "update must reinforce, not reset");
        assert!(lesson.importance >= VERIFIED_LESSON_IMPORTANCE);
    }

    #[test]
    fn distinct_entries_produce_distinct_lessons() {
        let _home = setup();
        let project = Path::new("C:/bridge/distinct");
        let a = verified_entry(KnowledgeSection::Rule, "rule one");
        let b = verified_entry(KnowledgeSection::Problem, "problem one");

        let id_a = bridge_verified_entry(project, &a).expect("bridge a");
        let id_b = bridge_verified_entry(project, &b).expect("bridge b");
        assert_ne!(id_a, id_b);
        assert_eq!(project_memories(project).len(), 2);
    }

    #[test]
    fn best_effort_wrapper_never_panics() {
        let _home = setup();
        let project = Path::new("C:/bridge/best-effort");
        let entry = verified_entry(KnowledgeSection::Structure, "cargo workspace");
        assert!(bridge_best_effort(project, &entry).is_some());
    }
}
