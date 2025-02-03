<script setup>
import { Handle } from '@vue-flow/core';

defineProps(['boundary', 'data', 'sourcePosition', 'targetPosition']);
</script>

<template>
  <VTooltip :boundary="boundary" placement="right" :triggers="['hover']" :distance="20" :shift="true" :flip="true">
    <div class="component-node">
      <Handle type="target" :position="targetPosition" connectable="false" />
      <div class="text-lg font-medium text-surface-700 dark:text-surface-200">{{ data.component.name }}</div>
      <div v-if="data.component.subname" class="text-sm font-normal text-surface-500 dark:text-surface-400">{{ data.component.subname }}</div>
      <Handle type="source" :position="sourcePosition" connectable="false" />
    </div>

    <template #popper>
      <div>
        <p class="text-md font-bold">{{ data.component.name }} {{ data.component.subname }}</p>
        <p class="text-sm"><span class="font-semibold">Type: </span> {{ data.component.type }}</p>
        <br />
        <p class="text-sm"><span class="font-semibold">Bytes in: </span> 35.5MB</p>
      </div>
    </template>
  </VTooltip>
</template>
