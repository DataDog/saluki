<script setup>
import SpinningLoader from '../SpinningLoader.vue';

import { ref, watch } from 'vue';
import { useRoute } from 'vue-router';
import { useToast } from 'primevue/usetoast';

const route = useRoute();
const toast = useToast();

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
    await new Promise((r) => setTimeout(r, 2000));
    stats.value = { memory: 152, metrics: 285.4, logs: 0, traces: 0 };
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
          <div v-if="stats" class="text-surface-900 dark:text-surface-0 font-medium text-4xl">{{ stats.memory }} MB</div>
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
