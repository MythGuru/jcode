# Native `/peers` Overview Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a token-free `/peers` command that shows live approved peer status and the latest five privacy-safe peer activities from existing persisted transcripts, then build and reload Jcode onto the verified commit.

**Architecture:** Add one read-only lightweight protocol request whose authority is a currently attached session ID and whose server handler derives the exact canonical workspace identity. Keep durable history separate in a pure app-core transcript extractor plus a bounded session-file scanner. The TUI only coordinates the two read-only results, renders a compact card, and receives completion through the existing bus.

**Tech Stack:** Rust 2024 workspace, Tokio, Serde JSON wire protocol, Jcode snapshot/journal session persistence, Ratatui transcript cards, Windows Cargo self-development build.

## Global Constraints

- Peer messaging remains behind the existing default-off `features.peer_messaging` flag.
- Ambient Mode remains disabled in Michael's active configuration and gains no peer initiation path.
- `/peers` must not start a model turn or consume provider tokens.
- Live authorization derives from the requested live session's stored canonical working directory, never a caller-supplied path or alias.
- The overview request can only read status. It cannot send, reply, wake, interrupt, reserve a turn, or mutate configuration.
- Durable history uses existing Jcode transcripts. Do not create a peer-history database or journal.
- Display at most five activities. Never display message bodies, reply bodies, full paths, session IDs, capabilities, or exchange secrets.
- Inspect at most 12 recently updated matching Jcode sessions, skip a transcript over 2 MiB, and inspect at most the newest 500 messages in each parsed session.
- Preserve existing `peer list`, `peer send`, and `peer reply` schemas and behavior.
- Follow red-green-refactor for every production behavior.
- Use plain `cargo` commands on Windows. Do not use shell wrapper scripts.

---

## File Structure

**Create:**

- `crates/jcode-app-core/src/peer_activity.rs`
  - Pure activity types, message extraction, bounded persisted-session scan, sanitization, and rendering-independent tests.
- `crates/jcode-app-core/src/peer_overview.rs`
  - Read-only lightweight socket client used by the TUI to fetch `PeerOverview`.
- `crates/jcode-tui/src/tui/app/commands_peers.rs`
  - `/peers` parser, background orchestration, compact card renderer, completion handler, and TUI tests.

**Modify:**

- `crates/jcode-protocol/src/lib.rs`
  - Add `PeerIdentityInfo`, `PeerOverview`, and `PeerOverviewState`.
- `crates/jcode-protocol/src/wire.rs`
  - Add `Request::PeerOverview` and `ServerEvent::PeerOverviewResult`.
- `crates/jcode-protocol/src/protocol_tests/peer_messages.rs`
  - Add serialization, privacy, and lightweight-classification tests.
- `crates/jcode-app-core/src/lib.rs`
  - Export `peer_activity` and `peer_overview`.
- `crates/jcode-app-core/src/server/client_peer.rs`
  - Add the read-only overview handler using live session snapshots and exact peer identity.
- `crates/jcode-app-core/src/server/client_lightweight_control.rs`
  - Route `PeerOverview` without entering the exchange wait path.
- `crates/jcode-app-core/src/server/client_lifecycle.rs`
  - Keep the attached full TUI path explicit and reject tool-only peer exchange requests as before.
- `crates/jcode-app-core/src/server/client_lifecycle_logging.rs`
  - Mark the overview request as read-only and log only request type and ID.
- `crates/jcode-base/src/bus.rs`
  - Add `PeerOverviewCompleted` and `BusEvent::PeerOverviewCompleted`.
- `crates/jcode-tui/src/tui/app.rs`
  - Declare `commands_peers`.
- `crates/jcode-tui/src/tui/app/state_ui_input_helpers.rs`
  - Register `/peers` exactly once with the approved help text.
- `crates/jcode-tui/src/tui/app/commands.rs`
  - Dispatch `/peers` through `handle_session_command`; local and remote bus handlers call `commands_peers::handle_peer_overview_completed` directly.
- `crates/jcode-tui/src/tui/app/local.rs`
  - Deliver local completion bus events.
- `crates/jcode-tui/src/tui/app/remote.rs`
  - Deliver disconnected/remote completion bus events without creating a second command implementation.
- `docs/PEER_MESSAGING.md`
  - Explain `/peers`, history persistence, privacy, and disabled/error states.

---

### Task 1: Add the read-only peer overview protocol

**Files:**
- Modify: `crates/jcode-protocol/src/lib.rs:68-116`
- Modify: `crates/jcode-protocol/src/wire.rs:722-772`
- Modify: `crates/jcode-protocol/src/lib.rs:612-738`
- Test: `crates/jcode-protocol/src/protocol_tests/peer_messages.rs`

**Interfaces:**
- Produces:
  - `PeerOverviewState::{Enabled, Disabled, ConfigurationError, Unlisted}`
  - `PeerIdentityInfo { alias: String, group: String, project: String }`
  - `PeerOverview { state, identity, peers, error }`
  - `Request::PeerOverview { id: u64, session_id: String }`
  - `ServerEvent::PeerOverviewResult { id: u64, overview: PeerOverview }`

- [ ] **Step 1: Write failing protocol tests**

Add tests that construct this exact request and response shape:

```rust
#[test]
fn peer_overview_is_read_only_lightweight_and_contains_no_path_or_capability() -> Result<()> {
    let request = Request::PeerOverview {
        id: 21,
        session_id: "session-planner".to_string(),
    };
    let json = serde_json::to_string(&request)?;
    assert!(json.contains("\"type\":\"peer_overview\""));
    assert!(!json.contains("working_dir"));
    assert!(!json.contains("capability"));
    let decoded = parse_request_json(&json)?;
    assert_eq!(decoded.id(), 21);
    assert!(decoded.is_lightweight_control_request());
    Ok(())
}

#[test]
fn peer_overview_result_preserves_identity_and_sanitized_states() -> Result<()> {
    let event = ServerEvent::PeerOverviewResult {
        id: 22,
        overview: PeerOverview {
            state: PeerOverviewState::Enabled,
            identity: Some(PeerIdentityInfo {
                alias: "Planner".to_string(),
                group: "core-team".to_string(),
                project: "jcode".to_string(),
            }),
            peers: vec![PeerInfo {
                alias: "SpecScore".to_string(),
                group: "core-team".to_string(),
                project: "BlueprintMyApp".to_string(),
                state: PeerState::Idle,
            }],
            error: None,
        },
    };
    let json = serde_json::to_string(&event)?;
    assert!(!json.contains("working_dir"));
    assert!(!json.contains("session_id"));
    assert!(!json.contains("capability"));
    assert!(matches!(parse_event_json(&json)?, ServerEvent::PeerOverviewResult { .. }));
    Ok(())
}
```

- [ ] **Step 2: Run the focused test and confirm RED**

Run:

```text
cargo test -p jcode-protocol peer_overview -- --nocapture
```

Expected: compile failure because the overview types and variants do not exist.

- [ ] **Step 3: Add the minimal protocol types and variants**

Implement:

```rust
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PeerOverviewState {
    Enabled,
    Disabled,
    ConfigurationError,
    Unlisted,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PeerIdentityInfo {
    pub alias: String,
    pub group: String,
    pub project: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PeerOverview {
    pub state: PeerOverviewState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<PeerIdentityInfo>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub peers: Vec<PeerInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
```

Add the request/event variants and include `PeerOverview` in `Request::id()` and `Request::is_lightweight_control_request()`.

- [ ] **Step 4: Run protocol tests and confirm GREEN**

Run:

```text
cargo test -p jcode-protocol peer_overview -- --nocapture
cargo test -p jcode-protocol peer_ -- --nocapture
```

Expected: all matching protocol tests pass.

- [ ] **Step 5: Commit Task 1**

```text
git add crates/jcode-protocol/src/lib.rs crates/jcode-protocol/src/wire.rs crates/jcode-protocol/src/protocol_tests/peer_messages.rs
git commit -m "feat(peer): add read-only overview protocol"
```

---

### Task 2: Implement the session-bound server overview

**Files:**
- Modify: `crates/jcode-app-core/src/server/client_peer.rs:359-404`
- Modify: `crates/jcode-app-core/src/server/client_lightweight_control.rs:133-157`
- Modify: `crates/jcode-app-core/src/server/client_lifecycle.rs:2796-2805`
- Modify: `crates/jcode-app-core/src/server/client_lifecycle_logging.rs:50-71`
- Test: `crates/jcode-app-core/src/server/client_peer.rs` test module
- Test: `crates/jcode-app-core/src/server/client_lifecycle_tests.rs`

**Interfaces:**
- Consumes: `Request::PeerOverview { id, session_id }`.
- Produces: `ServerEvent::PeerOverviewResult` or `ServerEvent::Error`.
- Security rule: `session_id` must resolve to exactly one live server session. The server reads that session's stored `working_dir`; the request has no path or alias field.

- [ ] **Step 1: Write failing server tests**

Add one `OverviewFixture` helper beside the existing `live_member` fixtures. Its constructor accepts `feature_enabled: bool`, `config: PeerGroups`, and `planner_dir: &Path`, registers a live Planner session, and returns the session ID plus the `PeerServerContext` dependencies.

Write these assertions against the real `build_peer_overview` helper:

```rust
#[tokio::test]
async fn overview_derives_identity_from_live_session_working_dir() {
    let fixture = OverviewFixture::listed(true).await;
    let overview = build_peer_overview(&fixture.planner_session_id, &fixture.context())
        .await
        .expect("listed live session overview");
    assert_eq!(overview.state, PeerOverviewState::Enabled);
    assert_eq!(overview.identity.as_ref().map(|id| id.alias.as_str()), Some("Planner"));
    assert_eq!(overview.identity.as_ref().map(|id| id.group.as_str()), Some("core-team"));
    assert_eq!(overview.peers.len(), 1);
    assert_eq!(overview.peers[0].alias, "SpecScore");
}

#[tokio::test]
async fn overview_rejects_unknown_or_detached_session_id() {
    let fixture = OverviewFixture::listed(true).await;
    let error = build_peer_overview("missing-session", &fixture.context())
        .await
        .expect_err("detached session must be refused");
    assert_eq!(error, "This session is not currently attached to the Jcode server.");
}

#[tokio::test]
async fn overview_reports_disabled_invalid_and_unlisted_without_guessing() {
    let disabled = OverviewFixture::listed(false).await;
    assert_eq!(
        build_peer_overview(&disabled.planner_session_id, &disabled.context())
            .await
            .expect("disabled overview")
            .state,
        PeerOverviewState::Disabled
    );

    let invalid = OverviewFixture::invalid().await;
    let invalid_overview = build_peer_overview(&invalid.planner_session_id, &invalid.context())
        .await
        .expect("invalid configuration is a display state");
    assert_eq!(invalid_overview.state, PeerOverviewState::ConfigurationError);
    assert!(invalid_overview.error.as_deref().is_some_and(|text| text.contains("Peer groups configuration is invalid")));

    let unlisted = OverviewFixture::unlisted().await;
    let unlisted_overview = build_peer_overview(&unlisted.planner_session_id, &unlisted.context())
        .await
        .expect("unlisted overview");
    assert_eq!(unlisted_overview.state, PeerOverviewState::Unlisted);
    assert!(unlisted_overview.identity.is_none());
    assert!(unlisted_overview.peers.is_empty());
}

#[tokio::test]
async fn overview_does_not_reserve_or_consume_peer_turns() {
    let fixture = OverviewFixture::listed(true).await;
    assert!(!fixture.turn_coordinator.session_is_busy(&fixture.planner_session_id));
    assert_eq!(fixture.peer_exchanges.active_exchange_count(), 0);
    let _ = build_peer_overview(&fixture.planner_session_id, &fixture.context())
        .await
        .expect("read-only overview");
    assert!(!fixture.turn_coordinator.session_is_busy(&fixture.planner_session_id));
    assert_eq!(fixture.peer_exchanges.active_exchange_count(), 0);
}
```

- [ ] **Step 2: Run the tests and confirm RED**

Run:

```text
cargo test -p jcode-app-core overview_ -- --nocapture
```

Expected: failures because overview routing and handler logic do not exist.

- [ ] **Step 3: Add a pure overview builder**

In `client_peer.rs`, add a helper with this boundary:

```rust
async fn build_peer_overview(
    session_id: &str,
    context: &PeerServerContext<'_>,
) -> Result<PeerOverview, String>
```

Behavior:

1. If feature off, return `PeerOverviewState::Disabled` with no peers.
2. If the peer-group loader has an error, return `ConfigurationError` and the existing sanitized error text.
3. Read session snapshots and find exactly the requested live session.
4. Read that session's server-owned working directory.
5. Call `peer_groups.identity_for_dir` to derive identity.
6. If absent, return `Unlisted` with no peers.
7. Build `PeerIdentityInfo` and the other group members using existing `visible_state`.
8. Never call exchange registration, turn-coordinator reservation, wake, or notification functions.

- [ ] **Step 4: Route overview separately from exchange requests**

In lightweight control dispatch:

```rust
Request::PeerOverview { id, session_id } => {
    let event = match build_peer_overview(&session_id, &context).await {
        Ok(overview) => ServerEvent::PeerOverviewResult { id, overview },
        Err(message) => ServerEvent::Error { id, message, retry_after_secs: None },
    };
    let _ = client_event_tx.send(event);
}
```

Keep `PeerList`, `PeerSend`, `PeerReply`, and `PeerCancel` on their existing bounded exchange path. Route `PeerOverview` only through lightweight control. Add it to the full attached-client rejection match with the exact message `peer control requests require a lightweight control connection`. Add `peer_overview` to read-only lifecycle logging.

- [ ] **Step 5: Run server tests and confirm GREEN**

Run:

```text
cargo test -p jcode-app-core overview_ -- --nocapture
cargo test -p jcode-app-core client_peer -- --nocapture
cargo test -p jcode-app-core peer_ -- --nocapture
```

Expected: overview tests and all existing peer tests pass.

- [ ] **Step 6: Commit Task 2**

```text
git add crates/jcode-app-core/src/server/client_peer.rs crates/jcode-app-core/src/server/client_lightweight_control.rs crates/jcode-app-core/src/server/client_lifecycle.rs crates/jcode-app-core/src/server/client_lifecycle_logging.rs
git commit -m "feat(peer): serve session-bound overview"
```

---

### Task 3: Build the pure transcript activity extractor

**Files:**
- Create: `crates/jcode-app-core/src/peer_activity.rs`
- Modify: `crates/jcode-app-core/src/lib.rs`
- Test: inline `#[cfg(test)]` module in `peer_activity.rs`

**Interfaces:**
- Produces:

```rust
pub enum PeerActivityDirection { Inbound, Outbound }
pub enum PeerActivityOutcome {
    Sent,
    Replied,
    CompletedWithoutReply,
    Failed,
    TimedOut,
    Cancelled,
    Received,
    OutcomeUnavailable,
}
pub struct PeerActivity {
    pub occurred_at: Option<DateTime<Utc>>,
    pub direction: PeerActivityDirection,
    pub peer_alias: String,
    pub peer_project: Option<String>,
    pub outcome: PeerActivityOutcome,
}
pub fn extract_peer_activities(messages: &[StoredMessage]) -> Vec<PeerActivity>
```

- [ ] **Step 1: Write failing extractor tests**

Create real `StoredMessage` values with `ContentBlock::ToolUse` and `ContentBlock::ToolResult`.

Tests must prove:

- `display_role == Peer` yields one inbound `Received` record.
- Outbound `peer` action `send` pairs by tool-use ID with its result.
- `Peer reply from`, `completed ... without replying`, `failed`, `timed out`, and `cancelled` map to exact outcomes.
- Missing result yields `OutcomeUnavailable`.
- Non-send peer actions are ignored.
- Message and reply body strings do not appear in `PeerActivity` debug/rendered fields.
- Malformed JSON input and legacy blocks do not panic.

- [ ] **Step 2: Run extractor tests and confirm RED**

Run:

```text
cargo test -p jcode-app-core peer_activity -- --nocapture
```

Expected: compile failure because `peer_activity` does not exist.

- [ ] **Step 3: Implement minimal extraction**

Use one pass to remember outbound peer send tool calls by ID and a second correlation map for results. Parse only fields needed for aliases and outcomes. Do not retain `message`, `reply`, `error`, raw input JSON, or full result content after classification.

Inbound aliases come from the verified prefix already stored by the peer path:

```text
Verified peer message from <alias> (`<project>`)
```

If the prefix cannot be safely parsed, use `Unknown peer` and no project rather than displaying raw body text.

- [ ] **Step 4: Run extractor tests and confirm GREEN**

Run:

```text
cargo test -p jcode-app-core peer_activity -- --nocapture
```

Expected: all extractor tests pass with no message-body leakage.

- [ ] **Step 5: Commit Task 3**

```text
git add crates/jcode-app-core/src/lib.rs crates/jcode-app-core/src/peer_activity.rs
git commit -m "feat(peer): extract sanitized transcript activity"
```

---

### Task 4: Add the bounded durable history scanner and overview client

**Files:**
- Extend: `crates/jcode-app-core/src/peer_activity.rs`
- Create: `crates/jcode-app-core/src/peer_overview.rs`
- Modify: `crates/jcode-app-core/src/lib.rs`
- Test: inline tests using temporary Jcode session storage

**Interfaces:**
- Produces:

```rust
pub struct PeerActivityReport {
    pub activities: Vec<PeerActivity>,
    pub history_limited: bool,
    pub read_errors: usize,
}

pub fn load_recent_peer_activity(
    canonical_working_dir: &Path,
) -> Result<PeerActivityReport>

pub async fn fetch_peer_overview(session_id: &str) -> Result<PeerOverview>
```

- [ ] **Step 1: Write failing persistence and bounds tests**

Use a temporary `JCODE_HOME` and real `Session::save()` snapshots.

Test:

- An older persisted session in the same canonical workspace is visible from a fresh session.
- A different workspace is excluded.
- Results are newest-first and capped at five.
- Only 12 newest matching sessions are considered.
- A snapshot over 2 MiB is skipped and sets `history_limited`.
- Only the newest 500 messages per parsed session are inspected.
- Malformed snapshot files increment `read_errors` and do not panic.

- [ ] **Step 2: Run scanner tests and confirm RED**

Run:

```text
cargo test -p jcode-app-core recent_peer_activity -- --nocapture
```

Expected: compile failure because the scanner does not exist.

- [ ] **Step 3: Implement bounded session discovery and loading**

Implementation rules:

1. Resolve the local sessions directory through `crate::storage::jcode_dir()?.join("sessions")`.
2. Collect `.json` snapshots with metadata only.
3. Sort by modified time newest-first.
4. Read startup/session metadata only until 12 canonical working-directory matches are found.
5. Skip a file over `2 * 1024 * 1024` bytes and set `history_limited`.
6. Load matching sessions with `Session::load_from_path`, which merges supported journal persistence.
7. Slice to the newest 500 messages.
8. Extract activities and stop after enough newest records are known.
9. Sort by timestamp with session update/order fallback, deduplicate correlated records, truncate to five.

- [ ] **Step 4: Implement the read-only overview socket client**

In `peer_overview.rs`, call the existing crate-private communicate transport:

```rust
pub async fn fetch_peer_overview(session_id: &str) -> Result<PeerOverview> {
    let request = Request::PeerOverview {
        id: 1,
        session_id: session_id.to_string(),
    };
    match crate::tool::communicate::transport::send_request(request).await? {
        ServerEvent::PeerOverviewResult { overview, .. } => Ok(overview),
        ServerEvent::Error { message, .. } => Err(anyhow::anyhow!(message)),
        event => Err(anyhow::anyhow!("Unexpected peer overview response: {event:?}")),
    }
}
```

- [ ] **Step 5: Run scanner and client tests and confirm GREEN**

Run:

```text
cargo test -p jcode-app-core recent_peer_activity -- --nocapture
cargo test -p jcode-app-core peer_overview -- --nocapture
```

Expected: all tests pass.

- [ ] **Step 6: Commit Task 4**

```text
git add crates/jcode-app-core/src/lib.rs crates/jcode-app-core/src/peer_activity.rs crates/jcode-app-core/src/peer_overview.rs
git commit -m "feat(peer): load durable bounded activity"
```

---

### Task 5: Add the native `/peers` TUI command and card

**Files:**
- Create: `crates/jcode-tui/src/tui/app/commands_peers.rs`
- Modify: `crates/jcode-base/src/bus.rs:268-272,394-467`
- Modify: `crates/jcode-tui/src/tui/app.rs`
- Modify: `crates/jcode-tui/src/tui/app/state_ui_input_helpers.rs:39-230`
- Modify: `crates/jcode-tui/src/tui/app/commands.rs:1703-1724`
- Modify: `crates/jcode-tui/src/tui/app/local.rs:153-182`
- Modify: `crates/jcode-tui/src/tui/app/remote.rs:559-590`
- Test: `commands_peers.rs` and existing command-registration tests

**Interfaces:**
- Produces:

```rust
pub struct PeerOverviewCompleted {
    pub session_id: String,
    pub result: Result<String, String>,
}

pub(super) fn handle_peers_command(app: &mut App, trimmed: &str) -> bool
pub(super) fn handle_peer_overview_completed(app: &mut App, event: PeerOverviewCompleted)
```

- [ ] **Step 1: Write failing registration, dispatch, rendering, and no-model tests**

Tests must prove:

- `/peers` is registered exactly once with `Show approved peer projects, live availability, and recent activity`.
- `/peers extra` renders `Usage: /peers`.
- Dispatch claims `/peers` through the shared command table.
- The handler spawns background read work and does not queue or start a model prompt.
- Disabled state performs no history scan.
- Enabled state renders identity, Ambient state, four peer states, and latest five activities.
- Rendered output contains no message body, full path, session ID, capability, or message/exchange ID.
- Completion for another session is ignored.

- [ ] **Step 2: Run TUI tests and confirm RED**

Run:

```text
cargo test -p jcode-tui peers_command -- --nocapture
cargo test -p jcode-tui registered_commands_have_no_duplicate_names -- --nocapture
```

Expected: failures because the command and bus event do not exist.

- [ ] **Step 3: Add the bus event and module wiring**

Add:

```rust
#[derive(Clone, Debug)]
pub struct PeerOverviewCompleted {
    pub session_id: String,
    pub result: std::result::Result<String, String>,
}
```

Add the corresponding `BusEvent` variant and route it through both local and remote bus handlers to one shared completion function.

- [ ] **Step 4: Implement command orchestration**

Behavior:

1. Accept only `/peers`.
2. Capture active session ID, active working directory, and current Ambient enabled state.
3. Save the current local session first so newest activity is durable.
4. Set `Peer overview loading...` status.
5. Spawn one background thread with a small Tokio runtime.
6. Fetch live overview first.
7. If state is `Disabled` or `ConfigurationError`, skip history.
8. Canonicalize the active workspace and load durable activity for `Enabled` and `Unlisted`; skip history for `Disabled` and `ConfigurationError`.
9. Render one sanitized Markdown/text card.
10. Publish `PeerOverviewCompleted`.

- [ ] **Step 5: Implement the approved renderer**

Use these headings and labels:

```text
Peer Messaging: ON|OFF|CONFIGURATION ERROR
You: <alias> (`<project>`) · group `<group>`
Ambient initiation: OFF|ON (peer initiation unavailable)

Approved peers
● <alias> · ready · <project>
◐ <alias> · busy · <project>
○ <alias> · offline · <project>
! <alias> · ambiguous · <project>

Recent activity
<time> → <alias> · <outcome>
<time> ← <alias> · <outcome>
```

If history was bounded or partially unreadable, add one final plain line:

```text
Some older activity was skipped to keep this view fast.
```

- [ ] **Step 6: Run TUI tests and confirm GREEN**

Run:

```text
cargo test -p jcode-tui peers_command -- --nocapture
cargo test -p jcode-tui registered_commands_have_no_duplicate_names -- --nocapture
cargo test -p jcode-tui both_entry_points_use_the_shared_dispatch_table -- --nocapture
```

Expected: all tests pass.

- [ ] **Step 7: Commit Task 5**

```text
git add crates/jcode-base/src/bus.rs crates/jcode-tui/src/tui/app.rs crates/jcode-tui/src/tui/app/commands_peers.rs crates/jcode-tui/src/tui/app/state_ui_input_helpers.rs crates/jcode-tui/src/tui/app/commands.rs crates/jcode-tui/src/tui/app/local.rs crates/jcode-tui/src/tui/app/remote.rs
git commit -m "feat(tui): add native peers overview"
```

---

### Task 6: Documentation and regression proof

**Files:**
- Modify: `docs/PEER_MESSAGING.md`
- Modify tests only where broader regression coverage requires it

- [ ] **Step 1: Update the user guide**

Document:

- Type `/peers` in an interactive session.
- It makes no model call and costs no provider tokens.
- Live state comes from the current server.
- Latest five activities come from existing transcripts and survive restart.
- Message and reply bodies are deliberately omitted.
- Feature-off, invalid-config, unlisted-workspace, no-history, and server-unavailable wording.
- Ambient Mode is reported but is not enabled and cannot initiate peer messages.

- [ ] **Step 2: Run all focused peer and affected-crate tests**

Run:

```text
cargo test -p jcode-protocol peer_ -- --nocapture
cargo test -p jcode-app-core peer_ -- --nocapture
cargo test -p jcode-tui peers_command -- --nocapture
cargo test -p jcode-tui registered_commands_have_no_duplicate_names -- --nocapture
cargo test -p jcode-tui both_entry_points_use_the_shared_dispatch_table -- --nocapture
```

Expected: zero failures.

- [ ] **Step 3: Recheck feature-off invariants**

Run the existing baseline tests proving the model-facing peer tool disappears when disabled:

```text
cargo test -p jcode-app-core feature_off_tool_definitions_match_the_pre_peer_baseline -- --nocapture
cargo test -p jcode-app-core feature_off_real_agent_surfaces_do_not_expose_peer -- --nocapture
```

Expected: both pass.

- [ ] **Step 4: Commit Task 6**

```text
git add docs/PEER_MESSAGING.md
git commit -m "docs(peer): explain native peers overview"
```

---

### Task 7: Full verification, latest binary activation, dogfood, and report

**Files:**
- Create after verification: `docs/reports/jcode-peer-messaging-phase-4-completion-2026-08-06.html`
- No production edits unless verification exposes a test-backed defect.

- [ ] **Step 1: Run formatting and strict lint**

```text
cargo fmt --all -- --check
cargo clippy --workspace -- -D warnings
```

Expected: exit code 0.

- [ ] **Step 2: Run affected crate suites and workspace test compilation**

```text
cargo test -p jcode-protocol
cargo test -p jcode-app-core
cargo test -p jcode-tui --no-run
cargo test --workspace --no-run
```

Expected: exit code 0 for each command. If the complete TUI runtime suite exceeds the harness limit, report that honestly and use the compiled suite plus focused tests as evidence.

- [ ] **Step 3: Verify repository scope and commit any test-backed corrections**

```text
git status --short
git diff --check
git diff --name-only
git log --oneline -8
```

Stage only intended files. Commit corrections in focused commits. Confirm the tree is clean before building.

- [ ] **Step 4: Build the exact current commit**

```text
cargo build --profile selfdev -p jcode --bin jcode
```

Expected: exit code 0. Record the full Git hash before reload.

- [ ] **Step 5: Reload onto the new binary**

Use `selfdev reload` after the successful build. Continue automatically after reload.

- [ ] **Step 6: Dogfood `/peers` inside the new binary**

Verify:

- `/version` reports the expected Git hash.
- `/peers` renders without invoking a model.
- Current identity/group are correct.
- Approved peers have truthful live states.
- Durable history is visible after reload.
- No message body or sensitive identifier is shown.
- Peer messaging remains enabled.
- Ambient Mode remains disabled.
- Existing `peer list/send/reply` safety tests remain green.

- [ ] **Step 7: Create the detailed plain-English HTML report**

The report must explain for a non-coder:

- What was added and what Michael types.
- Why it is useful.
- Why existing transcripts were reused instead of making another database.
- What live state means.
- How privacy is protected.
- What did not change.
- Every test/build command and its outcome.
- Every commit created.
- The exact running Jcode version and hash.
- Confirmation that Ambient Mode is off.
- Any remaining limitation or recommended next step.

Include simple diagrams showing `/peers` reading live status and historical transcripts without contacting a model.

- [ ] **Step 8: Open the report in a new Firefox tab**

Use the browser bridge, verify the title, capture a screenshot, and confirm the report is readable at normal zoom.

- [ ] **Step 9: Commit the completion report and confirm final alignment**

```text
git add docs/reports/jcode-peer-messaging-phase-4-completion-2026-08-06.html
git commit -m "docs(peer): report phase 4 completion"
git status --short
git rev-parse HEAD
```

If the report commit changes the final Git hash after the binary build, rebuild and reload once more so the running binary matches the final committed repository exactly. Re-run `/version` and `/peers` after that final reload.
