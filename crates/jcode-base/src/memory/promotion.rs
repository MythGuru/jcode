//! Promotion and demotion between working (short-term) and long-term memory.
//!
//! Working memory (`working.rs`) is a small per-session buffer whose items
//! evaporate when the session ends. The bridge to durability is rehearsal: an
//! item the agent keeps leaning on earns its way into the long-term graph.
//!
//! Promotion always routes through the existing `remember_*` path on
//! [`MemoryManager`], so storage-layer dedup, embedding generation, and graph
//! edges come for free instead of being re-implemented here.
//!
//! The reverse direction also lives here: judge-verified long-term hits can be
//! "activated" into free working-memory slots, which makes them part of the
//! every-turn context instead of a one-off injection. Activation never evicts:
//! a retrieved memory must not displace something the agent explicitly noted.

use super::MemoryManager;
use super::working::{self, WorkingMemoryItem, WorkingMemoryKind};
use crate::memory_types::{MemoryCategory, MemoryEntry};
use anyhow::Result;

/// Rehearsals at which a resident item is promoted to long-term memory.
pub const REHEARSAL_PROMOTION_THRESHOLD: u32 = 3;

/// Rehearsals at which an item leaving the buffer (eviction or session end)
/// still earns promotion. Lower than the resident threshold: the item proved
/// useful more than once and is about to be lost forever, so the bar drops.
pub const EXIT_PROMOTION_THRESHOLD: u32 = 2;

/// Importance floor below which ambient pruning may still remove an entry.
/// Entries at or above this are never pruned (see `prune_low_confidence`).
pub const PRUNE_PROTECTION_IMPORTANCE: f32 = 0.8;

/// Importance seeded on a freshly promoted entry.
///
/// More rehearsals mean the item mattered more, but the ceiling stays below
/// the explicit-user-request range (0.9+) so automatic promotion can never
/// outrank something the user deliberately marked important.
pub fn seed_importance(rehearsals: u32) -> f32 {
    (0.5 + 0.1 * rehearsals as f32).clamp(0.5, 0.9)
}

/// What promotion did with an item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromotionOutcome {
    /// A new long-term memory was created with this id.
    Created(String),
    /// An existing long-term memory (the item's source) was reinforced.
    Reinforced(String),
}

impl PromotionOutcome {
    pub fn memory_id(&self) -> &str {
        match self {
            Self::Created(id) | Self::Reinforced(id) => id,
        }
    }
}

fn kind_tag(kind: WorkingMemoryKind) -> String {
    format!("stm-{kind}")
}

/// Reinforce an already-promoted memory instead of creating a near-duplicate.
/// Returns false when the source id no longer exists in either graph (it may
/// have been pruned or forgotten), in which case the caller creates a new one.
fn reinforce_existing(manager: &MemoryManager, memory_id: &str) -> Result<bool> {
    let mut project = manager.load_project_graph()?;
    if let Some(entry) = project.get_memory_mut(memory_id) {
        entry.reinforce("working_memory_promotion", 0);
        entry.adjust_importance(0.05);
        entry.mark_promoted();
        manager.save_project_graph(&project)?;
        return Ok(true);
    }

    let mut global = manager.load_global_graph()?;
    if let Some(entry) = global.get_memory_mut(memory_id) {
        entry.reinforce("working_memory_promotion", 0);
        entry.adjust_importance(0.05);
        entry.mark_promoted();
        manager.save_global_graph(&global)?;
        return Ok(true);
    }

    Ok(false)
}

/// Promote one working-memory item into long-term memory.
///
/// If the item was originally activated FROM a long-term memory, that original
/// is reinforced rather than duplicated. Otherwise a new entry is created via
/// the standard `remember_*` path (project scope when a project graph exists,
/// global otherwise), seeded with rehearsal-derived importance.
pub fn promote_item(manager: &MemoryManager, item: &WorkingMemoryItem) -> Result<PromotionOutcome> {
    if let Some(source_id) = item.source_memory_id.as_deref()
        && reinforce_existing(manager, source_id)?
    {
        return Ok(PromotionOutcome::Reinforced(source_id.to_string()));
    }

    let mut entry = MemoryEntry::new(MemoryCategory::Fact, item.content.clone())
        .with_source("working_memory")
        .with_tags(vec!["stm-promoted".to_string(), kind_tag(item.kind)]);
    entry.set_importance(seed_importance(item.rehearsals));
    entry.mark_promoted();

    let id = if manager.project_memory_path()?.is_some() {
        manager.remember_project(entry)?
    } else {
        manager.remember_global(entry)?
    };
    Ok(PromotionOutcome::Created(id))
}

/// Rehearse a resident item and promote it once it crosses the threshold.
///
/// After a successful promotion the item stays resident but records the
/// long-term id it became, so further rehearsals reinforce that memory instead
/// of minting duplicates. Returns the updated item and, when promotion
/// happened this call, the outcome.
pub fn rehearse_with_promotion(
    manager: &MemoryManager,
    session_id: &str,
    item_id: &str,
) -> Option<(WorkingMemoryItem, Option<PromotionOutcome>)> {
    let item = working::rehearse(session_id, item_id)?;
    if item.rehearsals < REHEARSAL_PROMOTION_THRESHOLD {
        return Some((item, None));
    }

    match promote_item(manager, &item) {
        Ok(outcome) => {
            working::set_source_memory_id(session_id, item_id, outcome.memory_id());
            let updated = working::list(session_id)
                .into_iter()
                .find(|candidate| candidate.id == item_id)
                .unwrap_or_else(|| item.clone());
            Some((updated, Some(outcome)))
        }
        Err(err) => {
            crate::logging::info(&format!(
                "Working-memory promotion failed for {item_id}: {err}"
            ));
            Some((item, None))
        }
    }
}

/// Promote items that are leaving the buffer (evicted or drained) when they
/// earned it. Returns how many were promoted. Failures are logged, not
/// propagated: losing a promotion must never break the operation that caused
/// the exit.
pub fn promote_exiting_items(manager: &MemoryManager, items: &[WorkingMemoryItem]) -> usize {
    let mut promoted = 0usize;
    for item in items {
        if item.rehearsals < EXIT_PROMOTION_THRESHOLD {
            continue;
        }
        match promote_item(manager, item) {
            Ok(_) => promoted += 1,
            Err(err) => {
                crate::logging::info(&format!(
                    "Working-memory exit promotion failed for {}: {err}",
                    item.id
                ));
            }
        }
    }
    promoted
}

/// End-of-session sweep: drain the session's buffer, promote what earned it,
/// and delete the persisted file so a finished session leaves nothing behind.
/// Returns how many items were promoted.
pub fn promote_on_session_end(manager: &MemoryManager, session_id: &str) -> usize {
    let drained = working::clear(session_id);
    working::delete_working_memory_file(session_id);
    if drained.is_empty() {
        return 0;
    }
    promote_exiting_items(manager, &drained)
}

/// Activate judge-verified long-term memories into FREE working-memory slots.
///
/// Once resident, a memory is re-stated every turn instead of being injected
/// once and suppressed, which is the right behavior for something the judge
/// decided is relevant to the active work. Rules:
/// - never evicts: only fills slots up to capacity,
/// - skips memories already resident (matched by source id),
/// - no-op unless the working-memory flag is on.
///
/// Returns how many memories were activated.
pub fn activate_memories(session_id: &str, entries: &[MemoryEntry]) -> usize {
    if !super::working_memory_enabled() {
        return 0;
    }
    activate_memories_with_limits(
        session_id,
        entries,
        super::working_memory_capacity(),
        super::working_memory_item_chars(),
    )
}

/// Capacity-injected form of [`activate_memories`] for tests, mirroring
/// `working::push_with_limits`.
pub fn activate_memories_with_limits(
    session_id: &str,
    entries: &[MemoryEntry],
    capacity: usize,
    item_chars: usize,
) -> usize {
    let resident = working::list(session_id);
    let mut free = capacity.saturating_sub(resident.len());
    if free == 0 {
        return 0;
    }

    let resident_sources: std::collections::HashSet<&str> = resident
        .iter()
        .filter_map(|item| item.source_memory_id.as_deref())
        .collect();

    let mut activated = 0usize;
    for entry in entries {
        if free == 0 {
            break;
        }
        if resident_sources.contains(entry.id.as_str()) {
            continue;
        }

        let kind = match entry.category {
            MemoryCategory::Correction => WorkingMemoryKind::Constraint,
            _ => WorkingMemoryKind::Fact,
        };
        let (item, evicted) =
            working::push_with_limits(session_id, &entry.content, kind, capacity, item_chars);
        debug_assert!(evicted.is_empty(), "activation must never evict");
        working::set_source_memory_id(session_id, &item.id, &entry.id);
        free -= 1;
        activated += 1;
    }
    activated
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serializes tests that touch the process-global working-memory buffers.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    struct TestHome {
        _home: tempfile::TempDir,
        _env: std::sync::MutexGuard<'static, ()>,
        _buffers: std::sync::MutexGuard<'static, ()>,
        prev: Option<std::ffi::OsString>,
    }

    fn setup() -> (MemoryManager, TestHome) {
        let buffers = TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        working::clear_all();
        let env = crate::storage::lock_test_env();
        let home = tempfile::tempdir().expect("home");
        let prev = std::env::var_os("JCODE_HOME");
        crate::env::set_var("JCODE_HOME", home.path());
        let manager = MemoryManager::new_test();
        manager.clear_test_storage().ok();
        (
            manager,
            TestHome {
                _home: home,
                _env: env,
                _buffers: buffers,
                prev,
            },
        )
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            working::clear_all();
            match self.prev.take() {
                Some(v) => crate::env::set_var("JCODE_HOME", v),
                None => crate::env::remove_var("JCODE_HOME"),
            }
        }
    }

    fn item_with_rehearsals(content: &str, rehearsals: u32) -> WorkingMemoryItem {
        let (mut item, _) =
            working::push_with_limits("promo-test", content, WorkingMemoryKind::Fact, 7, 240);
        item.rehearsals = rehearsals;
        working::remove("promo-test", &item.id);
        item
    }

    #[test]
    fn seed_importance_is_bounded_and_monotone() {
        assert_eq!(seed_importance(0), 0.5);
        let mut prev = 0.0f32;
        for rehearsals in 0..20 {
            let score = seed_importance(rehearsals);
            assert!((0.5..=0.9).contains(&score), "out of bounds: {score}");
            assert!(score >= prev, "importance must not decrease");
            prev = score;
        }
        assert_eq!(seed_importance(10), 0.9, "must cap below explicit range");
    }

    #[test]
    fn promote_creates_entry_with_seeded_importance() {
        let (manager, _home) = setup();
        let item = item_with_rehearsals("use tokio for async", 3);

        let outcome = promote_item(&manager, &item).expect("promotion");
        let PromotionOutcome::Created(id) = outcome else {
            panic!("fresh item must create, got {outcome:?}");
        };

        let all = manager.list_all().expect("list");
        let entry = all.iter().find(|e| e.id == id).expect("promoted entry");
        assert_eq!(entry.content, "use tokio for async");
        assert!(
            (entry.importance - 0.8).abs() < 1e-6,
            "seeded from 3 rehearsals"
        );
        assert!(entry.promoted_at.is_some());
        assert!(entry.tags.iter().any(|t| t == "stm-promoted"));
    }

    #[test]
    fn promote_reinforces_original_instead_of_duplicating() {
        let (manager, _home) = setup();
        let item = item_with_rehearsals("the API rate limit is 100/min", 3);

        let first = promote_item(&manager, &item).expect("first promotion");
        let mut promoted_twice = item.clone();
        promoted_twice.source_memory_id = Some(first.memory_id().to_string());

        let second = promote_item(&manager, &promoted_twice).expect("second promotion");
        assert_eq!(
            second,
            PromotionOutcome::Reinforced(first.memory_id().to_string())
        );

        let count = manager
            .list_all()
            .expect("list")
            .iter()
            .filter(|e| e.content == "the API rate limit is 100/min")
            .count();
        assert_eq!(count, 1, "re-promotion must not duplicate");
    }

    #[test]
    fn promote_falls_back_to_create_when_source_is_gone() {
        let (manager, _home) = setup();
        let mut item = item_with_rehearsals("orphaned but valuable", 3);
        item.source_memory_id = Some("mem_pruned_away".to_string());

        let outcome = promote_item(&manager, &item).expect("promotion");
        assert!(
            matches!(outcome, PromotionOutcome::Created(_)),
            "missing source must fall back to creation, got {outcome:?}"
        );
    }

    #[test]
    fn rehearsal_promotes_at_threshold_and_records_source() {
        let (manager, _home) = setup();
        let session = "rehearse-promote";
        let (item, _) =
            working::push_with_limits(session, "keep this", WorkingMemoryKind::Goal, 7, 240);

        let (_, outcome) =
            rehearse_with_promotion(&manager, session, &item.id).expect("rehearse 1");
        assert!(outcome.is_none(), "below threshold");
        let (_, outcome) =
            rehearse_with_promotion(&manager, session, &item.id).expect("rehearse 2");
        assert!(outcome.is_none(), "below threshold");

        let (updated, outcome) =
            rehearse_with_promotion(&manager, session, &item.id).expect("rehearse 3");
        let outcome = outcome.expect("threshold crossed");
        assert!(matches!(outcome, PromotionOutcome::Created(_)));
        assert_eq!(
            updated.source_memory_id.as_deref(),
            Some(outcome.memory_id()),
            "item must remember what it became"
        );

        // A fourth rehearsal reinforces the promoted memory, not a duplicate.
        let (_, outcome) =
            rehearse_with_promotion(&manager, session, &item.id).expect("rehearse 4");
        assert!(matches!(
            outcome.expect("still above threshold"),
            PromotionOutcome::Reinforced(_)
        ));
        assert_eq!(manager.list_all().expect("list").len(), 1);
    }

    #[test]
    fn exit_promotion_requires_two_rehearsals() {
        let (manager, _home) = setup();
        let earned = item_with_rehearsals("rehearsed enough", 2);
        let unearned = item_with_rehearsals("never rehearsed", 0);
        let once = item_with_rehearsals("rehearsed once", 1);

        let promoted = promote_exiting_items(&manager, &[earned, unearned, once]);
        assert_eq!(promoted, 1);

        let all = manager.list_all().expect("list");
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].content, "rehearsed enough");
    }

    #[test]
    fn session_end_drains_promotes_and_cleans_up() {
        let (manager, _home) = setup();
        let session = "ending-session";
        let (keeper, _) =
            working::push_with_limits(session, "hard-won lesson", WorkingMemoryKind::Fact, 7, 240);
        working::rehearse(session, &keeper.id);
        working::rehearse(session, &keeper.id);
        working::push_with_limits(session, "ephemeral note", WorkingMemoryKind::Fact, 7, 240);
        working::save_working_memory(session);

        let promoted = promote_on_session_end(&manager, session);
        assert_eq!(promoted, 1);
        assert!(working::is_empty(session), "buffer must be drained");
        assert_eq!(
            working::load_working_memory(session),
            0,
            "persisted file must be gone"
        );

        let all = manager.list_all().expect("list");
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].content, "hard-won lesson");
    }

    #[test]
    fn activation_fills_free_slots_without_evicting() {
        let (_manager, _home) = setup();
        let session = "activate";
        working::push_with_limits(session, "already noted", WorkingMemoryKind::Goal, 3, 240);

        let entries: Vec<MemoryEntry> = (0..5)
            .map(|i| MemoryEntry::new(MemoryCategory::Fact, format!("ltm hit {i}")))
            .collect();

        let activated = activate_memories_with_limits(session, &entries, 3, 240);
        assert_eq!(activated, 2, "only the free slots may be filled");

        let items = working::list(session);
        assert_eq!(items.len(), 3);
        assert!(
            items.iter().any(|i| i.content == "already noted"),
            "activation must never evict what the agent noted"
        );
        let sourced = items
            .iter()
            .filter(|i| i.source_memory_id.is_some())
            .count();
        assert_eq!(sourced, 2, "activated items must record their source");
    }

    #[test]
    fn activation_skips_already_resident_memories() {
        let (_manager, _home) = setup();
        let session = "activate-dedup";
        let entry = MemoryEntry::new(MemoryCategory::Fact, "prefers rebase over merge");

        assert_eq!(
            activate_memories_with_limits(session, std::slice::from_ref(&entry), 7, 240),
            1
        );
        assert_eq!(
            activate_memories_with_limits(session, &[entry], 7, 240),
            0,
            "the same memory must not be activated twice"
        );
        assert_eq!(working::list(session).len(), 1);
    }

    #[test]
    fn activation_maps_corrections_to_constraints() {
        let (_manager, _home) = setup();
        let session = "activate-kinds";
        let correction = MemoryEntry::new(MemoryCategory::Correction, "never push to main");

        activate_memories_with_limits(session, &[correction], 7, 240);
        let items = working::list(session);
        assert_eq!(items[0].kind, WorkingMemoryKind::Constraint);
    }

    #[test]
    fn activation_is_gated_by_the_flag() {
        let (_manager, _home) = setup();
        // Default config leaves working memory disabled, so the public
        // entrypoint must refuse even with free slots available.
        let entry = MemoryEntry::new(MemoryCategory::Fact, "should not appear");
        assert_eq!(activate_memories("flag-off", &[entry]), 0);
        assert!(working::is_empty("flag-off"));
    }
}
