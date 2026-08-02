# Memory Systems Health Check - 2026-08-02

Live verification of Michael's memory/knowledge upgrades in the running jcode
installation (v0.61.2-dev, self-dev session, Windows). Each system was
exercised end-to-end with harmless test data, config flags were read from
`~/.jcode/config.toml`, logs were scanned, and the memory-related test suites
were run with plain cargo.

## Summary

| # | System | Flag(s) in `~/.jcode/config.toml` | State | Result |
|---|--------|-----------------------------------|-------|--------|
| 1 | Working (short-term) memory | `agents.working_memory_enabled = true` (capacity 7, item chars 240) | ON | PASS (one small bug found and fixed, see below) |
| 1 | Long-term memory + importance | `features.memory = true`, `agents.memory_importance_enabled = true`, sidecar + local embeddings on | ON | PASS |
| 2 | Project knowledge map + verification gate | `agents.project_knowledge_enabled = true` (budget 4000) | ON | PASS |
| 3 | Core memory | `agents.core_memory_enabled = true` (budget 4000) | ON | PASS |
| 4 | Initiative / task graph | `agents.task_graph_enabled = true` (`ambient_continuation = false`) | ON | PASS |
| 5 | Ambient background maintenance | `ambient.enabled = false` | OFF (by choice) | PASS (correctly inert) |

All memory-related cargo test suites pass: `jcode-memory-types` 29/29,
`jcode-base memory` 132/132, `jcode-base knowledge` 42/42, `jcode-base goal`
34/34, `jcode-app-core memory` 17/17, plus the new regression test added today.

## Evidence per system

### 1a. Working memory (short-term buffer)

- `memory action=note` added an item and returned a `wm_*` id. PASS
- `memory action=working` listed 6 items including session context and the new
  note. PASS
- Prompt injection: the `# Working Memory` section appears in the live session
  context every turn (verified in this session's own prompt). PASS
- Buffer capacity, eviction, rehearsal, promotion, isolation, and persistence
  logic covered by 20+ unit tests in `crates/jcode-base/src/memory/working.rs`
  and `promotion.rs`, all green.

**Bug found and fixed:** `memory action=forget` with a `wm_*` id returned
"Not found" because forget only searched the long-term graphs, never the
session working buffer. There was no other tool path to remove a working-memory
item (`remove_working_memory` existed but was unreachable). Fixed in
`crates/jcode-app-core/src/tool/memory.rs`: forget now falls through to the
working buffer when the graph lookup misses. Regression test
`forget_removes_working_memory_items_by_id` added and passing.

### 1b. Long-term memory (importance-ranked graph)

- `remember` stored a test fact (project scope), returned a `mem_*` id. PASS
- `search` found it by exact token. PASS
- `forget` removed it and the store confirmed removal. PASS
- Retrieval pipeline is live: `~/.jcode/logs/memory-events-2026-08-02.jsonl`
  shows the full async cascade running (embedding_started/complete,
  candidate_filter, judge_decision x137, memory_injected x31,
  maintenance_started/complete x133, maintenance_linked, tag inference).
- Importance system verified live: core entries carry importance up to 1.0 and
  `set_importance` guardrails are tested (`core_entries_refuse_forget_and_low_importance`).
- Minor observation (not a bug): a semantic `recall` for the test fact seconds
  after `remember` returned nothing while exact `search` worked. This is the
  documented async design: embeddings/judging complete in the background and
  results surface on later turns.

### 2. Project knowledge map + verification gate

- `knowledge show` rendered the map with verified and proposed entries plus
  ids. PASS
- `propose` created a new entry marked (proposed). PASS
- Gate refusal: `verify` on the fresh entry before any build/test in this
  session was refused with "no successful build/test verification event in
  this session yet". PASS - the gate demands evidence.
- Gate acceptance: after running a passing `cargo test`, `verify` on a second
  probe entry succeeded and recorded the exact command and exit code as
  provenance. PASS
- `remove` cleaned up both probes. PASS

### 3. Core memory

- `core_show` rendered all core entries in deterministic order (identity,
  style, rules, history, then by creation). PASS
- `core_recall` returned full details with importance and tags. PASS
- `core_propose` staged a probe to `~/.jcode/memory/core_proposals.json`
  without touching the graph (confirmed on disk: graph unchanged, proposal
  staged). PASS - the probe was then removed from the staging file without
  confirming, and an unrelated pending proposal from another session was
  preserved untouched.
- Protection: `forget` and importance-lowering refusals for core entries are
  enforced in code and covered by tests. Injection into this session's prompt
  is live (the `# Core Memory` section is present).
- Note: one pending core proposal from another session
  (`core-proposal-f244a7a06f00426ea68bdee272c9941a`, "Identity protection &
  real-world focus") is still staged and awaiting Michael's explicit
  confirmation. Its near-identical sibling already exists in the graph
  (`mem_1785658701019...`), so it may be a leftover duplicate - flagging for
  Michael rather than deciding.

### 4. Initiative / task graph

- `initiative list` returned the completed "World-Class Expert Skills Program"
  goal with status, scope, progress, milestone, and id. PASS
- Storage confirmed on disk at `~/.jcode/goals/projects/<hash>/` and
  `~/.jcode/goals/sessions/`. PASS
- `jcode-base goal` tests: 34 passed.

### 5. Ambient background maintenance

- `[ambient] enabled = false` in config; `~/.jcode/ambient/` is empty, no
  ambient log activity. The system is present but correctly inert. Memory
  *maintenance* (link strengthening, tagging, gap detection) runs inside the
  memory agent pipeline and is active, which matches the docs (that is not
  the ambient work loop). PASS

## Docs vs behavior (commits 34315d678, 3d5fed6ec)

`docs/reports/memory-systems-overview.md` and the visual guide are accurate on
flags, budgets, gate semantics, core protection, and storage layout, with two
discrepancies found:

1. **Working-memory disk persistence is not wired in production.** The docs
   describe `~/.jcode/memory/working/` per-session buffers on disk. The code
   (`save_working_memory`, `load_working_memory`, `promote_on_session_end`)
   exists, is exported, and is well tested, but nothing in the production
   session lifecycle calls it - the only callers are tests. Consequently
   `~/.jcode/memory/working/` does not exist on this machine and session-end
   promotion (the "2+ rehearsals promote on exit" rule at session end) never
   fires in live use. In-memory behavior within a session is correct.
   This is a wiring gap, not a data-loss risk, but the docs overstate current
   behavior. Left unfixed deliberately: hooking session shutdown paths is not
   a "small bug" fix and deserves its own reviewed change.
2. **Core budget default.** Docs say default 2000 chars, which matches the
   code default. Michael's config raises it to 4000. Not a bug, just worth
   knowing the live value differs from the documented default.

## Logs

`~/.jcode/logs/jcode-2026-08-01.log` and `jcode-2026-08-02.log`: no
memory/knowledge errors or failures. The only WARN touching these systems is a
single `TUI_SLOW_FRAME` (50ms render) while the knowledge tool ran - cosmetic.
`memory-events-*.jsonl` shows a healthy pipeline with no error events.

## Test runs (plain cargo, Windows)

| Suite | Result |
|-------|--------|
| `cargo test -p jcode-memory-types` | 29 passed, 0 failed |
| `cargo test -p jcode-base memory` | 132 passed, 0 failed |
| `cargo test -p jcode-base knowledge` | 42 passed, 0 failed |
| `cargo test -p jcode-base goal` | 34 passed, 0 failed |
| `cargo test -p jcode-app-core memory` | 17 passed, 0 failed |
| `cargo test -p jcode-app-core forget_removes_working_memory` (new) | 1 passed, 0 failed |

## Changes made

- Fix: `memory` tool `forget` now removes working-memory (`wm_*`) items when
  the id is not in the long-term graph, instead of reporting "Not found".
- Test: `forget_removes_working_memory_items_by_id` regression test.
- Committed locally, not pushed.

## Follow-ups for Michael

1. Decide whether to wire `save_working_memory` / `load_working_memory` /
   `promote_on_session_end` into the real session lifecycle (docs already
   promise it), or soften the docs.
2. Review the pending core proposal
   `core-proposal-f244a7a06f00426ea68bdee272c9941a` - confirm or discard (a
   near-identical entry already exists in the graph).
