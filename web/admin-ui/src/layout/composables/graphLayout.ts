import { graphlib } from 'dagre-d3-es';
import { layout as dagreLayout } from 'dagre-d3-es/src/dagre/layout';
import type { Edge, Node } from '@vue-flow/core';
import { Position, useVueFlow } from '@vue-flow/core';

export function recalculateGraphLayout<NodeType extends Node, EdgeType extends Edge>(nodes: NodeType[], edges: EdgeType[]) {
    const newGraph = new graphlib.Graph();

    newGraph.setDefaultEdgeLabel(() => ({}));
    newGraph.setGraph({ rankdir: 'LR' });

    for (const node of nodes) {
      newGraph.setNode(node.id, { width: 150, height: 50 });
    }

    for (const edge of edges) {
        newGraph.setEdge(edge.source, edge.target);
    }

    dagreLayout(newGraph, {});

    return nodes.map((node) => {
      const nodeWithPosition = newGraph.node(node.id);

      return {
        ...node,
        targetPosition: Position.Left,
        sourcePosition: Position.Right,
        position: { x: nodeWithPosition.x, y: nodeWithPosition.y },
      };
    });
}
