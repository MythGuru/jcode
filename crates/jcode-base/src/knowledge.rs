//! Verification-gated living project knowledge model (K1: data model + storage).
//!
//! A readable, per-project map of what jcode has learned about the codebase:
//! how it is structured, the decisions that shaped it, the rules work must
//! respect, the problems everyone keeps tripping over, and which components
//! are responsible for what.
//!
//! The defining property is the verification gate: an entry starts as
//! `Proposed` and may only become `Verified` when a verification event (build
//! and tests passing, or explicit user confirmation) backs it. This module
//! holds the data model and storage only; the gate logic that consumes
//! verification events arrives in K2 and calls [`ProjectKnowledge::mark_verified`].
//!
//! Storage follows the working-memory precedent deliberately:
//! - versioned JSON file, serde defaults, so old files load and unknown
//!   versions are ignored rather than misparsed,
//! - lives under `~/.jcode/knowledge/projects/<hash>.json`, a directory old
//!   binaries never read, so downgrading is a no-op,
//! - best-effort persistence: failures degrade to a log line, never an error
//!   that could block a turn,
//! - a rendered `<hash>.md` sibling is written on every save so the user can
//!   always open a plain readable map.

pub mod bridge;
pub mod verification;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Whether the project knowledge model is active. Read live (not cached) so
/// toggling the flag takes effect without a restart, like the STM flags.
pub fn project_knowledge_enabled() -> bool {
    crate::config::config().agents.project_knowledge_enabled
}

/// Character budget for the rendered prompt section (used in K5). Clamped so a
/// misconfigured value can neither erase the section nor flood the prompt.
pub fn project_knowledge_max_chars() -> usize {
    crate::config::config()
        .agents
        .project_knowledge_max_chars
        .clamp(256, 16_000)
}

/// Which part of the map an entry belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum KnowledgeSection {
    /// How the project is laid out (crates, layers, key directories).
    Structure,
    /// A decision that was made and should not be silently revisited.
    Decision,
    /// A rule work in this project must respect.
    Rule,
    /// A known problem or recurring pitfall.
    Problem,
    /// Which component is responsible for what.
    #[default]
    Responsibility,
}

impl KnowledgeSection {
    /// Section heading used when rendering the map.
    pub fn heading(self) -> &'static str {
        match self {
            Self::Structure => "Structure",
            Self::Decision => "Decisions",
            Self::Rule => "Rules",
            Self::Problem => "Known Problems",
            Self::Responsibility => "Responsibilities",
        }
    }

    /// Render order: orientation first, then the constraints that steer work,
    /// then the details.
    fn rank(self) -> u8 {
        match self {
            Self::Structure => 0,
            Self::Rule => 1,
            Self::Decision => 2,
            Self::Problem => 3,
            Self::Responsibility => 4,
        }
    }

    pub fn parse(value: &str) -> Self {
        match value.trim().to_lowercase().as_str() {
            "structure" => Self::Structure,
            "decision" => Self::Decision,
            "rule" => Self::Rule,
            "problem" => Self::Problem,
            _ => Self::Responsibility,
        }
    }
}

impl std::fmt::Display for KnowledgeSection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::Structure => "structure",
            Self::Decision => "decision",
            Self::Rule => "rule",
            Self::Problem => "problem",
            Self::Responsibility => "responsibility",
        };
        f.write_str(text)
    }
}

/// Verification status of an entry. The whole point of the model: `Verified`
/// is unreachable except through [`ProjectKnowledge::mark_verified`], which K2
/// only calls with real evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum KnowledgeStatus {
    #[default]
    Proposed,
    Verified,
}

/// One entry in the project map.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeEntry {
    pub id: String,
    #[serde(default)]
    pub section: KnowledgeSection,
    pub content: String,
    #[serde(default)]
    pub status: KnowledgeStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// When this entry was last verified, if ever.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_at: Option<DateTime<Utc>>,
    /// What verified it, e.g. "cargo test -p jcode-base (exit 0)" or
    /// "user confirmation". Empty while `Proposed`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provenance: Vec<String>,
}

impl KnowledgeEntry {
    pub fn new(section: KnowledgeSection, content: impl Into<String>) -> Self {
        let now = Utc::now();
        let rand: u64 = rand::random();
        Self {
            id: format!("pk_{}_{}", now.timestamp_millis(), rand),
            section,
            content: content.into().trim().to_string(),
            status: KnowledgeStatus::Proposed,
            created_at: now,
            updated_at: now,
            verified_at: None,
            provenance: Vec::new(),
        }
    }
}

const KNOWLEDGE_FILE_VERSION: u32 = 1;

fn default_file_version() -> u32 {
    KNOWLEDGE_FILE_VERSION
}

/// The per-project knowledge map. This is the on-disk shape too; keeping them
/// identical means load/save cannot drift from the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectKnowledge {
    #[serde(default = "default_file_version")]
    pub version: u32,
    #[serde(default)]
    pub entries: Vec<KnowledgeEntry>,
    pub updated_at: DateTime<Utc>,
}

impl Default for ProjectKnowledge {
    fn default() -> Self {
        Self {
            version: KNOWLEDGE_FILE_VERSION,
            entries: Vec::new(),
            updated_at: Utc::now(),
        }
    }
}

impl ProjectKnowledge {
    /// Add a new proposed entry. Content matching an existing entry in the
    /// same section (case-insensitive) updates that entry's timestamp instead
    /// of duplicating it, and returns the existing id.
    pub fn propose(&mut self, section: KnowledgeSection, content: &str) -> String {
        let normalized = content.trim().to_lowercase();
        if let Some(existing) = self.entries.iter_mut().find(|entry| {
            entry.section == section && entry.content.trim().to_lowercase() == normalized
        }) {
            existing.updated_at = Utc::now();
            let id = existing.id.clone();
            self.touch();
            return id;
        }
        let entry = KnowledgeEntry::new(section, content);
        let id = entry.id.clone();
        self.entries.push(entry);
        self.touch();
        id
    }

    /// Update an entry's content. The edit invalidates prior verification:
    /// changed claims need fresh evidence, so status returns to `Proposed`.
    /// Returns false when the id does not exist.
    pub fn revise(&mut self, id: &str, content: &str) -> bool {
        let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == id) else {
            return false;
        };
        entry.content = content.trim().to_string();
        entry.status = KnowledgeStatus::Proposed;
        entry.verified_at = None;
        entry.updated_at = Utc::now();
        self.touch();
        true
    }

    /// The verification gate's single entry point (called by K2). Marks an
    /// entry verified and records what verified it. Returns false when the id
    /// does not exist.
    pub fn mark_verified(&mut self, id: &str, provenance: &str) -> bool {
        let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == id) else {
            return false;
        };
        let now = Utc::now();
        entry.status = KnowledgeStatus::Verified;
        entry.verified_at = Some(now);
        entry.updated_at = now;
        let provenance = provenance.trim();
        if !provenance.is_empty() {
            entry.provenance.push(provenance.to_string());
        }
        self.touch();
        true
    }

    /// Remove an entry. Returns it when it existed.
    pub fn remove(&mut self, id: &str) -> Option<KnowledgeEntry> {
        let idx = self.entries.iter().position(|entry| entry.id == id)?;
        let entry = self.entries.remove(idx);
        self.touch();
        Some(entry)
    }

    pub fn get(&self, id: &str) -> Option<&KnowledgeEntry> {
        self.entries.iter().find(|entry| entry.id == id)
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn touch(&mut self) {
        self.updated_at = Utc::now();
    }

    /// Render the full map as readable markdown. Sections come in orientation
    /// order; within a section, verified entries come first (they are the
    /// trustworthy part), each proposed entry is labeled `(proposed)`.
    pub fn render_markdown(&self) -> String {
        let mut output = String::from("# Project Knowledge\n");
        let mut sections: Vec<&KnowledgeEntry> = self.entries.iter().collect();
        sections.sort_by_key(|entry| {
            (
                entry.section.rank(),
                entry.status != KnowledgeStatus::Verified,
            )
        });

        let mut current: Option<KnowledgeSection> = None;
        for entry in sections {
            if current != Some(entry.section) {
                output.push_str(&format!("\n## {}\n", entry.section.heading()));
                current = Some(entry.section);
            }
            match entry.status {
                KnowledgeStatus::Verified => {
                    output.push_str(&format!("- {}\n", entry.content));
                }
                KnowledgeStatus::Proposed => {
                    output.push_str(&format!("- (proposed) {}\n", entry.content));
                }
            }
        }
        output.trim_end().to_string()
    }
}

// === Health summary (K6) ===

/// Aggregate health counters over every project knowledge map on this machine.
/// Read-only: ambient reports these to the gardener, which may then suggest
/// cleanup to the user. Ambient never proposes, verifies, or removes entries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KnowledgeHealth {
    /// Number of project maps found.
    pub projects: usize,
    pub verified_entries: usize,
    pub proposed_entries: usize,
    /// Proposed entries older than [`STALE_PROPOSED_DAYS`]: claims that were
    /// never backed by evidence and are probably wrong or forgotten.
    pub stale_proposed: usize,
}

/// A proposed entry older than this is counted as stale.
pub const STALE_PROPOSED_DAYS: i64 = 14;

/// Scan `~/.jcode/knowledge/projects/*.json` and accumulate counters.
/// Unreadable or foreign files are skipped, never fatal.
pub fn gather_knowledge_health() -> KnowledgeHealth {
    let mut health = KnowledgeHealth::default();
    let Ok(dir) = crate::storage::jcode_dir().map(|d| d.join("knowledge").join("projects")) else {
        return health;
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return health;
    };

    let stale_before = Utc::now() - chrono::Duration::days(STALE_PROPOSED_DAYS);
    for file in entries.flatten() {
        let path = file.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(knowledge) = crate::storage::read_json::<ProjectKnowledge>(&path) else {
            continue;
        };
        if knowledge.version != KNOWLEDGE_FILE_VERSION {
            continue;
        }
        health.projects += 1;
        for entry in &knowledge.entries {
            match entry.status {
                KnowledgeStatus::Verified => health.verified_entries += 1,
                KnowledgeStatus::Proposed => {
                    health.proposed_entries += 1;
                    if entry.updated_at < stale_before {
                        health.stale_proposed += 1;
                    }
                }
            }
        }
    }
    health
}

// === Prompt injection (K5) ===

/// The `# Project Knowledge` section to inject into this turn's prompt, if any.
///
/// Single gate, mirroring `working_memory_prompt_section`: returns `None`
/// unless ALL of the following hold, so a caller cannot accidentally inject
/// the section by forgetting a check:
/// - the `project_knowledge_enabled` flag is on (default OFF),
/// - a working directory is known (no project, no map),
/// - that project's map is non-empty.
///
/// The section is truncated to the configured char budget, dropping whole
/// entries (never splitting one mid-sentence), verified entries surviving
/// preferentially because they render first within each section.
pub fn project_knowledge_prompt_section(working_dir: Option<&Path>) -> Option<String> {
    if !project_knowledge_enabled() {
        return None;
    }
    let project_dir = working_dir?;
    let knowledge = load(project_dir);
    if knowledge.is_empty() {
        return None;
    }
    Some(render_budgeted(&knowledge, project_knowledge_max_chars()))
}

/// Render the map within a char budget by dropping whole lines from the end.
/// Verified-first ordering inside `render_markdown` means proposed entries are
/// the first to go, then the least-oriented sections.
fn render_budgeted(knowledge: &ProjectKnowledge, max_chars: usize) -> String {
    let full = knowledge.render_markdown();
    if full.chars().count() <= max_chars {
        return full;
    }
    let mut out = String::new();
    for line in full.lines() {
        // +1 for the newline; the truncation marker needs room too.
        if out.chars().count() + line.chars().count() + 1 > max_chars.saturating_sub(24) {
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.push_str("(truncated to budget)");
    out
}

// === Persistence ===

/// Stable per-project hash, matching how `memory.rs` keys project graphs so
/// one project's memory and knowledge use the same identity.
fn project_hash(project_dir: &Path) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    project_dir.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// JSON path for a project's knowledge file.
pub fn knowledge_path(project_dir: &Path) -> anyhow::Result<PathBuf> {
    let dir = crate::storage::jcode_dir()?.join("knowledge").join("projects");
    Ok(dir.join(format!("{}.json", project_hash(project_dir))))
}

/// Rendered markdown path, sibling of the JSON file.
pub fn knowledge_markdown_path(project_dir: &Path) -> anyhow::Result<PathBuf> {
    let dir = crate::storage::jcode_dir()?.join("knowledge").join("projects");
    Ok(dir.join(format!("{}.md", project_hash(project_dir))))
}

/// Load a project's knowledge map. Missing file, unknown version, or parse
/// failure all yield an empty default: knowledge is a cache of learned facts,
/// never a correctness dependency.
pub fn load(project_dir: &Path) -> ProjectKnowledge {
    let Ok(path) = knowledge_path(project_dir) else {
        return ProjectKnowledge::default();
    };
    if !path.exists() {
        return ProjectKnowledge::default();
    }
    match crate::storage::read_json::<ProjectKnowledge>(&path) {
        Ok(knowledge) if knowledge.version == KNOWLEDGE_FILE_VERSION => knowledge,
        Ok(knowledge) => {
            crate::logging::info(&format!(
                "Ignoring project knowledge at {}: unsupported version {}",
                path.display(),
                knowledge.version
            ));
            ProjectKnowledge::default()
        }
        Err(err) => {
            crate::logging::info(&format!(
                "Failed to load project knowledge from {}: {err}",
                path.display()
            ));
            ProjectKnowledge::default()
        }
    }
}

/// Persist a project's knowledge map (JSON + rendered markdown sibling).
/// Best-effort by design; an empty map removes both files instead of leaving
/// stale ones behind.
pub fn save(project_dir: &Path, knowledge: &ProjectKnowledge) {
    let (Ok(json_path), Ok(md_path)) = (
        knowledge_path(project_dir),
        knowledge_markdown_path(project_dir),
    ) else {
        return;
    };

    if knowledge.is_empty() {
        let _ = std::fs::remove_file(&json_path);
        let _ = std::fs::remove_file(&md_path);
        return;
    }

    if let Err(err) = crate::storage::write_json(&json_path, knowledge) {
        crate::logging::info(&format!(
            "Failed to persist project knowledge to {}: {err}",
            json_path.display()
        ));
        return;
    }
    if let Err(err) = std::fs::write(&md_path, knowledge.render_markdown()) {
        crate::logging::info(&format!(
            "Failed to render project knowledge map to {}: {err}",
            md_path.display()
        ));
    }
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn propose_dedups_within_section_and_keeps_across_sections() {
        let mut knowledge = ProjectKnowledge::default();
        let first = knowledge.propose(KnowledgeSection::Rule, "never push to upstream");
        let dup = knowledge.propose(KnowledgeSection::Rule, "  Never push to upstream ");
        assert_eq!(first, dup, "same section + content must not duplicate");
        assert_eq!(knowledge.entries.len(), 1);

        let other = knowledge.propose(KnowledgeSection::Problem, "never push to upstream");
        assert_ne!(first, other, "same content in another section is distinct");
        assert_eq!(knowledge.entries.len(), 2);
    }

    #[test]
    fn entries_start_proposed_and_only_the_gate_verifies() {
        let mut knowledge = ProjectKnowledge::default();
        let id = knowledge.propose(KnowledgeSection::Decision, "use cargo only on Windows");
        assert_eq!(knowledge.get(&id).unwrap().status, KnowledgeStatus::Proposed);
        assert!(knowledge.get(&id).unwrap().verified_at.is_none());

        assert!(knowledge.mark_verified(&id, "cargo test -p jcode-base (exit 0)"));
        let entry = knowledge.get(&id).unwrap();
        assert_eq!(entry.status, KnowledgeStatus::Verified);
        assert!(entry.verified_at.is_some());
        assert_eq!(entry.provenance, vec!["cargo test -p jcode-base (exit 0)"]);

        assert!(!knowledge.mark_verified("pk_missing", "x"), "unknown id");
    }

    #[test]
    fn revising_a_verified_entry_demotes_it_to_proposed() {
        let mut knowledge = ProjectKnowledge::default();
        let id = knowledge.propose(KnowledgeSection::Structure, "single crate");
        knowledge.mark_verified(&id, "cargo check (exit 0)");

        assert!(knowledge.revise(&id, "workspace with many crates"));
        let entry = knowledge.get(&id).unwrap();
        assert_eq!(
            entry.status,
            KnowledgeStatus::Proposed,
            "edited claims need fresh evidence"
        );
        assert!(entry.verified_at.is_none());
        assert_eq!(
            entry.provenance,
            vec!["cargo check (exit 0)"],
            "history of what once verified it is kept"
        );
    }

    #[test]
    fn markdown_orders_sections_and_puts_verified_first() {
        let mut knowledge = ProjectKnowledge::default();
        knowledge.propose(KnowledgeSection::Responsibility, "memory.rs owns retrieval");
        let rule_id = knowledge.propose(KnowledgeSection::Rule, "flags default off");
        knowledge.propose(KnowledgeSection::Rule, "commit per phase");
        knowledge.propose(KnowledgeSection::Structure, "cargo workspace");
        knowledge.mark_verified(&rule_id, "user confirmation");

        let md = knowledge.render_markdown();
        assert!(md.starts_with("# Project Knowledge"));

        let structure_at = md.find("## Structure").expect("structure section");
        let rules_at = md.find("## Rules").expect("rules section");
        let resp_at = md.find("## Responsibilities").expect("responsibilities");
        assert!(structure_at < rules_at && rules_at < resp_at);

        let verified_at = md.find("- flags default off").expect("verified entry");
        let proposed_at = md
            .find("- (proposed) commit per phase")
            .expect("proposed entry labeled");
        assert!(
            verified_at < proposed_at,
            "verified entries render before proposed ones:\n{md}"
        );
    }

    #[test]
    fn persistence_round_trips_and_empty_map_removes_files() {
        let _home = setup();
        let project = std::path::Path::new("C:/some/project");

        let mut knowledge = ProjectKnowledge::default();
        let id = knowledge.propose(KnowledgeSection::Rule, "use cargo only");
        knowledge.mark_verified(&id, "cargo test (exit 0)");
        save(project, &knowledge);

        let loaded = load(project);
        assert_eq!(loaded.entries.len(), 1);
        let entry = &loaded.entries[0];
        assert_eq!(entry.content, "use cargo only");
        assert_eq!(entry.status, KnowledgeStatus::Verified);
        assert_eq!(entry.provenance, vec!["cargo test (exit 0)"]);

        let md_path = knowledge_markdown_path(project).expect("md path");
        let md = std::fs::read_to_string(&md_path).expect("rendered map exists");
        assert!(md.contains("use cargo only"));

        // Draining the map must remove both files, not leave stale ones.
        let mut emptied = loaded;
        emptied.remove(&emptied.entries[0].id.clone());
        save(project, &emptied);
        assert!(!knowledge_path(project).expect("path").exists());
        assert!(!md_path.exists());
        assert!(load(project).is_empty());
    }

    #[test]
    fn unknown_version_and_corrupt_files_load_as_empty() {
        let _home = setup();
        let project = std::path::Path::new("C:/versioned/project");
        let path = knowledge_path(project).expect("path");
        std::fs::create_dir_all(path.parent().unwrap()).expect("mkdir");

        std::fs::write(
            &path,
            r#"{"version": 999, "entries": [], "updated_at": "2026-01-01T00:00:00Z"}"#,
        )
        .expect("write");
        assert!(load(project).is_empty(), "future version must be ignored");

        std::fs::write(&path, "not json at all").expect("write");
        assert!(load(project).is_empty(), "corrupt file must not error");
    }

    #[test]
    fn projects_are_isolated_by_path_hash() {
        let _home = setup();
        let a = std::path::Path::new("C:/project/a");
        let b = std::path::Path::new("C:/project/b");

        let mut ka = ProjectKnowledge::default();
        ka.propose(KnowledgeSection::Rule, "rule for a");
        save(a, &ka);

        assert_eq!(load(a).entries.len(), 1);
        assert!(load(b).is_empty(), "another project must see nothing");
        assert_ne!(
            knowledge_path(a).unwrap(),
            knowledge_path(b).unwrap(),
            "distinct projects must map to distinct files"
        );
    }

    #[test]
    fn prompt_section_requires_flag_dir_and_content() {
        let _home = setup();
        let project = std::path::Path::new("C:/prompt/project");

        // Flag off (default): always None, even with a populated map.
        let mut knowledge = ProjectKnowledge::default();
        knowledge.propose(KnowledgeSection::Rule, "some rule");
        save(project, &knowledge);
        assert!(project_knowledge_prompt_section(Some(project)).is_none());

        // The remaining gates are testable via the pure renderer: no dir and
        // empty map are handled before rendering.
        assert!(project_knowledge_prompt_section(None).is_none());
    }

    #[test]
    fn budgeted_render_drops_whole_lines_and_marks_truncation() {
        let mut knowledge = ProjectKnowledge::default();
        for i in 0..50 {
            knowledge.propose(
                KnowledgeSection::Rule,
                &format!("rule number {i} with some padding text"),
            );
        }
        let full = knowledge.render_markdown();

        // A generous budget returns the full render untouched.
        assert_eq!(render_budgeted(&knowledge, full.chars().count()), full);

        // A tight budget truncates on line boundaries with a marker.
        let tight = render_budgeted(&knowledge, 300);
        assert!(tight.chars().count() <= 300);
        assert!(tight.ends_with("(truncated to budget)"));
        for line in tight.lines() {
            if line.starts_with("- ") {
                assert!(
                    full.contains(line),
                    "truncation must keep whole lines, found fragment: {line}"
                );
            }
        }
    }

    #[test]
    fn budgeted_render_prefers_verified_entries() {
        let mut knowledge = ProjectKnowledge::default();
        // One verified and many proposed entries in the same section: under
        // pressure the verified one must survive because it renders first.
        let keeper = knowledge.propose(KnowledgeSection::Rule, "the verified keeper rule");
        knowledge.mark_verified(&keeper, "cargo test (exit 0)");
        for i in 0..30 {
            knowledge.propose(KnowledgeSection::Rule, &format!("proposed filler {i}"));
        }

        let tight = render_budgeted(&knowledge, 120);
        assert!(
            tight.contains("the verified keeper rule"),
            "verified entry must survive the budget:\n{tight}"
        );
    }

    #[test]
    fn health_counts_verified_proposed_and_stale() {
        let _home = setup();
        let a = std::path::Path::new("C:/health/a");
        let b = std::path::Path::new("C:/health/b");

        let mut ka = ProjectKnowledge::default();
        let v = ka.propose(KnowledgeSection::Rule, "verified rule");
        ka.mark_verified(&v, "cargo test (exit 0)");
        ka.propose(KnowledgeSection::Problem, "fresh proposal");
        // A stale proposal: backdate updated_at past the threshold.
        let stale_id = ka.propose(KnowledgeSection::Decision, "stale proposal");
        if let Some(entry) = ka.entries.iter_mut().find(|e| e.id == stale_id) {
            entry.updated_at = Utc::now() - chrono::Duration::days(STALE_PROPOSED_DAYS + 1);
        }
        save(a, &ka);

        let mut kb = ProjectKnowledge::default();
        kb.propose(KnowledgeSection::Structure, "another project");
        save(b, &kb);

        let health = gather_knowledge_health();
        assert_eq!(health.projects, 2);
        assert_eq!(health.verified_entries, 1);
        assert_eq!(health.proposed_entries, 3);
        assert_eq!(health.stale_proposed, 1);
    }

    #[test]
    fn health_is_empty_when_no_maps_exist() {
        let _home = setup();
        assert_eq!(gather_knowledge_health(), KnowledgeHealth::default());
    }

    #[test]
    fn config_accessors_default_off_and_clamped() {
        // Defaults come from K0: disabled, 4000-char budget. The clamp must
        // hold even though the raw config value is user-controlled.
        assert!(!project_knowledge_enabled());
        let budget = project_knowledge_max_chars();
        assert!((256..=16_000).contains(&budget));
    }
}
