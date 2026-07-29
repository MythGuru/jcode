# Task Graph: the Persistent Hierarchical Plan

> **Status:** Implemented (T0-T7)
> **Flag-gated:** `agents.task_graph_enabled` (default **off**) and
> `agents.task_graph_ambient_continuation` (default **off**, requires the
> first flag too), with `task_graph_max_prompt_chars` (default 2400).
> Env overrides: `JCODE_TASK_GRAPH_*`.

jcode keeps a durable, hierarchical plan that survives sessions:
**initiatives (goals) -> milestones -> steps**, with dependency edges between
steps and between milestones. Steps can only be marked done when their declared
verification is backed by evidence, the plan connects to the project knowledge
map and the prioritized memory system, and ambient mode may continue steps the
user explicitly marked safe.

The graph deliberately reuses what already existed:

- the hierarchy is the existing `Goal` / `GoalMilestone` / `GoalStep` model
  (`jcode-task-types`), stored under `~/.jcode/goals/`,
- readiness/cycle math is the existing swarm plan engine
  (`jcode_plan::summarize_plan_graph`), adapted per goal,
- verification evidence is the existing knowledge-gate event store
  (`knowledge/verification.rs`): foreground `cargo build/check/clippy/test`
  completions observed by the bash tool, session-scoped, never persisted.

## The model (T1)

`GoalStep` gains, all serde-defaulted so legacy goal files load unchanged:

| Field | Meaning |
|---|---|
| `blocked_by` | Step ids (same goal) that must complete first |
| `verification` | What observable evidence proves this step done |
| `verification_evidence` | Recorded provenance once satisfied (gate-owned) |
| `safe_for_ambient` | Explicit opt-in for ambient continuation (default false) |
| `propose_knowledge` | Lesson to propose into the knowledge map on completion |
| `knowledge_ids` | Knowledge entries the step's work should respect |

`GoalMilestone.blocked_by` names milestone ids; those edges expand to
step-level edges in the readiness computation (`goal/graph.rs`). Cycles and
unknown dependency ids are reported as plan-authoring errors and never read as
ready. `Goal.working_dir` records where a project goal lives.

## Verification-gated completion (T2)

A step that declares `verification` may only reach `completed` through the
gate (`goal/verification.rs`):

- at goal save time, a newly arriving completion keeps its status only when
  the session holds qualifying evidence (newest successful cargo command with
  no failure after it). Otherwise it is parked as
  **`done_pending_verification`**: honest, visible, and it keeps dependents
  blocked,
- `verify_step` upgrades a parked step later, with fresh evidence
  (`authority=evidence`, default) or explicit user confirmation
  (`authority=user`),
- previously completed steps are grandfathered, but their recorded evidence
  is restored from disk so callers cannot rewrite provenance,
- steps without a `verification` field behave exactly as before.

## Tool surface (T3)

The `initiative` tool gains two flag-gated actions:

- **`ready`** - the goal's frontier: ready steps (with `[ambient-safe]` and
  `consult knowledge:` hints), blocked steps and why, steps awaiting
  verification, cycles/unknown deps.
- **`verify_step`** - upgrade a parked step (`id`, `step_id`, optional
  `authority`, `note`).

The todo bridge: a `TodoGoal` may carry `graph_ref:
"<initiative-id>/<step-id>"`. When a todo write closes that group, the durable
step is checkpointed through the T2 gate. Best-effort: a broken ref logs a
note, never fails the todo write.

## Knowledge + memory links (T4)

- When a step *newly completes* (any path), its `propose_knowledge` lesson is
  **proposed** into the project knowledge map (requires the knowledge flag
  too). Proposed only: the K2 gate and the user remain the only paths to
  Verified, so the graph can seed knowledge but never manufacture trust.
- Goal memories (synced on every save, importance-protected) include the plan
  frontier ("Plan: 2 ready, 1 blocked, 3 completed" plus ready ids), so
  memory recall shows where the plan stands. Verified lessons still reach
  long-term memory through the existing K4 bridge at importance 0.85.

## Prompt injection (T5)

`# Active Plan` is injected in the dynamic prompt part between Project
Knowledge and working memory: project map, then plan frontier, then
session-local context. Single gate: flag on AND session id AND
attached/resumable initiative AND non-empty plan, so the flag-off prompt is
byte-identical. Budgeted whole-line truncation at
`task_graph_max_prompt_chars` (clamped 256-16000); ready steps survive
preferentially. Context breakdown shows it as `plan`.

## Ambient continuation (T6)

Ambient mode may continue plan steps, but the envelope is deliberately narrow.
A step surfaces to the ambient cycle prompt only when ALL hold:

- both flags on (`task_graph_enabled` + `task_graph_ambient_continuation`),
- the goal is Active and its recorded `working_dir` still exists,
- the step is ready per the dependency graph,
- the step was explicitly marked `safe_for_ambient` (per step, never
  inferred).

At most 5 steps surface per cycle, and ambient gains **information, not
powers**: code changes still require `request_permission`, and completion
still passes the T2 gate, so ambient work parks as
`done_pending_verification` for a human session to confirm. Ambient can
propose progress; it cannot mint verified completion.

## Turning it on

```toml
# ~/.jcode/config.toml
[agents]
task_graph_enabled = true
# only after you trust the graph:
task_graph_ambient_continuation = true
# optional budget tweak:
task_graph_max_prompt_chars = 2400
```

Or per session: `JCODE_TASK_GRAPH_ENABLED=true`,
`JCODE_TASK_GRAPH_AMBIENT_CONTINUATION=true`,
`JCODE_TASK_GRAPH_MAX_PROMPT_CHARS=2400`.

## Rollback

Turn the flags off (behavior reverts immediately; flags are read live) or
downgrade the binary: every new field is serde-defaulted and skipped when
empty, so files written by this version remain readable by older binaries,
and goals never written with graph fields are byte-identical.

## File map

| Piece | Where |
|---|---|
| Config flags | `jcode-config-types` (`AgentsConfig`), env overrides in `jcode-base/src/config/` |
| Data model | `jcode-task-types/src/lib.rs` (`GoalStep`, `GoalMilestone`, `Goal`, `TodoGoal.graph_ref`) |
| Readiness graph | `jcode-base/src/goal/graph.rs` |
| Completion gate | `jcode-base/src/goal/verification.rs` |
| Knowledge link | `jcode-base/src/goal/knowledge_link.rs` |
| Ambient link | `jcode-base/src/goal/ambient_link.rs` |
| Prompt section | `jcode-base/src/prompt.rs` (dynamic part) |
| Tool actions | `jcode-app-core/src/tool/goal.rs` (`ready`, `verify_step`) |
| Todo bridge | `jcode-app-core/src/tool/todo.rs` (`checkpoint_linked_graph_steps`) |
| Ambient prompt | `jcode-app-core/src/ambient/prompt.rs` |
