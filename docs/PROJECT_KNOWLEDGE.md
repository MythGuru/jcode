# Project Knowledge: the Verification-Gated Living Project Map

> **Status:** Implemented (K0-K6)
> **Flag-gated:** `agents.project_knowledge_enabled` (default **off**), with
> `project_knowledge_max_chars` (default 4000). Env overrides:
> `JCODE_PROJECT_KNOWLEDGE_*`.

jcode keeps a living, readable map of each project: how it is structured, the
decisions that shaped it, the rules work must respect, the known problems, and
which components are responsible for what. The defining property is the
**verification gate**: the map may only claim something is *verified* when a
successful build/test run or an explicit user confirmation backs it.

## The model

Each entry belongs to one of five sections and carries a status:

| Section | Meaning |
|---|---|
| Structure | How the project is laid out (crates, layers, key dirs) |
| Decisions | Choices made, so they are not silently revisited |
| Rules | Constraints work must respect |
| Known Problems | Recurring pitfalls |
| Responsibilities | Which component owns what |

- Entries start as **Proposed**. A proposed entry is a claim, not knowledge.
- **Verified** is reachable only through the gate (`knowledge/verification.rs`)
  or explicit user confirmation. Provenance (what verified it) is recorded and
  kept.
- Editing a verified entry demotes it to Proposed: changed claims need fresh
  evidence.

## The verification gate

Evidence is collected per session, in process, never persisted:

- The bash tool reports every completed foreground `cargo build` / `check` /
  `clippy` / `test` (`nextest` included) with its exit code. Missing exit codes
  count as failure. Classification is conservative token matching; anything
  ambiguous produces no event.
- `cargo test` outranks build-family commands within one command chain.

The gate rule, enforced in exactly one place (`try_verify`):

> An entry may move from Proposed to Verified only when the session holds a
> successful verification event **at least as new as the entry's last edit**,
> with **no relevant failure after that success**.

Refusals are typed and explained: `NoEvidence`, `StaleEvidence`,
`InvalidatedByFailure`, `AlreadyVerified`, `UnknownEntry`, `Disabled`.

User confirmation (`verify_by_user`) is its own authority: no build evidence
needed, provenance records "user confirmation" plus an optional note. The
agent may only invoke it after the user actually confirmed in conversation.

## The memory bridge

A verified entry is bridged into the project's long-term memory graph as a
lesson (`knowledge/bridge.rs`):

- content `[project <section>] <text>`, source `project_knowledge`, tags
  `knowledge-verified` + `pk-id:<entry-id>`,
- importance **0.85**: above the prune-protection floor (0.8), below the
  explicit-user band (0.9+),
- one lesson per entry, forever: re-verification updates and reinforces the
  same memory (identity tag), refreshing content and clearing the stale
  embedding, never duplicating,
- best-effort: a bridge failure logs; the verification stands.

## Prompt injection

`project_knowledge_prompt_section(working_dir)` is the single injection gate,
called at the shared prompt chokepoint (`prompt.rs`), in the dynamic part,
before working memory. It returns `None` unless the flag is on, a working dir
exists, and the map is non-empty, so the flag-off prompt is byte-identical to
pre-feature behavior. Rendering is budgeted (`project_knowledge_max_chars`,
clamped 256-16000): whole lines are dropped from the end, and verified entries
survive preferentially because they render first.

## The `knowledge` tool

| Action | Effect |
|---|---|
| `show` | Rendered map plus entry ids |
| `propose` | Add an entry as Proposed |
| `revise` | Edit an entry (demotes to Proposed) |
| `verify` | Run the gate against this session's evidence |
| `confirm` | Record explicit user confirmation (user authority) |
| `remove` | Delete an entry |
| `history` | This session's verification event trail |

Project scope comes from the call's working directory, the same resolution the
memory tool uses.

## Ambient awareness

Ambient cycles see a read-only `## Project Knowledge Health` section (maps,
verified/proposed counts, stale proposed entries older than 14 days) when the
feature is on and at least one map exists. Ambient may suggest cleanup to the
user but has **no write path** to the knowledge map; the gate remains the only
mutation authority.

## Storage and rollback

- JSON at `~/.jcode/knowledge/projects/<hash>.json` (same project-hash scheme
  as memory graphs) plus a rendered readable `<hash>.md` sibling.
- Versioned with serde defaults: unknown versions and corrupt files load as
  empty; old binaries never read the directory, so downgrade is a no-op.
- Best-effort persistence: failures log, never block a turn. An emptied map
  removes both files.
- Full rollback = flip the flag off (prompt returns to byte-identical) and, if
  desired, delete `~/.jcode/knowledge/`.

## File map

| File | Role |
|---|---|
| `crates/jcode-base/src/knowledge.rs` | Model, storage, rendering, prompt section, health (K1/K5/K6) |
| `crates/jcode-base/src/knowledge/verification.rs` | Events, classification, the gate (K2) |
| `crates/jcode-base/src/knowledge/bridge.rs` | Verified-lesson memory bridge (K4) |
| `crates/jcode-app-core/src/tool/knowledge.rs` | Agent-facing tool (K3) |
| `crates/jcode-app-core/src/tool/bash.rs` | One-line evidence hook (K2) |
| `crates/jcode-app-core/src/ambient/prompt.rs` | Health section (K6) |
| `crates/jcode-config-types`, `config/*` | Flags (K0) |
