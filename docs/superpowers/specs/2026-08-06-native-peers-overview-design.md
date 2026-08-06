# Native `/peers` Overview Design

- **Date:** 2026-08-06
- **Status:** Approved design, specification review pending
- **Owner:** Michael
- **Implementation owner:** Jcode
- **Depends on:** `docs/superpowers/specs/2026-08-04-cross-repository-peer-messaging-design.md`

## 1. Purpose

Add a native, read-only `/peers` command that lets Michael understand peer
messaging at a glance without asking a model, spending tokens, or inspecting
configuration files.

The command must answer four questions:

1. Is peer messaging enabled and healthy?
2. Which approved peer identity does this exact workspace have?
3. Which approved peers are ready, busy, offline, or ambiguous right now?
4. What were the latest five peer exchanges involving this exact workspace,
   including exchanges recorded before the current Jcode process started?

This phase improves visibility only. It does not expand who can communicate,
when messages can be sent, or how many replies are allowed.

## 2. Plain-English outcome

Michael types `/peers` in a normal interactive Jcode session and immediately
sees a small card in the chat:

```text
Peer Messaging: ON
You: Planner (`jcode`) · group `core-team`
Ambient initiation: OFF

Approved peers
● SpecScore  · ready   · BlueprintMyApp
◐ Strategy   · busy    · healthview-platform
○ Eve        · offline · healthview-app

Recent activity
17:52 → SpecScore · replied
16:41 ← Eve       · completed without reply
Yesterday → Atlas · timed out
```

The command runs locally. It does not start a model turn and does not consume
provider tokens.

## 3. Chosen approach

### 3.1 Live overview from the shared server

The TUI asks the existing shared Jcode server for a read-only peer overview over
the local lightweight-control transport and includes the interactive session ID.
The server requires that ID to name a currently live session and derives the
visible identity only from that session's pinned allowlist identity.

The server returns:

- Feature state.
- Peer-group configuration health.
- The current session's approved peer identity, if any.
- Group name.
- Other approved group members.
- Each member's project label and current visible state.

The request inherits the existing local-daemon socket trust model. The server
validates that the requested session is live, but the lightweight connection is
not cryptographically bound to the TUI attachment and carries no server-minted
per-request capability. Therefore, a local process that can access the daemon
socket and learn another live session ID could request that session's sanitized
overview. This is accepted for the current single-user local daemon and must be
revisited before the socket is treated as a multi-user or network security
boundary. The route does not use the model-only hidden turn capability because
`/peers` is a user command, not a model tool call.

The route must be incapable of:

- Sending a message.
- Replying to a message.
- Waking or interrupting a session.
- Reserving a turn.
- Changing configuration.
- Changing peer identity.

The existing model-facing `peer list`, `peer send`, and `peer reply` behavior
remains unchanged.

### 3.2 Durable history from existing transcripts

Recent activity is derived from Jcode's existing persisted session transcripts.
There is no new peer-history database or journal.

The TUI performs a bounded scan of recent local Jcode sessions whose canonical
working directory exactly matches the current session's canonical working
directory. It extracts structured peer activity from:

- Inbound user messages carrying the stored peer display role.
- Outbound `peer` tool calls with action `send`.
- Correlated peer tool results and terminal outcomes.

The scanner returns the latest five complete or meaningful activity records in
newest-first order.

This survives a Jcode restart because the source transcripts already survive a
restart. It also survives opening a fresh session in the same exact workspace
because the bounded scan includes recent sessions for that canonical working
directory.

### 3.3 Why this approach was chosen

The first peer-messaging design made normal session transcripts the visible
audit trail. Reusing that source of truth:

- Avoids duplicating sensitive activity into a second file.
- Avoids retention, cleanup, corruption, and migration rules for another store.
- Preserves truthful provenance already recorded in session history.
- Keeps the feature small and reversible.

## 4. Command behavior

### 4.1 Registration

Register `/peers` as a public slash command with this help text:

> Show approved peer projects, live availability, and recent activity

It must be routed through the single shared local command dispatch table so
local and disconnected-remote entry points cannot drift.

The command remains discoverable when peer messaging is disabled. In that
state it explains that the feature is off and performs no history scan.

### 4.2 Presentation

Render one compact, non-modal card in the transcript, following existing local
status-card patterns.

The card contains these sections in order:

1. **Peer Messaging**
   - `ON`, `OFF`, or `CONFIGURATION ERROR`.
2. **You**
   - Approved alias, project label, and group.
   - If this exact directory is not allowlisted, say so plainly.
3. **Ambient initiation**
   - Always report whether Ambient Mode is enabled.
   - This phase does not enable it or grant it peer initiation.
4. **Approved peers**
   - Alias.
   - Project label.
   - State: `ready`, `busy`, `offline`, or `ambiguous`.
5. **Recent activity**
   - Up to five entries.
   - Timestamp or a clear date label.
   - Direction relative to the current workspace.
   - Peer alias or truthful fallback label.
   - Outcome.

### 4.3 State language

Use plain language:

- `ready`: the peer has one unambiguous live session and can currently receive.
- `busy`: the peer is live but is running or reserved.
- `offline`: no matching live session is attached to this server.
- `ambiguous`: more than one live session currently claims that peer identity,
  so Jcode refuses to guess.

The view reports state only. It never automatically retries or contacts a peer.

## 5. Durable activity model

### 5.1 Activity shape

Use a small internal view model with exactly these fields:

- Optional occurred-at timestamp.
- Direction: inbound or outbound.
- Peer alias.
- Peer project label when known.
- Exchange or tool-call identifier when available for correlation only.
- Outcome.

The display model must not contain the message body.

### 5.2 Outcomes

Supported display outcomes:

- `sent`
- `replied`
- `completed without reply`
- `failed`
- `timed out`
- `cancelled`
- `received`
- `outcome unavailable`

When an outbound tool call exists without a correlated terminal result, show
`outcome unavailable` rather than inventing success.

### 5.3 Privacy and data minimization

The `/peers` overview must not display:

- Peer message bodies.
- Reply bodies.
- Full session IDs.
- Hidden turn capabilities.
- Exchange tokens or secrets.
- Full filesystem paths.
- Provider prompts or model reasoning.

The command may show approved aliases, short project labels, direction, time,
and outcome because those are necessary for the requested overview.

### 5.4 Bounded scanning

History scanning uses these fixed first-release limits:

- Consider at most the 256 most recently updated local Jcode session files as
  candidates, regardless of workspace. If older candidates remain, record that
  history was partially limited.
- From that candidate window, load at most 12 sessions whose canonical working
  directory matches the current workspace.
- Skip any individual transcript larger than 2 MiB and record that history was
  partially limited.
- Inspect at most the newest 500 persisted messages in each parsed transcript.
- Stop after five newest activity entries have been collected and cannot be
  displaced by any remaining newer session.
- Use no network access and make no model call.

Sessions are considered in newest-updated-first order. Activities inside one
session use persisted message timestamps when available and transcript order as
the stable fallback. A missing timestamp is displayed as `time unavailable`
rather than guessed.

If older candidates, matching sessions, or unusually large transcripts fall
outside those limits, the command shows the newest evidence it found and does
not claim completeness beyond the latest five displayed entries.

## 6. Error and empty states

### 6.1 Feature disabled

Show:

```text
Peer Messaging: OFF
No peer status or history was read.
Enable it in Jcode configuration when you want to use it.
```

Do not expose the peer model tool and do not scan transcript history.

### 6.2 Invalid peer-group configuration

Show the existing plain configuration error without weakening validation.
Do not guess an identity and do not list peers.

### 6.3 Current directory not allowlisted

Show that peer messaging is enabled but this exact directory has no approved
identity. Do not infer identity from repository name, parent directory, Git
root, or another live session.

### 6.4 Server unavailable

Show that live availability could not be loaded. Durable transcript history may
still be shown if it can be read safely, clearly labelled as historical rather
than live.

### 6.5 No recent activity

Show `No peer exchanges found for this workspace.` This is not an error.

### 6.6 Partial or malformed historical entries

Skip entries that cannot be identified as peer activity. For a recognizable
outbound exchange with an unreadable or missing result, show
`outcome unavailable`. Never panic on an old or malformed transcript.

## 7. Security boundaries

This phase must preserve every existing peer-messaging boundary:

- Exact canonical directory allowlisting.
- Same-server live delivery only.
- Idle-recipient requirement.
- One normal user-directed sender turn per exchange.
- At most one reply.
- No arbitrary session-ID targeting.
- No busy-session queue.
- Truthful peer provenance.
- Default-off feature flag.

The new server request is read-only and session-bound. It must not accept an
arbitrary working directory or alias supplied by the caller as authority.
The server derives identity from the requesting attached session and its
canonical working directory.

Transcript-derived history is informational only. Historical aliases or tool
arguments never become authorization inputs for live peer messaging.

## 8. Ambient Mode

Ambient Mode remains disabled in Michael's active configuration during this
phase.

The `/peers` card reports Ambient's current enabled/disabled state for clarity,
but this work does not:

- Enable Ambient Mode.
- Let Ambient initiate a peer message.
- Add scheduled peer messages.
- Add unattended retries.
- Change Ambient permissions or tools.

A future Ambient rollout remains a separate, explicitly approved project.

## 9. Architecture boundaries

Keep the implementation separated into small responsibilities:

1. **Protocol types**
   - Read-only overview request and response types.
2. **Server overview handler**
   - Derive session-bound identity and live peer states.
3. **Transcript activity extractor**
   - Pure, testable conversion from persisted messages to sanitized activity.
4. **Bounded session-history reader**
   - Find recent sessions for one canonical working directory and enforce
     limits.
5. **TUI command handler and renderer**
   - Coordinate live overview plus durable history and append the card.

The transcript parser must not depend on the TUI renderer. The live overview
handler must not read historical transcripts. Live authorization and historical
presentation remain separate.

## 10. Test requirements

Follow test-driven development. Each behavior begins with a failing test.

### 10.1 Command tests

- `/peers` is registered exactly once with public help text.
- The shared dispatch table handles it from both supported in-process entry
  paths.
- Running `/peers` does not start a model turn.

### 10.2 Protocol and server tests

- Overview request/response JSON round-trip.
- Feature-off response.
- Invalid configuration response.
- Exact-directory identity success.
- Unlisted-directory response.
- Ready, busy, offline, and ambiguous peer states.
- Caller cannot supply another directory or alias as authority.
- Overview request cannot send, reply, reserve, wake, or mutate.

### 10.3 History extraction tests

- Inbound peer display-role message becomes one sanitized inbound record.
- Outbound send plus reply result becomes one `replied` record.
- Completed-without-reply, failed, timed-out, and cancelled outcomes map
  correctly.
- Missing result becomes `outcome unavailable`.
- Message and reply bodies never appear in the rendered overview.
- Full paths, capabilities, and session IDs never appear.
- Malformed and legacy transcript entries do not panic.
- Results are newest-first and limited to five.

### 10.4 Persistence and bounds tests

- Activity remains visible after reconstructing state from temporary persisted
  session files, simulating a restart.
- A fresh session in the same canonical workspace can see recent activity from
  an older session.
- A different workspace cannot see that activity.
- Session-count and transcript-size limits are enforced.

### 10.5 Regression tests

- Existing peer tool schemas and actions remain unchanged.
- Feature-off model-facing tool definitions remain at the recorded baseline.
- Existing 42 peer-focused tests continue to pass.
- Existing TUI slash-command registration and dispatch invariants continue to
  pass.

## 11. Verification and activation

Before activation:

1. Run focused parser, protocol, server, and TUI tests.
2. Run the full peer-focused test set.
3. Run `cargo fmt --all -- --check`.
4. Run `cargo clippy --workspace -- -D warnings`.
5. Run broader affected-crate tests and compile the workspace tests.
6. Confirm Git contains only intended files.
7. Commit implementation changes.
8. Build the self-development Jcode binary on Windows with plain Cargo.
9. Reload onto that exact commit.
10. Run `/peers` inside the new binary and verify the rendered result.
11. Confirm peer messaging remains enabled and Ambient Mode remains disabled.
12. Confirm rollback remains available by disabling the peer feature and
    restarting.

## 12. Documentation and completion report

Update `docs/PEER_MESSAGING.md` with the `/peers` command and its privacy model.

Create a detailed plain-English completion report that explains:

- What `/peers` does.
- Why transcript-derived history was chosen.
- What is live and what is historical.
- What privacy protections apply.
- What tests passed.
- Which Git commits contain the work.
- Which Jcode binary is running.
- Whether Ambient Mode is enabled.
- What, if anything, remains to be done.

Open the report in a new Firefox tab for Michael after the new binary is live
and dogfooded.

## 13. Explicit non-goals

This phase does not add:

- A peer message composer for Michael.
- Manual `/peer send` slash syntax.
- Message-body previews in `/peers`.
- A global peer-history database.
- Cross-machine messaging.
- Offline delivery or a durable inbox.
- Busy-recipient queuing.
- Group chat.
- Automatic retries.
- Ambient or scheduled peer initiation.
- Changes to peer authorization or reply limits.

## 14. Success criteria

The phase is complete only when:

- `/peers` works without a model call.
- Live status comes from the server and is bound to the attached session.
- Recent activity survives restart by using existing persisted transcripts.
- The latest five entries reveal no message bodies or sensitive identifiers.
- All existing peer safety limits remain intact.
- Tests, formatting, and strict workspace Clippy pass.
- All intended work is committed.
- The running Jcode binary matches the final verified commit.
- Ambient Mode remains disabled.
- The detailed plain-English Firefox report is open.
