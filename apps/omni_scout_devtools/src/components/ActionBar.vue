<script setup lang="ts">
import { ref } from 'vue';
import { useDevtoolsStore } from '../composables/useDevtools';

const { activeId, busy, elements, doType, doKeypress, doScroll, clickElement, refresh } =
  useDevtoolsStore();
const text = ref('');
const keys = ref('');
</script>

<template>
  <h3>Actions</h3>
  <label>Type text</label>
  <div class="row" style="margin-bottom: 6px">
    <input v-model="text" placeholder="text" @keyup.enter="doType(text)" />
    <button :disabled="!activeId || busy" @click="doType(text)">Type</button>
  </div>

  <label>Keypress</label>
  <div class="row" style="margin-bottom: 6px">
    <input v-model="keys" placeholder="e.g. Return" @keyup.enter="doKeypress(keys)" />
    <button :disabled="!activeId || busy" @click="doKeypress(keys)">Send</button>
  </div>

  <div class="row" style="margin-bottom: 6px">
    <button :disabled="!activeId || busy" @click="doScroll('up')">Scroll up</button>
    <button :disabled="!activeId || busy" @click="doScroll('down')">Scroll down</button>
  </div>

  <button class="primary" style="width: 100%" :disabled="!activeId || busy" @click="refresh()">
    Refresh capture
  </button>

  <h3>Elements <span style="color: #57606a; font-weight: normal">({{ elements.length }})</span></h3>
  <div v-for="(el, i) in elements" :key="i" class="el-row" @click="clickElement(i)">
    <span class="n">{{ i + 1 }}</span>
    <span><b>{{ el.role }}</b> {{ el.label }}</span>
  </div>
</template>
