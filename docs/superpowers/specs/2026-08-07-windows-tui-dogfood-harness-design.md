# Windows TUI Dogfood Harness Design

- **Date:** 2026-08-07
- **Status:** Approved by Michael on 2026-08-07
- **Owner:** Michael
- **Implementation owner:** Jcode
- **Initial scenario:** Native `/peers` overview in the real Windows TUI

## 1. Purpose

Add a permanent, model-free development harness that launches the real Jcode
terminal application inside a Windows pseudo terminal, drives a small command
sequence, observes the rendered screen, and saves reproducible evidence.

The first use is closing the remaining peer-messaging verification gap. Unit
and protocol tests already cover the implementation, but they do not prove that
a freshly built Windows TUI can start, accept `/peers`, and visibly render the
configured peer overview.

## 2. Plain-English outcome

A developer can run one Cargo command and receive a pass or failure plus local
artifacts:

```text
cargo run --features dev-bins --bin windows_tui_dogfood -- --binary target\selfdev\jcode.exe --cwd C:\Users\micha\dev\jcode --arg=--no-update --command /peers --expect "Peer Messaging" --expect Planner --expect "Ambient initiation: OFF" --expect SpecScore --expect Strategy --expect NPLabs --expect Flackton
```

The harness launches the actual executable, waits for the TUI screen to settle,
types `/peers`, waits for a stable matching result, terminates only the client it
created, and writes:

- `raw.ansi`: exact bytes read from the pseudo terminal.
- `screen.txt`: final plain-text terminal screen.
- `result.json`: arguments, timing, assertions, process outcome, and failure
  reason when applicable.

## 3. Scope

### 3.1 Included

- A root development binary named `windows_tui_dogfood`.
- A real pseudo-terminal session through Windows ConPTY.
- Real keyboard input and ANSI terminal output.
- Deterministic screen stabilization and timeout behavior.
- Required-text and forbidden-text assertions.
- Artifacts on both success and failure.
- Safe cleanup of the child handle owned by the harness.
- Pure unit tests for screen observation, assertion behavior, and artifact
  serialization.
- A live `/peers` dogfood run after the Jcode build is reloaded.

### 3.2 Excluded

- Normal CI execution.
- Provider/model calls.
- Automatic peer messages.
- Mutation of `~/.jcode/config.toml` or peer-group configuration.
- Starting, stopping, or replacing the shared Jcode daemon.
- Sending console-wide Ctrl+C or control-break events.
- Screenshot or pixel-perfect visual comparison.
- A general macro language for multi-step terminal scripts.

The first version intentionally supports one command per run. Repeated command
sequences can be added later only when a real use case requires them.

## 4. Architecture

### 4.1 Development-only binary

Declare the binary in the root `Cargo.toml` with
`required-features = ["dev-bins"]`. Add `portable-pty` and `vt100` as optional
dependencies activated only by `dev-bins`, so normal release builds do not
compile or ship the harness dependencies.

The implementation is split into three focused files:

- `src/bin/windows_tui_dogfood.rs`
  - CLI parsing, PTY lifecycle, reader thread, command injection, timeout loop,
    cleanup, and final exit code.
- `src/bin/windows_tui_dogfood/screen.rs`
  - ANSI parsing, screen normalization, stable-screen tracking, and required or
    forbidden text evaluation.
- `src/bin/windows_tui_dogfood/artifacts.rs`
  - Serializable result model and atomic-enough artifact directory writing.

### 4.2 Pseudo-terminal lifecycle

Use `portable-pty 0.9.0`:

1. Open a native PTY with fixed rows and columns.
2. Build a child command with the requested executable, arguments, working
   directory, and `TERM=xterm-256color`.
3. Spawn the real Jcode client into the slave side.
4. Drop the parent slave handle after spawning.
5. Clone the master reader and take its single writer.
6. Run the blocking reader on a dedicated thread and forward byte chunks over a
   standard channel. After child cleanup, drain that channel for a short bounded
   interval so final terminal bytes reach the artifacts; never block forever on
   joining the reader thread.
7. Keep the child handle in the main thread for `try_wait`, `kill`, and `wait`.

The normal dogfood invocation includes `--no-update`, and it runs only after
self-development reload has established the intended shared daemon. The harness
must not run server-management commands and must not kill by process name,
console group, or global control event.

### 4.3 Terminal observation

Use `vt100 0.16.2` with the same rows and columns as the PTY. Every received byte
chunk is appended to `raw.ansi` and processed by the parser.

The harness also performs the minimum terminal-side protocol needed for Jcode
startup: when output contains the cursor-position query `ESC [ 6 n`, including
when the sequence is split across reader chunks, write `ESC [ 1 ; 1 R` back to
the PTY exactly once for that query. Without this response Crossterm waits for a
real terminal reply and the TUI never completes startup.

`screen.txt` comes from `parser.screen().contents()` after normalization:

- Convert CRLF and lone CR to LF.
- Trim trailing spaces on each row.
- Remove trailing blank rows.
- Preserve internal spacing and line order.

The observer records when normalized screen text last changed. A screen is
stable only when it is non-empty and unchanged for the configured stability
interval.

### 4.4 Run state machine

The harness has four states:

1. **Starting**
   - Read terminal output until a non-empty screen is stable and every optional
     raw startup marker has appeared.
   - Fail if the child exits or the overall timeout expires.
2. **Command sent**
   - Write the exact command followed by carriage return and flush.
3. **Verifying**
   - Fail immediately if any forbidden string appears.
   - Once all required strings appear, require the screen to remain unchanged
     for the configured settlement interval.
   - Re-evaluate all assertions after settlement.
4. **Finishing**
   - Save artifacts.
   - Kill the owned client if still running, then wait for it.
   - Exit `0` only for a passed assertion result. Exit nonzero for timeout,
     early child exit, PTY failure, artifact failure, or assertion failure.

One overall timeout covers startup and verification so a stuck client cannot
leave the harness running indefinitely.

## 5. CLI contract

Use Clap derive with these arguments:

- `--binary <PATH>`: required executable path.
- `--cwd <PATH>`: required child working directory.
- `--arg <VALUE>`: repeatable child argument.
- `--startup-expect <RAW_TEXT>`: repeatable optional substring that must appear
  in raw terminal output before the command is sent. The live Jcode scenario
  uses `jcode:d:` so the new client is attached to the daemon rather than merely
  displaying an initially stable screen.
- `--command <TEXT>`: command to type, default `/peers`.
- `--expect <TEXT>`: repeatable required substring, at least one required.
- `--forbid <TEXT>`: repeatable forbidden substring.
- `--timeout-secs <N>`: overall timeout, default `30`.
- `--stable-ms <N>`: startup stability interval, default `750`.
- `--settle-ms <N>`: post-match stability interval, default `750`.
- `--rows <N>`: terminal rows, default `40`.
- `--cols <N>`: terminal columns, default `120`.
- `--artifacts <PATH>`: optional explicit artifact directory. When omitted,
  use `target/tui-dogfood/<UTC timestamp>`.

Validation rejects zero dimensions, zero timeout values, an empty command, an
empty startup marker, no required assertions, missing executable, or missing
working directory before a child is started.

## 6. Artifact contract

`result.json` is pretty-printed JSON containing:

- Schema version `1`.
- UTC start and finish timestamps.
- Duration in milliseconds.
- Binary and working directory.
- Child arguments, raw startup markers, and typed command.
- Terminal dimensions and timeout settings.
- Required and forbidden strings.
- Final status: `passed` or `failed`.
- Human-readable reason.
- Child process ID when available.
- Child exit status when observed.
- Paths of `raw.ansi` and `screen.txt`.

Artifacts are written for validation errors, PTY setup failures, assertion
failures, early child exits, and timeouts as well as success. Once the result
model and artifact path exist, runner errors are converted into a failed result
instead of escaping before evidence is written. If artifact writing itself
fails, the process still attempts child cleanup and returns a nonzero error that
names the artifact failure.

The default artifact root is already inside `target/`, so generated evidence is
not committed.

## 7. Safety boundaries

- The harness is available only with `dev-bins`.
- It does not alter normal Jcode startup or release behavior.
- It launches only the executable explicitly supplied by the caller.
- It terminates only the child represented by its own PTY child handle.
- It never enumerates or kills other Jcode processes.
- It never sends Ctrl+C or Ctrl+Break to the caller's console.
- It does not modify configuration, sessions, memory, knowledge, or task state.
- It makes no provider request itself. The `/peers` scenario is a local slash
  command and consumes no model tokens.
- The live peer test uses Michael's existing approved exact-root allowlist and
  ambient-disabled configuration without changing either.

## 8. Failure behavior

Every failure must be plain and actionable:

- Startup timeout: include the final normalized screen.
- Startup marker timeout: list missing raw terminal markers and include the
  final normalized screen.
- Verification timeout: list missing required strings.
- Forbidden text: name the first forbidden string found.
- Child exit: report that it exited before verification and include status when
  available.
- PTY read/write error: name the operation.
- Artifact error: name the destination path.

A failed run keeps its artifacts. No failure path may panic intentionally.

## 9. Verification contract

Implementation is accepted only after all of these are demonstrated:

1. Pure unit tests first fail for missing screen and artifact behavior, then
   pass after implementation.
2. `cargo test --features dev-bins --bin windows_tui_dogfood` passes.
3. `cargo clippy --features dev-bins --bin windows_tui_dogfood -- -D warnings`
   passes.
4. `cargo fmt --all -- --check` passes.
5. A self-development TUI build and reload succeeds.
6. The real harness launches the freshly built Windows TUI from the exact
   `C:\Users\micha\dev\jcode` working directory.
7. `/peers` visibly contains `Peer Messaging`, `Planner`, `Ambient initiation:
   OFF`, `SpecScore`, `Strategy`, `NPLabs`, and `Flackton` in the saved
   `screen.txt`.
8. `result.json` reports `passed` and the raw ANSI artifact is non-empty.
9. The feature branch receives a fresh independent diff review with no blocking
   findings.
10. The local main worktree is fast-forwarded only after the runtime proof,
    while preserving its untracked `.b.bat`, and nothing is pushed.

## 10. Rollback

Rollback is one focused revert of the harness commit. Because the binary and
both dependencies are feature-gated, reverting removes the tool without
changing peer messaging, normal Jcode execution, saved sessions, configuration,
or release artifacts.
