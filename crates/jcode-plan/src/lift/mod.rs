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
//! This module builds the first half of that loop: trace to explicit,
//! versionable graph. It takes what actually happened, an ordered list of
//! [`TraceEvent`]s, and recovers the graph that was implicitly executed:
//! segments of related work become nodes, and real dataflow between them becomes
//! edges. The result is an ordinary [`TaskGraph`], so everything the swarm engine
//! already does with authored graphs (persist, diff, render, simulate, re-run)
//! applies unchanged to a lifted one.
//!
//! The second half is not built, and the distinction matters. Closing the
//! paper's loop also requires replaying a lifted graph and showing the replay
//! reproduces the original outcome, then searching for a better structure and
//! measuring that it is actually faster, cheaper, or no worse. None of that
//! exists here. Until it does, this is a conservative trace lifter, not an
//! optimizer, and it should be described that way.
//!
//! The idea of recovering a process model from an execution log is also not new
//! in general: process mining has done it for business workflows for decades.
//! What is specific here is applying it to coding-agent transcripts, where the
//! evidence is tool calls and the recovered object is a task graph the same
//! runtime can execute.
//!
//! # What the lift can and cannot know
//!
//! Lifting is *recovery*, not inference of intent. Three honesty rules follow:
//!
//! 1. **Edges are evidence, not guesses.** An edge is emitted only when a later
//!    segment read a resource an earlier segment wrote, when two segments wrote
//!    the same resource (write-after-write ordering), when a later segment
//!    changed a resource an earlier one inspected (the read-then-edit pattern),
//!    or when a verification ran in the same turn as the change it follows.
//!    Mere adjacency in time is never an edge; sequential traces are not
//!    sequential graphs, and pretending otherwise would produce a chain that
//!    forbids the parallelism the real work allowed.
//! 2. **Status reflects the run, not a wish.** Segments that ended in a tool
//!    failure are lifted as [`NodeStatus::Failed`], so a lifted graph of a messy
//!    session looks messy. A lifted graph is a record first and a template
//!    second.
//! 3. **Recall is partial, so the metrics are bounded claims.** Dependencies
//!    that never touched a shared, observable resource, for example knowledge
//!    the model carried in its head from one step to the next, leave no trace
//!    and cannot be recovered. Because refusing to guess costs edges, and
//!    missing edges make work look more independent than it was,
//!    [`LiftReport::parallel_width`] is an upper bound on real concurrency, not
//!    a measurement of it. Treat it as a hypothesis to test, never as a licence
//!    to run those nodes in parallel.
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

/// Largest node count for which the exact maximum antichain is computed.
///
/// The exact computation is a bipartite matching over the transitive closure,
/// which is quadratic in memory and worse in time. Beyond this size the report
/// falls back to the layer-width lower bound and says so, because a slow lift is
/// a broken tool and a silently approximated headline number is a dishonest one.
pub const MAX_EXACT_WIDTH_NODES: usize = 512;

/// How much of the session could have run concurrently, and how sure we are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParallelWidth {
    /// The number of mutually independent nodes.
    pub value: usize,
    /// Whether `value` is the true maximum antichain (`true`) or a lower bound
    /// derived from the widest depth layer (`false`).
    pub exact: bool,
}

impl std::fmt::Display for ParallelWidth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.exact {
            write!(f, "{}", self.value)
        } else {
            write!(f, ">={} (approximate)", self.value)
        }
    }
}

impl LiftReport {
    /// Widest set of nodes that were mutually independent *in the recovered
    /// graph*, i.e. an upper bound on how much of this session could have run in
    /// parallel.
    ///
    /// Read the qualifier carefully. This is exact for the graph that was
    /// recovered, and the graph is missing every dependency that left no
    /// observable trace, so the true figure is at most this and usually lower. A
    /// chain has width 1; a large width means "worth investigating", not "safe to
    /// parallelize".
    ///
    /// Within the recovered graph the value is the true maximum antichain,
    /// obtained via Dilworth's theorem: the largest antichain equals the minimum
    /// chain cover, which equals `n - maximum bipartite matching` over the
    /// reachability relation. The cheaper widest-depth-layer count is only a
    /// lower bound, because an antichain may span several depths, so reporting it
    /// as the answer would understate the recovered concurrency. Above
    /// [`MAX_EXACT_WIDTH_NODES`] the bound is returned instead, flagged inexact.
    pub fn parallel_width(&self) -> ParallelWidth {
        let count = self.graph.len();
        if count > MAX_EXACT_WIDTH_NODES {
            return ParallelWidth {
                value: self.layer_width(),
                exact: false,
            };
        }
        ParallelWidth {
            value: self.maximum_antichain(),
            exact: true,
        }
    }

    /// Lower bound on [`Self::parallel_width`]: the most nodes sharing a depth.
    /// Every depth layer is an antichain, so this never exceeds the true width.
    pub fn layer_width(&self) -> usize {
        let depths = self.depths();
        let mut by_depth: HashMap<usize, usize> = HashMap::new();
        for depth in depths.values() {
            *by_depth.entry(*depth).or_insert(0) += 1;
        }
        by_depth.values().copied().max().unwrap_or(0)
    }

    /// Longest dependency chain in the recovered graph, i.e. a *lower* bound on
    /// the number of sequential rounds this work needed. Nodes are unit cost:
    /// this counts rounds, not time.
    ///
    /// The bound points the same way as [`Self::parallel_width`]'s: an edge that
    /// left no observable trace is missing, and every missing edge can only
    /// shorten the chain. So the recovered graph always makes a run look more
    /// parallel than it was, from both directions at once.
    pub fn critical_path(&self) -> usize {
        self.depths().values().copied().max().map_or(0, |d| d + 1)
    }

    /// Maximum antichain via Dilworth's theorem.
    fn maximum_antichain(&self) -> usize {
        let nodes = self.graph.nodes();
        let count = nodes.len();
        if count == 0 {
            return 0;
        }
        let index: HashMap<&str, usize> = nodes
            .iter()
            .enumerate()
            .map(|(position, node)| (node.id.as_str(), position))
            .collect();
        // Ancestors of each node. Dependencies always point backward in trace
        // order, so one in-order pass settles every row: a node's ancestors are
        // its parents plus their already-settled ancestors.
        let mut ancestors: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); count];
        for (position, node) in nodes.iter().enumerate() {
            let mut acc = BTreeSet::new();
            for dep in &node.depends_on {
                let Some(&parent) = index.get(dep.as_str()) else {
                    continue;
                };
                acc.insert(parent);
                let inherited: Vec<usize> = ancestors[parent].iter().copied().collect();
                acc.extend(inherited);
            }
            ancestors[position] = acc;
        }
        // Invert into the comparability relation used by the matching: `reach[a]`
        // is every node strictly after `a` in the partial order.
        let mut reach: Vec<Vec<usize>> = vec![Vec::new(); count];
        for (descendant, row) in ancestors.iter().enumerate() {
            for &ancestor in row {
                reach[ancestor].push(descendant);
            }
        }
        // Minimum chain cover = n - maximum bipartite matching (Kuhn's
        // algorithm); by Dilworth that cover size is the maximum antichain.
        let mut matched_right: Vec<Option<usize>> = vec![None; count];
        let mut matching = 0usize;
        for left in 0..count {
            let mut seen = vec![false; count];
            if augment(left, &reach, &mut matched_right, &mut seen) {
                matching += 1;
            }
        }
        count - matching
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

/// One augmenting-path step of Kuhn's bipartite matching.
fn augment(
    left: usize,
    reach: &[Vec<usize>],
    matched_right: &mut [Option<usize>],
    seen: &mut [bool],
) -> bool {
    for index in 0..reach[left].len() {
        let right = reach[left][index];
        if seen[right] {
            continue;
        }
        seen[right] = true;
        let free = matched_right[right].is_none();
        if free || augment(matched_right[right].unwrap(), reach, matched_right, seen) {
            matched_right[right] = Some(left);
            return true;
        }
    }
    false
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

/// Namespace prefix marking a resource as a filesystem path. Only file
/// resources participate in suffix matching; commands and URLs must match
/// exactly, since a URL ending in `src/lib.rs` is not that file.
pub(crate) const FILE_NS: &str = "file:";

/// Short paths that cannot be resolved to a single qualified path, and so must
/// never match by suffix.
///
/// Ambiguity is a property of the whole trace, not of one comparison. A bare
/// `lib.rs` compared against `a/src/lib.rs` looks unambiguous in isolation and
/// again unambiguous against `b/src/lib.rs`, yet linking it to both fabricates
/// a dependency. Resolution is therefore decided once, over every resource the
/// trace mentions, before any pair is considered.
#[derive(Debug, Default)]
struct Ambiguity {
    unresolvable: BTreeSet<String>,
}

impl Ambiguity {
    fn from_segments(segments: &[Segment]) -> Self {
        let mut all: BTreeSet<&str> = BTreeSet::new();
        for segment in segments {
            all.extend(segment.reads());
            all.extend(segment.writes());
        }
        let files: Vec<&str> = all.into_iter().filter(|r| is_file(r)).collect();
        let mut unresolvable = BTreeSet::new();
        for short in &files {
            let mut candidates = files.iter().filter(|long| is_path_suffix(short, long));
            if candidates.next().is_some() && candidates.next().is_some() {
                unresolvable.insert((*short).to_string());
            }
        }
        Self { unresolvable }
    }

    fn is_resolvable(&self, resource: &str) -> bool {
        !self.unresolvable.contains(resource)
    }
}

/// Find a resource shared by two sets, tolerating differences in how deeply a
/// *file path* was spelled.
///
/// A command run from a working directory names `showlines.js` while the write
/// that created it named `tmp/work/showlines.js`. These are the same file, and
/// requiring exact equality would drop the edge and overstate parallelism. So a
/// match is also accepted when one label is a component-aligned suffix of the
/// other.
///
/// Suffix matching is only sound when the short name resolves to exactly one
/// qualified path across the whole trace; `ambiguity` decides that beforehand.
/// Unresolvable names are skipped, because a missed edge understates a
/// dependency while a wrong one corrupts the artifact.
fn shared_resource<'a>(
    left: &BTreeSet<&'a str>,
    right: &BTreeSet<&'a str>,
    ambiguity: &Ambiguity,
) -> Option<&'a str> {
    // Exact matches first, so the reported resource is the most specific one
    // available and the result does not depend on iteration incidentals.
    if let Some(exact) = left.intersection(right).next() {
        return Some(exact);
    }
    for candidate in left
        .iter()
        .filter(|r| is_file(r) && ambiguity.is_resolvable(r))
    {
        for other in right
            .iter()
            .filter(|r| is_file(r) && ambiguity.is_resolvable(r))
        {
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

fn is_file(resource: &str) -> bool {
    resource.starts_with(FILE_NS)
}

/// Whether `short` is a component-aligned suffix of `long`, e.g. `src/lib.rs` of
/// `crates/p/src/lib.rs`. Character-level suffix matching would wrongly link
/// `lib.rs` to `mylib.rs`.
///
/// Both sides carry the same namespace prefix, which is stripped first so the
/// prefix itself cannot satisfy the component boundary.
fn is_path_suffix(short: &str, long: &str) -> bool {
    let (Some(short), Some(long)) = (short.strip_prefix(FILE_NS), long.strip_prefix(FILE_NS))
    else {
        return false;
    };
    if short.is_empty() || short.len() >= long.len() {
        return false;
    }
    long.strip_suffix(short)
        .is_some_and(|prefix| prefix.ends_with('/'))
}

/// Find every justified dependency between segments.
fn infer_edges(segments: &[Segment]) -> Vec<LiftedEdge> {
    // Which short names can be resolved at all is a whole-trace question, so it
    // is settled once up front rather than per comparison.
    let ambiguity = Ambiguity::from_segments(segments);
    let mut edges = Vec::new();
    for (index, later) in segments.iter().enumerate() {
        let later_reads = later.reads();
        let later_writes = later.writes();
        for earlier in segments[..index].iter() {
            let earlier_writes = earlier.writes();
            // Read-after-write is the primary dataflow signal: the later work
            // consumed something the earlier work produced.
            if let Some(resource) = shared_resource(&earlier_writes, &later_reads, &ambiguity) {
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
            if let Some(resource) = shared_resource(&earlier_writes, &later_writes, &ambiguity) {
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
            if let Some(resource) = shared_resource(&earlier.reads(), &later_writes, &ambiguity) {
                edges.push(LiftedEdge {
                    from: earlier.id.clone(),
                    to: later.id.clone(),
                    reason: EdgeReason::WriteAfterRead,
                    resource: Some(resource.to_string()),
                });
            }
        }
        // A verification in the same turn as a preceding change is attributed to
        // it. This is the one edge not backed by observed dataflow, so it is
        // deliberately narrow: only the nearest preceding change, only within
        // the same turn (a turn is a single user instruction, so the two are
        // part of one intent), and only when no dataflow edge already explains
        // the verification. Across turns the pairing would be mere temporal
        // adjacency, which this module refuses to treat as evidence.
        if later.activity == Activity::Verify
            && !edges.iter().any(|edge| edge.to == later.id)
            && let Some(change) = segments[..index]
                .iter()
                .rev()
                .take_while(|seg| seg.turn == later.turn)
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
            output: Some(node_artifact(segment, edges)),
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
    let names: Vec<String> = touched
        .iter()
        .take(3)
        .map(|r| display_resource(r))
        .collect();
    let extra = touched.len().saturating_sub(names.len());
    if extra == 0 {
        names.join(", ")
    } else {
        format!("{} (+{extra} more)", names.join(", "))
    }
}

/// A resource stripped of its namespace, for text a human reads. Namespaces
/// exist to keep identities apart during matching, not to be shown.
fn display_resource(resource: &str) -> String {
    for prefix in [FILE_NS, "cmd:", "url:"] {
        if let Some(rest) = resource.strip_prefix(prefix) {
            return rest.to_string();
        }
    }
    resource.to_string()
}

/// The node's persisted artifact.
///
/// Evidence carries the incoming edge justifications as well as the resources
/// touched. Without this the reasoning behind a dependency would live only in
/// the in-memory [`LiftReport`], so a persisted lifted graph would assert
/// dependencies a reviewer could not audit, which is precisely the opacity the
/// lift exists to cure.
fn node_artifact(segment: &Segment, edges: &[LiftedEdge]) -> HandoffArtifact {
    let mut evidence: Vec<String> = segment
        .writes()
        .union(&segment.reads())
        .map(|r| (*r).to_string())
        .collect();
    evidence.extend(
        edges
            .iter()
            .filter(|edge| edge.to == segment.id)
            .map(|edge| match &edge.resource {
                Some(resource) => {
                    format!(
                        "depends on {} [{}] via {resource}",
                        edge.from,
                        edge.reason.as_str()
                    )
                }
                None => format!("depends on {} [{}]", edge.from, edge.reason.as_str()),
            }),
    );
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
