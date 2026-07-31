# The jcode Memory Systems

### How short-term, long-term, core memory, and the knowledge graph all fit together

**Prepared for Michael - 2026-07-31**

---

## 1. The Big Picture

jcode remembers things across sessions using **four cooperating layers**, each with a different job, lifetime, and level of trust. Together they mimic how human memory works: things you are actively juggling (working memory), things you have learned over time (long-term memory), who you are and how you work together (core memory), and verified facts about a project (the knowledge map).

| Layer | What it holds | Lifetime | Where it lives |
|---|---|---|---|
| **Working (short-term) memory** | What we are doing *right now*: goals, constraints, decisions, open threads | One session | `~/.jcode/memory/working/` |
| **Long-term memory graph** | Facts, preferences, corrections, lessons - importance-ranked | Until superseded or pruned | `~/.jcode/memory/` (global + per-project) |
| **Core memory** | Identity, relationship, standing rules - shared across every project and every AI model | Permanent, human-approved only | Global memory graph, tagged `core` |
| **Project knowledge map** | Verified facts about a specific codebase: structure, decisions, rules, problems, responsibilities | Until revised | `~/.jcode/knowledge/projects/` |

A fifth piece, the **task graph** (initiatives, milestones, steps), ties plans into the memory and knowledge layers so progress itself is remembered.

Every layer was built the same careful way: **feature-flagged (default off), reversible, and safe for old versions of jcode to ignore.** When a flag is off, the prompt jcode sends to the model is byte-for-byte identical to before the feature existed.

---

## 2. Working (Short-Term) Memory

*The mental scratchpad. Flag: `agents.working_memory_enabled`.*

A small, fixed-capacity buffer (default **7 items**, hard ceiling 16, each capped at 240 characters) that holds what the agent is actively working on. Items are typed: **goal, constraint, fact, decision, or open thread**.

**How it differs from long-term memory:**

| | Working memory | Long-term graph |
|---|---|---|
| Lifetime | one session | until superseded/pruned |
| Injection into the prompt | re-stated **every turn** | once, then suppressed ~45 min |
| Capacity | hard cap of 7 | unbounded |
| Retrieval | always present, no search needed | embedding search + BM25 + LLM judge |

**Rehearsal and promotion.** Every time an item is "rehearsed" (touched again), it earns protection:

- Items rehearsed **3+ times** while in the buffer are promoted into the long-term graph, with starting importance `0.5 + 0.1 x rehearsals` (capped at 0.9).
- Items leaving the buffer (evicted or at session end) promote at **2+ rehearsals**.
- Items never rehearsed simply **evaporate** at session end - by design, like human short-term memory.

**Eviction** removes the least-rehearsed item first, oldest first, so things you keep coming back to are safe.

**Activation.** When a long-term memory is judged relevant to the current turn, it can be "activated" into a free short-term slot (never evicting anything), and while it sits there it is excluded from long-term retrieval so it is not surfaced twice.

---

## 3. Long-Term Memory: The Graph

*The learned experience. Storage: JSON graphs, global + one per project directory.*

Long-term memory is a **graph, not a list**. Each memory is a node connected to tags, clusters, and other memories.

### What a memory entry carries

- **Content and category**: fact, preference, procedure, or correction
- **Scope**: global (all projects) or project (one codebase)
- **Provenance**: user-stated, user-corrected, observed, inferred, or extracted
- **Lifecycle data**: created/updated/last-accessed timestamps, access count, strength
- **Confidence** (0-1, decays over time) and **importance** (0-1, see below)
- **An embedding** - a numerical fingerprint of the meaning, computed locally by a small model (all-MiniLM-L6-v2), so nothing leaves the machine for search

### How memories connect

| Edge | Meaning |
|---|---|
| `HasTag` | Memory carries an explicit label like `#rust` or `#preference` |
| `InCluster` | Memory belongs to an automatically discovered group of similar memories |
| `RelatesTo` | Weighted semantic link between two memories |
| `Supersedes` | A newer memory replaces an older one (the old one is kept but inactive) |
| `Contradicts` | Conflicting information, both kept and flagged |

### How recall works (cascade retrieval)

1. The current conversation context is embedded locally.
2. A similarity search finds the top matching memories.
3. From those hits, a **breadth-first walk through the graph** (depth 2) pulls in related memories via tag, cluster, and link edges, with scores decaying by distance.
4. A lightweight **sidecar model** (currently GPT-5.3 Codex Spark) double-checks each candidate: "is this actually relevant right now?"
5. Only verified memories are injected into the next turn.

All of this is **fully asynchronous**: the main agent never waits for memory. Results computed during turn N appear at turn N+1.

### The importance system

Every long-term entry carries an `importance` score (default 0.5 = neutral). When `agents.memory_importance_enabled` is on:

- Retrieval gets a **bounded nudge** of at most ±15%, so importance breaks near-ties without letting one loud memory dominate.
- Importance **drifts with evidence**: +0.02 when the judge verifies a memory as relevant, -0.01 when it is retrieved but rejected.
- Entries with importance **>= 0.8 are protected from ambient pruning**.
- The memory tool exposes `set_importance`, `rehearse`, and `promote` for manual control.

### Self-maintenance

After serving memories, the memory agent does quiet background housekeeping: strengthening links between memories that were relevant together, boosting confidence on verified hits, gently decaying rejected ones, detecting "gaps" (contexts where nothing relevant existed), and periodically refreshing clusters. Duplicates found at write time are **reinforced instead of duplicated**, and contradictions found at write time are superseded.

---

## 4. Core Memory

*The identity layer. Flag: `agents.core_memory_enabled`, budget `core_memory_budget_chars` (default 2000).*

Core memory is the most protected layer. It holds durable, user-level context: who we are to each other, how we work together, standing rules, and shared history. It is **global** - the same across every project directory - and **model-independent**, so whether the session runs on Fable 5, Opus, or GPT, the same core context is present.

### Protection rules (all enforced in code)

1. **Propose/confirm only.** Writing to core memory is a two-step flow: `core_propose` stages a change without touching the graph, and `core_confirm` applies it only after explicit review. Nothing changes silently.
2. **Importance forced to 1.0** with prune protection. Ambient background processing is read-only toward core entries and can never modify or remove them.
3. **`forget` is refused** for core entries, and attempts to lower their importance are refused.
4. **Deterministic ordering.** Entries render in a fixed priority: `core-identity`, then `core-style`, then `core-rules`, then `core-history`, then everything else by creation order - so the prompt reads the same way every session.
5. **Budgeted injection.** When enabled and non-empty, a `# Core Memory` section is injected at the top of the dynamic prompt (before Project Knowledge), capped at the character budget, Unicode-safe.

Your own Core Memory v0.1 (identity, working style, standing rules, shared history) lives in exactly this system.

---

## 5. The Project Knowledge Map

*Verified facts about a codebase. Flag: `agents.project_knowledge_enabled`, budget 4000 chars.*

Where long-term memory holds *experience*, the knowledge map holds *claims about a project* - and its defining property is the **verification gate**: nothing may be called "verified" without evidence.

### The model

Each entry belongs to one of five sections and carries a status:

| Section | Meaning |
|---|---|
| Structure | How the project is laid out |
| Decisions | Choices made, so they are not silently revisited |
| Rules | Constraints work must respect |
| Known Problems | Recurring pitfalls |
| Responsibilities | Which component owns what |

- Entries start as **Proposed** - a claim, not knowledge.
- **Verified** is reachable only two ways: a successful build/test run after the entry was written (with no failure afterwards), or an **explicit user confirmation** in conversation. Provenance is recorded either way.
- Editing a verified entry **demotes it back to Proposed** - a changed claim needs fresh evidence.

### The evidence system

The bash tool quietly watches every completed foreground `cargo build / check / clippy / test` in the session and records its exit code, in memory only, never persisted. The gate rule, enforced in exactly one place, is:

> An entry may become Verified only when the session holds a successful build/test event **at least as new as the entry's last edit**, with **no relevant failure after that success**.

Refusals are typed and explained (`NoEvidence`, `StaleEvidence`, `InvalidatedByFailure`, and so on) so it is always clear why something did not verify.

### The memory bridge

When an entry becomes verified, it is bridged into the project's long-term memory graph as a lesson at **importance 0.85** - above the prune-protection floor (0.8), below the explicit-user band (0.9+). One lesson per entry, forever: re-verifying refreshes the same memory rather than creating duplicates.

### Ambient safety

Ambient background cycles can *see* a read-only health summary of the knowledge map (counts, stale proposals) and may suggest cleanup - but ambient has **no write path**. The gate and the user are the only mutation authorities.

---

## 6. The Task Graph (Plans that Remember)

*Flag: `agents.task_graph_enabled`.*

Durable plans - **initiatives -> milestones -> steps** with dependency edges - stored under `~/.jcode/goals/` and surviving across sessions. It links into everything above:

- **Verification-gated completion**: a step that declares verification can only complete when the same build/test evidence system backs it. Otherwise it parks honestly as `done_pending_verification` and keeps dependents blocked.
- **Knowledge link**: a completed step can *propose* a lesson into the knowledge map - proposed only, never auto-verified, so plans can seed knowledge but never manufacture trust.
- **Memory link**: the plan frontier ("2 ready, 1 blocked, 3 completed") is synced into memory on every save, importance-protected, so recall shows where the plan stands.
- **Ambient continuation** (second flag, off by default): ambient may continue only steps explicitly marked `safe_for_ambient`, and its completions still park pending human verification.

---

## 7. How It All Reaches the Model: Prompt Injection

Everything meets at **one shared prompt chokepoint**, so the TUI and the app-core paths carry identical sections. The dynamic part of every prompt is assembled in a fixed order:

1. `# Core Memory` (budget 2000 chars)
2. `# Project Knowledge` (budget 4000 chars, verified entries survive truncation preferentially)
3. `# Active Plan` (budget 2400 chars, ready steps survive preferentially)
4. `# Working Memory` (re-stated every turn)
5. Relevant long-term memories surfaced by the retrieval pipeline (injected once, then suppressed ~45 minutes to avoid repetition)

Each section has a character budget with whole-line truncation, and each returns nothing at all when its flag is off - keeping the flag-off prompt byte-identical.

---

## 8. Safety, Rollback, and Trust

- **Everything is a flag, default off.** Turning a flag off returns behavior to exactly what it was.
- **Serde-defaulted schemas**: old memory files load unchanged in new versions, and new files remain readable by old versions. New directories (like `memory/working/`) are ones old binaries never read, so downgrading is a no-op.
- **One-time snapshot insurance**: the first graph write with the new flags enabled saves a `*.pre-stm.json` copy of the pre-upgrade file. Restoring is a plain file copy.
- **Human-readable storage**: all of it is JSON on disk that you can open, read, and edit.
- **Privacy**: memory content is filtered against secrets (API keys, passwords, `.env` content), embeddings are computed locally, and the whole system can be disabled.
- **Trust boundaries are explicit**: ambient can suggest but not write; proposals need confirmation; verification needs evidence; core memory needs you.

---

## 9. Where Everything Lives

```
~/.jcode/
├── memory/
│   ├── global.json              # user-wide long-term graph (core memory lives here)
│   ├── projects/<hash>.json     # per-project long-term graphs
│   ├── working/                 # per-session short-term buffers
│   └── core_proposals.json      # staged core-memory proposals awaiting confirmation
├── knowledge/
│   └── projects/<hash>.json     # project knowledge maps (+ readable .md sibling)
└── goals/                       # durable task graph (initiatives/milestones/steps)
```

Key source files, for reference:

| Area | File |
|---|---|
| Working memory | `crates/jcode-base/src/memory/working.rs` |
| Promotion & importance | `crates/jcode-base/src/memory/promotion.rs` |
| Core memory | `crates/jcode-base/src/memory/core.rs` |
| Long-term graph | `crates/jcode-memory-types/src/graph.rs` |
| Retrieval agent | `crates/jcode-base/src/memory_agent.rs` |
| Knowledge map & gate | `crates/jcode-base/src/knowledge.rs`, `knowledge/verification.rs` |
| Knowledge-memory bridge | `crates/jcode-base/src/knowledge/bridge.rs` |
| Task graph links | `crates/jcode-base/src/goal/knowledge_link.rs` |
| Memory tool | `crates/jcode-app-core/src/tool/memory.rs` |
| Knowledge tool | `crates/jcode-app-core/src/tool/knowledge.rs` |

---

## 10. In One Sentence

jcode's memory is a layered system where fleeting session context earns its way into a long-term graph through rehearsal, project claims earn trust through build evidence, plans remember their own progress, and the identity we built together sits at the top - protected, human-approved, and shared across every model and every project.
