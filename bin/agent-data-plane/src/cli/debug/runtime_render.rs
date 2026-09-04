//! Rendering of supervision tree snapshots for the terminal.
//!
//! Both renderers are pure functions over a snapshot, so they can be exercised without a running process.
//!
//! Output is deliberately uncoloured. A tree is most useful when it can be piped into a file, diffed against an
//! earlier capture, or pasted into an issue, and the branch glyphs plus the aligned columns already carry the
//! structure without needing colour to do it.

use std::fmt::Write as _;

use bytesize::ByteSize;
use chrono::{DateTime, SecondsFormat};
use saluki_core::runtime::{NodeKind, NodeSnapshot, NodeState, RestartMode, TreeSnapshot};

/// Renders a snapshot as an indented tree.
pub(super) fn render_tree(snapshot: &TreeSnapshot) -> String {
    let mut out = String::new();
    render_summary(snapshot, &mut out);
    out.push('\n');
    render_node(&snapshot.root, "", "", &mut out);
    out
}

/// Renders the header block: what was captured, when, and the tree-wide totals.
fn render_summary(snapshot: &TreeSnapshot, out: &mut String) {
    let totals = &snapshot.totals;
    let processes = totals.supervisors + totals.workers;

    let _ = writeln!(
        out,
        "Supervision tree for '{}', captured {}",
        snapshot.root.name,
        format_timestamp(snapshot.captured_at.0)
    );
    let _ = writeln!(
        out,
        "  {} processes ({} supervisors, {} workers): {} running, {} exited, {} registered",
        processes, totals.supervisors, totals.workers, totals.running, totals.exited, totals.registered
    );
    let _ = writeln!(
        out,
        "  {} restarts across the tree, max depth {}",
        totals.restarts, totals.max_depth
    );

    // Zero bytes with tracking off means nothing is measuring, which is a different thing from nothing being
    // allocated, so say which it is rather than printing a misleading zero.
    if snapshot.resource_tracking_enabled {
        let _ = writeln!(
            out,
            "  resource tracking on: {} live, {} CPU",
            ByteSize::b(totals.live_bytes),
            format_cpu(totals.cpu_time_nanos)
        );
    } else {
        let _ = writeln!(
            out,
            "  resource tracking off: no allocation or CPU figures are available"
        );
    }
}

/// Renders one node and, recursively, its children.
///
/// `prefix` is what precedes this node's own line; `child_prefix` is what precedes its descendants' lines.
fn render_node(node: &NodeSnapshot, prefix: &str, child_prefix: &str, out: &mut String) {
    let _ = writeln!(out, "{}{}", prefix, format_node(node));

    let last = node.children.len().saturating_sub(1);
    for (index, child) in node.children.iter().enumerate() {
        let (branch, continuation) = if index == last {
            ("`-- ", "    ")
        } else {
            ("|-- ", "|   ")
        };
        render_node(
            child,
            &format!("{}{}", child_prefix, branch),
            &format!("{}{}", child_prefix, continuation),
            out,
        );
    }
}

/// Formats a single node's line: its name, then the facts worth scanning down a column.
fn format_node(node: &NodeSnapshot) -> String {
    let mut line = format!(
        "{}  [{}] {}",
        node.name,
        match node.kind {
            NodeKind::Supervisor => "sup",
            NodeKind::Worker => "worker",
        },
        match node.state {
            NodeState::Running => "running",
            NodeState::Exited => "EXITED",
            NodeState::Registered => "registered",
        }
    );

    if let Some(process_id) = node.process_id {
        let _ = write!(line, "  pid={}", process_id);
    }
    if let Some(uptime) = node.uptime_ms {
        let _ = write!(line, "  up={}", format_duration(uptime));
    }
    if node.restart_count > 0 {
        let _ = write!(line, "  restarts={}", node.restart_count);
    }
    if node.significant {
        line.push_str("  significant");
    }

    if let Some(supervision) = &node.supervision {
        let _ = write!(
            line,
            "  {}({}/{})",
            match supervision.restart_mode {
                RestartMode::OneForOne => "one_for_one",
                RestartMode::OneForAll => "one_for_all",
            },
            supervision.restart_intensity,
            format_duration(supervision.restart_period_ms)
        );
        if let Some(threads) = supervision.dedicated_threads {
            let _ = write!(line, "  rt={}thr", threads);
        }
    }

    if let Some(resources) = &node.resources {
        let _ = write!(line, "  live={}", ByteSize::b(resources.live_bytes));
        if resources.cpu_time_nanos > 0 {
            let _ = write!(line, "  cpu={}", format_cpu(resources.cpu_time_nanos));
        }
    }

    if let Some(exited_at) = node.exited_at {
        let _ = write!(line, "  exited={}", format_timestamp(exited_at.0));
    }

    line
}

/// Renders a snapshot as a Graphviz DOT graph, for rendering with `dot`.
pub(super) fn render_dot(snapshot: &TreeSnapshot) -> String {
    let mut out = String::new();
    out.push_str("digraph supervision_tree {\n");
    out.push_str("  rankdir=LR;\n");
    out.push_str("  node [shape=box, style=rounded, fontname=\"monospace\", fontsize=10];\n");
    out.push_str("  edge [arrowsize=0.7];\n");

    let mut next_id = 0;
    render_dot_node(&snapshot.root, None, &mut next_id, &mut out);

    out.push_str("}\n");
    out
}

/// Emits one node and its edge from `parent`, then recurses.
fn render_dot_node(node: &NodeSnapshot, parent: Option<usize>, next_id: &mut usize, out: &mut String) {
    let id = *next_id;
    *next_id += 1;

    let mut label = escape_dot(&node.name);
    label.push_str("\\n");
    label.push_str(match node.kind {
        NodeKind::Supervisor => "supervisor",
        NodeKind::Worker => "worker",
    });
    if let Some(process_id) = node.process_id {
        label.push_str(&format!(" pid={}", process_id));
    }
    if node.restart_count > 0 {
        label.push_str(&format!("\\nrestarts={}", node.restart_count));
    }

    // A supervisor is the load-bearing structure, so give it visual weight; a node that has stopped is what someone
    // reading the graph is usually looking for, so make it impossible to miss.
    let style = match (node.kind, node.state) {
        (_, NodeState::Exited) => ", style=\"rounded,filled\", fillcolor=\"#f8d7da\"",
        (_, NodeState::Registered) => ", style=\"rounded,dashed\"",
        (NodeKind::Supervisor, _) => ", style=\"rounded,bold\"",
        (NodeKind::Worker, _) => "",
    };

    let _ = writeln!(out, "  n{} [label=\"{}\"{}];", id, label, style);
    if let Some(parent) = parent {
        let _ = writeln!(out, "  n{} -> n{};", parent, id);
    }

    for child in &node.children {
        render_dot_node(child, Some(id), next_id, out);
    }
}

/// Escapes a string for use inside a DOT quoted label.
fn escape_dot(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Formats a duration in milliseconds, coarsening the unit as it grows.
fn format_duration(millis: u64) -> String {
    let secs = millis / 1000;
    if secs == 0 {
        return format!("{}ms", millis);
    }
    if secs < 60 {
        return format!("{}s", secs);
    }

    let mins = secs / 60;
    if mins < 60 {
        return format!("{}m{}s", mins, secs % 60);
    }

    let hours = mins / 60;
    if hours < 24 {
        return format!("{}h{}m", hours, mins % 60);
    }

    format!("{}d{}h", hours / 24, hours % 24)
}

/// Formats CPU time given in nanoseconds.
fn format_cpu(nanos: u64) -> String {
    if nanos < 1_000_000 {
        format!("{}us", nanos / 1_000)
    } else if nanos < 1_000_000_000 {
        format!("{}ms", nanos / 1_000_000)
    } else {
        format!("{:.1}s", nanos as f64 / 1_000_000_000.0)
    }
}

/// Formats a Unix-millisecond timestamp as an RFC 3339 instant, falling back to the raw value if it isn't
/// representable.
fn format_timestamp(millis: u64) -> String {
    match i64::try_from(millis).ok().and_then(DateTime::from_timestamp_millis) {
        Some(timestamp) => timestamp.to_rfc3339_opts(SecondsFormat::Secs, true),
        None => format!("{}ms since epoch", millis),
    }
}

#[cfg(test)]
mod tests {
    use saluki_core::runtime::{AutoShutdown, ResourceUsage, RestartType, SupervisionSettings, TreeTotals, UnixMillis};

    use super::*;

    /// Builds a node with the fields a test doesn't care about set to something inert.
    fn node(name: &str, kind: NodeKind, state: NodeState) -> NodeSnapshot {
        NodeSnapshot {
            name: name.to_string(),
            kind,
            process_name: Some(format!("root.{name}")),
            process_id: Some(7),
            state,
            restart: RestartType::Permanent,
            significant: false,
            created_at: UnixMillis(1_700_000_000_000),
            started_at: Some(UnixMillis(1_700_000_000_000)),
            uptime_ms: matches!(state, NodeState::Running).then_some(8_100_000),
            restart_count: 0,
            exited_at: None,
            resource_group: Some(format!("root.{name}")),
            resources: None,
            supervision: None,
            children: Vec::new(),
        }
    }

    /// Builds the supervision settings a supervisor node carries.
    fn supervision() -> SupervisionSettings {
        SupervisionSettings {
            restart_mode: RestartMode::OneForOne,
            restart_intensity: 3,
            restart_period_ms: 5_000,
            auto_shutdown: AutoShutdown::Never,
            shutdown_budget_ms: None,
            dedicated_threads: None,
            restarts_performed: 0,
            generation: 1,
        }
    }

    /// Builds a three-level tree: a root with a worker and a nested supervisor of its own.
    fn fixture() -> TreeSnapshot {
        let mut inner = node("inner-worker", NodeKind::Worker, NodeState::Exited);
        inner.exited_at = Some(UnixMillis(1_700_000_050_000));
        inner.restart_count = 2;
        inner.uptime_ms = None;

        let mut nested = node("child-sup", NodeKind::Supervisor, NodeState::Running);
        nested.supervision = Some(SupervisionSettings {
            dedicated_threads: Some(1),
            ..supervision()
        });
        nested.resources = Some(ResourceUsage {
            live_bytes: 2_097_152,
            cpu_time_nanos: 1_500_000_000,
            ..Default::default()
        });
        nested.children.push(inner);

        let mut root = node("adp-root", NodeKind::Supervisor, NodeState::Running);
        root.process_name = Some("adp_root".to_string());
        root.supervision = Some(supervision());
        root.children
            .push(node("bootstrap", NodeKind::Worker, NodeState::Running));
        root.children.push(nested);

        TreeSnapshot {
            captured_at: UnixMillis(1_700_000_100_000),
            resource_tracking_enabled: true,
            totals: TreeTotals {
                supervisors: 2,
                workers: 2,
                running: 3,
                exited: 1,
                registered: 0,
                restarts: 2,
                live_bytes: 2_097_152,
                cpu_time_nanos: 1_500_000_000,
                max_depth: 3,
            },
            root,
        }
    }

    #[test]
    fn tree_render_shows_nesting_with_branch_glyphs() {
        let rendered = render_tree(&fixture());

        // The last child of a level closes its branch; earlier siblings keep the trunk running past them, which is
        // what makes a deep tree readable.
        assert!(rendered.contains("\n|-- bootstrap  [worker] running"), "{rendered}");
        assert!(rendered.contains("\n`-- child-sup  [sup] running"), "{rendered}");
        assert!(
            rendered.contains("\n    `-- inner-worker  [worker] EXITED"),
            "{rendered}"
        );

        // A node's own line starts at column zero only for the root.
        assert!(rendered.contains("\nadp-root  [sup] running"), "{rendered}");
    }

    #[test]
    fn tree_render_reports_the_facts_worth_scanning() {
        let rendered = render_tree(&fixture());

        assert!(
            rendered.contains("4 processes (2 supervisors, 2 workers)"),
            "{rendered}"
        );
        assert!(rendered.contains("3 running, 1 exited, 0 registered"), "{rendered}");
        assert!(
            rendered.contains("2 restarts across the tree, max depth 3"),
            "{rendered}"
        );
        assert!(
            rendered.contains("resource tracking on: 2.0 MiB live, 1.5s CPU"),
            "{rendered}"
        );

        assert!(rendered.contains("up=2h15m"), "uptime is coarsened: {rendered}");
        assert!(rendered.contains("restarts=2"), "{rendered}");
        assert!(rendered.contains("one_for_one(3/5s)"), "{rendered}");
        assert!(rendered.contains("rt=1thr"), "{rendered}");
        assert!(rendered.contains("live=2.0 MiB"), "{rendered}");
        assert!(
            rendered.contains("2023-11-14T22:14:10Z"),
            "exit time is shown: {rendered}"
        );

        // A node with no restarts shouldn't carry a `restarts=0` column: the point of the line is what stands out.
        assert!(!rendered.contains("restarts=0"), "{rendered}");
    }

    #[test]
    fn tree_render_says_when_nothing_is_measuring() {
        // Zero bytes with tracking off is a different statement from zero bytes with tracking on, and the renderer
        // has to make that distinction rather than printing a misleading zero.
        let mut snapshot = fixture();
        snapshot.resource_tracking_enabled = false;

        let rendered = render_tree(&snapshot);
        assert!(rendered.contains("resource tracking off"), "{rendered}");
        assert!(!rendered.contains("live, "), "{rendered}");
    }

    #[test]
    fn dot_render_emits_one_node_and_edge_per_child() {
        let rendered = render_dot(&fixture());

        assert!(rendered.starts_with("digraph supervision_tree {\n"), "{rendered}");
        assert!(rendered.ends_with("}\n"), "{rendered}");

        // Four nodes, and an edge for every node but the root.
        assert_eq!(rendered.matches(" [label=").count(), 4, "{rendered}");
        assert_eq!(rendered.matches(" -> ").count(), 3, "{rendered}");

        assert!(
            rendered.contains("n0 [label=\"adp-root\\nsupervisor pid=7\""),
            "{rendered}"
        );
        assert!(rendered.contains("n0 -> n1;"), "{rendered}");

        // An exited node is what a reader is usually hunting for, so it is filled rather than merely labelled.
        assert!(
            rendered.contains("restarts=2\", style=\"rounded,filled\""),
            "{rendered}"
        );
    }

    #[test]
    fn dot_render_escapes_label_metacharacters() {
        let mut snapshot = fixture();
        snapshot.root.name = r#"weird"name\with"#.to_string();

        let rendered = render_dot(&snapshot);
        assert!(rendered.contains(r#"label="weird\"name\\with\n"#), "{rendered}");
    }

    #[test]
    fn durations_coarsen_as_they_grow() {
        assert_eq!(format_duration(0), "0ms");
        assert_eq!(format_duration(250), "250ms");
        assert_eq!(format_duration(5_000), "5s");
        assert_eq!(format_duration(59_999), "59s");
        assert_eq!(format_duration(90_000), "1m30s");
        assert_eq!(format_duration(3_600_000), "1h0m");
        assert_eq!(format_duration(8_100_000), "2h15m");
        assert_eq!(format_duration(90_000_000), "1d1h");
    }

    #[test]
    fn cpu_time_coarsens_as_it_grows() {
        assert_eq!(format_cpu(500), "0us");
        assert_eq!(format_cpu(5_000), "5us");
        assert_eq!(format_cpu(5_000_000), "5ms");
        assert_eq!(format_cpu(1_500_000_000), "1.5s");
    }

    #[test]
    fn a_timestamp_beyond_representation_falls_back_to_the_raw_value() {
        assert_eq!(format_timestamp(1_700_000_100_000), "2023-11-14T22:15:00Z");
        assert_eq!(format_timestamp(u64::MAX), format!("{}ms since epoch", u64::MAX));
    }
}
