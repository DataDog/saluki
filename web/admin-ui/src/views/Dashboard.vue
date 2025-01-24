<script setup>
import ToplineStatsWidget from '@/components/dashboard/ToplineStatsWidget.vue';
import IngestThroughput from '@/components/dashboard/IngestThroughput.vue';
</script>

<template>
    <div class="grid grid-cols-12 gap-8">
        <ToplineStatsWidget />
        <IngestThroughput />
    </div>
</template>
