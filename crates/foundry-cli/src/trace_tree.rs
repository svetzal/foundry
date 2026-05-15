//! Convert a flat trace response into a span tree for rendering.
//!
//! Spans form a tree rooted at events whose `parent_span_id` is empty (or
//! refers to a span not present in the response).  Block executions attach
//! to the span they ran inside (`block.parent_span_id`).
//!
//! This module is pure data-shaping plus an ASCII renderer; it has no I/O.
//! Wiring into the `foundry trace` command is handled in `commands.rs`.

use crate::proto::{TraceBlockExecution, TraceEvent};
use std::collections::HashMap;
use std::fmt::Write as _;

/// One node in a span tree.
#[derive(Debug)]
pub struct SpanNode {
    pub span_id: String,
    pub parent_span_id: Option<String>,
    pub events: Vec<TraceEvent>,
    pub blocks: Vec<TraceBlockExecution>,
    pub children: Vec<SpanNode>,
}

/// Build a forest of span trees from a flat list of events and blocks.
///
/// Roots are spans whose `parent_span_id` is empty (or refers to a span
/// not present in the response). Multiple roots are normal — e.g. when
/// the trace is rooted at a cycle, blocks within it are children of the
/// cycle span. When the trace is rooted at a workflow, the workflow span
/// is root.
pub fn build_forest(events: Vec<TraceEvent>, blocks: Vec<TraceBlockExecution>) -> Vec<SpanNode> {
    // Group events by span_id.
    let mut events_by_span: HashMap<String, Vec<TraceEvent>> = HashMap::new();
    for e in events {
        events_by_span.entry(e.span_id.clone()).or_default().push(e);
    }

    // For each span_id, build a node with its events.
    let mut node_by_span: HashMap<String, SpanNode> = HashMap::new();
    let mut parent_of: HashMap<String, Option<String>> = HashMap::new();

    for (span_id, evs) in events_by_span {
        let parent = evs.first().and_then(|e| {
            if e.parent_span_id.is_empty() {
                None
            } else {
                Some(e.parent_span_id.clone())
            }
        });
        parent_of.insert(span_id.clone(), parent.clone());
        node_by_span.insert(
            span_id.clone(),
            SpanNode {
                span_id,
                parent_span_id: parent,
                events: evs,
                blocks: vec![],
                children: vec![],
            },
        );
    }

    // Attach blocks to their parent span (block.parent_span_id == owning span_id).
    for b in blocks {
        if let Some(node) = node_by_span.get_mut(&b.parent_span_id) {
            node.blocks.push(b);
        }
    }

    // Determine roots and a parent → children index.
    let span_ids: Vec<String> = node_by_span.keys().cloned().collect();
    let mut roots: Vec<String> = vec![];
    let mut children_of: HashMap<String, Vec<String>> = HashMap::new();
    for sid in &span_ids {
        if let Some(parent) = parent_of.get(sid).cloned().flatten() {
            if node_by_span.contains_key(&parent) {
                children_of.entry(parent).or_default().push(sid.clone());
            } else {
                // Parent isn't in this response — treat this span as a root.
                roots.push(sid.clone());
            }
        } else {
            roots.push(sid.clone());
        }
    }

    // Sort children deterministically by span_id so output is stable.
    for v in children_of.values_mut() {
        v.sort();
    }
    roots.sort();

    roots
        .into_iter()
        .filter_map(|r| take_subtree(&mut node_by_span, &children_of, &r))
        .collect()
}

/// Remove the node at `sid` from `node_by_span` and recursively attach its
/// children (looked up via `children_of`). Returns `None` if the node was
/// already taken or never existed.
fn take_subtree(
    node_by_span: &mut HashMap<String, SpanNode>,
    children_of: &HashMap<String, Vec<String>>,
    sid: &str,
) -> Option<SpanNode> {
    let mut node = node_by_span.remove(sid)?;
    if let Some(cs) = children_of.get(sid) {
        for c in cs {
            if let Some(child) = take_subtree(node_by_span, children_of, c) {
                node.children.push(child);
            }
        }
    }
    Some(node)
}

/// Render a forest of spans as a human-readable ASCII tree.
pub fn render(forest: &[SpanNode], out: &mut String) {
    for (i, node) in forest.iter().enumerate() {
        render_node(node, "", i == forest.len() - 1, out);
    }
}

fn render_node(node: &SpanNode, prefix: &str, is_last: bool, out: &mut String) {
    let connector = if is_last { "└── " } else { "├── " };
    let span_label = node.events.first().map_or("span", |e| e.event_type.as_str());
    let parent_label = node.parent_span_id.as_deref().map_or_else(|| "∅".to_string(), short);
    let _ = writeln!(
        out,
        "{prefix}{connector}[span] {span_label}  span={}  parent={parent_label}",
        short(&node.span_id),
    );

    let child_prefix = format!("{prefix}{}", if is_last { "    " } else { "│   " });

    let event_count = node.events.len();
    let block_count = node.blocks.len();
    let child_count = node.children.len();

    for (i, e) in node.events.iter().enumerate() {
        let last = i + 1 == event_count && block_count == 0 && child_count == 0;
        let conn = if last { "└── " } else { "├── " };
        let _ = writeln!(out, "{child_prefix}{conn}{}  project={}", e.event_type, e.project,);
    }

    for (i, b) in node.blocks.iter().enumerate() {
        let last = i + 1 == block_count && child_count == 0;
        let conn = if last { "└── " } else { "├── " };
        let _ = writeln!(
            out,
            "{child_prefix}{conn}[block: {}]  block_span={}  duration={}ms",
            b.block_name,
            short(&b.span_id),
            b.duration_ms,
        );
    }

    for (i, child) in node.children.iter().enumerate() {
        let last = i + 1 == child_count;
        render_node(child, &child_prefix, last, out);
    }
}

/// Truncate a long span id for compact display.
fn short(id: &str) -> String {
    // Take a UTF-8-safe 8-character prefix, then append an ellipsis if truncated.
    let mut iter = id.char_indices();
    if let Some((byte_idx, _)) = iter.nth(8) {
        // There were more than 8 chars; truncate at the 9th char boundary.
        format!("{}…", &id[..byte_idx])
    } else {
        id.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evt(span: &str, parent: &str, ty: &str) -> TraceEvent {
        TraceEvent {
            event_id: format!("evt_{ty}_{span}"),
            event_type: ty.to_string(),
            project: "p".to_string(),
            occurred_at: "2026-05-01T00:00:00Z".to_string(),
            throttle: 0,
            trace_id: "trace1".to_string(),
            span_id: span.to_string(),
            parent_span_id: parent.to_string(),
        }
    }

    fn block(name: &str, span: &str, parent: &str) -> TraceBlockExecution {
        TraceBlockExecution {
            block_name: name.to_string(),
            trigger_event_id: format!("trigger_{name}"),
            success: true,
            summary: String::new(),
            emitted_event_ids: vec![],
            duration_ms: 100,
            raw_output: String::new(),
            exit_code: 0,
            trigger_payload_json: String::new(),
            emitted_payload_jsons: vec![],
            audit_artifacts: vec![],
            span_id: span.to_string(),
            parent_span_id: parent.to_string(),
        }
    }

    #[test]
    fn two_level_tree() {
        let evs = vec![
            evt("a", "", "maintenance_cycle_started"),
            evt("b", "a", "project_run_started"),
        ];
        let forest = build_forest(evs, vec![]);
        assert_eq!(forest.len(), 1);
        assert_eq!(forest[0].span_id, "a");
        assert_eq!(forest[0].children.len(), 1);
        assert_eq!(forest[0].children[0].span_id, "b");
        assert_eq!(forest[0].children[0].parent_span_id.as_deref(), Some("a"),);
    }

    #[test]
    fn dangling_parent_treated_as_root() {
        let evs = vec![evt("b", "missing", "x")];
        let forest = build_forest(evs, vec![]);
        assert_eq!(forest.len(), 1);
        assert_eq!(forest[0].span_id, "b");
        // Parent reference is preserved even though no node exists for it.
        assert_eq!(forest[0].parent_span_id.as_deref(), Some("missing"));
    }

    #[test]
    fn block_attached_to_parent_span() {
        let evs = vec![evt("a", "", "cycle_started")];
        let blk = block("FanOut", "block-a", "a");
        let forest = build_forest(evs, vec![blk]);
        assert_eq!(forest.len(), 1);
        assert_eq!(forest[0].blocks.len(), 1);
        assert_eq!(forest[0].blocks[0].block_name, "FanOut");
    }

    #[test]
    fn block_with_unknown_parent_is_dropped() {
        // A block whose parent span isn't in the response has no home; we
        // currently drop it rather than synthesizing a root. This documents
        // that behaviour so a future change is intentional.
        let evs = vec![evt("a", "", "cycle_started")];
        let blk = block("Orphan", "block-x", "missing-span");
        let forest = build_forest(evs, vec![blk]);
        assert_eq!(forest.len(), 1);
        assert_eq!(forest[0].blocks.len(), 0);
    }

    #[test]
    fn multiple_roots_when_unrelated_spans() {
        let evs = vec![
            evt("a", "", "first_started"),
            evt("b", "", "second_started"),
        ];
        let forest = build_forest(evs, vec![]);
        assert_eq!(forest.len(), 2);
        // Sorted by span_id for determinism.
        assert_eq!(forest[0].span_id, "a");
        assert_eq!(forest[1].span_id, "b");
    }

    #[test]
    fn events_grouped_by_span() {
        // Two events with the same span_id should collapse into one node.
        let evs = vec![
            evt("a", "", "cycle_started"),
            evt("a", "", "cycle_completed"),
        ];
        let forest = build_forest(evs, vec![]);
        assert_eq!(forest.len(), 1);
        assert_eq!(forest[0].events.len(), 2);
    }

    #[test]
    fn render_includes_span_labels_and_block_names() {
        let evs = vec![
            evt("aaaaaaaaaaaa", "", "cycle_started"),
            evt("bbbbbbbbbbbb", "aaaaaaaaaaaa", "project_run_started"),
        ];
        let blocks = vec![block("FanOut", "block-a", "aaaaaaaaaaaa")];
        let forest = build_forest(evs, blocks);
        let mut out = String::new();
        render(&forest, &mut out);

        assert!(out.contains("cycle_started"), "missing root event type:\n{out}");
        assert!(out.contains("project_run_started"), "missing child event:\n{out}");
        assert!(out.contains("FanOut"), "missing block name:\n{out}");
        assert!(out.contains("[span]"), "missing span marker:\n{out}");
        assert!(out.contains("[block:"), "missing block marker:\n{out}");
        // Long span_ids should be truncated with an ellipsis.
        assert!(out.contains("aaaaaaaa…"), "expected truncated span id:\n{out}");
    }

    #[test]
    fn render_uses_empty_set_for_orphan_root_parent() {
        let evs = vec![evt("aaaaaaaaaaaa", "", "cycle_started")];
        let forest = build_forest(evs, vec![]);
        let mut out = String::new();
        render(&forest, &mut out);
        assert!(out.contains("parent=∅"), "expected ∅ marker for root parent:\n{out}");
    }

    #[test]
    fn short_handles_short_ids_unchanged() {
        assert_eq!(short("abc"), "abc");
        assert_eq!(short("abcdefgh"), "abcdefgh"); // exactly 8 chars
        assert_eq!(short("abcdefghi"), "abcdefgh…");
    }
}
