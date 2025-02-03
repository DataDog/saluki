import { defineStore } from 'pinia';
import { recalculateGraphLayout } from '@/layout/composables/graphLayout.ts';
import { h } from 'vue';

function nodeLabel(primary: string, secondary: string | undefined) {
  let children = [h('div', { class: 'text-lg font-medium text-surface-700 dark:text-surface-200' }, primary)];

  if (secondary) {
    children.push(h('div', { class: 'text-sm font-normal text-surface-500 dark:text-surface-400' }, secondary));
  }

  return h('div', children);
}

function node(id: string, primary: string, secondary: string | undefined) {
  return { id, data: { label: nodeLabel(primary, secondary) }, type: 'component', position: { x: 0, y: 0 }, connectable: false };
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
        newNodes.push(node(rawNode.id, rawNode.primary, rawNode.secondary));

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
