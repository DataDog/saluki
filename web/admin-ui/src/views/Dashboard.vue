<script setup>
import ToplineStatsWidget from '@/components/dashboard/ToplineStatsWidget.vue';
import TopologyMap from '@/components/dashboard/TopologyMap.vue';
</script>

<template>
  <div class="grid grid-cols-12 gap-8">
    <ToplineStatsWidget />
    <TopologyMap />
  </div>
</template>
