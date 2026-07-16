<script setup lang="ts">
import { onMounted } from 'vue';
import { provideDevtools } from './composables/useDevtools';
import SessionPanel from './components/SessionPanel.vue';
import Viewport from './components/Viewport.vue';
import ActionBar from './components/ActionBar.vue';
import ConfigPanel from './components/ConfigPanel.vue';

const { status, image, elements, width, height, clickAt, connect } = provideDevtools();

onMounted(() => {
  void connect();
});
</script>

<template>
  <header>
    <strong>Scout DevTools</strong>
    <span style="color: #8b949e">devtools service via /api proxy → scout</span>
  </header>

  <div class="cols">
    <aside>
      <SessionPanel />
      <ConfigPanel />
    </aside>

    <main class="viewport-wrap">
      <Viewport
        :image="image"
        :elements="elements"
        :width="width"
        :height="height"
        @click="clickAt"
      />
    </main>

    <aside class="right">
      <ActionBar />
    </aside>
  </div>

  <div id="status" :class="{ err: status.startsWith('error') }">{{ status }}</div>
</template>
