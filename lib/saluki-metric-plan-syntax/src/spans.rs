//! Where each part of a parsed artifact came from in the source.

use std::collections::BTreeMap;

use crate::diagnostic::Span;

/// The source spans of a parsed artifact's nodes, parameters, and edges.
///
/// Validation reports problems against the artifact: a node, a parameter path within a node's parameters (such as
/// `rules[0].action`, in the form [`ParamError`](saluki_metric_plan::ir::ParamError) paths take), or an edge by its
/// position in the artifact's edge list. A span map turns each of those back into the source text the author wrote,
/// so a tool can underline it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SpanMap {
    nodes: BTreeMap<String, Span>,
    params: BTreeMap<String, BTreeMap<String, Span>>,
    edges: Vec<Span>,
}

impl SpanMap {
    /// Returns the span of the node with the local ID `id`: its `node` definition, or the `emit` or `drop` target of an
    /// inline terminal.
    pub fn node(&self, id: &str) -> Option<Span> {
        self.nodes.get(id).copied()
    }

    /// Returns the span of the value at parameter path `path` of the node `id`.
    ///
    /// Paths name a parameter, then list elements by index and record or map members by name: `predicate`, `tags[2]`,
    /// `rules[0].action`. When the exact path has no span of its own, as for a member nested inside a predicate, this
    /// returns the span of the nearest enclosing value that has one. Returns `None` if the node has no parameter on
    /// the path.
    pub fn param(&self, id: &str, path: &str) -> Option<Span> {
        let params = self.params.get(id)?;
        let mut path = path;
        loop {
            if let Some(span) = params.get(path) {
                return Some(*span);
            }
            path = parent_path(path)?;
        }
    }

    /// Returns the span of the edge at position `index` of the artifact's edge list.
    pub fn edge(&self, index: usize) -> Option<Span> {
        self.edges.get(index).copied()
    }

    pub(crate) fn insert_node(&mut self, id: &str, span: Span) {
        self.nodes.insert(id.to_string(), span);
    }

    pub(crate) fn insert_param(&mut self, id: &str, path: String, span: Span) {
        self.params.entry(id.to_string()).or_default().insert(path, span);
    }

    pub(crate) fn push_edge(&mut self, span: Span) {
        self.edges.push(span);
    }
}

/// Returns the path of the value enclosing the one at `path`, or `None` for a parameter name.
fn parent_path(path: &str) -> Option<&str> {
    if let Some(stripped) = path.strip_suffix(']') {
        return stripped.rfind('[').map(|open| &path[..open]);
    }
    path.rfind('.').map(|dot| &path[..dot])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parameter_lookups_fall_back_to_the_nearest_enclosing_value() {
        let mut spans = SpanMap::default();
        spans.insert_param("n", "rules".to_string(), Span::new(0, 50));
        spans.insert_param("n", "rules[0]".to_string(), Span::new(1, 20));
        spans.insert_param("n", "rules[0].action".to_string(), Span::new(5, 14));

        // (path, expected)
        let cases = [
            ("rules[0].action", Some(Span::new(5, 14))),
            ("rules[0].action.inner", Some(Span::new(5, 14))),
            ("rules[0].tags[3]", Some(Span::new(1, 20))),
            ("rules[1]", Some(Span::new(0, 50))),
            ("rules", Some(Span::new(0, 50))),
            ("other", None),
            ("other[0]", None),
        ];
        for (path, expected) in cases {
            assert_eq!(spans.param("n", path), expected, "{}", path);
        }
        assert_eq!(spans.param("m", "rules"), None);
    }
}
