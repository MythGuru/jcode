//! The `/lift` command: reconstruct and show the task graph a session executed.
//!
//! A session normally leaves behind a transcript, which is a story rather than a
//! structure. [`jcode_plan::lift`] recovers the structure: chunks of related
//! work become nodes, and observed dataflow between them becomes edges. This
//! command is the user-facing way to look at that.
//!
//! It is deliberately read-only. It loads a stored session, lifts it in-process,
//! prints the result, and changes nothing. No model is called, so it costs
//! nothing but a few milliseconds of local work.

use jcode_plan::lift::{LiftReport, lift, session::trace_from_session};
use std::path::Path;

/// How many nodes to list before summarizing the rest. A long session lifts to
/// hundreds of nodes, and a wall of them in the transcript is unreadable.
const MAX_LISTED_NODES: usize = 40;

/// Parsed form of a `/lift` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LiftCommand {
    /// Lift the active session.
    Current,
    /// Lift a specific stored session, by id or path.
    Session(String),
    /// Explain usage, because the arguments did not parse.
    Usage,
}

/// Parse a trimmed input line into a [`LiftCommand`], or `None` if the line is
/// not a `/lift` invocation at all.
pub(crate) fn parse(trimmed: &str) -> Option<LiftCommand> {
    let rest = if trimmed == "/lift" {
        ""
    } else {
        trimmed.strip_prefix("/lift ")?
    };
    let rest = rest.trim();
    if rest.is_empty() {
        return Some(LiftCommand::Current);
    }
    if rest == "help" || rest == "--help" || rest.starts_with('-') {
        return Some(LiftCommand::Usage);
    }
    Some(LiftCommand::Session(rest.to_string()))
}

pub(crate) fn usage() -> String {
    "Usage: /lift [session-id-or-path]\n\n\
     Reconstructs the task graph this session actually executed: chunks of work \
     become steps, and observed dataflow between them becomes dependencies. \
     Read-only, runs locally, and calls no model."
        .to_string()
}

/// Lift the session stored at `path` and render a human-readable report.
pub(crate) fn lift_session_at(path: &Path) -> Result<String, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|error| format!("Cannot read {}: {error}", path.display()))?;
    let document: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|error| format!("{} is not valid session JSON: {error}", path.display()))?;
    let trace = trace_from_session(&document);
    if trace.is_empty() {
        return Ok(format!(
            "No tool calls found in {}, so there is no graph to recover.",
            display_name(path)
        ));
    }
    Ok(render(&lift(&trace), &display_name(path)))
}

fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string())
}

/// Render a lifted graph.
///
/// Both headline figures are bounds, and the text says so. A dependency that
/// left no observable trace cannot be recovered, and every missing edge both
/// shortens the chain and widens the antichain, so a recovered graph always
/// makes a run look more parallel than it really was.
fn render(report: &LiftReport, title: &str) -> String {
    let nodes = report.graph.nodes();
    let total = nodes.len();
    let linked = nodes
        .iter()
        .filter(|node| !node.depends_on.is_empty())
        .count();
    let width = report.parallel_width();
    let mut out = String::new();
    out.push_str(&format!("Lifted graph for {title}\n\n"));
    out.push_str(&format!(
        "  {} tool calls -> {total} steps, {} dependencies\n",
        report.events_considered,
        total_deps(report)
    ));
    out.push_str(&format!(
        "  at least {} rounds were needed; at most {} steps were independent\n",
        report.critical_path(),
        width.value
    ));
    out.push_str(&format!(
        "  {linked} of {total} steps ({:.0}%) have a recovered dependency\n\n",
        percentage(linked, total)
    ));
    out.push_str(
        "  Both figures are bounds, not measurements: dependencies that left no\n\
         \x20 trace cannot be recovered, and every missing one makes the work look\n\
         \x20 more parallel than it was.\n\n",
    );

    for node in nodes.iter().take(MAX_LISTED_NODES) {
        let deps = if node.depends_on.is_empty() {
            "-".to_string()
        } else {
            node.depends_on.join(", ")
        };
        out.push_str(&format!(
            "  {:<14} {:<10} {:<7} <- {deps}\n      {}\n",
            node.id,
            format!("{:?}", node.kind),
            format!("{:?}", node.status),
            node.content
        ));
    }
    if total > MAX_LISTED_NODES {
        out.push_str(&format!(
            "\n  ... and {} more steps\n",
            total - MAX_LISTED_NODES
        ));
    }
    out
}

fn total_deps(report: &LiftReport) -> usize {
    report
        .graph
        .nodes()
        .iter()
        .map(|node| node.depends_on.len())
        .sum()
}

fn percentage(part: usize, whole: usize) -> f64 {
    if whole == 0 {
        return 0.0;
    }
    100.0 * part as f64 / whole as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_the_bare_command_and_an_explicit_session() {
        assert_eq!(parse("/lift"), Some(LiftCommand::Current));
        assert_eq!(
            parse("/lift session_abc.json"),
            Some(LiftCommand::Session("session_abc.json".to_string()))
        );
        assert_eq!(parse("/lift   "), Some(LiftCommand::Current));
        assert_eq!(parse("/lift help"), Some(LiftCommand::Usage));
    }

    #[test]
    fn ignores_lines_that_are_not_the_command() {
        // A prefix match would swallow a future `/lifted` command and, worse,
        // silently intercept ordinary user text.
        assert_eq!(parse("/lifting off"), None);
        assert_eq!(parse("/li"), None);
        assert_eq!(parse("lift"), None);
        assert_eq!(parse("  /lift"), None, "callers trim before parsing");
    }

    #[test]
    fn a_session_with_no_tool_calls_says_so_rather_than_showing_an_empty_graph() {
        let dir = std::env::temp_dir().join("jcode-lift-empty-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session_empty.json");
        std::fs::write(&path, json!({"messages": []}).to_string()).unwrap();

        let out = lift_session_at(&path).unwrap();
        assert!(out.contains("no graph to recover"), "got {out}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_real_trace_reports_bounds_and_lists_steps() {
        let session = json!({"messages": [
            {"role": "user", "content": [{"type": "text", "text": "do it"}]},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "a", "name": "Write",
                 "input": {"file_path": "src/lib.rs", "content": "x"}},
            ]},
            {"role": "user", "content": [{"type": "text", "text": "now check"}]},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "b", "name": "Read",
                 "input": {"file_path": "src/lib.rs"}},
            ]},
        ]});
        let dir = std::env::temp_dir().join("jcode-lift-real-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session_real.json");
        std::fs::write(&path, session.to_string()).unwrap();

        let out = lift_session_at(&path).unwrap();
        assert!(
            out.contains("2 tool calls -> 2 steps, 1 dependencies"),
            "got {out}"
        );
        // The honesty line must survive refactoring: without it the numbers
        // read as measurements.
        assert!(out.contains("bounds, not measurements"), "got {out}");
        assert!(out.contains("implement.1"), "got {out}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_missing_or_invalid_file_reports_an_error_rather_than_panicking() {
        let missing = std::env::temp_dir().join("jcode-lift-does-not-exist.json");
        assert!(lift_session_at(&missing).is_err());

        let dir = std::env::temp_dir().join("jcode-lift-invalid-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session_invalid.json");
        std::fs::write(&path, "{ not json").unwrap();
        let error = lift_session_at(&path).unwrap_err();
        assert!(error.contains("not valid session JSON"), "got {error}");
        let _ = std::fs::remove_file(&path);
    }
}
