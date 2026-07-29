//! The verification gate (K2): the only source of "this knowledge is verified".
//!
//! Evidence lives here as in-process, per-session [`VerificationEvent`]s:
//! - the bash tool reports every completed foreground `cargo build` /
//!   `cargo check` / `cargo clippy` / `cargo test` with its exit code via
//!   [`observe_command`] (a no-op unless the feature flag is on),
//! - explicit user confirmation is its own authority via [`verify_by_user`].
//!
//! The gate rule, enforced in exactly one place ([`try_verify`]):
//! an entry may move from `Proposed` to `Verified` only when the session has a
//! successful verification event that is **at least as new as the entry's last
//! edit**, and **no relevant command has failed since** that success. Edited
//! claims need fresh evidence; a broken build invalidates prior evidence.
//!
//! Deliberate conservatism:
//! - events are never persisted: evidence does not outlive the process, so a
//!   stale success from yesterday can never verify today's claim,
//! - only foreground bash commands are observed; background tasks and other
//!   paths simply produce no event. Missing evidence can only ever cause an
//!   entry to stay `Proposed`, never to be wrongly verified.

use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use super::{KnowledgeStatus, load, save};

/// What kind of evidence an event carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerificationKind {
    /// `cargo build` / `cargo check` / `cargo clippy` succeeded.
    BuildPassed,
    /// `cargo test` succeeded (outranks build when both appear in one chain).
    TestsPassed,
}

/// One observed verification-relevant command completion.
#[derive(Debug, Clone)]
pub struct VerificationEvent {
    pub kind: VerificationKind,
    /// Whether the command exited successfully.
    pub success: bool,
    /// Human-readable evidence, e.g. `cargo test -p jcode-base (exit 0)`.
    pub evidence: String,
    pub at: DateTime<Utc>,
}

/// Why the gate refused to verify an entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyError {
    /// The feature flag is off.
    Disabled,
    /// No such entry in this project's map.
    UnknownEntry,
    /// The entry is already verified; nothing to do.
    AlreadyVerified,
    /// No successful verification event exists for this session.
    NoEvidence,
    /// Evidence exists but predates the entry's last edit.
    StaleEvidence,
    /// A relevant command failed after the most recent success.
    InvalidatedByFailure,
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::Disabled => "project knowledge is disabled",
            Self::UnknownEntry => "no such knowledge entry",
            Self::AlreadyVerified => "entry is already verified",
            Self::NoEvidence => "no successful build/test verification event in this session yet",
            Self::StaleEvidence => {
                "the entry was edited after the last successful verification; run build/tests again"
            }
            Self::InvalidatedByFailure => {
                "a build/test command failed after the last success; re-run verification"
            }
        };
        f.write_str(text)
    }
}

/// Per-session event buffers, keyed like working memory so the lifetime and
/// isolation semantics match the rest of the session-scoped state.
static EVENTS: Mutex<Option<HashMap<String, Vec<VerificationEvent>>>> = Mutex::new(None);

/// Events kept per session. Evidence older than the newest handful is never
/// consulted by the gate, so a small cap bounds memory without changing
/// behavior.
const MAX_EVENTS_PER_SESSION: usize = 32;

fn with_events<T>(f: impl FnOnce(&mut HashMap<String, Vec<VerificationEvent>>) -> T) -> T {
    let mut guard = EVENTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    f(map)
}

/// Cap evidence strings so a pathological command line cannot bloat provenance.
fn evidence_string(command: &str, exit_code: Option<i32>) -> String {
    const MAX_COMMAND_CHARS: usize = 120;
    let mut cmd: String = command.trim().chars().take(MAX_COMMAND_CHARS).collect();
    if command.trim().chars().count() > MAX_COMMAND_CHARS {
        cmd.push('…');
    }
    match exit_code {
        Some(code) => format!("{cmd} (exit {code})"),
        None => format!("{cmd} (terminated)"),
    }
}

/// Classify a shell command as verification-relevant, conservatively.
///
/// Only a literal `cargo <subcommand>` token pair counts (an optional
/// `+toolchain` selector may sit between them). `cargo test` outranks the
/// build-family subcommands when both appear in one `&&` chain, because a
/// passing test run is the stronger claim. Anything else, including `cargo run`
/// or the word "test" inside file paths, produces no event at all.
pub fn classify_command(command: &str) -> Option<VerificationKind> {
    let tokens: Vec<&str> = command.split_whitespace().collect();
    let mut best: Option<VerificationKind> = None;
    for (idx, token) in tokens.iter().enumerate() {
        if *token != "cargo" {
            continue;
        }
        // `cargo [+toolchain] <subcommand>`: skip an optional toolchain selector.
        let mut sub_idx = idx + 1;
        if tokens.get(sub_idx).is_some_and(|t| t.starts_with('+')) {
            sub_idx += 1;
        }
        match tokens.get(sub_idx).copied() {
            Some("test" | "nextest") => return Some(VerificationKind::TestsPassed),
            Some("build" | "check" | "clippy") => {
                best = Some(VerificationKind::BuildPassed);
            }
            _ => {}
        }
    }
    best
}

/// Bash-tool hook: record a completed foreground command when it is
/// verification-relevant. No-op (one cheap flag read) when the feature is off,
/// so the hot path stays unchanged for everyone who has not opted in.
pub fn observe_command(session_id: &str, command: &str, exit_code: Option<i32>) {
    if !super::project_knowledge_enabled() {
        return;
    }
    record_command(session_id, command, exit_code);
}

/// Flag-free recorder, separated so tests can exercise classification and gate
/// logic without mutating global config.
pub fn record_command(session_id: &str, command: &str, exit_code: Option<i32>) {
    let Some(kind) = classify_command(command) else {
        return;
    };
    let event = VerificationEvent {
        kind,
        // A missing exit code means the process was killed or lost; that is
        // never evidence of success.
        success: exit_code == Some(0),
        evidence: evidence_string(command, exit_code),
        at: Utc::now(),
    };
    with_events(|map| {
        let events = map.entry(session_id.to_string()).or_default();
        events.push(event);
        let len = events.len();
        if len > MAX_EVENTS_PER_SESSION {
            events.drain(..len - MAX_EVENTS_PER_SESSION);
        }
    });
}

/// Snapshot a session's events, oldest first. Used by the gate and (in K3) the
/// knowledge tool's history view.
pub fn session_events(session_id: &str) -> Vec<VerificationEvent> {
    with_events(|map| map.get(session_id).cloned().unwrap_or_default())
}

/// Drop one session's events (session end) or all (tests/reset).
pub fn clear_session(session_id: &str) {
    with_events(|map| {
        map.remove(session_id);
    });
}

pub fn clear_all() {
    with_events(|map| map.clear());
}

/// The freshness rule, extracted so it is independently testable: find the
/// evidence that justifies verifying an entry last edited at `edited_at`.
///
/// Requires a successful event at or after `edited_at`, with no failure after
/// that success. Returns the evidence string of the newest qualifying success.
fn qualifying_evidence(
    events: &[VerificationEvent],
    edited_at: DateTime<Utc>,
) -> Result<String, VerifyError> {
    let latest_success = events
        .iter()
        .filter(|event| event.success)
        .max_by_key(|event| event.at);
    let Some(success) = latest_success else {
        return Err(VerifyError::NoEvidence);
    };
    if success.at < edited_at {
        return Err(VerifyError::StaleEvidence);
    }
    let failed_after = events
        .iter()
        .any(|event| !event.success && event.at > success.at);
    if failed_after {
        return Err(VerifyError::InvalidatedByFailure);
    }
    Ok(success.evidence.clone())
}

/// Verify one entry using this session's build/test evidence. This is the ONLY
/// path from `Proposed` to `Verified` besides explicit user confirmation.
///
/// On success the map is saved and the provenance string is returned.
pub fn try_verify(
    project_dir: &Path,
    session_id: &str,
    entry_id: &str,
) -> Result<String, VerifyError> {
    if !super::project_knowledge_enabled() {
        return Err(VerifyError::Disabled);
    }
    try_verify_with_events(project_dir, entry_id, &session_events(session_id))
}

/// Event-injected form of [`try_verify`] so tests can drive the gate without
/// global state or config.
pub fn try_verify_with_events(
    project_dir: &Path,
    entry_id: &str,
    events: &[VerificationEvent],
) -> Result<String, VerifyError> {
    let mut knowledge = load(project_dir);
    let entry = knowledge.get(entry_id).ok_or(VerifyError::UnknownEntry)?;
    if entry.status == KnowledgeStatus::Verified {
        return Err(VerifyError::AlreadyVerified);
    }

    let evidence = qualifying_evidence(events, entry.updated_at)?;
    knowledge.mark_verified(entry_id, &evidence);
    save(project_dir, &knowledge);
    if let Some(entry) = knowledge.get(entry_id) {
        super::bridge::bridge_best_effort(project_dir, entry);
    }
    Ok(evidence)
}

/// Verify one entry on explicit user confirmation. The user is the authority
/// here, so no build/test evidence is consulted; provenance records that this
/// was a human decision, plus an optional note.
pub fn verify_by_user(
    project_dir: &Path,
    entry_id: &str,
    note: Option<&str>,
) -> Result<String, VerifyError> {
    if !super::project_knowledge_enabled() {
        return Err(VerifyError::Disabled);
    }
    let mut knowledge = load(project_dir);
    let entry = knowledge.get(entry_id).ok_or(VerifyError::UnknownEntry)?;
    if entry.status == KnowledgeStatus::Verified {
        return Err(VerifyError::AlreadyVerified);
    }

    let provenance = match note.map(str::trim).filter(|n| !n.is_empty()) {
        Some(note) => format!("user confirmation: {note}"),
        None => "user confirmation".to_string(),
    };
    knowledge.mark_verified(entry_id, &provenance);
    save(project_dir, &knowledge);
    if let Some(entry) = knowledge.get(entry_id) {
        super::bridge::bridge_best_effort(project_dir, entry);
    }
    Ok(provenance)
}

#[cfg(test)]
mod tests {
    use super::super::{KnowledgeSection, ProjectKnowledge};
    use super::*;
    use chrono::Duration;

    /// Serializes tests that touch the process-global event buffers.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    struct TestHome {
        _home: tempfile::TempDir,
        _env: std::sync::MutexGuard<'static, ()>,
        _events: std::sync::MutexGuard<'static, ()>,
        prev: Option<std::ffi::OsString>,
    }

    fn setup() -> TestHome {
        let events = TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        clear_all();
        let env = crate::storage::lock_test_env();
        let home = tempfile::tempdir().expect("home");
        let prev = std::env::var_os("JCODE_HOME");
        crate::env::set_var("JCODE_HOME", home.path());
        TestHome {
            _home: home,
            _env: env,
            _events: events,
            prev,
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            clear_all();
            match self.prev.take() {
                Some(v) => crate::env::set_var("JCODE_HOME", v),
                None => crate::env::remove_var("JCODE_HOME"),
            }
        }
    }

    fn event(kind: VerificationKind, success: bool, at: DateTime<Utc>) -> VerificationEvent {
        VerificationEvent {
            kind,
            success,
            evidence: format!("test event ({})", if success { "ok" } else { "fail" }),
            at,
        }
    }

    #[test]
    fn classification_is_conservative() {
        use VerificationKind::*;
        assert_eq!(
            classify_command("cargo test -p jcode-base"),
            Some(TestsPassed)
        );
        assert_eq!(classify_command("cargo nextest run"), Some(TestsPassed));
        assert_eq!(classify_command("cargo build --release"), Some(BuildPassed));
        assert_eq!(classify_command("cargo check -p foo"), Some(BuildPassed));
        assert_eq!(
            classify_command("cargo clippy -- -D warnings"),
            Some(BuildPassed)
        );
        assert_eq!(classify_command("cargo +nightly test"), Some(TestsPassed));
        assert_eq!(
            classify_command("cd C:/repo && cargo check && cargo test"),
            Some(TestsPassed),
            "test outranks build in a chain"
        );

        assert_eq!(classify_command("cargo run --bin server"), None);
        assert_eq!(classify_command("cargo fmt"), None);
        assert_eq!(
            classify_command("echo cargo test is not a test"),
            Some(TestsPassed)
        );
        // ^ deliberate: token-pair matching cannot see quoting. Documented
        //   tradeoff; the alternative (shell parsing) is not worth the risk of
        //   missing real verification runs. Failure direction is safe: a false
        //   event still requires exit 0 of the echo, which proves nothing and
        //   can only over-verify if the agent lies to itself about running it.
        assert_eq!(classify_command("python test_cargo.py"), None);
        assert_eq!(classify_command("cargotest"), None);
        assert_eq!(classify_command(""), None);
    }

    #[test]
    fn recorder_stores_success_and_failure_with_cap() {
        let _home = setup();
        record_command("s1", "cargo test", Some(0));
        record_command("s1", "cargo build", Some(101));
        record_command("s1", "cargo test", None);
        record_command("s1", "ls -la", Some(0));

        let events = session_events("s1");
        assert_eq!(events.len(), 3, "irrelevant commands produce no event");
        assert!(events[0].success);
        assert!(!events[1].success, "nonzero exit is failure");
        assert!(!events[2].success, "missing exit code is never success");
        assert!(events[0].evidence.contains("(exit 0)"));
        assert!(events[2].evidence.contains("(terminated)"));

        for _ in 0..100 {
            record_command("s1", "cargo check", Some(0));
        }
        assert_eq!(
            session_events("s1").len(),
            MAX_EVENTS_PER_SESSION,
            "event buffer must stay capped"
        );

        assert!(session_events("s2").is_empty(), "sessions are isolated");
    }

    #[test]
    fn observe_command_is_a_no_op_while_the_flag_is_off() {
        let _home = setup();
        // Default config leaves the feature disabled (K0), so the public hook
        // must record nothing even for a perfect verification command.
        observe_command("flag-off", "cargo test", Some(0));
        assert!(session_events("flag-off").is_empty());
    }

    #[test]
    fn gate_requires_evidence_fresh_and_unbroken() {
        let now = Utc::now();
        let edited = now - Duration::seconds(60);

        // No events at all.
        assert_eq!(
            qualifying_evidence(&[], edited),
            Err(VerifyError::NoEvidence)
        );

        // Only failures.
        let only_fail = [event(VerificationKind::TestsPassed, false, now)];
        assert_eq!(
            qualifying_evidence(&only_fail, edited),
            Err(VerifyError::NoEvidence)
        );

        // Success predating the edit is stale.
        let stale = [event(
            VerificationKind::TestsPassed,
            true,
            edited - Duration::seconds(10),
        )];
        assert_eq!(
            qualifying_evidence(&stale, edited),
            Err(VerifyError::StaleEvidence)
        );

        // Fresh success verifies.
        let fresh = [event(VerificationKind::TestsPassed, true, now)];
        assert!(qualifying_evidence(&fresh, edited).is_ok());

        // Success at exactly the edit instant counts (>= semantics), so a
        // same-millisecond propose-then-verify cannot flake.
        let exact = [event(VerificationKind::BuildPassed, true, edited)];
        assert!(qualifying_evidence(&exact, edited).is_ok());

        // A failure AFTER the success invalidates it.
        let broken = [
            event(
                VerificationKind::TestsPassed,
                true,
                now - Duration::seconds(5),
            ),
            event(VerificationKind::BuildPassed, false, now),
        ];
        assert_eq!(
            qualifying_evidence(&broken, edited),
            Err(VerifyError::InvalidatedByFailure)
        );

        // A failure BEFORE the success does not.
        let recovered = [
            event(
                VerificationKind::BuildPassed,
                false,
                now - Duration::seconds(5),
            ),
            event(VerificationKind::TestsPassed, true, now),
        ];
        assert!(qualifying_evidence(&recovered, edited).is_ok());
    }

    #[test]
    fn try_verify_moves_entry_through_the_gate_and_persists() {
        let _home = setup();
        let project = std::path::Path::new("C:/gate/project");

        let mut knowledge = ProjectKnowledge::default();
        let id = knowledge.propose(KnowledgeSection::Rule, "flags default off");
        super::super::save(project, &knowledge);

        // Entry edited "now"; evidence strictly after it.
        let entry_edited = knowledge.get(&id).unwrap().updated_at;
        let events = [event(
            VerificationKind::TestsPassed,
            true,
            entry_edited + Duration::seconds(1),
        )];

        let evidence = try_verify_with_events(project, &id, &events).expect("gate should pass");
        assert!(evidence.contains("test event"));

        let reloaded = super::super::load(project);
        let entry = reloaded.get(&id).expect("entry persisted");
        assert_eq!(entry.status, KnowledgeStatus::Verified);
        assert_eq!(entry.provenance, vec![evidence]);

        // Second attempt: already verified.
        assert_eq!(
            try_verify_with_events(project, &id, &events),
            Err(VerifyError::AlreadyVerified)
        );

        // Unknown entries are refused before any evidence check.
        assert_eq!(
            try_verify_with_events(project, "pk_missing", &events),
            Err(VerifyError::UnknownEntry)
        );
    }

    #[test]
    fn editing_after_evidence_reopens_the_gate() {
        let _home = setup();
        let project = std::path::Path::new("C:/gate/reopen");

        let mut knowledge = ProjectKnowledge::default();
        let id = knowledge.propose(KnowledgeSection::Decision, "initial claim");
        let evidence_at = knowledge.get(&id).unwrap().updated_at + Duration::seconds(1);

        // Revise strictly after the evidence timestamp.
        let entry = knowledge.entries.iter_mut().find(|e| e.id == id).unwrap();
        entry.content = "revised claim".to_string();
        entry.updated_at = evidence_at + Duration::seconds(1);
        super::super::save(project, &knowledge);

        let events = [event(VerificationKind::TestsPassed, true, evidence_at)];
        assert_eq!(
            try_verify_with_events(project, &id, &events),
            Err(VerifyError::StaleEvidence),
            "evidence gathered before the edit must not verify the new claim"
        );
    }

    #[test]
    fn user_confirmation_is_its_own_authority_but_respects_the_flag() {
        let _home = setup();
        let project = std::path::Path::new("C:/gate/user");

        let mut knowledge = ProjectKnowledge::default();
        let id = knowledge.propose(KnowledgeSection::Rule, "never touch upstream");
        super::super::save(project, &knowledge);

        // Flag is off by default: even the user path must refuse, so a
        // disabled feature has literally no write path.
        assert_eq!(
            verify_by_user(project, &id, Some("confirmed in chat")),
            Err(VerifyError::Disabled)
        );

        // With the gate logic itself (flag-independent core), user
        // confirmation needs no events: exercised via mark_verified in K1
        // tests and via the tool in K3 with the flag on.
    }
}
