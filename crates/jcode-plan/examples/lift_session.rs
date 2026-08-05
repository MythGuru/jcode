//! Lift a real jcode session transcript into an explicit task graph.
//!
//! This is the demonstration surface for [`jcode_plan::lift`]: point it at a
//! stored session and it prints the graph that session implicitly executed,
//! including how much of the work could have run in parallel.
//!
//! ```text
//! cargo run -p jcode-plan --example lift_session -- <session.json>
//! cargo run -p jcode-plan --example lift_session          # newest session
//! ```
//!
//! Add `--mermaid` to emit a diagram, or `--edges` to audit the evidence behind
//! every dependency the lifter drew.

use jcode_plan::bridge::apply_task_graph;
use jcode_plan::lift::{LiftReport, lift, session::trace_from_session};
use jcode_plan::{VersionedPlan, mermaid::swarm_plan_mermaid};
use std::path::{Path, PathBuf};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let want_mermaid = args.iter().any(|a| a == "--mermaid");
    let want_edges = args.iter().any(|a| a == "--edges");
    let explicit = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .map(PathBuf::from);

    let path = match explicit.or_else(newest_session) {
        Some(path) => path,
        None => {
            eprintln!("no session file given and none found in ~/.jcode/sessions");
            std::process::exit(2);
        }
    };

    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) => {
            eprintln!("cannot read {}: {err}", path.display());
            std::process::exit(2);
        }
    };
    let document: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(value) => value,
        Err(err) => {
            eprintln!("{} is not valid session JSON: {err}", path.display());
            std::process::exit(2);
        }
    };

    let trace = trace_from_session(&document);
    let report = lift(&trace);
    print_summary(&path, &report);

    if want_edges {
        print_edges(&report);
    }
    if want_mermaid {
        // Round-tripping through the plan storage proves the lifted graph is a
        // real artifact, not a private structure: it renders with the same code
        // that renders authored swarm plans.
        let mut plan = VersionedPlan::new();
        apply_task_graph(&mut plan, &report.graph);
        match swarm_plan_mermaid(&plan.items) {
            Some(diagram) => println!("\n{diagram}"),
            None => println!("\n(no diagram: nothing was lifted)"),
        }
    }
}

fn print_summary(path: &Path, report: &LiftReport) {
    let title = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    println!("session:        {title}");
    println!("tool calls:     {}", report.events_considered);
    println!("lifted nodes:   {}", report.graph.len());
    println!("edges (kept):   {}", total_deps(report));
    println!("edges (found):  {}", report.edges.len());
    println!("critical path:  {}", report.critical_path());
    println!("parallel width: {}", report.parallel_width());
    if report.critical_path() > 0 {
        // The headline: an emergent run executes strictly sequentially, so any
        // width above 1 is concurrency the session left unused.
        println!(
            "sequential rounds needed: {} of {} nodes",
            report.critical_path(),
            report.graph.len()
        );
    }
    println!("\nnodes:");
    for node in report.graph.nodes() {
        let deps = if node.depends_on.is_empty() {
            "-".to_string()
        } else {
            node.depends_on.join(", ")
        };
        println!(
            "  {:<14} {:<10} {:<7} <- {deps}\n      {}",
            node.id,
            format!("{:?}", node.kind),
            format!("{:?}", node.status),
            node.content
        );
    }
}

fn print_edges(report: &LiftReport) {
    println!("\nedge evidence:");
    for edge in &report.edges {
        let resource = edge.resource.as_deref().unwrap_or("-");
        println!(
            "  {} -> {}  [{}] {resource}",
            edge.from,
            edge.to,
            edge.reason.as_str()
        );
    }
}

fn total_deps(report: &LiftReport) -> usize {
    report
        .graph
        .nodes()
        .iter()
        .map(|node| node.depends_on.len())
        .sum()
}

/// Newest `session_*.json` under `~/.jcode/sessions`, so the example is useful
/// with no arguments at all.
fn newest_session() -> Option<PathBuf> {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()?;
    let dir = Path::new(&home).join(".jcode").join("sessions");
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else {
            continue;
        };
        if best.as_ref().is_none_or(|(best, _)| modified > *best) {
            best = Some((modified, path));
        }
    }
    best.map(|(_, path)| path)
}
