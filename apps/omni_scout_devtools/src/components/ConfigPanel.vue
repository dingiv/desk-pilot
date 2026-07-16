<script setup lang="ts">
import { ref, watch } from 'vue';
import { useDevtoolsStore } from '../composables/useDevtools';

const { config, setCaptureInterval } = useDevtoolsStore();
const interval = ref(1000);

// Track the server's interval once config loads.
watch(
  () => config.value?.captureIntervalMs,
  (ms) => {
    if (typeof ms === 'number') interval.value = ms;
  },
);
</script>

<template>
  <h3>Capture stream</h3>
  <label>SSE interval (ms)</label>
  <div class="row">
    <input v-model.number="interval" type="number" min="50" step="50" />
    <button @click="setCaptureInterval(interval)">Set</button>
  </div>
</template>
