<script setup lang="ts">
import { ref } from 'vue';
import { useDevtoolsStore } from '../composables/useDevtools';

const { sessions, activeId, live, busy, createSession, selectSession, deleteSession, toggleLive } =
  useDevtoolsStore();
const backend = ref('');
</script>

<template>
  <h3>Sessions</h3>
  <div class="row" style="margin-bottom: 8px">
    <select v-model="backend">
      <option value="">(default backend)</option>
      <option value="multi-screen">multi-screen</option>
      <option value="mock">mock</option>
      <option value="desktop">desktop</option>
    </select>
    <button class="primary" :disabled="busy" @click="createSession(backend || undefined)">New</button>
  </div>

  <div v-if="!sessions.length" style="color: #57606a">No sessions. Create one.</div>
  <div
    v-for="s in sessions"
    :key="s.id"
    class="session"
    :class="{ active: s.id === activeId }"
    @click="selectSession(s.id)"
  >
    <span>{{ s.id }}</span>
    <small>{{ s.backend }}</small>
    <button class="del" title="delete" @click.stop="deleteSession(s.id)">×</button>
  </div>

  <h3>Live</h3>
  <button :disabled="!activeId" @click="toggleLive()">
    {{ live ? 'Stop live' : 'Start live' }}
  </button>
</template>
