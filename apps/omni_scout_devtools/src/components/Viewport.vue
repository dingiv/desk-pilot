<script setup lang="ts">
import { onMounted, ref, watch } from 'vue';
import type { UiElement } from '../types';

const props = defineProps<{
  image: ImageBitmap | null;
  elements: UiElement[];
  width: number;
  height: number;
}>();

const emit = defineEmits<{ click: [x: number, y: number] }>();

const canvas = ref<HTMLCanvasElement | null>(null);

function draw(): void {
  const c = canvas.value;
  if (!c) return;
  const w = props.width || props.image?.width || 640;
  const h = props.height || props.image?.height || 400;
  if (c.width !== w) c.width = w;
  if (c.height !== h) c.height = h;
  const ctx = c.getContext('2d');
  if (!ctx) return;
  ctx.clearRect(0, 0, c.width, c.height);
  if (props.image) ctx.drawImage(props.image, 0, 0);
  // Set-of-Mark overlay: numbered box per element (mark = index + 1, matching SoM order).
  ctx.font = 'bold 13px sans-serif';
  ctx.lineWidth = 2;
  props.elements.forEach((el, i) => {
    const b = el.bounds;
    if (!b) return;
    ctx.strokeStyle = '#1f6feb';
    ctx.strokeRect(b.x, b.y, b.width, b.height);
    ctx.fillStyle = '#1f6feb';
    ctx.fillRect(b.x, Math.max(0, b.y - 17), 22, 17);
    ctx.fillStyle = '#fff';
    ctx.fillText(String(i + 1), b.x + 4, Math.max(12, b.y - 4));
  });
}

watch(() => [props.image, props.elements, props.width, props.height], draw, { flush: 'post' });
onMounted(draw);

function onClick(e: MouseEvent): void {
  const c = canvas.value;
  if (!c) return;
  const rect = c.getBoundingClientRect();
  // Map the (CSS-scaled) click back to screenshot pixel coordinates.
  const x = Math.round(((e.clientX - rect.left) * c.width) / rect.width);
  const y = Math.round(((e.clientY - rect.top) * c.height) / rect.height);
  emit('click', x, y);
}
</script>

<template>
  <canvas ref="canvas" class="viewport" @click="onClick" />
</template>
