//! Working (short-term) memory buffer.
//!
//! A small, fixed-capacity, per-session buffer holding what the agent is
//! actively working on: the current goal, the constraints it must respect, the
//! decisions it has made, and the threads still open.
//!
//! This is deliberately NOT the long-term memory graph, and the difference is
//! the point:
//!
//! | | Working memory (here) | Long-term memory (graph) |
//! |---|---|---|
//! | Lifetime | one session | until superseded/pruned |
//! | Injection | re-stated EVERY turn | once, then suppressed ~45min |
//! | Capacity | hard cap (default 7) | unbounded |
//! | Retrieval | always, no search | embedding + BM25 + LLM judge |
//! | Cost | per request | per retrieval |
//!
//! Because every slot is re-injected on every turn, capacity is a per-request
//! cost, which is why it is capped in config *and* clamped by an absolute
//! ceiling in `memory.rs`.
//!
//! Items that keep proving useful are "rehearsed"; rehearsal both protects an
//! item from eviction and is the signal that promotes it into long-term memory
//! (see P4). Items that are never rehearsed simply evaporate when the session
//! ends. That is the intended behavior of a short-term store, not a bug.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Mutex;

/// Kind of working-memory item. Purely descriptive: it groups items in the
/// injected prompt so the model can tell a hard constraint from a loose note.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum WorkingMemoryKind {
    /// What we are trying to achieve.
    Goal,
    /// A rule the work must respect (and that is expensive to violate).
    Constraint,
    /// Something established as true during this session.
    #[default]
    Fact,
    /// A choice that was made, so it is not silently revisited.
    Decision,
    /// A thread that is still unresolved.
    Open,
}

impl WorkingMemoryKind {
    /// Section heading used when rendering the buffer into a prompt.
    pub fn heading(self) -> &'static str {
        match self {
            Self::Goal => "Goals",
            Self::Constraint => "Constraints",
            Self::Fact => "Facts",
            Self::Decision => "Decisions",
            Self::Open => "Open",
        }
    }

    /// Order sections by how much they should steer the next action.
    fn rank(self) -> u8 {
        match self {
            Self::Goal => 0,
            Self::Constraint => 1,
            Self::Decision => 2,
            Self::Open => 3,
            Self::Fact => 4,
        }
    }

    pub fn parse(value: &str) -> Self {
        match value.trim().to_lowercase().as_str() {
            "goal" => Self::Goal,
            "constraint" => Self::Constraint,
            "decision" => Self::Decision,
            "open" | "question" => Self::Open,
            _ => Self::Fact,
        }
    }
}

impl std::fmt::Display for WorkingMemoryKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::Goal => "goal",
            Self::Constraint => "constraint",
            Self::Fact => "fact",
            Self::Decision => "decision",
            Self::Open => "open",
        };
        f.write_str(text)
    }
}

/// A single slot in the working-memory buffer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkingMemoryItem {
    pub id: String,
    pub content: String,
    #[serde(default)]
    pub kind: WorkingMemoryKind,
    pub created_at: DateTime<Utc>,
    /// How many times this item has been reinforced while resident.
    ///
    /// Doubles as the eviction shield and the promotion trigger: an item that
    /// keeps mattering survives pressure and eventually graduates to long-term
    /// memory.
    #[serde(default)]
    pub rehearsals: u32,
    #[serde(default)]
    pub last_rehearsed: Option<DateTime<Utc>>,
    /// Set when this item was pulled down from a long-term memory, so promotion
    /// can reinforce the original instead of creating a near-duplicate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_memory_id: Option<String>,
}

impl WorkingMemoryItem {
    fn new(content: String, kind: WorkingMemoryKind) -> Self {
        let now = Utc::now();
        let rand: u64 = rand::random();
        Self {
            id: format!("wm_{}_{}", now.timestamp_millis(), rand),
            content,
            kind,
            created_at: now,
            rehearsals: 0,
            last_rehearsed: None,
            source_memory_id: None,
        }
    }
}

/// In-memory buffers keyed by session id, mirroring how `pending.rs` keys its
/// global state so both stores have the same lifetime and isolation semantics.
static WORKING_MEMORY: Mutex<Option<HashMap<String, VecDeque<WorkingMemoryItem>>>> =
    Mutex::new(None);

fn with_buffers<T>(f: impl FnOnce(&mut HashMap<String, VecDeque<WorkingMemoryItem>>) -> T) -> T {
    let mut guard = WORKING_MEMORY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    f(map)
}

/// Truncate on a char boundary so multi-byte content cannot panic or produce
/// invalid UTF-8 when the cap lands mid-character.
fn truncate_content(content: &str, max_chars: usize) -> String {
    let trimmed = content.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }
    let mut out: String = trimmed.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Choose which slot to drop when the buffer is over capacity.
///
/// Least-rehearsed first, oldest breaking ties. Plain FIFO would evict the item
/// the agent has leaned on most simply because it arrived first, which is the
/// opposite of how rehearsal is supposed to work.
fn eviction_index(items: &VecDeque<WorkingMemoryItem>) -> Option<usize> {
    items
        .iter()
        .enumerate()
        .min_by(|(left_idx, left), (right_idx, right)| {
            left.rehearsals
                .cmp(&right.rehearsals)
                .then_with(|| left.created_at.cmp(&right.created_at))
                .then_with(|| left_idx.cmp(right_idx))
        })
        .map(|(idx, _)| idx)
}

/// Push an item into a session's working memory.
///
/// Returns the new item plus anything evicted to make room, so the caller (P4)
/// can decide whether the evicted item earned promotion to long-term memory.
pub fn push(
    session_id: &str,
    content: &str,
    kind: WorkingMemoryKind,
) -> (WorkingMemoryItem, Vec<WorkingMemoryItem>) {
    push_with_limits(
        session_id,
        content,
        kind,
        super::working_memory_capacity(),
        super::working_memory_item_chars(),
    )
}

/// Capacity/limit-injected form of [`push`], so tests can exercise eviction
/// without mutating global config.
pub fn push_with_limits(
    session_id: &str,
    content: &str,
    kind: WorkingMemoryKind,
    capacity: usize,
    item_chars: usize,
) -> (WorkingMemoryItem, Vec<WorkingMemoryItem>) {
    let capacity = capacity.max(1);
    let item = WorkingMemoryItem::new(truncate_content(content, item_chars), kind);

    let evicted = with_buffers(|map| {
        let buffer = map.entry(session_id.to_string()).or_default();
        buffer.push_back(item.clone());

        let mut evicted = Vec::new();
        while buffer.len() > capacity {
            let Some(idx) = eviction_index(buffer) else {
                break;
            };
            if let Some(dropped) = buffer.remove(idx) {
                evicted.push(dropped);
            }
        }
        evicted
    });

    (item, evicted)
}

/// Reinforce an item, protecting it from eviction and moving it toward
/// promotion. Returns the updated item, or `None` if the id is not resident.
pub fn rehearse(session_id: &str, item_id: &str) -> Option<WorkingMemoryItem> {
    with_buffers(|map| {
        let buffer = map.get_mut(session_id)?;
        let item = buffer.iter_mut().find(|item| item.id == item_id)?;
        item.rehearsals = item.rehearsals.saturating_add(1);
        item.last_rehearsed = Some(Utc::now());
        Some(item.clone())
    })
}

/// Snapshot a session's working memory in insertion order.
pub fn list(session_id: &str) -> Vec<WorkingMemoryItem> {
    with_buffers(|map| {
        map.get(session_id)
            .map(|buffer| buffer.iter().cloned().collect())
            .unwrap_or_default()
    })
}

/// Remove one item. Returns it when it was resident.
pub fn remove(session_id: &str, item_id: &str) -> Option<WorkingMemoryItem> {
    with_buffers(|map| {
        let buffer = map.get_mut(session_id)?;
        let idx = buffer.iter().position(|item| item.id == item_id)?;
        buffer.remove(idx)
    })
}

/// Drop a session's entire buffer, returning whatever it held so the caller can
/// run end-of-session promotion before the contents are lost.
pub fn clear(session_id: &str) -> Vec<WorkingMemoryItem> {
    with_buffers(|map| {
        map.remove(session_id)
            .map(|buffer| buffer.into_iter().collect())
            .unwrap_or_default()
    })
}

/// Drop every session's buffer. Used by agent reset and tests.
pub fn clear_all() {
    with_buffers(|map| map.clear());
}

/// True when the session has nothing in working memory.
pub fn is_empty(session_id: &str) -> bool {
    with_buffers(|map| map.get(session_id).is_none_or(|buffer| buffer.is_empty()))
}

/// Restore a session's buffer, e.g. after resuming a persisted session.
/// Trims to capacity using the same eviction rule as `push`.
pub fn restore(session_id: &str, items: Vec<WorkingMemoryItem>) {
    let capacity = super::working_memory_capacity();
    with_buffers(|map| {
        let mut buffer: VecDeque<WorkingMemoryItem> = items.into_iter().collect();
        while buffer.len() > capacity {
            let Some(idx) = eviction_index(&buffer) else {
                break;
            };
            buffer.remove(idx);
        }
        if buffer.is_empty() {
            map.remove(session_id);
        } else {
            map.insert(session_id.to_string(), buffer);
        }
    });
}

/// Render a session's working memory as a markdown block for injection.
///
/// Returns `None` when there is nothing to say, so callers can skip the section
/// entirely rather than injecting an empty header.
pub fn format_for_prompt(session_id: &str) -> Option<String> {
    format_items_for_prompt(&list(session_id))
}

/// Pure formatter, separated from the global store so it is directly testable.
pub fn format_items_for_prompt(items: &[WorkingMemoryItem]) -> Option<String> {
    if items.is_empty() {
        return None;
    }

    let mut ordered: Vec<&WorkingMemoryItem> = items.iter().collect();
    // Stable sort keeps insertion order within a section while grouping the
    // most action-guiding kinds first.
    ordered.sort_by_key(|item| item.kind.rank());

    let mut output = String::from("# Working Memory\n");
    let mut current: Option<WorkingMemoryKind> = None;
    let mut index = 0usize;

    for item in ordered {
        if current != Some(item.kind) {
            output.push_str(&format!("\n## {}\n", item.kind.heading()));
            current = Some(item.kind);
            index = 0;
        }
        index += 1;
        output.push_str(&format!("{}. {}\n", index, item.content));
    }

    Some(output.trim_end().to_string())
}

// === Persistence ===
//
// Working memory is a cache, never a correctness dependency: every operation
// below degrades to a log line on failure rather than propagating an error into
// the turn. Losing the buffer costs the session some context; blocking a turn on
// a disk hiccup would cost the user their work.

/// On-disk shape. Versioned so a future format change can be detected rather
/// than silently misparsed.
#[derive(Debug, Serialize, Deserialize)]
struct WorkingMemoryFile {
    #[serde(default = "default_file_version")]
    version: u32,
    #[serde(default)]
    items: Vec<WorkingMemoryItem>,
}

fn default_file_version() -> u32 {
    1
}

const WORKING_MEMORY_FILE_VERSION: u32 = 1;

/// Path for a session's buffer. Lives in its own directory that older builds
/// never read, so downgrading is a no-op rather than a parse error.
fn working_memory_path(session_id: &str) -> anyhow::Result<std::path::PathBuf> {
    // Session ids come from our own generator, but they end up in a filename, so
    // reject anything that could escape the directory.
    let safe: String = session_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let dir = crate::storage::jcode_dir()?.join("memory").join("working");
    Ok(dir.join(format!("{safe}.json")))
}

/// Persist a session's buffer. Best-effort by design.
pub fn save_working_memory(session_id: &str) {
    let items = list(session_id);
    let Ok(path) = working_memory_path(session_id) else {
        return;
    };

    if items.is_empty() {
        // Nothing to remember: drop the file instead of leaving a stale one that
        // would resurrect cleared items on resume.
        let _ = std::fs::remove_file(&path);
        return;
    }

    let file = WorkingMemoryFile {
        version: WORKING_MEMORY_FILE_VERSION,
        items,
    };
    if let Err(err) = crate::storage::write_json(&path, &file) {
        crate::logging::info(&format!(
            "Failed to persist working memory for session {session_id}: {err}"
        ));
    }
}

/// Load a session's buffer from disk into the in-memory store.
/// Returns the number of items restored.
pub fn load_working_memory(session_id: &str) -> usize {
    let Ok(path) = working_memory_path(session_id) else {
        return 0;
    };
    if !path.exists() {
        return 0;
    }

    match crate::storage::read_json::<WorkingMemoryFile>(&path) {
        Ok(file) if file.version == WORKING_MEMORY_FILE_VERSION => {
            let count = file.items.len();
            restore(session_id, file.items);
            list(session_id).len().min(count)
        }
        Ok(file) => {
            crate::logging::info(&format!(
                "Ignoring working memory for session {session_id}: unsupported version {}",
                file.version
            ));
            0
        }
        Err(err) => {
            crate::logging::info(&format!(
                "Failed to load working memory for session {session_id}: {err}"
            ));
            0
        }
    }
}

/// Delete a session's persisted buffer.
pub fn delete_working_memory_file(session_id: &str) {
    if let Ok(path) = working_memory_path(session_id) {
        let _ = std::fs::remove_file(path);
    }
}

// === Prefixed aliases ===
//
// `memory.rs` re-exports a flat API, so these carry the `working_memory`
// qualifier that the short module-local names omit.

/// See [`push`].
pub fn push_working_memory(
    session_id: &str,
    content: &str,
    kind: WorkingMemoryKind,
) -> (WorkingMemoryItem, Vec<WorkingMemoryItem>) {
    push(session_id, content, kind)
}

/// See [`rehearse`].
pub fn rehearse_working_memory(session_id: &str, item_id: &str) -> Option<WorkingMemoryItem> {
    rehearse(session_id, item_id)
}

/// See [`list`].
pub fn list_working_memory(session_id: &str) -> Vec<WorkingMemoryItem> {
    list(session_id)
}

/// See [`remove`].
pub fn remove_working_memory(session_id: &str, item_id: &str) -> Option<WorkingMemoryItem> {
    remove(session_id, item_id)
}

/// See [`clear`].
pub fn clear_working_memory(session_id: &str) -> Vec<WorkingMemoryItem> {
    clear(session_id)
}

/// See [`clear_all`].
pub fn clear_all_working_memory() {
    clear_all();
}

/// See [`is_empty`].
pub fn working_memory_is_empty(session_id: &str) -> bool {
    is_empty(session_id)
}

/// See [`restore`].
pub fn restore_working_memory(session_id: &str, items: Vec<WorkingMemoryItem>) {
    restore(session_id, items);
}

/// See [`format_for_prompt`].
pub fn format_working_memory_for_prompt(session_id: &str) -> Option<String> {
    format_for_prompt(session_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes tests that touch the process-global buffer.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn guard() -> std::sync::MutexGuard<'static, ()> {
        let g = TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        clear_all();
        g
    }

    #[test]
    fn push_and_list_round_trip() {
        let _g = guard();
        push_with_limits(
            "s1",
            "ship the memory work",
            WorkingMemoryKind::Goal,
            7,
            240,
        );
        let items = list("s1");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].content, "ship the memory work");
        assert_eq!(items[0].kind, WorkingMemoryKind::Goal);
        assert_eq!(items[0].rehearsals, 0);
    }

    #[test]
    fn capacity_is_never_exceeded() {
        let _g = guard();
        for i in 0..50 {
            push_with_limits("s1", &format!("item {i}"), WorkingMemoryKind::Fact, 7, 240);
            assert!(
                list("s1").len() <= 7,
                "capacity exceeded after {} pushes",
                i + 1
            );
        }
        assert_eq!(list("s1").len(), 7);
    }

    #[test]
    fn eviction_returns_the_dropped_item() {
        let _g = guard();
        for i in 0..3 {
            push_with_limits("s1", &format!("item {i}"), WorkingMemoryKind::Fact, 3, 240);
        }
        let (_, evicted) = push_with_limits("s1", "item 3", WorkingMemoryKind::Fact, 3, 240);
        assert_eq!(evicted.len(), 1);
        assert_eq!(evicted[0].content, "item 0");
    }

    /// The core guarantee: rehearsal protects. An item the agent keeps leaning
    /// on must not be evicted just because it arrived first.
    #[test]
    fn rehearsed_items_survive_eviction_pressure() {
        let _g = guard();
        let (first, _) = push_with_limits("s1", "critical", WorkingMemoryKind::Goal, 3, 240);
        rehearse("s1", &first.id);
        rehearse("s1", &first.id);

        for i in 0..10 {
            push_with_limits(
                "s1",
                &format!("filler {i}"),
                WorkingMemoryKind::Fact,
                3,
                240,
            );
        }

        let items = list("s1");
        assert_eq!(items.len(), 3);
        assert!(
            items.iter().any(|item| item.id == first.id),
            "rehearsed item was evicted despite pressure: {:?}",
            items.iter().map(|i| &i.content).collect::<Vec<_>>()
        );
    }

    #[test]
    fn least_rehearsed_is_evicted_first() {
        let _g = guard();
        let (a, _) = push_with_limits("s1", "a", WorkingMemoryKind::Fact, 3, 240);
        let (b, _) = push_with_limits("s1", "b", WorkingMemoryKind::Fact, 3, 240);
        let (c, _) = push_with_limits("s1", "c", WorkingMemoryKind::Fact, 3, 240);
        rehearse("s1", &a.id);
        rehearse("s1", &c.id);

        // b is the only un-rehearsed item, so it must be the one to go.
        let (_, evicted) = push_with_limits("s1", "d", WorkingMemoryKind::Fact, 3, 240);
        assert_eq!(evicted.len(), 1);
        assert_eq!(evicted[0].id, b.id);
    }

    #[test]
    fn rehearse_increments_and_stamps() {
        let _g = guard();
        let (item, _) = push_with_limits("s1", "x", WorkingMemoryKind::Fact, 7, 240);
        assert!(item.last_rehearsed.is_none());
        let updated = rehearse("s1", &item.id).expect("item should be resident");
        assert_eq!(updated.rehearsals, 1);
        assert!(updated.last_rehearsed.is_some());
        assert!(rehearse("s1", "wm_missing").is_none());
    }

    #[test]
    fn sessions_are_isolated() {
        let _g = guard();
        push_with_limits("s1", "session one", WorkingMemoryKind::Fact, 7, 240);
        push_with_limits("s2", "session two", WorkingMemoryKind::Fact, 7, 240);

        assert_eq!(list("s1").len(), 1);
        assert_eq!(list("s2").len(), 1);
        assert_eq!(list("s1")[0].content, "session one");
        assert_eq!(list("s2")[0].content, "session two");

        clear("s1");
        assert!(is_empty("s1"));
        assert_eq!(
            list("s2").len(),
            1,
            "clearing one session must not touch another"
        );
    }

    #[test]
    fn content_is_truncated_to_the_cap() {
        let _g = guard();
        let long = "x".repeat(500);
        let (item, _) = push_with_limits("s1", &long, WorkingMemoryKind::Fact, 7, 100);
        assert_eq!(item.content.chars().count(), 100);
        assert!(item.content.ends_with('…'));
    }

    /// Truncation must not panic or corrupt multi-byte text.
    #[test]
    fn truncation_respects_char_boundaries() {
        let _g = guard();
        let emoji = "🧠".repeat(50);
        let (item, _) = push_with_limits("s1", &emoji, WorkingMemoryKind::Fact, 7, 10);
        assert_eq!(item.content.chars().count(), 10);
        assert!(item.content.starts_with('🧠'));
    }

    #[test]
    fn remove_and_clear_behave() {
        let _g = guard();
        let (item, _) = push_with_limits("s1", "gone soon", WorkingMemoryKind::Fact, 7, 240);
        assert!(remove("s1", &item.id).is_some());
        assert!(remove("s1", &item.id).is_none());
        assert!(is_empty("s1"));

        push_with_limits("s1", "a", WorkingMemoryKind::Fact, 7, 240);
        push_with_limits("s1", "b", WorkingMemoryKind::Fact, 7, 240);
        let drained = clear("s1");
        assert_eq!(drained.len(), 2, "clear must return contents for promotion");
        assert!(is_empty("s1"));
    }

    #[test]
    fn format_groups_by_kind_with_goals_first() {
        let _g = guard();
        push_with_limits("s1", "a fact", WorkingMemoryKind::Fact, 7, 240);
        push_with_limits("s1", "the goal", WorkingMemoryKind::Goal, 7, 240);
        push_with_limits("s1", "a rule", WorkingMemoryKind::Constraint, 7, 240);

        let rendered = format_for_prompt("s1").expect("non-empty buffer should render");
        assert!(rendered.starts_with("# Working Memory"));

        let goal_at = rendered.find("## Goals").expect("goals section");
        let constraint_at = rendered
            .find("## Constraints")
            .expect("constraints section");
        let fact_at = rendered.find("## Facts").expect("facts section");
        assert!(
            goal_at < constraint_at && constraint_at < fact_at,
            "sections must be ordered by how much they steer the next action:\n{rendered}"
        );
        assert!(rendered.contains("1. the goal"));
    }

    #[test]
    fn format_is_none_when_empty() {
        let _g = guard();
        assert!(format_for_prompt("nobody").is_none());
        assert!(format_items_for_prompt(&[]).is_none());
    }

    #[test]
    fn restore_trims_to_capacity() {
        let _g = guard();
        let items: Vec<WorkingMemoryItem> = (0..100)
            .map(|i| WorkingMemoryItem::new(format!("item {i}"), WorkingMemoryKind::Fact))
            .collect();
        restore("s1", items);
        assert!(
            list("s1").len() <= super::super::working_memory_capacity(),
            "restore must respect capacity so a tampered file cannot bloat the prompt"
        );
    }

    #[test]
    fn item_serialization_round_trips() {
        let _g = guard();
        let (item, _) = push_with_limits("s1", "persist me", WorkingMemoryKind::Decision, 7, 240);
        let json = serde_json::to_string(&item).expect("serialize");
        let back: WorkingMemoryItem = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.id, item.id);
        assert_eq!(back.content, "persist me");
        assert_eq!(back.kind, WorkingMemoryKind::Decision);
    }

    #[test]
    fn persistence_round_trips_through_disk() {
        let _g = guard();
        let _env = crate::storage::lock_test_env();
        let home = tempfile::tempdir().expect("home");
        let prev = std::env::var_os("JCODE_HOME");
        crate::env::set_var("JCODE_HOME", home.path());

        push_with_limits("persist-me", "the goal", WorkingMemoryKind::Goal, 7, 240);
        let (item, _) = push_with_limits(
            "persist-me",
            "a rule",
            WorkingMemoryKind::Constraint,
            7,
            240,
        );
        rehearse("persist-me", &item.id);
        save_working_memory("persist-me");

        // Drop the in-memory copy so the reload is genuinely reading disk.
        clear_all();
        assert!(is_empty("persist-me"));

        let restored = load_working_memory("persist-me");
        assert_eq!(restored, 2);
        let items = list("persist-me");
        assert_eq!(items.len(), 2);
        assert!(items.iter().any(|i| i.content == "the goal"));
        let rule = items
            .iter()
            .find(|i| i.content == "a rule")
            .expect("constraint should survive the round trip");
        assert_eq!(rule.rehearsals, 1, "rehearsal count must persist");

        // Clearing then saving must remove the file, not resurrect old items.
        clear("persist-me");
        save_working_memory("persist-me");
        clear_all();
        assert_eq!(load_working_memory("persist-me"), 0);

        match prev {
            Some(v) => crate::env::set_var("JCODE_HOME", v),
            None => crate::env::remove_var("JCODE_HOME"),
        }
    }

    /// A session id with path separators must not escape the working-memory
    /// directory.
    #[test]
    fn session_ids_cannot_escape_the_directory() {
        let path = working_memory_path("../../evil/../../etc/passwd").expect("path");
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .expect("file name");
        assert!(
            !name.contains(".."),
            "sanitized name still traverses: {name}"
        );
        assert!(path.ends_with(name));
        assert!(
            path.parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                == Some("working"),
            "file must stay inside memory/working: {}",
            path.display()
        );
    }

    /// State-space sweep over operation sequences: whatever order pushes,
    /// rehearsals and removals arrive in, the buffer must stay within capacity
    /// and never evict a strictly-most-rehearsed item while a weaker one
    /// remains.
    #[test]
    fn invariants_hold_across_operation_sequences() {
        let _g = guard();
        for capacity in 1..=5usize {
            for seed in 0..40u64 {
                clear_all();
                let mut ids: Vec<String> = Vec::new();
                let mut state = seed.wrapping_mul(6364136223846793005).wrapping_add(1);

                for step in 0..30 {
                    state = state
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(1442695040888963407);
                    let op = (state >> 33) % 3;
                    match op {
                        0 => {
                            let (item, _) = push_with_limits(
                                "sweep",
                                &format!("item {step}"),
                                WorkingMemoryKind::Fact,
                                capacity,
                                240,
                            );
                            ids.push(item.id);
                        }
                        1 => {
                            if !ids.is_empty() {
                                let idx = ((state >> 17) as usize) % ids.len();
                                rehearse("sweep", &ids[idx]);
                            }
                        }
                        _ => {
                            if !ids.is_empty() {
                                let idx = ((state >> 11) as usize) % ids.len();
                                remove("sweep", &ids[idx]);
                            }
                        }
                    }

                    let items = list("sweep");
                    assert!(
                        items.len() <= capacity,
                        "capacity {capacity} exceeded at step {step} (seed {seed})"
                    );
                }
            }
        }
    }
}
