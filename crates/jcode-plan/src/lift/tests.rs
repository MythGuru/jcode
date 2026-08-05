//! Tests for trace-to-graph lifting.
//!
//! These pin the two honesty rules from the module docs: an edge appears only
//! where the trace shows real dataflow, and the reconstruction is deterministic
//! and acyclic so a lifted graph can be diffed across runs.

use super::*;
use crate::dag::NodeStatus;

/// Resource labels are namespaced by the adapter, and the lifter's matching
/// rules depend on that namespace, so the helpers apply it exactly as
/// [`super::session`] does. Tests that used bare paths would exercise a shape
/// no real transcript produces.
fn file(path: &str) -> String {
    format!("{FILE_NS}{path}")
}

fn read(seq: usize, turn: usize, path: &str) -> TraceEvent {
    TraceEvent::new(seq, turn, "Read", format!("Read {path}")).reads([file(path)])
}

fn write(seq: usize, turn: usize, path: &str) -> TraceEvent {
    TraceEvent::new(seq, turn, "Write", format!("Write {path}")).writes([file(path)])
}

fn build(seq: usize, turn: usize, ok: bool) -> TraceEvent {
    TraceEvent::new(seq, turn, "Bash", "cargo test -p jcode-plan").failed(!ok)
}

fn dep_ids(report: &LiftReport, node: &str) -> Vec<String> {
    let mut deps = report
        .graph
        .get(node)
        .unwrap_or_else(|| panic!("missing node {node}"))
        .depends_on
        .clone();
    deps.sort();
    deps
}

#[test]
fn empty_trace_lifts_to_empty_graph() {
    let report = lift(&[]);
    assert!(report.graph.is_empty());
    assert_eq!(report.parallel_width().value, 0);
    assert_eq!(report.critical_path(), 0);
}

#[test]
fn consecutive_same_activity_events_collapse_into_one_node() {
    let events = vec![read(0, 0, "a.rs"), read(1, 0, "b.rs"), read(2, 0, "c.rs")];
    let report = lift(&events);
    assert_eq!(report.graph.len(), 1);
    assert_eq!(report.segments[0].events.len(), 3);
}

#[test]
fn activity_change_and_turn_change_both_split_segments() {
    // Same turn, activity flips read -> write -> read: three nodes.
    let events = vec![read(0, 0, "a.rs"), write(1, 0, "a.rs"), read(2, 0, "b.rs")];
    assert_eq!(lift(&events).graph.len(), 3);

    // Same activity, but a new user turn is a real boundary in intent.
    let events = vec![read(0, 0, "a.rs"), read(1, 1, "b.rs")];
    assert_eq!(lift(&events).graph.len(), 2);
}

#[test]
fn segment_size_is_capped_so_long_runs_stay_readable() {
    let events: Vec<TraceEvent> = (0..MAX_EVENTS_PER_NODE * 2 + 1)
        .map(|i| read(i, 0, &format!("f{i}.rs")))
        .collect();
    let report = lift(&events);
    assert_eq!(report.graph.len(), 3);
    assert!(
        report
            .segments
            .iter()
            .all(|s| s.events.len() <= MAX_EVENTS_PER_NODE)
    );
}

#[test]
fn unrelated_work_gets_no_edges_so_parallelism_is_visible() {
    // Two independent read-then-edit pairs on disjoint files. A naive lifter
    // that used time order would emit a 4-long chain. The real answer is two
    // pairs that could have run side by side: width 2, depth 2.
    let events = vec![
        read(0, 0, "a.rs"),
        write(1, 0, "a.rs"),
        read(2, 1, "b.rs"),
        write(3, 1, "b.rs"),
    ];
    let report = lift(&events);
    assert_eq!(report.graph.len(), 4);
    // Nothing in the second pair depends on anything in the first.
    assert!(dep_ids(&report, "explore.2").is_empty());
    assert_eq!(dep_ids(&report, "implement.2"), vec!["explore.2"]);
    assert_eq!(report.parallel_width().value, 2);
    assert_eq!(report.critical_path(), 2);
}

#[test]
fn read_after_write_creates_an_edge() {
    let events = vec![write(0, 0, "a.rs"), read(1, 1, "a.rs")];
    let report = lift(&events);
    assert_eq!(dep_ids(&report, "explore.1"), vec!["implement.1"]);
    assert_eq!(report.edges[0].reason, EdgeReason::ReadAfterWrite);
    assert_eq!(
        report.edges[0].resource.as_deref(),
        Some(file("a.rs").as_str())
    );
}

#[test]
fn a_path_matches_the_same_file_spelled_at_a_different_depth() {
    // A script is created with a full path and then run from its directory, so
    // the two labels differ in depth. Requiring exact equality here would drop
    // the edge and claim the two steps could have run at the same time.
    let events = vec![
        write(0, 0, "tmp/work/showlines.js"),
        read(1, 1, "showlines.js"),
    ];
    let report = lift(&events);
    assert_eq!(dep_ids(&report, "explore.1"), vec!["implement.1"]);
    assert_eq!(
        report.edges[0].resource.as_deref(),
        Some(file("tmp/work/showlines.js").as_str()),
        "the more qualified label is reported as the evidence"
    );
}

#[test]
fn suffix_matching_respects_component_boundaries() {
    // `lib.rs` must not match `mylib.rs`: a character-level suffix test would
    // fabricate a dependency between unrelated files.
    let events = vec![write(0, 0, "src/mylib.rs"), read(1, 1, "lib.rs")];
    let report = lift(&events);
    assert!(
        report.edges.is_empty(),
        "no edge between distinct files sharing a name ending"
    );
}

#[test]
fn an_ambiguous_bare_name_yields_no_edge_rather_than_a_guess() {
    // Two distinct files share a basename and a later step names only the
    // basename. The trace genuinely does not say which was meant, so resolving
    // it either way would fabricate a dependency. Regression for a case that
    // previously linked the reader to *both* writers.
    let events = vec![
        write(0, 0, "a/src/lib.rs"),
        write(1, 0, "b/src/lib.rs"),
        read(2, 1, "lib.rs"),
    ];
    let report = lift(&events);
    assert!(
        report.edges.is_empty(),
        "ambiguous reference must not resolve to an arbitrary candidate"
    );
}

#[test]
fn same_basename_in_different_trees_is_not_one_resource() {
    // Regression: path identity was truncated to its last three components, so
    // these two distinct files compared equal and produced an edge.
    let events = vec![
        write(0, 0, "packages/frontend/src/components/Button.tsx"),
        read(1, 1, "packages/admin/src/components/Button.tsx"),
    ];
    let report = lift(&events);
    assert!(
        report.edges.is_empty(),
        "deep paths that differ early are different files"
    );
}

#[test]
fn resources_of_different_kinds_never_match() {
    // A fetched URL ending in `src/lib.rs` is not the local file of that name.
    let events = vec![
        write(0, 0, "src/lib.rs"),
        TraceEvent::new(1, 1, "webfetch", "fetch docs")
            .reads(["url:https://example.invalid/reference/src/lib.rs"]),
    ];
    let report = lift(&events);
    assert!(
        report.edges.is_empty(),
        "a URL and a file are not the same resource"
    );
}

#[test]
fn parallel_width_is_the_true_antichain_not_the_widest_layer() {
    // Exhaustive search found this shape: one node fans out to two, plus one
    // isolated node. Depth layers are {a,d} and {b,c}, so the widest layer is 2,
    // but {b,c,d} are mutually independent, so the true width is 3. Reporting
    // the layer count would understate available parallelism.
    let events = vec![
        write(0, 0, "a.rs"),         // implement.1  (a)
        read(1, 1, "a.rs"),          // explore.1    (b) depends on a
        read(2, 2, "a.rs"),          // explore.2    (c) depends on a
        write(3, 3, "unrelated.rs"), // implement.2 (d) independent
    ];
    let report = lift(&events);
    assert_eq!(report.critical_path(), 2);
    let width = report.parallel_width();
    assert!(width.exact, "small graphs get the exact answer");
    assert_eq!(width.value, 3, "true maximum antichain spans depths");
    assert_eq!(
        report.layer_width(),
        2,
        "the layer bound is strictly weaker"
    );
}

#[test]
fn parallel_width_of_a_chain_is_one() {
    let events = vec![
        write(0, 0, "a.rs"),
        write(1, 1, "a.rs"),
        write(2, 2, "a.rs"),
    ];
    let report = lift(&events);
    assert_eq!(
        report.parallel_width().value,
        1,
        "a chain has no parallelism"
    );
}

#[test]
fn verification_is_not_attributed_to_a_change_from_an_earlier_turn() {
    // Editing README in one turn and running an unrelated test suite in the
    // next is temporal adjacency, not evidence. Regression: the fallback used
    // to link any preceding change regardless of turn.
    let events = vec![write(0, 0, "README.md"), build(1, 1, true)];
    let report = lift(&events);
    assert!(
        report.edges.is_empty(),
        "cross-turn verification is adjacency, not dataflow"
    );
}

#[test]
fn verification_is_still_attributed_within_one_turn() {
    // The narrow case the fallback exists for: one instruction, change then
    // check. Both belong to a single intent, so the attribution is warranted.
    let events = vec![write(0, 0, "a.rs"), build(1, 0, true)];
    let report = lift(&events);
    assert_eq!(dep_ids(&report, "verify.1"), vec!["implement.1"]);
    assert_eq!(report.edges[0].reason, EdgeReason::VerifiesChange);
}

#[test]
fn edge_evidence_survives_into_the_persisted_node() {
    // The graph is the artifact that outlives the report, so a dependency it
    // asserts must carry its own justification.
    let events = vec![write(0, 0, "a.rs"), read(1, 1, "a.rs")];
    let report = lift(&events);
    let artifact = report
        .graph
        .get("explore.1")
        .and_then(|node| node.output.clone())
        .expect("lifted node carries an artifact");
    assert!(
        artifact.evidence.iter().any(
            |line| line.contains("depends on implement.1") && line.contains("read-after-write")
        ),
        "edge reasons are persisted, got {:?}",
        artifact.evidence
    );
}

#[test]
fn write_after_write_preserves_order_on_the_same_resource() {
    let events = vec![write(0, 0, "a.rs"), read(1, 0, "b.rs"), write(2, 0, "a.rs")];
    let report = lift(&events);
    assert_eq!(dep_ids(&report, "implement.2"), vec!["implement.1"]);
    assert!(
        report
            .edges
            .iter()
            .any(|e| e.reason == EdgeReason::WriteAfterWrite)
    );
}

#[test]
fn verification_depends_on_the_nearest_change_only() {
    let events = vec![write(0, 0, "a.rs"), write(1, 1, "b.rs"), build(2, 1, true)];
    let report = lift(&events);
    // Not linked to every prior edit, only the nearest one.
    assert_eq!(dep_ids(&report, "verify.1"), vec!["implement.2"]);
    assert_eq!(
        report.edges.last().unwrap().reason,
        EdgeReason::VerifiesChange
    );
}

#[test]
fn verification_with_real_dataflow_prefers_the_dataflow_edge() {
    let events = vec![
        write(0, 0, "a.rs"),
        TraceEvent::new(1, 1, "Bash", "cargo test").reads([file("a.rs")]),
    ];
    let report = lift(&events);
    assert_eq!(report.edges.len(), 1);
    assert_eq!(report.edges[0].reason, EdgeReason::ReadAfterWrite);
}

#[test]
fn transitive_edges_are_reduced_away_but_kept_as_evidence() {
    // a -> b -> c all touch the same file, so the direct a -> c edge is implied.
    let events = vec![
        write(0, 0, "a.rs"),
        write(1, 1, "a.rs"),
        write(2, 2, "a.rs"),
    ];
    let report = lift(&events);
    assert_eq!(dep_ids(&report, "implement.3"), vec!["implement.2"]);
    // The full evidence set still contains the redundant edge.
    assert!(
        report
            .edges
            .iter()
            .any(|e| e.from == "implement.1" && e.to == "implement.3")
    );
    assert_eq!(report.critical_path(), 3);
    assert_eq!(report.parallel_width().value, 1);
}

#[test]
fn lifted_graphs_are_acyclic_and_edges_always_point_forward() {
    let events = vec![
        read(0, 0, "a.rs"),
        write(1, 0, "a.rs"),
        read(2, 1, "a.rs"),
        write(3, 1, "b.rs"),
        build(4, 1, true),
        write(5, 2, "a.rs"),
    ];
    let report = lift(&events);
    assert!(report.graph.cycle_nodes().is_empty());
    let position: std::collections::HashMap<&str, usize> = report
        .graph
        .nodes()
        .iter()
        .enumerate()
        .map(|(i, n)| (n.id.as_str(), i))
        .collect();
    for node in report.graph.nodes() {
        for dep in &node.depends_on {
            assert!(
                position[dep.as_str()] < position[node.id.as_str()],
                "edge {dep} -> {} points backwards",
                node.id
            );
        }
    }
}

#[test]
fn failure_is_recorded_rather_than_smoothed_over() {
    let events = vec![write(0, 0, "a.rs"), build(1, 0, false)];
    let report = lift(&events);
    assert_eq!(
        report.graph.get("verify.1").unwrap().status,
        NodeStatus::Failed
    );
    assert_eq!(
        report
            .graph
            .get("verify.1")
            .unwrap()
            .output
            .as_ref()
            .unwrap()
            .validation
            .as_deref(),
        Some("failed")
    );
}

#[test]
fn a_change_after_a_failed_verification_lifts_as_a_fix() {
    let events = vec![
        write(0, 0, "a.rs"),
        build(1, 0, false),
        write(2, 1, "a.rs"),
        build(3, 1, true),
        write(4, 2, "c.rs"),
    ];
    let report = lift(&events);
    assert_eq!(
        report.graph.get("implement.1").unwrap().kind,
        NodeKind::Implement
    );
    assert_eq!(report.graph.get("implement.2").unwrap().kind, NodeKind::Fix);
    // Once verification passes, later work is ordinary implementation again.
    assert_eq!(
        report.graph.get("implement.3").unwrap().kind,
        NodeKind::Implement
    );
}

#[test]
fn lift_is_deterministic_so_graphs_can_be_diffed_across_runs() {
    let events = vec![
        read(0, 0, "a.rs"),
        write(1, 0, "b.rs"),
        read(2, 1, "b.rs"),
        build(3, 1, true),
    ];
    assert_eq!(lift(&events).graph, lift(&events).graph);
}

#[test]
fn lifted_nodes_carry_lift_provenance_and_survive_the_plan_bridge() {
    let report = lift(&[write(0, 0, "a.rs")]);
    let node = report.graph.get("implement.1").unwrap();
    assert_eq!(node.origin, Some(NodeOrigin::Lift));

    // Round-trip through the live plan storage: a lifted graph must be
    // persistable, or it is a trace by another name and fails the artifact test.
    let mut plan = crate::VersionedPlan::new();
    crate::bridge::apply_task_graph(&mut plan, &report.graph);
    let restored = crate::bridge::to_task_graph(&plan);
    assert_eq!(
        restored.get("implement.1").unwrap().origin,
        Some(NodeOrigin::Lift)
    );
}

#[test]
fn classification_is_conservative_about_unknown_and_ambiguous_tools() {
    assert_eq!(classify("Write", "x"), Activity::Implement);
    assert_eq!(classify("Read", "x"), Activity::Explore);
    assert_eq!(classify("Bash", "cargo test -p jcode"), Activity::Verify);
    assert_eq!(classify("Bash", "git commit -m x"), Activity::Implement);
    assert_eq!(classify("Bash", "ls -la"), Activity::Explore);
    // An unrecognized tool must not be guessed into a change or a check.
    assert_eq!(
        classify("SomeFutureTool", "does something"),
        Activity::Explore
    );
}

#[test]
fn node_text_reads_as_re_runnable_work_not_as_a_log_line() {
    let report = lift(&[write(0, 0, "src/a.rs"), write(1, 0, "src/b.rs")]);
    assert_eq!(
        report.graph.get("implement.1").unwrap().content,
        "Change: src/a.rs, src/b.rs"
    );
}
