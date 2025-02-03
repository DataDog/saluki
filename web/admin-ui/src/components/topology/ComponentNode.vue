<script setup>
import { h, ref, Fragment } from 'vue';
import { Handle } from '@vue-flow/core';
import { autoPlacement, arrow, autoUpdate, flip, offset, shift, useFloating } from '@floating-ui/vue';
import VNodes from '@/components/VNodes.ts';

const props = defineProps(['data', 'sourcePosition', 'targetPosition']);

const label = typeof props.data.label !== 'string' && props.data.label ? h(props.data.label) : h(Fragment, [props.data.label]);

const reference = ref(null);
const floating = ref(null);
const floatingArrow = ref(null);
const tooltipVisible = ref(false);
const { floatingStyles, middlewareData } = useFloating(reference, floating, {
  whileElementsMounted: autoUpdate,
  middleware: [autoPlacement(), offset(6), flip(), shift({ padding: 5 }), arrow({ element: floatingArrow })]
});

function showTooltip() {
  tooltipVisible.value = true;
}

function hideTooltip() {
  tooltipVisible.value = false;
}
</script>

<template>
  <div>
    <div reference="reference" @mouseenter="showTooltip" @mouseleave="hideTooltip" class="component-node">
      <Handle type="target" :position="targetPosition" connectable="false" />
      <VNodes :node="label" />
      <Handle type="source" :position="sourcePosition" connectable="false" />
    </div>
    <div v-if="tooltipVisible" ref="floating" class="component-tooltip" :style="floatingStyles">
      Floating
      <div
        ref="floatingArrow"
        class="component-tooltip-arrow"
        :style="{
          position: 'absolute',
          left: middlewareData.arrow?.x != null ? `${middlewareData.arrow.x}px` : '',
          top: middlewareData.arrow?.y != null ? `${middlewareData.arrow.y}px` : ''
        }"></div>
    </div>
  </div>
</template>
