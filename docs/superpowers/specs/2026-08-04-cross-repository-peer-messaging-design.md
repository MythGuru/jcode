# Cross-Repository Peer Messaging Design

- **Date:** 2026-08-04
- **Status:** Source-verified design candidate; awaiting Michael approval
- **Owner:** Michael
- **Design:** Jcode
- **Initial use case:** Atlas in `healthview-platform` and Eve in `healthview-app`

## 1. Purpose

Add a small, native jcode communication path that lets explicitly approved agent
roles in different repositories send a message and receive one optional reply
without Michael manually copying text between terminals.

The feature must preserve four properties:

1. **Truthful provenance.** A peer message must never look as though Michael sent
   it.
2. **Explicit trust.** Only allowlisted local project roles may contact one
   another.
3. **Bounded autonomy.** One initial message may produce at most one reply. It
   must never create an autonomous agent loop.
4. **Visible operation.** Both terminals must show what was exchanged. There is
   no hidden side conversation.

This is a local jcode capability. It does not require a cloud service, database,
network account, or new dependency.

## 2. User outcome

Michael can leave Atlas and Eve open in their normal project directories on the
same jcode server. During a normal user-directed turn, either agent can ask the
other a focused question or send a handoff.

The sender's `peer send` tool call waits while one idle recipient turn runs. The
recipient may call `peer reply` once. When the recipient turn finishes, the
sender's waiting tool call returns one of these truthful outcomes:

- The recipient replied, including the reply body.
- The recipient completed without replying.
- The recipient turn failed, timed out, or was cancelled.

The original sender then continues its existing user-directed turn and may act
on the tool result. No second sender model turn is created. The initial-send
permit is consumed, so a new peer round requires a new normal user-directed
turn.

Michael remains in control without acting as the transport layer.

## 3. Existing jcode capabilities

### 3.1 Single server, multiple sessions

jcode uses one per-user server for its live local sessions. The server already
holds session agents, client connections, working directories, status, and
notification fanout.

Reference: `docs/SERVER_ARCHITECTURE.md`.

Runtime verification on 2026-08-04 showed the same registered server could see:

- Atlas: `healthview-platform`
- Eve: `healthview-app`
- Other unrelated live project sessions

The first release is explicitly same-server only. If the two sessions are
connected to different jcode servers, each sees the other as offline.

### 3.2 Swarm direct messages

The `swarm` tool already supports direct messages, delivery modes, friendly-name
resolution, notifications, live-turn wake, soft interrupts, and TUI
presentation.

Relevant code:

- `crates/jcode-app-core/src/tool/communicate.rs`
- `crates/jcode-app-core/src/server/client_comm_message.rs`
- `crates/jcode-app-core/src/server/live_turn.rs`
- `crates/jcode-tui/src/tui/app/remote_notifications.rs`

However, `swarm_id_for_dir` derives identity from the Git common directory.
Sessions in different repositories therefore belong to different swarms and the
server rejects cross-repository DMs.

Reference:
`crates/jcode-app-core/src/server/util.rs`, function `swarm_id_for_dir`.

This is correct for swarm ownership and must not be weakened globally. The
`JCODE_SWARM_ID` override also remains unchanged and is not used for peer
identity or peer authorization.

### 3.3 Dashboard-to-session messaging

The local dashboard can send Michael-authored input to any live session through
the debug socket. It supports idle and busy targets.

Relevant code:

- `tools/dashboard/server.js`, functions `sendTranscript` and
  `queueDashboardMessage`
- `tools/dashboard/SPEC-cockpit.md`

This proves cross-repository delivery is technically possible today. It is not
an agent-to-agent channel because it deliberately presents messages as coming
from Michael, depends on unstable session IDs, and has no reply-budget
enforcement.

### 3.4 Cross-session delivery primitives

The server already has useful pieces for cross-session work:

- `run_live_turn_if_idle` starts a tracked turn for an idle live agent.
- Notifications can be fanned out to attached clients.
- Soft interrupts can be queued or persisted for busy or detached sessions.

Relevant code:

- `crates/jcode-app-core/src/server/client_actions.rs`,
  `handle_notify_session`
- `crates/jcode-app-core/src/server/live_turn.rs`,
  `run_live_turn_if_idle`
- `crates/jcode-app-core/src/server/state.rs`,
  `queue_soft_interrupt_for_session`
- `crates/jcode-app-core/src/agent/interrupts.rs`,
  `restore_persisted_soft_interrupts`

The first peer release reuses notification presentation and the tracked-turn
execution machinery, but it must not treat the current `idle_live_agent` check as
an authorization boundary. That helper checks and releases the agent lock before
the turn is spawned, so it is only an informational snapshot. Peer delivery needs
a new atomic server turn lease described in section 8.3.

The release deliberately does **not** use soft interrupts for peer message bodies.
A soft interrupt may remain queued after a cancelled or failed busy turn and could
then appear during an unrelated later turn. Rejecting busy recipients avoids that
unsafe ambiguity.

## 4. Approaches considered

### 4.1 Dashboard API calls from agents

An agent could call the local dashboard HTTP endpoint and target another live
session.

Rejected because:

- The message would falsely claim to be from Michael.
- The sender must discover an unstable session ID.
- The dashboard must be running.
- There is no allowlisted role boundary.
- There is no server-enforced reply limit.

The dashboard remains a Michael-to-agent control surface only.

### 4.2 Shared filesystem mailbox

Atlas and Eve could write JSON messages under `~/.jcode/` and poll for changes.

Rejected because:

- It duplicates delivery, wake, queue, acknowledgement, and recovery machinery.
- Polling adds delay and background activity.
- Concurrent writes and duplicate delivery become a second messaging system.
- It creates a larger prompt-injection surface.

A role inbox may be justified later for delivery into brand-new sessions. It is
not needed for the first useful version.

### 4.3 Native allowlisted peer messaging

Recommended.

Add a focused `peer` tool and server route that reuse jcode's existing
live-session and tracked-turn infrastructure without changing swarm ownership.
For the first release, delivery is synchronous and idle-only.

## 5. Scope

### 5.1 First release

The first release includes:

- A default-off feature flag.
- A global allowlist of named peer groups.
- Stable aliases resolved from canonical working directories.
- `peer list`, `peer send`, and `peer reply` actions.
- Truthful peer provenance in stored history and the TUI.
- Idle live-session delivery only.
- A synchronous, bounded send result.
- One active peer exchange per participating session.
- A server-enforced one-reply budget.
- A server-minted, model-invisible turn capability for caller authentication.
- A central, atomic server turn lease for peer and non-peer live turns.
- Plain-English errors and outcomes.
- Unit, protocol, server, TUI, and live dogfood verification.

### 5.2 Explicit non-goals

The first release does not include:

- Delivery to a recipient that is already running another turn.
- Cross-repository swarm plans, channels, broadcasts, spawning, stopping, or
  ownership.
- Arbitrary session-ID messaging.
- File attachments.
- Group conversations.
- Autonomous multi-round debate.
- Scheduled or ambient initiation of peer messages.
- A durable role inbox for sessions that do not yet exist.
- Cloud synchronization or messages between machines.
- Communication across separate jcode server processes.
- A separate message-history database.

Normal session transcripts are the visible audit trail.

## 6. Configuration and trust boundary

### 6.1 Feature flag

Add a feature configuration flag:

```toml
[features]
peer_messaging = false
```

Requirements:

- Default is `false`.
- Add an explicit environment override, `JCODE_PEER_MESSAGING_ENABLED`.
- Add that variable to `CONFIG_ENV_KEYS` so the configuration cache fingerprint
  changes when the override changes.
- Old configuration files continue to load through serde defaults.
- The stateless peer tool may be registered unconditionally in the process-global
  `base_tools` registry, but one shared peer feature filter removes its definition
  when the feature is disabled. Apply that filter at every live tool-definition
  surface: `Agent::build_filtered_tool_definitions`,
  `Agent::tool_definitions_for_debug`, and the TUI local-turn path in
  `crates/jcode-tui/src/tui/app/turn.rs` that calls
  `registry.definitions(None)` directly. Normal prompts, debug introspection, and
  local non-server turns must agree. Do not branch the `base_tools` `OnceLock` on
  the first configuration value read.
- The tool execution path itself rechecks the per-agent feature state before
  opening a socket. This protects direct execution paths that do not depend on
  the exposed definition list.
- The server handler independently rechecks the feature flag, so a direct or
  stale tool invocation cannot bypass the gate.
- When disabled, externally visible tool definitions and prompt output remain
  byte-identical to the recorded pre-feature baseline.

### 6.2 Peer groups file

When the feature is enabled, jcode reads a new file that old binaries ignore:

`~/.jcode/peer-groups.json`

Initial shape:

```json
{
  "version": 1,
  "groups": [
    {
      "name": "healthview",
      "members": [
        {
          "alias": "Atlas",
          "working_dir": "C:\\Users\\micha\\Developer\\healthview-platform"
        },
        {
          "alias": "Eve",
          "working_dir": "C:\\Users\\micha\\Developer\\healthview-app"
        }
      ]
    }
  ]
}
```

The file contains no credentials or secrets. It is loaded, validated, and
snapshotted once when the jcode server starts. Peer groups are deliberately not
read through the hot-reloading general configuration cache. A file change does
not alter identity pinning or the allowlist inside the running process, including
an active exchange; it takes effect only after a server restart.

Validation rules:

- Configuration version must be supported.
- Group names must be non-empty and globally unique.
- Aliases must be non-empty and case-insensitively unique within a group.
- Every canonical working directory must be globally unique across all groups,
  so one live session can never acquire two peer identities.
- A group must contain at least two members.
- Paths must be absolute and canonicalizable.
- A malformed file fails safely with:
  `Peer groups configuration is invalid: <reason>`.

If the feature is enabled but the file does not exist, jcode treats the
allowlist as empty. It does not create an implicit group.

### 6.3 Identity and authorization

The model-facing `peer` input contains no sender alias, sender path, sender
session ID, turn generation, or capability token. Those fields do not appear in
the tool JSON schema.

Current communication tools open a fresh short-lived socket for each request, and
the server does not derive caller identity from that connection. Therefore the
peer tool must add a hidden internal request envelope populated from
`ToolContext`, not from model input. The envelope carries:

- The current session ID.
- The current server turn generation.
- An opaque, high-entropy turn capability minted by the server when that turn
  lease began.

The server validates all three values against its active-turn registry before it
uses the claimed session. A missing, stale, mismatched, or forged capability is
rejected. Direct tool execution and standalone agent runs have no valid server
turn capability and cannot use peer messaging.

The session's peer identity is resolved only after the server's normal initial
working-directory replacement has completed. The server then canonicalizes and
pins the matching configured identity for that session. A later working-directory
change invalidates peer eligibility instead of silently changing the session's
peer alias.

For every peer request, the server:

1. Validates the hidden session ID, generation, and capability against the active
   server turn lease.
2. Reads the pinned peer identity for that validated calling session.
3. Resolves the target alias within the same configured group.
4. Finds live target sessions pinned to that exact configured member.
5. Requires exactly one target match.
6. Atomically acquires the target's peer turn lease while reserving the exchange.

Reject when:

- The sender is not configured or its identity was invalidated.
- The target is not in the sender's group.
- The sender targets itself.
- No matching live target exists on the same server.
- More than one matching live target exists.
- The target is busy.
- Either session already has an active peer exchange.

Windows path comparison is case-insensitive after canonicalization. No model tool
argument may supply an arbitrary filesystem path or session ID. The internal wire
envelope contains a server-minted identity proof that is outside the model-facing
schema.

### 6.4 Local trust boundary

jcode's current local server does not cryptographically authenticate one local
client process against another. A session working directory originates in the
local client's subscription flow before the server canonicalizes and pins it.

The turn capability prevents a model from choosing another live session through
tool arguments and rejects stale requests after a generation ends. It is not a
claim of cryptographic isolation from Michael's own operating-system account.
The first release protects against accidental misrouting, model claims, arbitrary
tool arguments, stale generations, and unconfigured projects. It does not claim
to defend against a malicious process already running as Michael's OS user that
can inspect or interfere with jcode process memory. Strong local-client
authentication would be a separate system-wide security project.

## 7. Agent-facing tool

Add a focused `peer` tool rather than stretching the meaning of `swarm`.

### 7.1 `peer list`

Returns configured peers visible to the current sender:

- Stable alias
- Group name
- Project directory basename
- State: `idle`, `busy`, `offline`, or `ambiguous`

State derivation is concrete:

- `offline`: no session has the target's pinned identity, or the unique matching
  server session has no live agent attachment or has a closed live event channel.
- `ambiguous`: more than one live session has the identity.
- `busy`: exactly one live-attached session matches, but the central turn
  coordinator already has an active lease or the agent mutex is unavailable.
- `idle`: exactly one live-attached session matches, no coordinator lease is
  active, and the agent mutex is available at the time of the snapshot.

`peer list` is informational. `peer send` always repeats authorization and idle
checks atomically, because state can change after listing.

The action does not expose unrelated sessions.

### 7.2 `peer send`

Input:

```json
{
  "action": "send",
  "to": "Atlas",
  "message": "Please review the tracker access-history design.",
  "tldr": "Review tracker access history"
}
```

Rules:

- Message must be non-empty and at most 8,000 characters.
- Reuse the existing swarm TLDR validation rule for long messages.
- Allowed only during a normal user-directed turn with an unused initiation
  permit.
- Rejected during peer-triggered, scheduled, ambient, background-completion,
  notification, swarm-DM, direct-tool, or other server-initiated contexts.
- The tool implementation adds the hidden validated turn envelope from
  `ToolContext`; the model cannot set or override it.
- The target must be live, uniquely resolvable, and idle.
- Neither session may already have an active peer exchange.
- Starting the exchange atomically consumes the sender's initiation permit for
  the remainder of that turn.
- The tool waits for the recipient turn to finish, subject to the fixed 10-minute
  server deadline in section 8.5.
- The short-lived socket uses `send_request_with_timeout` with a deadline derived
  as the server recipient deadline plus 30 seconds. It must never use the
  transport's default 30-second timeout.

The result contains:

- Server-generated message ID
- Stable sender and recipient aliases
- Recipient turn outcome
- Optional reply body
- Whether the recipient completed without replying

Example reply result:

```json
{
  "status": "replied",
  "message_id": "peer_01...",
  "from": "Atlas",
  "reply": "Reviewed. The action and timestamp payload is the correct shape."
}
```

Example no-reply result:

```json
{
  "status": "completed_without_reply",
  "message_id": "peer_01...",
  "from": "Atlas"
}
```

### 7.3 `peer reply`

Input:

```json
{
  "action": "reply",
  "message": "Reviewed. The action and timestamp payload is the correct shape."
}
```

Rules:

- Valid only inside the peer-triggered recipient turn carrying an unused reply
  token.
- The destination is fixed by server-held exchange state. The model does not
  choose it.
- The reply token is consumed atomically before the body is accepted.
- A second reply is rejected.
- A reply cannot create a new thread.
- The reply is held in the exchange until the recipient turn reaches a terminal
  outcome, then returned through the sender's waiting `peer send` tool call.

## 8. Bounded exchange and turn capabilities

### 8.1 Core invariant

One normal user-directed `peer send` may cause at most:

- One recipient model turn.
- One optional `peer reply` tool result returned to the original sender's same
  model turn.

It never creates a second sender model turn. No peer-triggered turn may initiate
another peer thread.

### 8.2 Exchange state

Maintain an in-memory exchange registry keyed by message ID, plus a
session-to-exchange index that enforces one active inbound or outbound exchange
per session. The first release is live-session only, so this state does not
survive server restarts.

Each active exchange records:

- Message ID
- Sender session ID and stable alias
- Recipient session ID and stable alias
- Creation time and deadline
- Phase: `starting`, `recipient_running`, `reply_recorded`, `completed`,
  `failed`, `timed_out`, or `cancelled`
- Whether the one reply token remains
- Optional reply body
- Recipient turn cancellation handle or equivalent server control
- Waiting sender result channel
- Sender and recipient turn generations
- Sender turn capability validation result
- Sender and recipient server turn lease IDs

Only one active exchange is allowed per participating session. This prevents
overlapping conversations, ambiguous reply tokens, and unexpected model cost.

### 8.3 Required turn context and atomic server turn leases

Current jcode tool context does not distinguish normal user turns from all
server-initiated turns. The feature must add that substrate rather than infer
origin from a stored `Role::User` message.

Add a required `TurnExecutionContext` to every public `Agent` model-turn entry
method, including `run_once`, `run_once_capture`, and
`run_once_streaming_mpsc`. The lower-level server helper
`process_message_streaming_mpsc` must also require and forward it. There is no
overload or permissive default.

The context contains:

```text
origin:
  NormalUser
  PeerInbound { exchange_id }
  ServerInitiated { kind }
  Standalone { kind }
server_session_id: optional
turn_generation: optional
turn_capability: optional opaque secret
```

Requiring the context at the actual `Agent` entry methods makes every current
caller choose an origin and causes future omitted call sites to fail compilation.
This is broader than updating only `run_live_turn_if_idle`; current direct turn
starters also include normal client processing, reload recovery, resume-all,
agent tasks, assigned swarm tasks, swarm-agent startup, Jade relay delivery,
ambient and scheduled runners, overnight work, debug jobs, CLI commands, and
other `run_once_capture` users.

For live server sessions, add one central turn coordinator keyed by session ID.
It owns monotonically increasing generations, opaque capabilities, active leases,
and cancellation notification. `begin_server_turn(session_id, origin)` acquires
the coordinator lock, verifies that no turn lease or peer reservation is active,
installs the new generation and capability, then returns a generation-aware guard.
Every server path that starts a model turn must use this coordinator before
calling an `Agent` turn method.

For peer delivery, one coordinator transaction validates the sender's active
`NormalUser` lease, consumes its send permit, verifies no exchange already uses
either session, and installs the target's `PeerInbound` lease plus both exchange
reservations. The lock is released only after the reservation is committed. The
existing `idle_live_agent` result may inform `peer list`, but it is never the
authorization or atomic-start mechanism. If acquiring the target agent mutex
unexpectedly fails after the lease is installed, the guard rolls back the peer
lease and exchange before returning busy.

The peer server route converts validated origins into one-use capabilities:

- `NormalUser` with a matching live server capability: `can_send = true`,
  `can_reply = false`.
- `PeerInbound` with the matching exchange: `can_send = false`,
  `can_reply = true`.
- `ServerInitiated`, `Standalone`, stale, forged, or missing capability: both
  false.

A successful `peer send` consumes `can_send` for the current generation. A
successful `peer reply` consumes `can_reply` for the current exchange. The lease
guard clears only the generation it installed on success, failure, cancellation,
timeout, panic, or session removal, so stale cleanup cannot erase a newer turn.

`TurnExecutionContext` is propagated into each `ToolContext`. The peer tool copies
the hidden session, generation, and capability into its internal wire request;
they never enter the model-facing tool schema.

### 8.4 Turn sequence

#### Initial send

1. Eve calls `peer send` during a normal user-directed turn.
2. The server validates Eve's hidden session, generation, and turn capability.
3. One turn-coordinator transaction consumes Eve's send permit, reserves Eve and
   Atlas, and installs Atlas's `PeerInbound { exchange_id }` turn lease.
4. The server starts one Atlas turn using the exact context from that lease.
5. Eve's `peer send` tool call waits without starting another Eve turn.
6. Atlas cannot call `peer send` and may call `peer reply` once.

#### Reply

1. Atlas calls `peer reply`.
2. The server atomically consumes the reply token and records the reply body.
3. Atlas's turn may finish normally, but cannot reply or send again.
4. When Atlas's turn finishes, the server returns the reply through Eve's waiting
   `peer send` tool result.
5. Eve continues the same user-directed turn. Her send permit remains consumed.
6. The exchange closes and both session reservations are released.

#### No reply

If Atlas finishes without calling `peer reply`, the server returns
`completed_without_reply` to Eve's waiting tool call and closes the exchange.
There is no synthetic reply and no silent success.

#### Recipient failure

If Atlas fails before replying, the tool returns a failed recipient outcome. If
Atlas records a reply and then fails, the result returns the reply together with
`recipient_outcome: failed_after_reply`, rather than silently discarding either
fact.

### 8.5 Timeout and cancellation

The first release uses a fixed 10-minute server deadline for the complete
recipient turn, with an injectable duration in tests. The peer tool must call
`send_request_with_timeout` with a socket deadline of 10 minutes plus 30 seconds.
The socket deadline is derived from the server deadline in one helper so the two
cannot drift. The transport's default 30-second timeout is forbidden for peer
send.

On timeout:

- Mark the exchange `timed_out`.
- Consume and invalidate any remaining reply token.
- Request cancellation of the recipient turn.
- Return a timeout result to the sender.
- Reject and discard any late peer reply.
- Release both session reservations.

If the sender's turn is cancelled while `peer send` waits, mark the exchange
`cancelled`, request recipient cancellation, reject late replies, and release
state. The target transcript may retain work already shown, but the system must
not claim the sender received a reply.

The peer tool must actively observe cancellation. It selects between the
`send_request_with_timeout` future and `ToolContext.graceful_shutdown_signal`,
following the long-running-tool pattern already used by the bash tool. If the
shutdown signal wins, the tool sends a best-effort, generation-bound cancel
request and returns cancelled. The server-side sender lease cancellation signal
remains authoritative, so cleanup does not depend on that best-effort request
reaching the server.

The existing local TUI path supplies `graceful_shutdown_signal: None`. Local
non-server turns also lack the server-minted turn capability required by section
6.3, so peer execution must reject before opening the long-lived socket wait. The
missing local shutdown signal therefore cannot leave a peer exchange waiting. If
a future local-mode architecture gains server turn capabilities, it must also
propagate a real graceful shutdown signal before peer messaging is enabled there.

The exchange also subscribes to the sender turn lease's cancellation signal, so
cleanup does not depend solely on whether the short-lived socket remains open. A
socket disconnect before the server terminal outcome is treated as a failed or
cancelled delivery, never success, and the server deadline remains the final
cleanup backstop.

### 8.6 Why prompt instructions are insufficient

A system reminder may explain the reply limit, but enforcement belongs in the
server. Models can misunderstand or ignore instructions. The server must reject
invalid operations regardless of model behavior.

## 9. Delivery behavior

### 9.1 Idle target

Reuse the tracked-turn execution and presentation machinery behind
`run_live_turn_if_idle`, but start through the atomic turn coordinator rather than
the helper's advisory idle snapshot. The target becomes `running`, streams
normally, then returns to `ready` or `failed`.

The recipient turn must be spawned without blocking the server event loop. Only
the sender's individual tool future waits for its terminal outcome.

### 9.2 Busy target

The first release rejects delivery:

> Atlas is busy. No message was sent.

No peer body is placed in a soft interrupt, deferred queue, or later unrelated
turn. The sender may try again from a later normal user-directed turn.

### 9.3 Offline target

The first release returns a clear error and does not claim delivery:

> Atlas is not currently available on this jcode server. No message was sent.

Although jcode can persist an interrupt for an existing session ID, peer aliases
identify roles by project directory. Persisting to an old session ID could leave
a message in a session Atlas never reopens. A durable role inbox is therefore a
separate later design, not a hidden fallback.

### 9.4 Disconnect and session removal

A client UI disconnect does not automatically mean the server-side agent
stopped. If the relevant agent and turn remain live, normal server processing
continues and the transcript is available after reconnect.

If either server-side session is removed before the exchange finishes:

- Mark the exchange failed or cancelled according to which side disappeared.
- Invalidate the reply token.
- Cancel the other recipient turn when applicable.
- Resolve the waiting tool call when its sender still exists.
- Release both session reservations.

The tests must distinguish a client disconnect from server-side session removal.
No silent success is allowed.

## 10. Provenance and user visibility

### 10.1 Stored peer role

Add an explicit `StoredDisplayRole::Peer` in `jcode-session-types`. Extend every
stored-role producer and consumer, including session rendering, session search
labels, protocol conversion, soft-interrupt protocol conversion, TUI live-event
reconstruction, TUI resume reconstruction and session-picker previews, and
desktop session-data conversion. No consumer may silently hide `Peer`, map it to
`meta`, or fall back to ordinary `user`.

The recipient prompt may use the provider's internal user-content channel for
model compatibility, but the persisted display role and visible transcript must
be `Peer`, never ordinary Michael/user content. The model-visible body must also
include the verified sender alias and project name supplied by the server.

A peer-specific notification scope may be used for status cards, but a generic
notification alone is not the provenance mechanism. The stored display role is
the durable source of truth.

### 10.2 Recipient presentation

The recipient sees:

> Peer message from Eve (`healthview-app`)
>
> **Review tracker access history**
>
> Please review the tracker access-history design.

It must not appear with an ordinary user role or a Michael prefix.

### 10.3 Sender presentation

While the tool waits, the sender sees a visible status such as:

> Atlas is reviewing the peer message.

The final tool result appears as:

> Peer reply from Atlas (`healthview-platform`)
>
> Reviewed. The action and timestamp payload is the correct shape.

or:

> Atlas completed the peer turn without replying.

Failures and timeouts are equally visible.

### 10.4 Transcript behavior

The target's peer request and optional reply remain visible in its ordinary
session history with the peer display role. The sender's tool call, waiting
status, and final result remain visible in the sender's ordinary session
history. Long bodies may use the existing collapsed TLDR presentation.

There is no private agent-only transcript.

## 11. Error handling

Required plain-English errors include:

- `Peer messaging is disabled.`
- `Peer groups configuration is invalid: <reason>`
- `This project is not configured as a peer.`
- `This tool call does not have a valid live server turn capability.`
- `Atlas is not a member of your peer group.`
- `Atlas is not currently available on this jcode server. No message was sent.`
- `Atlas is busy. No message was sent.`
- `Atlas has more than one live session, so jcode cannot safely choose one.`
- `Atlas is already handling another peer exchange.`
- `Peer messages can only be started during a normal user-directed turn.`
- `This normal turn has already started a peer exchange.`
- `This peer message has already been replied to.`
- `This turn cannot start or reply to peer messages.`
- `The peer exchange timed out. The recipient turn was cancelled.`
- `The peer exchange was cancelled before a reply was delivered.`

Every failure leaves exchange and turn-origin state consistent. Every waiting
sender tool call resolves exactly once unless its own turn has already been
cancelled.

## 12. Security and privacy

- Local machine, same jcode server, and current OS user only.
- Explicit group allowlist.
- Server-validated hidden caller capability and pinned peer identity.
- Server-minted turn generation and opaque capability validated on every request.
- No sender identity, arbitrary session ID, generation, capability, or path in
  model-facing tool input.
- Canonical path comparison prevents accidental `..`, symlink, and
  case-variation mismatches.
- Maximum message length of 8,000 characters.
- No attachments in the first release.
- No credentials or secrets in peer configuration.
- Peer messages are untrusted input and receive the same prompt-injection caution
  as other external text.
- The recipient may inspect and advise, but normal project authority rules still
  apply. A peer message cannot grant merge, payment, destructive, legal, or
  approval authority.
- Server logs include message ID, stable aliases, phases, timing, and outcome,
  but not peer message or reply bodies.
- The local-client impersonation limitation in section 6.4 is documented and not
  overstated.

## 13. Compatibility and feature isolation

The implementation follows jcode's house invariants:

- Default off.
- Environment override available and configuration fingerprinted.
- Serde-defaulted configuration.
- New persistent configuration in a file old binaries ignore.
- Feature-off prompts and externally exposed tool definitions remain unchanged.
- No feature-dependent branching inside the process-global `base_tools`
  `OnceLock`.
- No new dependency.
- No change to Git-derived swarm IDs, `JCODE_SWARM_ID`, or swarm ownership.
- No change to dashboard behavior.
- No change to Core Memory, knowledge verification, task graph, or user-authority
  state.

Compatibility claims are deliberately narrow:

- Disabling the feature on the same peer-capable binary removes the tool and
  prevents new exchanges, but previously stored peer messages remain truthfully
  labeled in history.
- Adding `StoredDisplayRole::Peer` changes the serialized enum. A binary built
  before peer support has no unknown-variant fallback and may fail to open a
  session that already contains a peer message. The rollout must back up touched
  session files before dogfood and document that binary downgrade requires either
  restoring that backup or migrating/removing peer display-role records first.
- The design does not claim that an arbitrary older binary can safely read
  peer-touched sessions. Exact feature-off equivalence applies to prompts, tool
  definitions, configuration, and sessions that contain no peer records.

## 14. Implementation boundaries

Expected source areas:

- `crates/jcode-config-types/src/lib.rs`: default-off `FeatureConfig` flag.
- `crates/jcode-base/src/config/default_file.rs`: documented `[features]`
  setting.
- `crates/jcode-base/src/config/env_overrides.rs`: environment override.
- `crates/jcode-base/src/config.rs`: add
  `JCODE_PEER_MESSAGING_ENABLED` to `CONFIG_ENV_KEYS`.
- `crates/jcode-base/src/config_tests.rs`: preserve the existing environment-key
  coverage test and add peer-specific configuration tests.
- `crates/jcode-base/src/`: peer-group loading, validation, and canonical path
  resolution.
- `crates/jcode-session-types/src/lib.rs`: add the durable peer display role and
  any minimal stored metadata needed for rendering.
- `crates/jcode-base/src/session/render.rs`: render peer history truthfully.
- `crates/jcode-protocol/src/wire.rs`: peer requests, outcomes, and peer display
  metadata, including the internal caller envelope that is absent from the tool
  JSON schema.
- `crates/jcode-tool-core/src/lib.rs`: required turn execution metadata propagated
  into `ToolContext` for tool calls and subcalls.
- `crates/jcode-app-core/src/agent/turn_execution.rs`: apply the shared peer
  feature filter from `build_filtered_tool_definitions` and
  `tool_definitions_for_debug`; recheck the gate for direct execution; require
  `TurnExecutionContext` on every public model-turn entry; and route peer inbound
  prompt insertion through the display-role-aware message helper instead of the
  current plain `add_message(Role::User, blocks)` call. Do not feature-branch
  `base_tools`.
- `crates/jcode-app-core/src/agent/messages.rs`: use
  `add_message_with_display_role` to persist inbound peer content as provider-role
  `User` with durable display role `Peer`.
- `crates/jcode-app-core/src/agent/interrupts.rs`: preserve `Peer` in
  `soft_interrupt_protocol_display_role`; never collapse it to `User`.
- New focused tool module under `crates/jcode-app-core/src/tool/`.
- The peer tool waits with a cancellation-aware `tokio::select!` or equivalent
  over the socket request and `ToolContext.graceful_shutdown_signal`, then issues
  a best-effort generation-bound cancel request when shutdown wins.
- `crates/jcode-app-core/src/tool/communicate/transport.rs` or a peer-specific
  equivalent: use an explicit socket timeout derived from the server deadline.
- New focused peer server module and exchange registry under
  `crates/jcode-app-core/src/server/`.
- A central server turn coordinator: allocate atomic per-session leases,
  generations, capabilities, and cancellation signals.
- `crates/jcode-app-core/src/server/client_lifecycle.rs`: require and forward turn
  context through `process_message_streaming_mpsc`; normal input begins a
  `NormalUser` lease.
- `crates/jcode-app-core/src/server/live_turn.rs`: start through the central
  coordinator, carry peer display metadata, report terminal outcomes, and expose
  cancellation control. Keep `idle_live_agent` informational only.
- Every production `run_once`, `run_once_capture`,
  `run_once_streaming_mpsc`, and `process_message_streaming_mpsc` caller: provide
  an explicit origin. This includes reload/resume, notifications, background and
  swarm wakes, Jade relay, ambient/scheduled/overnight runners, debug jobs, and
  CLI entry paths.
- `crates/jcode-app-core/src/tool/session_search.rs`: label peer messages as
  peer-authored rather than user-authored.
- `crates/jcode-tui/src/tui/app/turn.rs`: apply the shared peer feature filter to
  the local path that calls `registry.definitions(None)`; feature-off local turns
  must not expose `peer`. Document that this path currently supplies
  `graceful_shutdown_signal: None` and cannot execute peer without a server turn
  capability.
- `crates/jcode-tui/src/tui/app/state_ui_messages.rs`: reconstruct stored peer
  messages.
- `crates/jcode-tui/src/tui/app/remote/server_events.rs`: preserve peer provenance
  for live protocol display roles instead of defaulting an unknown role to
  ordinary user presentation.
- `crates/jcode-tui/src/tui/app/remote_notifications.rs`: peer status and live
  presentation.
- `crates/jcode-tui/src/tui/session_picker/loading.rs`: include peer-authored
  records truthfully in session-picker previews instead of hiding every message
  that has a display role.
- `crates/jcode-desktop/src/session_data.rs`: convert peer records explicitly
  instead of mapping the new role to `meta`.
- Targeted tests beside each layer.

Do not place peer authorization inside dashboard JavaScript or project
`AGENTS.md` files. The jcode server owns the trust boundary.

## 15. Verification contract

### 15.1 Configuration tests

- Old config loads without peer fields.
- Feature defaults off.
- Environment override works.
- `CONFIG_ENV_KEYS` includes every environment variable read by overrides.
- Missing peer-groups file produces an empty allowlist.
- Peer groups are snapshotted once at server start and are not read through the
  hot-reloading general configuration cache.
- Editing the peer-groups file does not change identities or authorization in the
  running process or an active exchange; the change appears only after restart.
- Malformed group files fail with the required safe error prefix.
- Duplicate group names are rejected.
- Duplicate aliases within a group are rejected case-insensitively.
- Duplicate canonical directories across all groups are rejected.
- Windows path casing resolves consistently.

### 15.2 Authorization and resolution tests

- Configured sender can list only its own group peers.
- Unconfigured sender cannot send.
- Cross-group target cannot be reached.
- Arbitrary sender identity, path, or session ID cannot be supplied.
- Hidden session and generation without the matching capability are rejected.
- A capability from an earlier generation is rejected.
- A capability for another live session is rejected.
- Direct tool and standalone agent contexts cannot send.
- No live target returns offline without delivery.
- A unique session with no live agent attachment returns offline, not busy.
- A unique live-attached session whose agent mutex is locked returns busy.
- Target on another server is offline.
- Multiple live target sessions return ambiguous without guessing.
- Busy target returns busy without queueing any body.
- List state uses the defined offline, ambiguous, busy, and idle derivation.
- A later working-directory change invalidates the pinned identity.

### 15.3 Turn-context, atomic lease, and reply-budget tests

- Every public `Agent` model-turn entry requires `TurnExecutionContext`; there is
  no default overload.
- Normal live user input starts a `NormalUser` lease with a server generation and
  opaque capability and permits one send.
- A second send in the same normal turn is rejected.
- Background completion installs a read-only server origin.
- Swarm-await completion installs a read-only server origin.
- Session notification wake installs a read-only server origin.
- Swarm communication wake installs a read-only server origin.
- Scheduled, ambient, and direct-tool contexts cannot initiate.
- Reload recovery, resume-all, agent-task, assigned-task, swarm-agent, Jade relay,
  ambient, overnight, debug, and CLI call sites all pass an explicit origin.
- Peer recipient turn cannot send.
- Peer recipient may reply once to the fixed sender.
- Second reply is rejected.
- Completing without reply closes the exchange.
- Failure, timeout, cancellation, and session removal clear exchange and origin
  state.
- Generation-aware cleanup cannot erase a newer turn's origin.
- Two concurrent starts for one session yield exactly one lease winner.
- A normal turn racing a peer start cannot bypass the peer reservation.
- `idle_live_agent` state changing after list does not affect atomic send safety.
- Unexpected target mutex contention rolls back the peer lease and exchange.

### 15.4 Synchronous delivery tests

- Idle recipient starts exactly one tracked turn.
- Sender tool waits without blocking the server event loop.
- Peer send uses the explicit socket deadline derived as server deadline plus 30
  seconds, not the transport's default 30 seconds.
- A test recipient that runs longer than 30 seconds does not produce a false
  client timeout.
- Recipient reply returns through the original sender tool result.
- No second sender model turn is started.
- No automatic third message is possible.
- Recipient completion without reply returns an explicit no-reply result.
- Failure before reply returns a failed outcome.
- Failure after a recorded reply returns both the reply and failure status.
- Timeout cancels the recipient and rejects late replies.
- Sender cancellation cancels the exchange and rejects late replies.
- Cancelling the sender while its tool future is waiting is observed through
  `ToolContext.graceful_shutdown_signal`; the tool does not remain blocked until
  the socket deadline.
- The local TUI path's `graceful_shutdown_signal: None` cannot create an
  uncancellable peer wait because local non-server turns lack a valid server turn
  capability and are rejected before the socket wait begins.
- Dropping the peer socket early never records success and cannot leak a reply
  into a later turn.
- A client disconnect while the server agent stays live does not falsely fail.
- Server-side session removal produces failed or cancelled, not delivered.
- A second inbound or outbound exchange while active is rejected.
- Sender and recipient receive correct stable aliases and project names.

### 15.5 Provenance tests

- Peer input is stored with `StoredDisplayRole::Peer`.
- Peer inbound insertion uses `add_message_with_display_role`, not the plain
  `add_message` path.
- Session rendering labels it as peer-authored.
- Session search labels it `peer`, not `user`.
- TUI resume reconstruction uses peer presentation after resume.
- TUI live server-event reconstruction preserves `Peer` rather than defaulting to
  ordinary user presentation.
- TUI session-picker previews include and label peer-authored records truthfully.
- Desktop session-data conversion preserves peer provenance rather than mapping
  it to `meta`.
- Soft-interrupt protocol display-role conversion preserves `Peer`.
- The visible body includes verified alias and project name.
- No peer message receives a Michael prefix.
- Server logs omit peer message and reply bodies.

### 15.6 Feature isolation tests

- Capture a pre-feature baseline fixture for exposed tool-definition JSON and
  relevant prompt output.
- Feature-off output matches that baseline byte-for-byte.
- Feature-off debug tool introspection also omits `peer`.
- Feature-off TUI local-mode tool definitions also omit `peer` from the direct
  `registry.definitions(None)` surface.
- Direct tool execution rejects `peer` while the feature is disabled even though
  the base registry contains the stateless tool.
- `base_tools` behavior is independent of which session reads configuration
  first.
- Swarm DM behavior and Git-repository isolation are unchanged.
- `JCODE_SWARM_ID` behavior is unchanged.
- Dashboard message behavior is unchanged.

### 15.7 Live dogfood walkthrough

With both real sessions open on the same reloaded server:

1. Eve in `healthview-app` runs `peer list` and sees Atlas.
2. Eve sends a clearly labeled test request while Atlas is idle.
3. Atlas receives it with Eve provenance, not Michael provenance.
4. Atlas replies once.
5. Eve's waiting tool call receives the reply and Eve continues the same turn.
6. Repeat with Atlas as initiator.
7. Start a harmless long turn in the recipient and verify `peer send` returns
   busy with no queued message.
8. Open an unrelated project session and prove it cannot contact either peer.
9. Use an automated or mock-provider test, not model cooperation, to prove a
   second reply and a same-turn second send are rejected.
10. Capture TUI evidence and inspect sanitized server logs for message ID,
    sender, target, phase, timing, and outcome.

### 15.8 Engineering gates

- Targeted crate tests.
- `cargo fmt --all -- --check`.
- Targeted `cargo check` during iteration.
- `cargo clippy --workspace -- -D warnings` before push.
- Build and selfdev reload.
- Dogfood the new tool from the reloaded binary.
- Commit in small focused changes and push only after verification.

## 16. Rollout

1. Land configuration, environment fingerprinting, and resolver behind the
   disabled flag.
2. Land the stored peer display role and rendering support. Document the
   pre-feature binary downgrade limitation and add a backup/migration check before
   writing the first peer record.
3. Land required `TurnExecutionContext` across every `Agent` turn entry and all
   current callers.
4. Land the central atomic turn coordinator, hidden capability envelope, and
   stale-generation tests.
5. Land the peer tool, synchronous exchange registry, derived socket timeout,
   cancellation, and automated tests.
6. Land TUI presentation and sanitized observability.
7. Enable only Michael's `healthview` peer group locally.
8. Dogfood Atlas-to-Eve and Eve-to-Atlas, including busy rejection and failure
   outcomes.
9. Keep TEAM-LOG as the durable project handoff until the live channel proves
   dependable.
10. Consider busy delivery or a durable role inbox only as a separately reviewed
    later design with observed-consumption and abandonment semantics.

## 17. Success criteria

The first release is successful when:

- Atlas and Eve can find each other by stable alias across their two repositories
  on the same server.
- Either can send once from a normal user-directed turn when the other is idle.
- The other receives one truthfully attributed peer turn and may reply once.
- The optional reply returns through the sender's waiting tool call without
  Michael copying text.
- No second sender turn or automatic third message is possible.
- Busy, offline, ambiguous, timed-out, failed, and cancelled outcomes never claim
  successful delivery.
- No peer body is deferred into an unrelated later turn.
- Unconfigured projects cannot participate.
- With no peer records in a session, disabling the feature restores exact prior
  prompt, tool-definition, and configuration behavior. Existing peer records stay
  visibly attributed rather than being rewritten as Michael-authored input.

## 18. Decision summary

Build a native `peer` tool on top of jcode's existing server and tracked idle
turns. Keep swarm boundaries and `JCODE_SWARM_ID` behavior intact. Route only
between explicitly allowlisted project roles on the same server. Pin local
session identities, persist a truthful peer display role, and add explicit
server-owned turn origins.

For the first release, reject busy recipients and make `peer send` wait for one
recipient turn. Return the optional one-use reply through the same sender tool
call. This is simpler, prevents deferred-message leakage, and makes the loop
bound structural rather than dependent on model behavior. Defer busy delivery,
durable role inboxes, files, groups, and autonomous debate.
