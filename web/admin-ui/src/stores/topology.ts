import { defineStore } from 'pinia';
import { recalculateGraphLayout } from '@/layout/composables/graphLayout.ts';

function node(id: string, type: string, name: string, subname: string | undefined) {
  return { id, data: { component: { name, subname, type } }, type: 'component', position: { x: 0, y: 0 }, connectable: false };
}

function edge(from: string, to: string) {
  return { id: `${from}_${to}`, source: from, target: to };
}

export const useTopologyStore = defineStore('dashboard.topology', {
  state: () => ({ _nodes: [], _edges: [] }),
  getters: {
    nodes: (state) => state._nodes,
    edges: (state) => state._edges
  },
  actions: {
    async processRawNodes(rawNodes) {
      const newNodes = [];
      const newEdges = [];

      for (const rawNode of rawNodes) {
        newNodes.push(node(rawNode.id, rawNode.type, rawNode.name, rawNode.subname));

        const sourceEdges = rawNode.sourceEdges || [];
        for (const sourceEdge of sourceEdges) {
          newEdges.push(edge(sourceEdge, rawNode.id));
        }
      }

      const processedNodes = recalculateGraphLayout(newNodes, newEdges);

      this._edges = newEdges;
      this._nodes = processedNodes;
    }
  }
});
