//! Trace-to-graph lifting: reconstruct an explicit [`TaskGraph`] from the
//! emergent execution trace of an ordinary agent session.
//!
//! # Why this exists
//!
//! Macedo (arXiv:2607.27578, *What makes prompts a graph*) defines prompt graph
//! engineering by four conditions: explicit structure (G1), separation of
//! structure from prompt content (G2), executable semantics (G3), and the graph
//! as a first-class artifact (G4). A coding harness driving a single agent fails
//! G1 and G4: which step runs next is decided by the model turn by turn, and what
//! survives the run is a transcript, not an object. The paper names the resulting
//! gap directly: no system today closes the loop from trace to versioned,
//! optimizable graph.
//!
//! This module closes it. It takes what actually happened, an ordered list of
//! [`TraceEvent`]s, and recovers the graph that was implicitly executed:
//! segments of related work become nodes, and real dataflow between them becomes
//! edges. The result is an ordinary [`TaskGraph`], so everything the swarm engine
//! already does with authored graphs (persist, diff, render, simulate, re-run)
//! applies unchanged to a lifted one.
//!
//! # What the lift can and cannot know
//!
//! Lifting is *recovery*, not inference of intent. Two honesty rules follow:
//!
//! 1. **Edges are evidence, not guesses.** An edge is emitted only when a later
//!    segment read a resource an earlier segment wrote, when two segments wrote
//!    the same resource (write-after-write ordering), when a later segment
//!    changed a resource an earlier one inspected (the read-then-edit pattern),
//!    or when a verification ran after a change it could plausibly be verifying.
//!    Mere adjacency in time is never an edge; sequential traces are not
//!    sequential graphs, and pretending otherwise would produce a chain that
//!    forbids the parallelism the real work allowed.
//! 2. **Status reflects the run, not a wish.** Segments that ended in a tool
//!    failure are lifted as [`NodeStatus::Failed`], so a lifted graph of a messy
//!    session looks messy. A lifted graph is a record first and a template
//!    second.
//!
//! Because every edge points from a lower sequence position to a higher one, the
//! output is acyclic by construction.

use crate::dag::{
    HandoffArtifact, Mode, NodeId, NodeKind, NodeOrigin, NodeStatus, TaskGraph, TaskNode,
};
use std::collections::{BTreeSet, HashMap};

pub mod session;

#[cfg(test)]
mod tests;

/// How much work one node may absorb before the lifter starts a new segment.
///
/// Without a cap, a session that reads two hundred files in one stretch lifts to
/// a single opaque node, which reproduces exactly the opacity the graph exists to
/// cure. The cap trades a little fidelity for structure that a human can read.
pub const MAX_EVENTS_PER_NODE: usize = 12;

/// One observed tool execution, normalized away from any particular transcript
/// format.
///
/// `reads` and `writes` are *resource labels*: file paths, command names, URLs.
/// They are the only channel through which the lifter can see dataflow, so an
/// adapter that leaves them empty will produce nodes with no edges, which is the
/// correct answer for a session whose steps genuinely did not feed one another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceEvent {
    /// Position in the trace. Must be non-decreasing across the input.
    pub seq: usize,
    /// The user turn this event belongs to. Segments never span turns, because a
    /// new instruction from the user is a real boundary in intent.
    pub turn: usize,
    /// Tool name as invoked, for example `Read` or `Bash`.
    pub tool: String,
    /// Short human-readable label, for example `Read src/lib.rs`.
    pub summary: String,
    /// Resources this event consumed.
    pub reads: Vec<String>,
    /// Resources this event produced or mutated.
    pub writes: Vec<String>,
    /// Whether the tool reported failure.
    pub failed: bool,
}

impl TraceEvent {
    /// A minimal event, for adapters and tests.
    pub fn new(
        seq: usize,
        turn: usize,
        tool: impl Into<String>,
        summary: impl Into<String>,
    ) -> Self {
        Self {
            seq,
            turn,
            tool: tool.into(),
            summary: summary.into(),
            reads: Vec::new(),
            writes: Vec::new(),
            failed: false,
        }
    }

    pub fn reads(mut self, items: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.reads = items.into_iter().map(Into::into).collect();
        self
    }

    pub fn writes(mut self, items: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.writes = items.into_iter().map(Into::into).collect();
        self
    }

    pub fn failed(mut self, failed: bool) -> Self {
        self.failed = failed;
        self
    }
}

/// The coarse activity class of an event. This is the segmentation key: a run of
/// consecutive same-class events in one turn becomes one node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    /// Gathering information: reads, searches, fetches.
    Explore,
    /// Changing something: writes, edits, patches.
    Implement,
    /// Checking that something works: builds, tests, lints.
    Verify,
}

impl Activity {
    fn to_kind(self) -> NodeKind {
        match self {
            Activity::Explore => NodeKind::Explore,
            Activity::Implement => NodeKind::Implement,
            Activity::Verify => NodeKind::Verify,
        }
    }
}

/// A contiguous run of events lifted into one node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub id: NodeId,
    pub activity: Activity,
    pub turn: usize,
    pub events: Vec<TraceEvent>,
}

impl Segment {
    fn reads(&self) -> BTreeSet<&str> {
        self.events
            .iter()
            .flat_map(|e| e.reads.iter())
            .map(String::as_str)
            .collect()
    }

    fn writes(&self) -> BTreeSet<&str> {
        self.events
            .iter()
            .flat_map(|e| e.writes.iter())
            .map(String::as_str)
            .collect()
    }

    fn failed(&self) -> bool {
        self.events.iter().any(|e| e.failed)
    }
}

/// Why the lifter drew a particular edge. Kept alongside the graph so a reviewer
/// can audit the reconstruction instead of trusting it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeReason {
    /// The downstream segment read a resource the upstream segment wrote.
    ReadAfterWrite,
    /// Both segments wrote the same resource; order must be preserved.
    WriteAfterWrite,
    /// The downstream segment changed a resource the upstream segment read.
    /// This is the read-then-edit pattern: the inspection informed the change.
    WriteAfterRead,
    /// A verification followed a change it plausibly verifies.
    VerifiesChange,
}

impl EdgeReason {
    pub fn as_str(self) -> &'static str {
        match self {
            EdgeReason::ReadAfterWrite => "read-after-write",
            EdgeReason::WriteAfterWrite => "write-after-write",
            EdgeReason::WriteAfterRead => "write-after-read",
            EdgeReason::VerifiesChange => "verifies-change",
        }
    }
}

/// One justified dependency edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiftedEdge {
    pub from: NodeId,
    pub to: NodeId,
    pub reason: EdgeReason,
    /// The resource that justified the edge, when there was one.
    pub resource: Option<String>,
}

/// The result of a lift: the graph plus the evidence behind it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiftReport {
    pub graph: TaskGraph,
    pub segments: Vec<Segment>,
    /// Every justified edge found, including ones removed by transitive
    /// reduction. The graph keeps the reduced set; this keeps the full evidence.
    pub edges: Vec<LiftedEdge>,
    pub events_considered: usize,
}

impl LiftReport {
    /// Widest set of nodes that were mutually independent, i.e. how much of this
    /// session could have run in parallel. This is the headline number a lifted
    /// graph exists to produce: a chain has width 1, and anything above that is
    /// concurrency the emergent run left on the table.
    pub fn parallel_width(&self) -> usize {
        let depths = self.depths();
        let mut by_depth: HashMap<usize, usize> = HashMap::new();
        for depth in depths.values() {
            *by_depth.entry(*depth).or_insert(0) += 1;
        }
        by_depth.values().copied().max().unwrap_or(0)
    }

    /// Longest dependency chain, i.e. the minimum number of sequential rounds
    /// this work needed.
    pub fn critical_path(&self) -> usize {
        self.depths().values().copied().max().map_or(0, |d| d + 1)
    }

    fn depths(&self) -> HashMap<&str, usize> {
        let mut depths: HashMap<&str, usize> = HashMap::new();
        // Nodes are in trace order and edges always point forward, so one pass
        // in order is enough to settle every depth.
        for node in self.graph.nodes() {
            let depth = node
                .depends_on
                .iter()
                .filter_map(|dep| depths.get(dep.as_str()).copied())
                .max()
                .map_or(0, |d| d + 1);
            depths.insert(node.id.as_str(), depth);
        }
        depths
    }
}

/// Classify a tool invocation into an activity class.
///
/// The mapping is deliberately conservative: anything unrecognized counts as
/// exploration, because treating an unknown tool as a change or a verification
/// would fabricate structure the trace does not support.
pub fn classify(tool: &str, summary: &str) -> Activity {
    let tool_lower = tool.to_ascii_lowercase();
    match tool_lower.as_str() {
        "write" | "edit" | "multiedit" | "patch" | "apply_patch" | "notebookedit" => {
            return Activity::Implement;
        }
        "read" | "agentgrep" | "grep" | "glob" | "ls" | "webfetch" | "websearch" | "outline" => {
            return Activity::Explore;
        }
        _ => {}
    }
    // Shell-style tools are split by what the command actually does.
    if matches!(
        tool_lower.as_str(),
        "bash" | "shell" | "powershell" | "selfdev"
    ) {
        if is_verification_command(summary) {
            return Activity::Verify;
        }
        if is_mutating_command(summary) {
            return Activity::Implement;
        }
    }
    Activity::Explore
}

fn is_verification_command(command: &str) -> bool {
    const MARKERS: [&str; 10] = [
        " test",
        "cargo test",
        "cargo check",
        "cargo build",
        "clippy",
        "pytest",
        "npm test",
        "npm run build",
        "make test",
        "check_guardrails",
    ];
    let lower = command.to_ascii_lowercase();
    MARKERS.iter().any(|marker| lower.contains(marker))
}

fn is_mutating_command(command: &str) -> bool {
    const MARKERS: [&str; 8] = [
        "git commit",
        "git apply",
        "git checkout",
        "git merge",
        "mv ",
        "rm ",
        "mkdir",
        "npm install",
    ];
    let lower = command.to_ascii_lowercase();
    MARKERS.iter().any(|marker| lower.contains(marker))
}

/// Lift an ordered trace into an explicit graph.
///
/// Events must be supplied in execution order. The lift is deterministic: the
/// same trace always produces the same graph, which is what makes a lifted graph
/// diffable across runs.
pub fn lift(events: &[TraceEvent]) -> LiftReport {
    let segments = segment(events);
    let edges = infer_edges(&segments);
    let reduced = transitive_reduction(&segments, &edges);
    let graph = build_graph(&segments, &reduced);
    LiftReport {
        graph,
        segments,
        edges,
        events_considered: events.len(),
    }
}

/// Group consecutive events into segments. A segment breaks on a turn change, an
/// activity-class change, or the size cap.
fn segment(events: &[TraceEvent]) -> Vec<Segment> {
    let mut segments: Vec<Segment> = Vec::new();
    let mut counters: HashMap<&'static str, usize> = HashMap::new();
    for event in events {
        let activity = classify(&event.tool, &event.summary);
        let extend = segments.last().is_some_and(|last| {
            last.activity == activity
                && last.turn == event.turn
                && last.events.len() < MAX_EVENTS_PER_NODE
        });
        if extend {
            if let Some(last) = segments.last_mut() {
                last.events.push(event.clone());
            }
            continue;
        }
        let prefix = match activity {
            Activity::Explore => "explore",
            Activity::Implement => "implement",
            Activity::Verify => "verify",
        };
        let counter = counters.entry(prefix).or_insert(0);
        *counter += 1;
        segments.push(Segment {
            id: format!("{prefix}.{counter}"),
            activity,
            turn: event.turn,
            events: vec![event.clone()],
        });
    }
    segments
}

/// Find a resource shared by two sets, tolerating differences in how deeply a
/// path was spelled.
///
/// A command run from a working directory names `showlines.js` while the write
/// that created it named `tmp/work/showlines.js`. These are the same file, and
/// requiring exact equality would drop the edge and overstate parallelism. So a
/// match is also accepted when one label is a component-aligned suffix of the
/// other.
///
/// The residual risk is a bare filename colliding with a same-named file in
/// another directory. That is accepted deliberately: the alternative is losing
/// the most common dataflow signal in shell-driven sessions, and adjacency in
/// the trace already makes the co-reference the likelier reading.
fn shared_resource<'a>(left: &BTreeSet<&'a str>, right: &BTreeSet<&'a str>) -> Option<&'a str> {
    // Exact matches first, so the reported resource is the most specific one
    // available and the result does not depend on iteration incidentals.
    if let Some(exact) = left.intersection(right).next() {
        return Some(exact);
    }
    for candidate in left {
        for other in right {
            if is_path_suffix(candidate, other) || is_path_suffix(other, candidate) {
                // Report the more qualified label; it is the more informative one.
                return Some(if candidate.len() >= other.len() {
                    candidate
                } else {
                    other
                });
            }
        }
    }
    None
}

/// Whether `short` is a component-aligned suffix of `long`, e.g. `src/lib.rs` of
/// `crates/p/src/lib.rs`. Character-level suffix matching would wrongly link
/// `lib.rs` to `mylib.rs`.
fn is_path_suffix(short: &str, long: &str) -> bool {
    if short.is_empty() || short.len() >= long.len() {
        return false;
    }
    long.strip_suffix(short)
        .is_some_and(|prefix| prefix.ends_with('/'))
}

/// Find every justified dependency between segments.
fn infer_edges(segments: &[Segment]) -> Vec<LiftedEdge> {
    let mut edges = Vec::new();
    for (index, later) in segments.iter().enumerate() {
        let later_reads = later.reads();
        let later_writes = later.writes();
        for earlier in segments[..index].iter() {
            let earlier_writes = earlier.writes();
            // Read-after-write is the primary dataflow signal: the later work
            // consumed something the earlier work produced.
            if let Some(resource) = shared_resource(&earlier_writes, &later_reads) {
                edges.push(LiftedEdge {
                    from: earlier.id.clone(),
                    to: later.id.clone(),
                    reason: EdgeReason::ReadAfterWrite,
                    resource: Some(resource.to_string()),
                });
                continue;
            }
            // Write-after-write is weaker but still binding: two edits to the
            // same file cannot be reordered freely.
            if let Some(resource) = shared_resource(&earlier_writes, &later_writes) {
                edges.push(LiftedEdge {
                    from: earlier.id.clone(),
                    to: later.id.clone(),
                    reason: EdgeReason::WriteAfterWrite,
                    resource: Some(resource.to_string()),
                });
                continue;
            }
            // Read-then-edit: the earlier segment inspected what the later one
            // changed, so the inspection is part of how the change was decided.
            if let Some(resource) = shared_resource(&earlier.reads(), &later_writes) {
                edges.push(LiftedEdge {
                    from: earlier.id.clone(),
                    to: later.id.clone(),
                    reason: EdgeReason::WriteAfterRead,
                    resource: Some(resource.to_string()),
                });
            }
        }
        // A verification with no resource link still depends on the change it
        // was run against. Only the nearest preceding change qualifies: linking
        // a build to every edit ever made would bury the real structure.
        if later.activity == Activity::Verify
            && !edges.iter().any(|edge| edge.to == later.id)
            && let Some(change) = segments[..index]
                .iter()
                .rev()
                .find(|seg| seg.activity == Activity::Implement)
        {
            edges.push(LiftedEdge {
                from: change.id.clone(),
                to: later.id.clone(),
                reason: EdgeReason::VerifiesChange,
                resource: None,
            });
        }
    }
    edges
}

/// Drop edges implied by longer paths, so the graph shows the dependency
/// skeleton rather than its closure. Without this a session that touches one
/// file repeatedly lifts to a dense mesh that hides its own shape.
fn transitive_reduction(segments: &[Segment], edges: &[LiftedEdge]) -> Vec<LiftedEdge> {
    let order: HashMap<&str, usize> = segments
        .iter()
        .enumerate()
        .map(|(index, seg)| (seg.id.as_str(), index))
        .collect();
    // Reachability over the full edge set, computed in topological (trace) order.
    let mut reach: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); segments.len()];
    let mut direct: Vec<Vec<usize>> = vec![Vec::new(); segments.len()];
    for edge in edges {
        let (Some(&from), Some(&to)) = (order.get(edge.from.as_str()), order.get(edge.to.as_str()))
        else {
            continue;
        };
        direct[to].push(from);
    }
    for index in 0..segments.len() {
        let mut acc = BTreeSet::new();
        for &parent in &direct[index] {
            acc.insert(parent);
            let inherited: Vec<usize> = reach[parent].iter().copied().collect();
            acc.extend(inherited);
        }
        reach[index] = acc;
    }
    edges
        .iter()
        .filter(|edge| {
            let (Some(&from), Some(&to)) =
                (order.get(edge.from.as_str()), order.get(edge.to.as_str()))
            else {
                return false;
            };
            // Keep the edge only if no *other* parent of `to` already reaches
            // `from`; otherwise the dependency is implied transitively.
            !direct[to]
                .iter()
                .any(|&other| other != from && reach[other].contains(&from))
        })
        .cloned()
        .collect()
}

fn build_graph(segments: &[Segment], edges: &[LiftedEdge]) -> TaskGraph {
    // Lifted graphs are records of work that already happened, so they carry no
    // gates: inserting critique nodes into history would invent work nobody did.
    let mut graph = TaskGraph::new(Mode::Light);
    let mut failed_verify_seen = false;
    for segment in segments {
        let depends_on: Vec<NodeId> = edges
            .iter()
            .filter(|edge| edge.to == segment.id)
            .map(|edge| edge.from.clone())
            .collect();
        // A change made after a failed verification is a repair, not new work.
        let kind = if segment.activity == Activity::Implement && failed_verify_seen {
            NodeKind::Fix
        } else {
            segment.activity.to_kind()
        };
        if segment.activity == Activity::Verify {
            failed_verify_seen = segment.failed();
        }
        graph.push_node(TaskNode {
            id: segment.id.clone(),
            content: node_content(segment),
            kind,
            status: if segment.failed() {
                NodeStatus::Failed
            } else {
                NodeStatus::Done
            },
            owner: None,
            parent: None,
            depends_on,
            expanded: false,
            is_gate: false,
            planner: None,
            priority: 0,
            output: Some(node_artifact(segment)),
            origin: Some(NodeOrigin::Lift),
        });
    }
    graph
}

/// The node's task text. A lifted node describes the work in the imperative, so
/// the graph reads as something re-runnable rather than as a log entry.
fn node_content(segment: &Segment) -> String {
    let verb = match segment.activity {
        Activity::Explore => "Investigate",
        Activity::Implement => "Change",
        Activity::Verify => "Verify",
    };
    let subject = segment_subject(segment);
    format!("{verb}: {subject}")
}

fn segment_subject(segment: &Segment) -> String {
    let touched: BTreeSet<&str> = segment.writes().union(&segment.reads()).copied().collect();
    if touched.is_empty() {
        return segment
            .events
            .first()
            .map(|e| e.summary.clone())
            .unwrap_or_else(|| "unlabelled work".to_string());
    }
    let names: Vec<&str> = touched.iter().take(3).copied().collect();
    let extra = touched.len().saturating_sub(names.len());
    if extra == 0 {
        names.join(", ")
    } else {
        format!("{} (+{extra} more)", names.join(", "))
    }
}

fn node_artifact(segment: &Segment) -> HandoffArtifact {
    let evidence: Vec<String> = segment
        .writes()
        .union(&segment.reads())
        .map(|r| (*r).to_string())
        .collect();
    HandoffArtifact {
        findings: format!(
            "{} tool call(s) in turn {}: {}",
            segment.events.len(),
            segment.turn,
            segment
                .events
                .iter()
                .map(|e| e.summary.as_str())
                .collect::<Vec<_>>()
                .join("; ")
        ),
        evidence,
        validation: if segment.activity == Activity::Verify {
            Some(if segment.failed() {
                "failed".to_string()
            } else {
                "passed".to_string()
            })
        } else {
            None
        },
        // Lifting recovers what happened; it cannot know what was overlooked.
        // Leaving this empty is the honest answer, and it is why a lifted graph
        // is a starting point for review rather than a verified one.
        confidence: Some("low (reconstructed from trace)".to_string()),
        ..HandoffArtifact::default()
    }
}
