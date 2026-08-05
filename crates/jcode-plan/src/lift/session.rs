//! Adapter from a stored jcode session transcript to [`TraceEvent`]s.
//!
//! This is the only part of lifting that knows about transcript shape. It reads
//! the persisted session JSON as untyped values rather than importing the
//! session types, so `jcode-plan` keeps its current dependency surface (serde
//! only) and the lifter stays usable on transcripts from older versions whose
//! schemas have since drifted.
//!
//! Extraction is deliberately narrow: only tool invocations become events, and a
//! tool's resource labels are read from the argument names it actually uses. An
//! unknown tool yields an event with no resources, which produces an isolated
//! node rather than a fabricated dependency.

use super::TraceEvent;
use serde_json::Value;

/// Longest label kept for a human-readable summary. Transcripts contain whole
/// file bodies and command output; unbounded labels would make lifted graphs
/// unreadable and bloat persisted plans.
const MAX_LABEL: usize = 120;

/// Longest resource *identity* retained. Identity must not use [`MAX_LABEL`]:
/// truncating an identity merges every resource sharing a long prefix, which
/// fabricates dependencies between unrelated files. This bound exists only to
/// stop a pathological transcript from bloating memory, and anything longer is
/// dropped rather than truncated, because a missing resource costs one missed
/// edge while a truncated one corrupts the graph.
const MAX_RESOURCE_ID: usize = 400;

/// Namespace prefix for a filesystem path resource.
const FILE_NS: &str = "file:";
/// Namespace prefix for a command-identity resource.
const CMD_NS: &str = "cmd:";
/// Namespace prefix for a fetched URL resource.
const URL_NS: &str = "url:";

/// Tag a resource with its namespace so unlike things cannot collide.
///
/// Without this, a local write to `src/lib.rs` and a fetch of
/// `https://host/reference/src/lib.rs` compare equal under suffix matching and
/// produce an edge between two entirely unrelated actions.
fn namespaced(prefix: &str, value: &str) -> Option<String> {
    if value.is_empty() || value.chars().count() > MAX_RESOURCE_ID {
        return None;
    }
    Some(format!("{prefix}{value}"))
}

/// Extract an ordered trace from a parsed session document.
///
/// `session` is the top-level object of a `session_*.json` file. Returns an
/// empty trace for any document without a usable `messages` array, since a
/// session with no tool calls genuinely has no graph to recover.
pub fn trace_from_session(session: &Value) -> Vec<TraceEvent> {
    let Some(messages) = session.get("messages").and_then(Value::as_array) else {
        return Vec::new();
    };
    // Tool results arrive in a later message than the call, so failures are
    // collected first and joined back by tool_use id.
    let failures = failed_tool_use_ids(messages);
    let mut events = Vec::new();
    let mut turn = 0usize;
    for message in messages {
        // A user message that is not a tool result is a new instruction, and so
        // a real boundary in intent that segmentation must not cross.
        if message.get("role").and_then(Value::as_str) == Some("user")
            && !contains_tool_result(message)
        {
            if !events.is_empty() {
                turn += 1;
            }
            continue;
        }
        for block in content_blocks(message) {
            if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            let Some(tool) = block.get("name").and_then(Value::as_str) else {
                continue;
            };
            let input = block.get("input").unwrap_or(&Value::Null);
            let failed = block
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| failures.contains(&id.to_string()));
            let seq = events.len();
            let (reads, writes) = resources(tool, input);
            events.push(
                TraceEvent::new(seq, turn, tool, summarize(tool, input))
                    .reads(reads)
                    .writes(writes)
                    .failed(failed),
            );
        }
    }
    events
}

fn content_blocks(message: &Value) -> impl Iterator<Item = &Value> {
    message
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| blocks.iter())
        .unwrap_or_else(|| [].iter())
}

fn contains_tool_result(message: &Value) -> bool {
    content_blocks(message)
        .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
}

/// Ids of tool calls whose result reported an error. Transcript formats have
/// varied here, so both the structured `is_error` flag and the conventional
/// `Error:` prefix are honored.
fn failed_tool_use_ids(messages: &[Value]) -> Vec<String> {
    let mut ids = Vec::new();
    for message in messages {
        for block in content_blocks(message) {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            let Some(id) = block.get("tool_use_id").and_then(Value::as_str) else {
                continue;
            };
            let flagged = block.get("is_error").and_then(Value::as_bool) == Some(true);
            let looks_failed = result_text(block).is_some_and(|text| {
                let head = text.trim_start();
                head.starts_with("Error:") || head.starts_with("Exit code: 1")
            });
            if flagged || looks_failed {
                ids.push(id.to_string());
            }
        }
    }
    ids
}

fn result_text(block: &Value) -> Option<&str> {
    match block.get("content") {
        Some(Value::String(text)) => Some(text.as_str()),
        Some(Value::Array(parts)) => parts
            .iter()
            .find_map(|part| part.get("text").and_then(Value::as_str)),
        _ => None,
    }
}

/// Resource labels for a tool call, as (reads, writes).
///
/// The mapping keys off argument names rather than a per-tool table so that new
/// tools following existing conventions are picked up without a code change.
fn resources(tool: &str, input: &Value) -> (Vec<String>, Vec<String>) {
    let mutating = matches!(
        tool.to_ascii_lowercase().as_str(),
        "write" | "edit" | "multiedit" | "patch" | "apply_patch" | "notebookedit"
    );
    let mut paths: Vec<String> = Vec::new();
    for key in ["file_path", "path", "notebook_path", "file", "target"] {
        if let Some(value) = input.get(key).and_then(Value::as_str) {
            paths.extend(file_resource(value));
        }
    }
    // Patch-style tools carry their targets inside the patch body.
    if paths.is_empty()
        && let Some(text) = input.get("patch_text").and_then(Value::as_str)
    {
        paths.extend(patch_targets(text));
    }
    if !paths.is_empty() {
        return if mutating {
            (Vec::new(), paths)
        } else {
            (paths, Vec::new())
        };
    }
    if let Some(command) = input.get("command").and_then(Value::as_str) {
        return command_resources(command);
    }
    if let Some(url) = input.get("url").and_then(Value::as_str) {
        // A fetched URL lives in its own namespace: a page whose address ends in
        // `src/lib.rs` is not the local file of that name.
        return (
            namespaced(URL_NS, url.trim()).into_iter().collect(),
            Vec::new(),
        );
    }
    (Vec::new(), Vec::new())
}

/// A namespaced filesystem-path resource, or nothing if the path is unusable.
fn file_resource(path: &str) -> Option<String> {
    namespaced(FILE_NS, &normalize_path(path))
}

/// Split a command line into tokens, keeping quoted spans whole.
///
/// Naive whitespace splitting mangles the common Windows shape
/// `"C:\Program Files\Git\bin\bash.exe" -lc "..."`, which both hides the program
/// identity and shreds the inline body into meaningless fragments.
fn tokenize(command: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for ch in command.chars() {
        match quote {
            Some(open) if ch == open => quote = None,
            Some(_) => current.push(ch),
            None if ch == '"' || ch == '\'' => quote = Some(ch),
            None if ch.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            None => current.push(ch),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Maximum shell-wrapper unwrapping depth. Nesting beyond this is pathological,
/// and a bound keeps a crafted transcript from driving unbounded recursion.
const MAX_UNWRAP_DEPTH: usize = 3;

/// The command that actually ran, with shell wrappers removed.
///
/// `bash -lc "bash s/plan.sh"` really runs `bash s/plan.sh`, and everything
/// downstream (identity, classification, path recovery) should see that. Only
/// *shell* wrappers are unwrapped: a shell's `-c` argument is a command, whereas
/// an interpreter's `-e`/`-c` argument is source code and must not be read as one.
fn resolve_command(command: &str) -> String {
    let mut current = effective_command(command).to_string();
    for _ in 0..MAX_UNWRAP_DEPTH {
        let tokens = tokenize(&current);
        let Some(program) = tokens.first() else { break };
        if !is_shell(program) {
            break;
        }
        let body = tokens
            .iter()
            .skip(1)
            .position(|token| is_shell_command_flag(token))
            .and_then(|index| tokens.get(index + 2));
        let Some(body) = body else { break };
        let inner = effective_command(body).to_string();
        if inner.is_empty() || inner == current {
            break;
        }
        current = inner;
    }
    current
}

fn is_shell(program: &str) -> bool {
    matches!(
        program_name(program).as_str(),
        "sh" | "bash" | "zsh" | "dash" | "pwsh" | "powershell" | "cmd"
    )
}

/// Flags whose value is a command to run, as opposed to source to evaluate.
fn is_shell_command_flag(token: &str) -> bool {
    let lower = token.to_ascii_lowercase();
    matches!(lower.as_str(), "-c" | "-lc" | "-lic" | "/c" | "/k")
        || lower == "-command"
        || lower == "--command"
}

/// Program name without directory or `.exe`, lowercased.
fn program_name(program: &str) -> String {
    program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(program)
        .trim_end_matches(".exe")
        .to_ascii_lowercase()
}

/// Resource labels for a shell command, as (reads, writes).
///
/// The command identity is always a read, so repeated runs of the same command
/// link up while unrelated commands stay separate. On top of that, file-looking
/// operands are recovered, because a shell step that runs a script written by an
/// earlier step depends on that step just as surely as a `Read` would. Without
/// this, a session that writes a helper and immediately runs it lifts to two
/// unconnected nodes and reports parallelism that never existed.
///
/// Redirection targets are writes; everything else that looks like a path is a
/// read. Operands are only recovered from commands that are plainly file-taking,
/// so that `git commit -m "fix a.rs bug"` does not invent a dependency on a file
/// it merely mentions.
fn command_resources(command: &str) -> (Vec<String>, Vec<String>) {
    let resolved = resolve_command(command);
    let mut reads: Vec<String> = namespaced(CMD_NS, &command_key(command))
        .into_iter()
        .collect();
    let mut writes = Vec::new();
    let mut tokens = tokenize(&resolved).into_iter().peekable();
    let mut seen_program = false;
    let mut operands_are_paths = false;
    while let Some(token) = tokens.next() {
        // `> out.txt` and `>> out.txt`, with or without a space after the arrow.
        // Redirection is scanned for every command, not just file-taking ones,
        // because capturing output to a file is the most common way one step
        // hands work to the next.
        if let Some(rest) = token.strip_prefix(">>").or_else(|| token.strip_prefix('>')) {
            let target = if rest.is_empty() {
                tokens.next().unwrap_or_default()
            } else {
                rest.to_string()
            };
            push_path(&mut writes, &target);
            continue;
        }
        if token.starts_with('-') {
            // Inline source (`node -e "..."`, `python -c "..."`) is code, not a
            // path, and the code body can contain anything at all. Stop
            // recovering operands rather than risk reading fragments of it.
            if is_eval_flag(&token) {
                operands_are_paths = false;
                seen_program = true;
                tokens.next();
            }
            continue;
        }
        if !seen_program {
            // `VAR=x node a.js` runs node; skip assignment prefixes so the
            // script operand is still recovered.
            if is_env_assignment(&token) {
                continue;
            }
            // The first bare token is the program itself, which is already
            // covered by the command key. A bare script path (`scripts/x.py`)
            // is not a known interpreter, so its own operands stay unread.
            seen_program = true;
            operands_are_paths = takes_file_operands(&token);
            continue;
        }
        if operands_are_paths {
            push_path(&mut reads, &token);
        }
    }
    (reads, writes)
}

/// Programs whose bare operands are reliably file paths. Deliberately short: a
/// wrong entry here fabricates dependencies, which is worse than missing one.
fn takes_file_operands(program: &str) -> bool {
    matches!(
        program_name(program).as_str(),
        "node"
            | "python"
            | "python3"
            | "py"
            | "deno"
            | "bun"
            | "ruby"
            | "sh"
            | "bash"
            | "zsh"
            | "pwsh"
            | "powershell"
            | "cat"
            | "type"
            | "head"
            | "tail"
            | "less"
            | "wc"
            | "source"
    )
}

/// Record an operand that actually looks like a file path. Interpreters are also
/// handed inline code and shell fragments, so a token only counts when it ends in
/// a short alphanumeric extension and contains no code punctuation. Skipping a
/// real path costs one missing edge; accepting a code fragment invents one.
fn push_path(paths: &mut Vec<String>, token: &str) {
    let cleaned = token.trim_matches(['"', '\'']);
    if cleaned.is_empty() || cleaned.contains(['(', ')', '{', '}', '=', '$', ',', ';']) {
        return;
    }
    let name = cleaned.rsplit(['/', '\\']).next().unwrap_or(cleaned);
    let Some((stem, extension)) = name.rsplit_once('.') else {
        return;
    };
    let plausible = !stem.is_empty()
        && (1..=6).contains(&extension.chars().count())
        && extension.chars().all(|c| c.is_ascii_alphanumeric());
    if !plausible {
        return;
    }
    let Some(resource) = file_resource(cleaned) else {
        return;
    };
    if !paths.contains(&resource) {
        paths.push(resource);
    }
}

/// Paths named by `*** Update/Add/Delete File:` headers in a Codex-style patch.
fn patch_targets(patch: &str) -> Vec<String> {
    patch
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            for prefix in ["*** Update File:", "*** Add File:", "*** Delete File:"] {
                if let Some(rest) = line.strip_prefix(prefix) {
                    return file_resource(rest.trim());
                }
            }
            None
        })
        .collect()
}

/// Reduce a command to a stable identity: the program plus its first argument.
/// Full command lines vary by flags and paths, which would make every run look
/// like a distinct resource and erase the dependency the repetition represents.
///
/// Shell wrappers and directory changes are stripped first. `cd x && npm test`
/// and `bash -lc "npm test"` both identify the same work as `npm test`, and
/// keying on `cd x` or on `bash -lc` would collapse unrelated commands into one
/// indistinguishable resource.
fn command_key(command: &str) -> String {
    let resolved = resolve_command(command);
    let mut key: Vec<String> = Vec::new();
    for token in tokenize(&resolved) {
        if token.starts_with('-') {
            // An eval flag means the rest is source code, which is unique to the
            // call and would defeat the point of a stable identity.
            if is_eval_flag(&token) {
                break;
            }
            continue;
        }
        if key.is_empty() {
            // `VAR=x node a.js` runs node; the assignment is a prefix, not the
            // program.
            if is_env_assignment(&token) {
                continue;
            }
            // Only the program is name-normalized: the same tool invoked as
            // `bash` and as `C:\Program Files\Git\bin\bash.exe` is one tool.
            key.push(program_name_or_token(&token));
        } else {
            // Arguments are kept as written; stripping their directories would
            // merge `s/plan.sh` with any other `plan.sh` in the session.
            key.push(token);
            break;
        }
    }
    // The key is an identity, so it is never truncated: shortening it would
    // merge every command sharing a long prefix. `namespaced` drops
    // pathological lengths outright instead.
    if key.is_empty() {
        resolved
    } else {
        key.join(" ")
    }
}

/// Flags whose value is source code to evaluate rather than a path or command.
fn is_eval_flag(token: &str) -> bool {
    matches!(token.to_ascii_lowercase().as_str(), "-e" | "-c" | "--eval")
}

/// Keep a token as written unless it is an absolute program path, in which case
/// the bare program name is the stable identity: the same tool invoked as
/// `bash` and as `C:\Program Files\Git\bin\bash.exe` is the same tool.
fn program_name_or_token(token: &str) -> String {
    if token.contains('/') || token.contains('\\') {
        let name = program_name(token);
        if !name.is_empty() {
            return name;
        }
    }
    token.to_string()
}

/// Whether a segment only prepares the environment rather than doing work, e.g.
/// `cd build`, `set VAR=1`, `export VAR=1`, or a bare `VAR=1`. Treating one of
/// these as the command would make unrelated steps share a resource label.
fn is_setup_segment(segment: &str) -> bool {
    let mut tokens = tokenize(segment).into_iter();
    let Some(first) = tokens.next() else {
        return true;
    };
    match first.as_str() {
        "cd" | "set" | "export" | "pushd" | "popd" => true,
        _ => is_env_assignment(&first) && tokens.next().is_none(),
    }
}

/// Whether a token is a `VAR=value` environment assignment. A Windows path such
/// as `C:\tmp` is not one, so the name must be a plain identifier.
fn is_env_assignment(token: &str) -> bool {
    let Some((name, _)) = token.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
        && name.chars().next().is_some_and(|c| !c.is_ascii_digit())
}

/// The part of a shell line that does the actual work: the first segment of a
/// `&&`/`;`/`|` chain that is not mere setup. Quoted spans are kept intact, so a
/// separator inside `bash -lc "a && b"` does not split the wrapper.
fn effective_command(command: &str) -> &str {
    let trimmed = command.trim();
    for (start, end) in top_level_segments(trimmed) {
        let segment = trimmed[start..end].trim();
        if segment.is_empty() || is_setup_segment(segment) {
            continue;
        }
        return segment;
    }
    trimmed
}

/// Byte ranges of `&`/`;`/`|`-separated segments, ignoring separators inside
/// quotes.
fn top_level_segments(command: &str) -> Vec<(usize, usize)> {
    let mut segments = Vec::new();
    let mut start = 0usize;
    let mut quote: Option<char> = None;
    for (index, ch) in command.char_indices() {
        match quote {
            Some(open) if ch == open => quote = None,
            Some(_) => {}
            None if ch == '"' || ch == '\'' => quote = Some(ch),
            None if ch == '&' || ch == ';' || ch == '|' => {
                segments.push((start, index));
                start = index + ch.len_utf8();
            }
            None => {}
        }
    }
    segments.push((start, command.len()));
    segments
}

/// Normalize a path for comparison so that the same file referred to with
/// different separators or a trailing slash still compares equal.
///
/// Crucially this preserves the *whole* path. An earlier version kept only the
/// last three components, which made `packages/frontend/src/components/Button.tsx`
/// and `packages/admin/src/components/Button.tsx` identical and produced an edge
/// between unrelated files. Two spellings of one file are reconciled later, by
/// suffix matching in the lifter, which is a comparison rather than a
/// destructive rewrite and so cannot merge distinct fully-qualified paths.
fn normalize_path(path: &str) -> String {
    let unified = path.replace('\\', "/");
    let trimmed = unified.trim().trim_end_matches('/');
    // Collapse empty and `.` components so `./a//b` and `a/b` agree.
    let parts: Vec<&str> = trimmed
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();
    parts.join("/")
}

fn summarize(tool: &str, input: &Value) -> String {
    if let Some(command) = input.get("command").and_then(Value::as_str) {
        // The resolved command is what the node is really about, and it is also
        // what activity classification keys on downstream: `bash -lc "npm test"`
        // must classify as a verification, not as an opaque shell invocation.
        return truncate(&format!("{tool}: {}", resolve_command(command)));
    }
    for key in ["file_path", "path", "url", "query", "pattern"] {
        if let Some(value) = input.get(key).and_then(Value::as_str) {
            return truncate(&format!("{tool}: {value}"));
        }
    }
    tool.to_string()
}

fn truncate(text: &str) -> String {
    let cleaned = text.replace(['\n', '\r'], " ");
    if cleaned.chars().count() <= MAX_LABEL {
        return cleaned;
    }
    let kept: String = cleaned.chars().take(MAX_LABEL - 1).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Resource labels are namespaced, so expectations name the namespace too.
    fn file(path: &str) -> String {
        format!("{FILE_NS}{path}")
    }

    fn cmd(key: &str) -> String {
        format!("{CMD_NS}{key}")
    }

    fn tool_use(id: &str, name: &str, input: Value) -> Value {
        json!({"type": "tool_use", "id": id, "name": name, "input": input})
    }

    fn assistant(blocks: Vec<Value>) -> Value {
        json!({"role": "assistant", "content": blocks})
    }

    fn tool_result(id: &str, text: &str, is_error: bool) -> Value {
        json!({
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": id,
                "content": text,
                "is_error": is_error,
            }]
        })
    }

    #[test]
    fn missing_or_empty_messages_yield_no_trace() {
        assert!(trace_from_session(&json!({})).is_empty());
        assert!(trace_from_session(&json!({"messages": []})).is_empty());
    }

    #[test]
    fn only_tool_calls_become_events() {
        let session = json!({"messages": [
            {"role": "user", "content": [{"type": "text", "text": "do a thing"}]},
            assistant(vec![
                json!({"type": "text", "text": "thinking"}),
                tool_use("a", "Read", json!({"file_path": "src/lib.rs"})),
            ]),
        ]});
        let trace = trace_from_session(&session);
        assert_eq!(trace.len(), 1);
        assert_eq!(trace[0].tool, "Read");
        assert_eq!(trace[0].reads, vec![file("src/lib.rs")]);
    }

    #[test]
    fn user_turns_advance_the_turn_counter_but_tool_results_do_not() {
        let session = json!({"messages": [
            {"role": "user", "content": [{"type": "text", "text": "first"}]},
            assistant(vec![tool_use("a", "Read", json!({"file_path": "a.rs"}))]),
            tool_result("a", "ok", false),
            assistant(vec![tool_use("b", "Read", json!({"file_path": "b.rs"}))]),
            {"role": "user", "content": [{"type": "text", "text": "second"}]},
            assistant(vec![tool_use("c", "Read", json!({"file_path": "c.rs"}))]),
        ]});
        let trace = trace_from_session(&session);
        assert_eq!(
            trace.iter().map(|e| e.turn).collect::<Vec<_>>(),
            vec![0, 0, 1]
        );
    }

    #[test]
    fn mutating_tools_write_and_reading_tools_read() {
        let session = json!({"messages": [assistant(vec![
            tool_use("a", "Write", json!({"file_path": "a.rs", "content": "x"})),
            tool_use("b", "Read", json!({"file_path": "a.rs"})),
        ])]});
        let trace = trace_from_session(&session);
        assert_eq!(trace[0].writes, vec![file("a.rs")]);
        assert!(trace[0].reads.is_empty());
        assert_eq!(trace[1].reads, vec![file("a.rs")]);
        assert!(trace[1].writes.is_empty());
    }

    #[test]
    fn patch_targets_are_recovered_from_the_patch_body() {
        let patch =
            "*** Begin Patch\n*** Update File: crates/a/src/lib.rs\n@@\n-x\n+y\n*** End Patch";
        let session = json!({"messages": [assistant(vec![tool_use(
            "a",
            "apply_patch",
            json!({"patch_text": patch}),
        )])]});
        let trace = trace_from_session(&session);
        assert_eq!(trace[0].writes, vec![file("crates/a/src/lib.rs")]);
    }

    #[test]
    fn separators_and_redundant_components_normalize_but_depth_is_preserved() {
        // Separator style and `./` noise are spelling; the identity is the same
        // file. Depth is *not* spelling: an absolute and a relative path are
        // kept distinct here and reconciled later by the lifter's suffix
        // matching, which can refuse when the reference is ambiguous. Folding
        // them together at this layer would silently merge same-named files in
        // different trees.
        let session = json!({"messages": [assistant(vec![
            tool_use("a", "Write", json!({"file_path": "crates\\p\\src\\lib.rs"})),
            tool_use("b", "Read", json!({"file_path": "./crates/p//src/lib.rs"})),
            tool_use("c", "Read", json!({"file_path": "C:/dev/jcode/crates/p/src/lib.rs"})),
        ])]});
        let trace = trace_from_session(&session);
        assert_eq!(
            trace[0].writes, trace[1].reads,
            "separator and ./ differences are spelling only"
        );
        assert_ne!(
            trace[0].writes, trace[2].reads,
            "a fully qualified path keeps its qualification"
        );
    }

    #[test]
    fn commands_reduce_to_a_stable_key_so_repeat_runs_link_up() {
        assert_eq!(command_key("cargo test -p jcode-plan --lib"), "cargo test");
        assert_eq!(command_key("cargo   build --release"), "cargo build");
        assert_eq!(command_key("ls"), "ls");
    }

    #[test]
    fn directory_changes_do_not_hide_the_real_command() {
        // Keying on `cd _build` would make every command in that directory look
        // like the same resource, inventing dependencies between unrelated runs.
        assert_eq!(command_key("cd _build && npm test"), "npm test");
        assert_eq!(command_key("cd a; cargo check -p x"), "cargo check");
        assert_eq!(command_key("cd _build"), "cd _build");
    }

    #[test]
    fn chained_commands_classify_by_the_work_they_actually_do() {
        let session = json!({"messages": [assistant(vec![tool_use(
            "a",
            "Bash",
            json!({"command": "cd _build && npm test"}),
        )])]});
        let trace = trace_from_session(&session);
        assert_eq!(
            crate::lift::classify(&trace[0].tool, &trace[0].summary),
            crate::lift::Activity::Verify
        );
    }

    #[test]
    fn failures_are_detected_from_both_the_flag_and_the_error_prefix() {
        let session = json!({"messages": [
            assistant(vec![
                tool_use("a", "Read", json!({"file_path": "a.rs"})),
                tool_use("b", "Read", json!({"file_path": "b.rs"})),
                tool_use("c", "Read", json!({"file_path": "c.rs"})),
            ]),
            tool_result("a", "fine", false),
            tool_result("b", "boom", true),
            tool_result("c", "Error: File not found", false),
        ]});
        let trace = trace_from_session(&session);
        assert_eq!(
            trace.iter().map(|e| e.failed).collect::<Vec<_>>(),
            vec![false, true, true]
        );
    }

    #[test]
    fn running_a_script_reads_it_so_write_then_run_links_up() {
        // The gap this closes: a session that writes a helper and immediately
        // runs it used to lift as two unconnected nodes, overstating how much
        // of the work could have run in parallel.
        let session = json!({"messages": [assistant(vec![
            tool_use("a", "Write", json!({"file_path": "C:\\tmp\\work\\recompute.js"})),
            tool_use("b", "Bash", json!({"command": "node C:\\tmp\\work\\recompute.js"})),
        ])]});
        let trace = trace_from_session(&session);
        assert_eq!(trace[0].writes, vec![file("C:/tmp/work/recompute.js")]);
        assert!(trace[1].reads.contains(&file("C:/tmp/work/recompute.js")));
    }

    #[test]
    fn only_file_taking_programs_contribute_path_operands() {
        // `git commit -m "fix a.rs bug"` merely mentions a file; treating the
        // mention as a dependency would fabricate structure.
        let (reads, writes) = command_resources("git commit -m \"fix a.rs bug\"");
        assert_eq!(reads, vec![cmd("git commit")]);
        assert!(writes.is_empty());

        let (reads, _) = command_resources("python scripts/build.py");
        assert!(reads.contains(&file("scripts/build.py")));
    }

    #[test]
    fn inline_interpreter_code_is_not_mistaken_for_a_path() {
        // Only the command identity survives; the code body contributes nothing.
        let (reads, writes) = command_resources("node -e \"console.log(1)\"");
        assert_eq!(reads.len(), 1, "no path recovered from inline code");
        assert!(writes.is_empty());

        let (reads, _) = command_resources("python -c \"import x; x.run('a.py')\"");
        assert_eq!(reads.len(), 1);
    }

    #[test]
    fn redirection_targets_are_writes_for_any_command() {
        let (_, writes) = command_resources("cargo test -p jcode-plan > target/out.txt 2>&1");
        assert_eq!(writes, vec![file("target/out.txt")]);

        let (_, appended) = command_resources("echo hi >> notes/log.txt");
        assert_eq!(appended, vec![file("notes/log.txt")]);

        let (_, spaced) = command_resources("cargo build > target/build.log");
        assert_eq!(spaced, vec![file("target/build.log")]);
    }

    #[test]
    fn capture_then_read_links_up_across_the_redirect() {
        let session = json!({"messages": [assistant(vec![
            tool_use("a", "Bash", json!({"command": "cargo test > target/out.txt 2>&1"})),
            tool_use("b", "Read", json!({"file_path": "target/out.txt"})),
        ])]});
        let trace = trace_from_session(&session);
        assert_eq!(trace[0].writes, vec![file("target/out.txt")]);
        assert_eq!(trace[1].reads, vec![file("target/out.txt")]);
    }

    #[test]
    fn shell_wrappers_are_unwrapped_to_the_command_they_run() {
        // The dominant Windows shape in real transcripts. Keying on the wrapper
        // would make every command in the session look like the same resource.
        assert_eq!(
            command_key("\"C:\\Program Files\\Git\\bin\\bash.exe\" -lc \"bash s/plan.sh\""),
            "bash s/plan.sh"
        );
        assert_eq!(command_key("bash -c \"cd repo && npm test\""), "npm test");
        assert_eq!(command_key("cmd /c \"cargo build\""), "cargo build");
    }

    #[test]
    fn wrapped_scripts_are_recovered_as_reads() {
        let session = json!({"messages": [assistant(vec![
            tool_use("a", "Write", json!({"file_path": "work/s/plan.sh"})),
            tool_use("b", "Bash", json!({
                "command": "\"C:\\Program Files\\Git\\bin\\bash.exe\" -lc \"bash work/s/plan.sh\""
            })),
        ])]});
        let trace = trace_from_session(&session);
        assert_eq!(trace[0].writes, vec![file("work/s/plan.sh")]);
        assert!(trace[1].reads.contains(&file("work/s/plan.sh")));
    }

    #[test]
    fn wrapped_verification_still_classifies_as_verification() {
        let session = json!({"messages": [assistant(vec![tool_use(
            "a",
            "Bash",
            json!({"command": "bash -lc \"cd _build && npm test\""}),
        )])]});
        let trace = trace_from_session(&session);
        assert_eq!(
            crate::lift::classify(&trace[0].tool, &trace[0].summary),
            crate::lift::Activity::Verify
        );
    }

    #[test]
    fn interpreter_eval_flags_are_not_treated_as_shell_wrappers() {
        // `node -e "..."` is source, not a command. Unwrapping it would let code
        // text masquerade as the work that ran.
        assert_eq!(command_key("node -e \"require('./a.js')\""), "node");
        assert_eq!(command_key("python -c \"import os\""), "python");
    }

    #[test]
    fn unwrapping_is_bounded_and_terminates_on_pathological_input() {
        let nested = "bash -lc \"bash -lc 'bash -lc \\\"bash -lc x\\\"'\"";
        // The only requirement is that this returns; the exact key is incidental.
        assert!(!command_key(nested).is_empty());
    }

    #[test]
    fn environment_prefixes_do_not_masquerade_as_the_program() {
        // `SANDBOX=... node b2.js` runs node. Keying on the assignment would
        // make every command sharing that variable look like one resource.
        assert_eq!(command_key("SANDBOX='/tmp/x' node b2.js"), "node b2.js");
        let (reads, _) = command_resources("cd s && SANDBOX='/tmp/x' node b2.js");
        assert!(reads.contains(&file("b2.js")));
        assert!(reads.contains(&cmd("node b2.js")));
    }

    #[test]
    fn a_windows_path_is_not_mistaken_for_an_assignment() {
        assert!(!is_env_assignment("C:\\tmp\\x"));
        assert!(is_env_assignment("SANDBOX=C:\\tmp\\x"));
        assert!(!is_env_assignment("2SANDBOX=x"));
    }

    #[test]
    fn summaries_truncate_but_identities_are_dropped_rather_than_shortened() {
        // Summaries are prose and may be shortened. Identities may not: a
        // truncated identity silently merges every resource sharing a prefix.
        let long = "x".repeat(500);
        let session = json!({"messages": [assistant(vec![tool_use(
            "a",
            "Bash",
            json!({"command": long.clone()}),
        )])]});
        let trace = trace_from_session(&session);
        assert!(trace[0].summary.chars().count() <= MAX_LABEL);
        for resource in trace[0].reads.iter().chain(trace[0].writes.iter()) {
            assert!(
                !resource.ends_with('…'),
                "identities are never elided: {resource}"
            );
        }

        // An identity past the hard cap is dropped entirely.
        let huge = format!("cat {}.txt", "y".repeat(MAX_RESOURCE_ID + 50));
        let (reads, _) = command_resources(&huge);
        assert!(
            reads
                .iter()
                .all(|r| r.chars().count() <= MAX_RESOURCE_ID + CMD_NS.len()),
            "oversized identities are dropped, not truncated: {reads:?}"
        );
    }

    #[test]
    fn unknown_tools_produce_isolated_nodes_rather_than_invented_edges() {
        let session = json!({"messages": [assistant(vec![tool_use(
            "a",
            "SomeFutureTool",
            json!({"mystery": "value"}),
        )])]});
        let trace = trace_from_session(&session);
        assert_eq!(trace.len(), 1);
        assert!(trace[0].reads.is_empty() && trace[0].writes.is_empty());
    }
}
