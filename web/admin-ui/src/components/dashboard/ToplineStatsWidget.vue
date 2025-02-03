<script setup>
import SpinningLoader from '../SpinningLoader.vue';
import { bytesToHumanFriendly, secondsToHumanFriendly } from '@/utils';
import { TelemetryService } from '@/gen/telemetry_pb';

import { createClient } from '@connectrpc/connect';
import { inject, ref, watch } from 'vue';
import { useRoute } from 'vue-router';
import { useToast } from 'primevue/usetoast';

const route = useRoute();
const toast = useToast();
const transport = inject('grpc-api-transport');
if (!transport) {
  throw new Error('No transport set by provider');
}
const client = createClient(TelemetryService, transport);

const loading = ref(false);
const stats = ref(null);
const retrying = ref(false);

// Start fetching statistics once the component is mounted.
watch(() => route, startStatisticsFetch, { immediate: true });

function startStatisticsFetch() {
  // Set the component to a known state.
  loading.value = true;
  stats.value = null;
  retrying.value = false;

  setTimeout(() => fetchStatistics(), 2000);
}

async function fetchStatistics() {
  let fetchDuration = 2000;

  try {
    const { pid, uptimeSecs, rssBytes } = await client.getProcessInformation();
    stats.value = { pid, uptimeSecs: Number(uptimeSecs), cpuUsage: 0.0, rssBytes: Number(rssBytes), metrics: 0, logs: 0, traces: 0 };

    toast.removeGroup('dashboardErrors');
    loading.value = false;
    retrying.value = false;
  } catch (err) {
    // Notify the user that we failed to load some data, but only when we initially fail: we don't want to spam them
    // over and over when we're still retrying.
    if (!retrying.value) {
      toast.add({ group: 'dashboardErrors', severity: 'error', summary: 'Failed to load some data.', detail: "Oops! We failed to load some metrics from ADP. We'll try again in a few seconds.", life: 5000 });
    }

    retrying.value = true;

    // We'll retry a little more often than usual until we're successful.
    fetchDuration = 1000;
  }

  // Enqueue our next statistics fetch.
  setTimeout(() => fetchStatistics(), fetchDuration);
}
</script>

<template>
  <div class="col-span-12 lg:col-span-6 xl:col-span-3">
    <div class="card mb-0">
      <div class="flex justify-between mb-2">
        <div>
          <span class="block text-muted-color font-medium mb-2 text-xl">Process ID</span>
          <div v-if="loading"><SpinningLoader /></div>
          <div v-if="stats" class="text-surface-900 dark:text-surface-0 font-medium text-3xl">{{ stats.pid }}</div>
        </div>
      </div>
    </div>
  </div>
  <div class="col-span-12 lg:col-span-6 xl:col-span-3">
    <div class="card mb-0">
      <div class="flex justify-between mb-2">
        <div>
          <span class="block text-muted-color font-medium mb-2 text-xl">Uptime</span>
          <div v-if="loading"><SpinningLoader /></div>
          <div v-if="stats" class="text-surface-900 dark:text-surface-0 font-medium text-3xl">{{ secondsToHumanFriendly(stats.uptimeSecs) }}</div>
        </div>
      </div>
    </div>
  </div>
  <div class="col-span-12 lg:col-span-6 xl:col-span-3">
    <div class="card mb-0">
      <div class="flex justify-between mb-2">
        <div>
          <span class="block text-muted-color font-medium mb-2 text-xl">CPU</span>
          <div v-if="loading"><SpinningLoader /></div>
          <div v-if="stats" class="text-surface-900 dark:text-surface-0 font-medium text-3xl">{{ stats.cpuUsage.toFixed(2) }} %</div>
        </div>
      </div>
    </div>
  </div>
  <div class="col-span-12 lg:col-span-6 xl:col-span-3">
    <div class="card mb-0">
      <div class="flex justify-between mb-2">
        <div>
          <span class="block text-muted-color font-medium mb-2 text-xl">Memory</span>
          <div v-if="loading"><SpinningLoader /></div>
          <div v-if="stats" class="text-surface-900 dark:text-surface-0 font-medium text-3xl">{{ bytesToHumanFriendly(stats.rssBytes) }}</div>
        </div>
      </div>
    </div>
  </div>
  <div class="col-span-12 lg:col-span-6 xl:col-span-4">
    <div class="card mb-0">
      <div class="flex justify-between mb-2">
        <div>
          <span class="block text-muted-color font-medium mb-2 text-xl">Metrics Ingested</span>
          <div v-if="loading"><SpinningLoader /></div>
          <div v-if="stats" class="text-surface-900 dark:text-surface-0 font-medium text-3xl">{{ stats.metrics }}</div>
        </div>
      </div>
    </div>
  </div>
  <div class="col-span-12 lg:col-span-6 xl:col-span-4">
    <div class="card mb-0">
      <div class="flex justify-between mb-2">
        <div>
          <span class="block text-muted-color font-medium mb-2 text-xl">Logs Ingested</span>
          <div v-if="loading"><SpinningLoader /></div>
          <div v-if="stats" class="text-surface-900 dark:text-surface-0 font-medium text-3xl">{{ stats.logs }}</div>
        </div>
      </div>
    </div>
  </div>
  <div class="col-span-12 lg:col-span-6 xl:col-span-4">
    <div class="card mb-0">
      <div class="flex justify-between mb-2">
        <div>
          <span class="block text-muted-color font-medium mb-2 text-xl">Traces Ingested</span>
          <div v-if="loading"><SpinningLoader /></div>
          <div v-if="stats" class="text-surface-900 dark:text-surface-0 font-medium text-3xl">{{ stats.traces }}</div>
        </div>
      </div>
    </div>
  </div>
</template>
