//! Nudge the agent to promote durable working-memory items into the project
//! knowledge map.
//!
//! Working memory is deliberately ephemeral: it dies with the session. The
//! knowledge map is deliberately curated: entries are only written when an
//! agent explicitly proposes them. The failure mode between the two is
//! neglect. A session learns something project-level (a decision, a rule),
//! records it in working memory, finishes the task, and never thinks to save
//! it durably. The map goes stale not because anything is wrong but because
//! nobody was reminded.
//!
//! This module closes that gap with a *nudge*, not automation. When a
//! working-memory item looks durable, a short prompt section asks the agent
//! to consider proposing it as project knowledge. The agent judges; the entry
//! still lands as `Proposed` and still needs verification or user
//! confirmation. Nothing is ever written to the map automatically.
//!
//! What counts as "durable-looking" is intentionally conservative:
//! - decisions and constraints qualify immediately (their kinds are durable
//!   by nature; a decision worth recording mid-session is worth keeping),
//! - facts qualify only once rehearsed (a fact that kept mattering has
//!   earned a look; a drive-by observation has not),
//! - goals and open threads never qualify (they are session-shaped),
//! - anything that substantially overlaps an existing knowledge entry is
//!   suppressed (the map already knows it),
//! - items pulled down from long-term memory are skipped (already durable
//!   elsewhere; promoting them here would fork the source of truth).
//!
//! Each item nudges at most once per session. A nudge the agent chose to
//! ignore was answered; repeating it would be nagging, and nagging trains
//! the model to ignore the section entirely.

use crate::memory::{WorkingMemoryItem, WorkingMemoryKind};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Mutex;

/// Word-overlap threshold above which a working-memory item is considered
/// already covered by a knowledge entry.
const OVERLAP_SUPPRESSION_RATIO: f32 = 0.5;

/// Per-session record of item ids that have already been nudged, so a session
/// asks about each item at most once. Process-local by design, mirroring the
/// lifetime of the working-memory buffers themselves.
static NUDGED: Mutex<Option<HashMap<String, HashSet<String>>>> = Mutex::new(None);

fn with_nudged<T>(f: impl FnOnce(&mut HashMap<String, HashSet<String>>) -> T) -> T {
    let mut guard = NUDGED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    f(map)
}

/// Lowercased word set for overlap comparison. Short words are dropped so
/// stopwords ("the", "a", "of") cannot manufacture overlap.
fn word_set(text: &str) -> HashSet<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 4)
        .map(str::to_string)
        .collect()
}

/// Fraction of the item's significant words already present in the entry.
/// Asymmetric on purpose: a long knowledge entry that fully contains a short
/// working-memory item should suppress it, even though the entry says more.
fn covered_ratio(item_words: &HashSet<String>, entry_words: &HashSet<String>) -> f32 {
    if item_words.is_empty() {
        return 1.0; // nothing significant to say; treat as covered
    }
    let covered = item_words.intersection(entry_words).count() as f32;
    covered / item_words.len() as f32
}

/// Whether an item's kind and history make it a promotion candidate at all.
fn kind_qualifies(item: &WorkingMemoryItem) -> bool {
    match item.kind {
        WorkingMemoryKind::Decision | WorkingMemoryKind::Constraint => true,
        WorkingMemoryKind::Fact => item.rehearsals >= 1,
        WorkingMemoryKind::Goal | WorkingMemoryKind::Open => false,
    }
}

/// Pure candidate selection, separated from the global stores so it is
/// directly testable: which items deserve a nudge, given the current map and
/// the set of already-nudged ids.
pub fn select_candidates<'a>(
    items: &'a [WorkingMemoryItem],
    knowledge: &crate::knowledge::ProjectKnowledge,
    already_nudged: &HashSet<String>,
) -> Vec<&'a WorkingMemoryItem> {
    let entry_words: Vec<HashSet<String>> = knowledge
        .entries
        .iter()
        .map(|entry| word_set(&entry.content))
        .collect();

    items
        .iter()
        .filter(|item| kind_qualifies(item))
        .filter(|item| item.source_memory_id.is_none())
        .filter(|item| !already_nudged.contains(&item.id))
        .filter(|item| {
            let words = word_set(&item.content);
            !entry_words
                .iter()
                .any(|entry| covered_ratio(&words, entry) >= OVERLAP_SUPPRESSION_RATIO)
        })
        .collect()
}

/// Pure renderer for the nudge section.
pub fn format_nudge(candidates: &[&WorkingMemoryItem]) -> Option<String> {
    if candidates.is_empty() {
        return None;
    }
    let mut out = String::from(
        "# Knowledge Promotion Check\n\n\
         These working-memory items look durable. If any captures project-level \
         knowledge future sessions should inherit (structure, a decision, a rule, \
         a known problem, a responsibility), save it now with the `knowledge` tool \
         (action=propose, matching section). Rephrase for a reader with no session \
         context. Skip anything session-specific; no reply is needed for skipped \
         items. This check will not repeat for these items.\n",
    );
    for item in candidates {
        out.push_str(&format!("\n- [{}] {}", item.kind, item.content));
    }
    Some(out)
}

/// The `# Knowledge Promotion Check` section for this turn's prompt, if any.
///
/// Single gate, mirroring `working_memory_prompt_section` and
/// `project_knowledge_prompt_section`: returns `None` unless ALL of the
/// following hold, so a caller cannot accidentally inject the section:
/// - both the working-memory and project-knowledge flags are on (each
///   defaults OFF, so the flag-off prompt stays byte-identical),
/// - a session id and working directory are known,
/// - at least one not-yet-nudged candidate survives selection.
///
/// Candidates are marked as nudged when the section is produced, so each item
/// is asked about at most once per session.
pub fn knowledge_nudge_prompt_section(
    session_id: Option<&str>,
    working_dir: Option<&Path>,
) -> Option<String> {
    if !crate::memory::working_memory_enabled() || !crate::knowledge::project_knowledge_enabled() {
        return None;
    }
    let session_id = session_id?;
    let project_dir = working_dir?;

    let items = crate::memory::list_working_memory(session_id);
    if items.is_empty() {
        return None;
    }
    let knowledge = crate::knowledge::load(project_dir);

    with_nudged(|map| {
        let nudged = map.entry(session_id.to_string()).or_default();
        let candidates = select_candidates(&items, &knowledge, nudged);
        let section = format_nudge(&candidates);
        if section.is_some() {
            for candidate in &candidates {
                nudged.insert(candidate.id.clone());
            }
        }
        section
    })
}

/// Drop a session's nudge history (session end / cleanup).
pub fn clear_nudged(session_id: &str) {
    with_nudged(|map| {
        map.remove(session_id);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge::{KnowledgeSection, ProjectKnowledge};

    fn item(kind: WorkingMemoryKind, content: &str, rehearsals: u32) -> WorkingMemoryItem {
        let now = chrono::Utc::now();
        WorkingMemoryItem {
            id: format!("wm_test_{}_{}", content.len(), rand::random::<u64>()),
            content: content.to_string(),
            kind,
            created_at: now,
            rehearsals,
            last_rehearsed: (rehearsals > 0).then_some(now),
            source_memory_id: None,
        }
    }

    #[test]
    fn decisions_and_constraints_qualify_immediately() {
        let items = vec![
            item(
                WorkingMemoryKind::Decision,
                "we chose sqlite over postgres",
                0,
            ),
            item(
                WorkingMemoryKind::Constraint,
                "never touch the billing tables",
                0,
            ),
        ];
        let selected = select_candidates(&items, &ProjectKnowledge::default(), &HashSet::new());
        assert_eq!(selected.len(), 2);
    }

    #[test]
    fn facts_need_rehearsal_and_goals_open_never_qualify() {
        let items = vec![
            item(WorkingMemoryKind::Fact, "the API uses cursor pagination", 0),
            item(WorkingMemoryKind::Fact, "the build needs node twenty", 2),
            item(WorkingMemoryKind::Goal, "ship the dashboard today", 5),
            item(WorkingMemoryKind::Open, "why does the test flake", 5),
        ];
        let selected = select_candidates(&items, &ProjectKnowledge::default(), &HashSet::new());
        assert_eq!(selected.len(), 1);
        assert!(selected[0].content.contains("node twenty"));
    }

    #[test]
    fn items_from_long_term_memory_are_skipped() {
        let mut ltm_item = item(WorkingMemoryKind::Decision, "restated durable decision", 0);
        ltm_item.source_memory_id = Some("mem_123".to_string());
        let items = vec![ltm_item];
        let selected = select_candidates(&items, &ProjectKnowledge::default(), &HashSet::new());
        assert!(selected.is_empty());
    }

    #[test]
    fn overlap_with_existing_entry_suppresses() {
        let mut knowledge = ProjectKnowledge::default();
        knowledge.propose(
            KnowledgeSection::Decision,
            "We decided sqlite replaces postgres for local storage going forward",
        );
        let items = vec![
            item(
                WorkingMemoryKind::Decision,
                "decided sqlite replaces postgres for local storage",
                0,
            ),
            item(
                WorkingMemoryKind::Decision,
                "release trains ship every friday",
                0,
            ),
        ];
        let selected = select_candidates(&items, &knowledge, &HashSet::new());
        assert_eq!(selected.len(), 1);
        assert!(selected[0].content.contains("friday"));
    }

    #[test]
    fn already_nudged_items_are_not_reselected() {
        let items = vec![item(
            WorkingMemoryKind::Decision,
            "we picked rust for the core",
            0,
        )];
        let mut nudged = HashSet::new();
        assert_eq!(
            select_candidates(&items, &ProjectKnowledge::default(), &nudged).len(),
            1
        );
        nudged.insert(items[0].id.clone());
        assert!(select_candidates(&items, &ProjectKnowledge::default(), &nudged).is_empty());
    }

    #[test]
    fn format_lists_items_and_is_none_when_empty() {
        assert!(format_nudge(&[]).is_none());
        let a = item(
            WorkingMemoryKind::Decision,
            "use feature flags for rollouts",
            0,
        );
        let refs = vec![&a];
        let text = format_nudge(&refs).expect("section");
        assert!(text.starts_with("# Knowledge Promotion Check"));
        assert!(text.contains("[decision] use feature flags for rollouts"));
        assert!(text.contains("action=propose"));
    }

    #[test]
    fn gate_requires_both_flags() {
        // Default config: both flags off, so the gate must be None even with
        // plausible arguments. This protects flag-off byte-identical prompts.
        assert!(knowledge_nudge_prompt_section(Some("s"), Some(Path::new("C:/x"))).is_none());
    }

    #[test]
    fn nudge_marks_items_and_never_repeats() {
        // Exercise the mark/clear bookkeeping directly (flags stay off; the
        // full gate is covered by prompt-level tests).
        let session = format!("nudge_test_{}", rand::random::<u64>());
        let a = item(WorkingMemoryKind::Decision, "one time nudge decision", 0);
        with_nudged(|map| {
            let nudged = map.entry(session.clone()).or_default();
            let selected = select_candidates(
                std::slice::from_ref(&a),
                &ProjectKnowledge::default(),
                nudged,
            );
            assert_eq!(selected.len(), 1);
            nudged.insert(a.id.clone());
            assert!(
                select_candidates(
                    std::slice::from_ref(&a),
                    &ProjectKnowledge::default(),
                    nudged
                )
                .is_empty()
            );
        });
        clear_nudged(&session);
        with_nudged(|map| assert!(!map.contains_key(&session)));
    }
}
