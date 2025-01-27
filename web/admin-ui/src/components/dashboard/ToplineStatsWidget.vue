<script setup>
import SpinningLoader from '../SpinningLoader.vue';
import { bytesToHumanFriendly } from '@/utils';
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
const failed = ref(false);

// watch the params of the route to fetch the data again
watch(() => route, fetchData, { immediate: true });

async function fetchData() {
  loading.value = true;
  stats.value = null;
  failed.value = false;

  try {
    const { pid, uptimeSecs, rssBytes } = await client.getProcessInformation();
    stats.value = { pid, uptimeSecs, rssBytes: Number(rssBytes), metrics: 285.4, logs: 0, traces: 0 };
  } catch (err) {
    failed.value = true;
    toast.add({ severity: 'error', summary: 'Failed to load some data.', detail: "Oops! We failed to load some metrics from ADP. We'll try again in a few seconds.", life: 5000 });
  } finally {
    loading.value = false;
  }
}
</script>

<template>
  <div class="col-span-12 lg:col-span-6 xl:col-span-3">
    <div class="card mb-0">
      <div class="flex justify-between mb-4">
        <div>
          <span class="block text-muted-color font-medium mb-4 text-xl">Memory</span>
          <div v-if="loading"><SpinningLoader /></div>
          <div v-if="failed"><i class="pi pi-question" style="font-size: 2rem"></i></div>
          <div v-if="stats" class="text-surface-900 dark:text-surface-0 font-medium text-4xl">{{ bytesToHumanFriendly(stats.rssBytes) }}</div>
        </div>
      </div>
    </div>
  </div>
  <div class="col-span-12 lg:col-span-6 xl:col-span-3">
    <div class="card mb-0">
      <div class="flex justify-between mb-4">
        <div>
          <span class="block text-muted-color font-medium mb-4 text-xl">Metrics Ingested</span>
          <div v-if="loading"><SpinningLoader /></div>
          <div v-if="failed"><i class="pi pi-question" style="font-size: 2rem"></i></div>
          <div v-if="stats" class="text-surface-900 dark:text-surface-0 font-medium text-4xl">{{ stats.metrics }} M</div>
        </div>
      </div>
    </div>
  </div>
  <div class="col-span-12 lg:col-span-6 xl:col-span-3">
    <div class="card mb-0">
      <div class="flex justify-between mb-4">
        <div>
          <span class="block text-muted-color font-medium mb-4 text-xl">Logs Ingested</span>
          <div v-if="loading"><SpinningLoader /></div>
          <div v-if="failed"><i class="pi pi-question" style="font-size: 2rem"></i></div>
          <div v-if="stats" class="text-surface-900 dark:text-surface-0 font-medium text-4xl">{{ stats.logs }}</div>
        </div>
      </div>
    </div>
  </div>
  <div class="col-span-12 lg:col-span-6 xl:col-span-3">
    <div class="card mb-0">
      <div class="flex justify-between mb-4">
        <div>
          <span class="block text-muted-color font-medium mb-4 text-xl">Traces Ingested</span>
          <div v-if="loading"><SpinningLoader /></div>
          <div v-if="failed"><i class="pi pi-question" style="font-size: 2rem"></i></div>
          <div v-if="stats" class="text-surface-900 dark:text-surface-0 font-medium text-4xl">{{ stats.traces }}</div>
        </div>
      </div>
    </div>
  </div>
</template>
