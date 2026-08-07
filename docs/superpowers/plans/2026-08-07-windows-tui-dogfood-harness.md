# Windows TUI Dogfood Harness Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a permanent dev-only Windows ConPTY harness that launches the real Jcode TUI, types `/peers`, verifies the rendered peer overview, and saves reproducible terminal artifacts.

**Architecture:** Register one root development binary behind `dev-bins`. Use `portable-pty` for owned Windows pseudo-terminal process control and `vt100` for deterministic ANSI screen parsing. Keep screen observation and artifact writing in pure modules with unit tests; keep PTY orchestration in the binary entry point and close it with a real runtime dogfood run.

**Tech Stack:** Rust 2024 workspace, Clap, Serde JSON, Chrono UTC timestamps, `portable-pty 0.9.0`, `vt100 0.16.2`, Windows Cargo self-development build.

## Global Constraints

- Michael approved this permanent harness on 2026-08-07.
- Use plain Cargo commands on Windows. Do not use shell-script wrappers.
- The harness is development-only and must not alter normal release behavior.
- Do not mutate Jcode configuration, peer groups, sessions, memory, knowledge, or task state.
- Do not start, stop, or replace the shared daemon from the harness.
- Terminate only the PTY child handle created by the harness.
- Never enumerate Jcode processes or send console-wide Ctrl+C or Ctrl+Break.
- The harness itself makes no model/provider call.
- Save `raw.ansi`, `screen.txt`, and `result.json` on pass and failure.
- Use one overall deterministic timeout and nonzero exit codes for failures.
- Follow red-green-refactor for pure behavior and CLI validation.
- Do not push or create a PR. The final integration is a local fast-forward only.

---

## File Structure

**Create:**

- `src/bin/windows_tui_dogfood.rs`
  - Clap CLI, validation, PTY spawn/read/write lifecycle, state loop, owned-child cleanup, and process exit.
- `src/bin/windows_tui_dogfood/screen.rs`
  - ANSI parser wrapper, normalized screen text, stability tracking, and assertion evaluation.
- `src/bin/windows_tui_dogfood/artifacts.rs`
  - Result schema and artifact directory writing.
- `docs/superpowers/specs/2026-08-07-windows-tui-dogfood-harness-design.md`
  - Approved behavioral and safety design.
- `docs/superpowers/plans/2026-08-07-windows-tui-dogfood-harness.md`
  - This implementation plan.

**Modify:**

- `Cargo.toml`
  - Register `windows_tui_dogfood`; add optional `portable-pty` and `vt100`; activate both from `dev-bins`.
- `Cargo.lock`
  - Record resolved development dependencies.

---

### Task 1: Add and prove deterministic terminal screen observation

**Files:**
- Modify: `Cargo.toml:99-129,131-176,208-215`
- Create: `src/bin/windows_tui_dogfood.rs`
- Create: `src/bin/windows_tui_dogfood/screen.rs`
- Modify: `Cargo.lock`

**Interfaces:**
- Produces:
  - `screen::ScreenObserver::new(rows: u16, cols: u16, now: Instant) -> ScreenObserver`
  - `ScreenObserver::process(&mut self, bytes: &[u8], now: Instant)`
  - `ScreenObserver::text(&self) -> &str`
  - `ScreenObserver::is_stable(&self, now: Instant, interval: Duration) -> bool`
  - `screen::evaluate_assertions(text: &str, expected: &[String], forbidden: &[String]) -> AssertionState`
  - `AssertionState { missing: Vec<String>, forbidden_match: Option<String> }`
  - `AssertionState::passed(&self) -> bool`

- [ ] **Step 1: Register the feature-gated binary and optional dependencies**

Add after `tui_bench`:

```toml
[[bin]]
name = "windows_tui_dogfood"
path = "src/bin/windows_tui_dogfood.rs"
required-features = ["dev-bins"]
```

Add optional dependencies:

```toml
portable-pty = { version = "0.9.0", optional = true }
vt100 = { version = "0.16.2", optional = true }
```

Extend the feature:

```toml
dev-bins = ["jcode-tui/dev-bins", "dep:portable-pty", "dep:vt100"]
```

Create a compile scaffold in `windows_tui_dogfood.rs`:

```rust
mod screen;

fn main() -> anyhow::Result<()> {
    anyhow::bail!("windows_tui_dogfood is not implemented yet")
}
```

- [ ] **Step 2: Write failing screen tests**

In `screen.rs`, add tests before implementation that require:

```rust
#[test]
fn normalizes_terminal_rows_without_destroying_internal_spacing() {
    let text = normalize_screen("Peer Messaging   \r\nPlanner  core-team  \r\n\r\n");
    assert_eq!(text, "Peer Messaging\nPlanner  core-team");
}

#[test]
fn assertion_state_reports_missing_and_forbidden_text() {
    let expected = vec!["Peer Messaging".to_string(), "Planner".to_string()];
    let forbidden = vec!["CONFIGURATION ERROR".to_string()];
    let state = evaluate_assertions("Peer Messaging\nCONFIGURATION ERROR", &expected, &forbidden);
    assert_eq!(state.missing, vec!["Planner"]);
    assert_eq!(state.forbidden_match.as_deref(), Some("CONFIGURATION ERROR"));
    assert!(!state.passed());
}

#[test]
fn screen_is_stable_only_after_non_empty_text_stops_changing() {
    let start = Instant::now();
    let mut observer = ScreenObserver::new(4, 40, start);
    assert!(!observer.is_stable(start + Duration::from_secs(1), Duration::from_millis(500)));
    observer.process(b"Planner", start + Duration::from_millis(10));
    assert!(!observer.is_stable(start + Duration::from_millis(400), Duration::from_millis(500)));
    assert!(observer.is_stable(start + Duration::from_millis(510), Duration::from_millis(500)));
}
```

- [ ] **Step 3: Run the tests and confirm RED**

Run:

```text
cargo test --features dev-bins --bin windows_tui_dogfood screen::tests -- --nocapture
```

Expected: compilation fails because `normalize_screen`, `evaluate_assertions`,
`AssertionState`, and `ScreenObserver` are not implemented.

- [ ] **Step 4: Implement the minimal screen module**

Implement `normalize_screen` by replacing CRLF and CR with LF, trimming only row
ends, and removing trailing blank rows. Implement `ScreenObserver` with a
`vt100::Parser`, cached normalized text, and `last_changed: Instant`. On each
`process`, feed bytes to the parser, normalize `screen().contents()`, and update
`last_changed` only when text changes.

Implement `evaluate_assertions` as case-sensitive substring matching. Preserve
expected order in `missing`; return the first configured forbidden string found.

- [ ] **Step 5: Run the focused tests and confirm GREEN**

Run:

```text
cargo test --features dev-bins --bin windows_tui_dogfood screen::tests -- --nocapture
```

Expected: all screen tests pass.

- [ ] **Step 6: Refactor without changing behavior**

Keep `normalize_screen` private outside the module. Expose only the observer and
assertion types required by the runner. Re-run Step 5 after refactoring.

---

### Task 2: Add failure-preserving artifact output

**Files:**
- Create: `src/bin/windows_tui_dogfood/artifacts.rs`
- Modify: `src/bin/windows_tui_dogfood.rs`

**Interfaces:**
- Produces:
  - `artifacts::RunStatus::{Passed, Failed}` serialized as snake case.
  - `artifacts::RunResult` with schema version, timestamps, duration, command,
    assertions, dimensions, outcome, process metadata, and artifact paths.
  - `artifacts::write_artifacts(dir: &Path, raw: &[u8], screen: &str, result: &mut RunResult) -> anyhow::Result<ArtifactPaths>`
  - `ArtifactPaths { raw_ansi: PathBuf, screen_text: PathBuf, result_json: PathBuf }`

- [ ] **Step 1: Declare the artifact module and write failing artifact tests**

Add `#[path = "windows_tui_dogfood/artifacts.rs"] mod artifacts;` beside the
screen module declaration in the binary entry point.

Add tests using `tempfile::tempdir()`:

```rust
#[test]
fn writes_raw_screen_and_pretty_result_artifacts() {
    let dir = tempfile::tempdir().unwrap();
    let mut result = RunResult::test_fixture(RunStatus::Passed, "all assertions matched");
    let paths = write_artifacts(dir.path(), b"\x1b[2JPlanner", "Planner", &mut result).unwrap();

    assert_eq!(std::fs::read(&paths.raw_ansi).unwrap(), b"\x1b[2JPlanner");
    assert_eq!(std::fs::read_to_string(&paths.screen_text).unwrap(), "Planner\n");
    let json = std::fs::read_to_string(&paths.result_json).unwrap();
    assert!(json.contains("\"schema_version\": 1"));
    assert!(json.contains("\"status\": \"passed\""));
}
```

Add a second test asserting that an existing non-directory artifact path returns
an error naming the path.

- [ ] **Step 2: Run the tests and confirm RED**

Run:

```text
cargo test --features dev-bins --bin windows_tui_dogfood artifacts::tests -- --nocapture
```

Expected: compilation fails because the artifact types and writer do not exist.

- [ ] **Step 3: Implement the minimal artifact module**

Use `serde::Serialize`, `chrono::DateTime<Utc>`, and `std::fs`. Create the
artifact directory, write `raw.ansi`, write `screen.txt` with exactly one final
newline, set the three paths in `RunResult`, then pretty-print `result.json`.

Do not persist environment variables. Do not swallow any filesystem or JSON
error; attach path context with `anyhow::Context`.

- [ ] **Step 4: Run artifact tests and confirm GREEN**

Run:

```text
cargo test --features dev-bins --bin windows_tui_dogfood artifacts::tests -- --nocapture
```

Expected: all artifact tests pass.

---

### Task 3: Implement the owned Windows PTY run loop

**Files:**
- Modify: `src/bin/windows_tui_dogfood.rs`
- Modify: `src/bin/windows_tui_dogfood/screen.rs`
- Modify: `src/bin/windows_tui_dogfood/artifacts.rs`

**Interfaces:**
- Consumes: `ScreenObserver`, `evaluate_assertions`, `RunResult`, and
  `write_artifacts`.
- Produces:
  - `Args` parsed by Clap.
  - `Args::validate(&self) -> anyhow::Result<()>`.
  - `run(args: Args) -> anyhow::Result<RunStatus>`.
  - Process exit `0` only for `RunStatus::Passed`.

- [ ] **Step 1: Write failing CLI validation tests**

Add tests that construct `Args` directly and assert:

```rust
#[test]
fn validation_rejects_zero_dimensions_and_missing_expectations() {
    let mut args = valid_args();
    args.rows = 0;
    assert!(args.validate().unwrap_err().to_string().contains("rows"));

    let mut args = valid_args();
    args.expected.clear();
    assert!(args.validate().unwrap_err().to_string().contains("--expect"));
}

#[test]
fn validation_rejects_missing_binary_before_spawning() {
    let mut args = valid_args();
    args.binary = PathBuf::from("missing-jcode.exe");
    assert!(args.validate().unwrap_err().to_string().contains("binary"));
}
```

- [ ] **Step 2: Run validation tests and confirm RED**

Run:

```text
cargo test --features dev-bins --bin windows_tui_dogfood validation_ -- --nocapture
```

Expected: compilation fails because `Args` and `validate` do not exist.

- [ ] **Step 3: Implement Clap arguments and validation**

Use the exact CLI contract from the design. Store repeated child arguments as
`Vec<OsString>` with `#[arg(long = "arg", allow_hyphen_values = true)]`, so
values such as `--arg=--no-update` are passed through rather than parsed as
harness flags. Compute the default artifact directory at runtime as:

```rust
PathBuf::from("target")
    .join("tui-dogfood")
    .join(Utc::now().format("%Y%m%dT%H%M%S%.3fZ").to_string())
```

Validation occurs before opening the PTY.

- [ ] **Step 4: Run validation tests and confirm GREEN**

Run the Step 2 command. Expected: all validation tests pass.

- [ ] **Step 5: Implement the PTY runner**

Use these concrete APIs:

```rust
let pty_system = portable_pty::native_pty_system();
let pair = pty_system.openpty(PtySize {
    rows: args.rows,
    cols: args.cols,
    pixel_width: 0,
    pixel_height: 0,
})?;
let mut command = CommandBuilder::new(&args.binary);
command.args(&args.child_args);
command.cwd(&args.cwd);
command.env("TERM", "xterm-256color");
let mut child = pair.slave.spawn_command(command)?;
drop(pair.slave);
let mut reader = pair.master.try_clone_reader()?;
let mut writer = pair.master.take_writer()?;
```

Spawn one reader thread that sends `Result<Vec<u8>, String>` over
`std::sync::mpsc`. Create the artifact path, raw buffer, screen observer, and
initial `RunResult` before opening the PTY. Convert validation, open, spawn,
read, write, assertion, child-exit, and timeout errors into a failed result so
those paths still attempt artifact output.

In the main loop:

1. Drain chunks with `recv_timeout(Duration::from_millis(25))`.
2. Append bytes to the raw buffer and call `observer.process`.
3. Poll `child.try_wait()` and fail on an early exit.
4. In startup, send `command + "\r"` only after the non-empty screen is stable.
5. In verification, fail immediately on a forbidden match.
6. After all expected strings match, require `settle_ms` of unchanged screen and
   re-evaluate assertions.
7. Fail at the single overall deadline and list missing strings.

Always attempt this cleanup after a result is known:

```rust
let mut exit_status = child.try_wait()?;
if exit_status.is_none() {
    child.kill()?;
    exit_status = child.wait().ok();
}
drop(writer);
drop(pair.master);
```

After cleanup, drain reader messages for at most 250 milliseconds, applying any
final chunks to the raw buffer and `ScreenObserver`. Do not wait indefinitely on
the reader thread. Artifact writing happens after the bounded drain so the final
screen and raw bytes are as complete as possible.

Preserve the first run failure if cleanup also fails, but append cleanup failure
text to the final reason. The only error allowed to escape without a written
`result.json` is failure to create or write the artifact destination itself.

- [ ] **Step 6: Run all binary unit tests**

Run:

```text
cargo test --features dev-bins --bin windows_tui_dogfood -- --nocapture
```

Expected: all tests pass with no harness warnings.

- [ ] **Step 7: Run focused strict Clippy and formatting**

Run:

```text
cargo fmt --all
cargo fmt --all -- --check
cargo clippy --features dev-bins --bin windows_tui_dogfood -- -D warnings
```

Expected: all commands exit `0`.

- [ ] **Step 8: Commit the documented, unit-tested harness**

Stage only:

```text
Cargo.toml
Cargo.lock
src/bin/windows_tui_dogfood.rs
src/bin/windows_tui_dogfood/screen.rs
src/bin/windows_tui_dogfood/artifacts.rs
docs/superpowers/specs/2026-08-07-windows-tui-dogfood-harness-design.md
docs/superpowers/plans/2026-08-07-windows-tui-dogfood-harness.md
```

Commit:

```text
git commit -m "test(tui): add Windows dogfood harness"
```

---

### Task 4: Prove the real `/peers` TUI and integrate locally

**Files:**
- Runtime artifacts only: `target/tui-dogfood/<timestamp>/`
- No expected source changes unless runtime evidence exposes a defect.

**Interfaces:**
- Consumes: compiled `windows_tui_dogfood.exe`, freshly reloaded `jcode.exe`,
  Michael's existing peer configuration, and the exact Jcode workspace.
- Produces: passing `result.json`, non-empty `raw.ansi`, matching `screen.txt`, a
  clean independent review, and a fast-forwarded local main worktree.

- [ ] **Step 1: Build and reload the TUI**

Use coordinated self-development build-reload with target `tui`. Continue
automatically after reload.

- [ ] **Step 2: Build the harness binary**

Run:

```text
cargo build --profile selfdev --features dev-bins --bin windows_tui_dogfood
```

Expected: `target\selfdev\windows_tui_dogfood.exe` exists.

- [ ] **Step 3: Run the real Windows dogfood scenario**

Run as one Windows command:

```text
target\selfdev\windows_tui_dogfood.exe --binary target\selfdev\jcode.exe --cwd C:\Users\micha\dev\jcode --arg=--no-update --command /peers --expect "Peer Messaging" --expect Planner --expect "Ambient initiation: OFF" --expect SpecScore --expect Strategy --expect NPLabs --expect Flackton --forbid "CONFIGURATION ERROR" --timeout-secs 45
```

If `--arg=--no-update` is rejected by Clap, fix argument parsing with a failing
CLI parsing test before changing production code.

- [ ] **Step 4: Inspect the generated evidence**

Read the printed artifact directory and verify:

- `result.json` contains `"status": "passed"`.
- `raw.ansi` has nonzero length.
- `screen.txt` contains all seven required peer strings.
- `screen.txt` does not contain `CONFIGURATION ERROR`.
- The shared daemon remains running after harness cleanup.

If the screen does not match, treat it as a runtime defect. Add a failing test
for any deterministic parser or runner bug before fixing it. Do not weaken the
required assertions to make the run pass.

- [ ] **Step 5: Run final source verification**

Run fresh:

```text
cargo fmt --all -- --check
cargo test --features dev-bins --bin windows_tui_dogfood -- --nocapture
cargo clippy --features dev-bins --bin windows_tui_dogfood -- -D warnings
git diff --check
git status --short
```

Also rerun the focused peer suites if runtime debugging changed peer source.

- [ ] **Step 6: Request an independent final diff review**

The reviewer must inspect the manifest, all three harness modules, docs, safety
cleanup, timeout behavior, artifact failure behavior, and the live evidence.
No blocking finding may remain before integration.

- [ ] **Step 7: Commit any review fixes and re-verify**

Use a focused commit. Repeat Steps 1 through 6 for any source behavior changed by
review.

- [ ] **Step 8: Fast-forward the local main worktree**

First inspect:

```text
cd /d C:\Users\micha\dev\jcode-premerge
git status --short
git branch --show-current
git log -1 --oneline
```

Confirm the only unrelated item remains untracked `.b.bat`. Then run:

```text
git merge --ff-only feature/peer-messaging
```

Do not stash, delete, stage, or modify `.b.bat`. Do not push.

- [ ] **Step 9: Verify the integrated worktree**

Run:

```text
git status --short
git log -3 --oneline
```

Confirm `.b.bat` remains untracked and the harness commit is the local main tip.
Report the exact runtime artifact directory, test results, review result, merge
result, and all known pre-existing broader-suite gaps.
