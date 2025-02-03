<script setup>
import ComponentNode from '@/components/topology/ComponentNode.vue';
import { useTopologyStore } from '@/stores/topology';

import { VueFlow, useVueFlow } from '@vue-flow/core';
import { onMounted } from 'vue';

const store = useTopologyStore();
const { fitView } = useVueFlow();

onMounted(async () => {
  await store.processRawNodes([
    { id: 'dsd_in_metrics', type: 'source', name: 'DogStatsD', subname: '(metrics)' },
    { id: 'dsd_in_events', type: 'source', name: 'DogStatsD', subname: '(events)' },
    { id: 'dsd_in_service_checks', type: 'source', name: 'DogStatsD', subname: '(service checks)' },
    { id: 'dsd_agg', type: 'transform', name: 'Aggregate', sourceEdges: ['dsd_in_metrics'] },
    { id: 'enrich', type: 'transform', name: 'Enrichment', sourceEdges: ['dsd_agg'] },
    { id: 'dsd_prefix_filter', type: 'transform', name: 'Prefix / Filter', sourceEdges: ['enrich'] },
    { id: 'dd_metrics_out', type: 'destination', name: 'Datadog', subname: '(metrics)', sourceEdges: ['dsd_prefix_filter'] },
    { id: 'dd_events_sc_out', type: 'destination', name: 'Datadog', subname: '(events / service checks)', sourceEdges: ['dsd_in_events', 'dsd_in_service_checks'] }
  ]);
});
</script>

<template>
  <div id="topologyMap" class="col-span-12 h-96">
    <h2 class="text-3xl font-semibold">Topology View</h2>
    <VueFlow :nodes="store.nodes" :edges="store.edges" @nodes-initialized="fitView()" :node-connectable="false" :nodes-draggable="false" :edges-updatable="false" :default-edge-options="{ animated: true }">
      <template #node-component="componentNodeProps">
        <ComponentNode v-bind="componentNodeProps" boundary="#topologyMap" />
      </template>
    </VueFlow>
  </div>
</template>
