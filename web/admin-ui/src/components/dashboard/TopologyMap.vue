<script setup>
import ComponentNode from '@/components/topology/ComponentNode.vue';
import { useTopologyStore } from '@/stores/topology';

import { VueFlow, useVueFlow } from '@vue-flow/core';
import { onMounted } from 'vue';

const store = useTopologyStore();
const { fitView } = useVueFlow();

onMounted(async () => {
  await store.processRawNodes([
    { id: 'dsd_in_metrics', primary: 'DogStatsD', secondary: '(metrics)' },
    { id: 'dsd_in_events', primary: 'DogStatsD', secondary: '(events)' },
    { id: 'dsd_in_service_checks', primary: 'DogStatsD', secondary: '(service checks)' },
    { id: 'dsd_agg', primary: 'Aggregate', sourceEdges: ['dsd_in_metrics'] },
    { id: 'enrich', primary: 'Enrichment', sourceEdges: ['dsd_agg'] },
    { id: 'dsd_prefix_filter', primary: 'Prefix / Filter', sourceEdges: ['enrich'] },
    { id: 'dd_metrics_out', primary: 'Datadog', secondary: '(metrics)', sourceEdges: ['dsd_prefix_filter'] },
    { id: 'dd_events_sc_out', primary: 'Datadog', secondary: '(events / service checks)', sourceEdges: ['dsd_in_events', 'dsd_in_service_checks'] }
  ]);
});
</script>

<template>
  <div class="col-span-12 h-96">
    <h2 class="text-3xl font-semibold">Topology View</h2>
    <VueFlow :nodes="store.nodes" :edges="store.edges" @nodes-initialized="fitView()" :node-connectable="false" :nodes-draggable="false" :edges-updatable="false" :default-edge-options="{ animated: true }">
      <template #node-component="componentNodeProps">
        <ComponentNode v-bind="componentNodeProps" />
      </template>
    </VueFlow>
  </div>
</template>
