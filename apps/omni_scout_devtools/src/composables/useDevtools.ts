import { inject, provide, ref, shallowRef, type InjectionKey } from 'vue';
import * as api from '../api';
import type { DevtoolsConfig, Frame, SessionInfo, UiElement } from '../types';

/**
 * Reactive store backing the DevTools UI. Owns the scout devtools connection: sessions,
 * the active session's capture + SoM elements, the live SSE toggle, and server config.
 * Provided at the app root (`App.vue`) and injected by the panels, so components stay dumb.
 */
export function useDevtools() {
  const sessions = ref<SessionInfo[]>([]);
  const activeId = ref<string | null>(null);
  const elements = ref<UiElement[]>([]);
  // ImageBitmap is opaque — use shallowRef so Vue doesn't try to proxy its internals.
  const image = shallowRef<ImageBitmap | null>(null);
  const width = ref(640);
  const height = ref(400);
  const live = ref(false);
  const config = ref<DevtoolsConfig | null>(null);
  const status = ref('disconnected');
  const busy = ref(false);

  let closeStream: (() => void) | null = null;

  function fail(e: unknown): void {
    status.value = 'error: ' + (e instanceof Error ? e.message : String(e));
  }

  async function connect(): Promise<void> {
    try {
      const h = await api.getHealth();
      status.value = `connected · ${h.sessions} session(s)`;
      sessions.value = await api.listSessions();
      config.value = await api.getConfig();
    } catch (e) {
      fail(e);
    }
  }

  async function refreshSessions(): Promise<void> {
    sessions.value = await api.listSessions();
  }

  async function createSession(backend?: string): Promise<void> {
    busy.value = true;
    try {
      const s = await api.createSession(backend);
      await refreshSessions();
      await selectSession(s.id);
    } catch (e) {
      fail(e);
    } finally {
      busy.value = false;
    }
  }

  async function selectSession(id: string): Promise<void> {
    activeId.value = id;
    stopLive();
    await refresh();
    await api.putConfig({ activeSessionId: id }).catch(() => undefined);
  }

  async function deleteSession(id: string): Promise<void> {
    await api.deleteSession(id);
    if (activeId.value === id) {
      activeId.value = null;
      stopLive();
      image.value = null;
      elements.value = [];
    }
    await refreshSessions();
  }

  async function refresh(): Promise<void> {
    if (!activeId.value) return;
    busy.value = true;
    try {
      const blob = await api.capture(activeId.value);
      const bmp = await createImageBitmap(blob);
      image.value = bmp;
      width.value = bmp.width;
      height.value = bmp.height;
      elements.value = await api.getElements(activeId.value);
      status.value = `session ${activeId.value} · ${elements.value.length} elements`;
    } catch (e) {
      fail(e);
    } finally {
      busy.value = false;
    }
  }

  async function clickAt(x: number, y: number): Promise<void> {
    if (!activeId.value) return;
    await api.action(activeId.value, 'click', { x, y });
    if (!live.value) await refresh();
  }

  async function clickElement(index: number): Promise<void> {
    const el = elements.value[index];
    if (!el?.bounds) return;
    await clickAt(Math.round(el.bounds.x + el.bounds.width / 2), Math.round(el.bounds.y + el.bounds.height / 2));
  }

  async function doType(text: string): Promise<void> {
    if (!activeId.value) return;
    await api.action(activeId.value, 'type', { text });
    if (!live.value) await refresh();
  }

  async function doKeypress(keys: string): Promise<void> {
    if (!activeId.value) return;
    await api.action(activeId.value, 'keypress', { keys });
    if (!live.value) await refresh();
  }

  async function doScroll(direction: 'up' | 'down'): Promise<void> {
    if (!activeId.value) return;
    await api.action(activeId.value, 'scroll', {
      x: Math.round(width.value / 2),
      y: Math.round(height.value / 2),
      direction,
    });
    if (!live.value) await refresh();
  }

  function toggleLive(): void {
    live.value ? stopLive() : startLive();
  }

  function startLive(): void {
    if (!activeId.value) return;
    live.value = true;
    closeStream = api.streamSession(
      activeId.value,
      async (frame: Frame) => {
        const bytes = Uint8Array.from(atob(frame.png), (c) => c.charCodeAt(0));
        image.value = await createImageBitmap(new Blob([bytes], { type: 'image/png' }));
        width.value = frame.width;
        height.value = frame.height;
        elements.value = frame.elements ?? [];
      },
      () => {
        status.value = 'live stream error';
      },
    );
    status.value = 'live';
  }

  function stopLive(): void {
    if (closeStream) {
      closeStream();
      closeStream = null;
    }
    live.value = false;
  }

  async function setCaptureInterval(ms: number): Promise<void> {
    config.value = await api.putConfig({ captureIntervalMs: ms });
    status.value = `capture interval = ${ms}ms`;
  }

  return {
    sessions,
    activeId,
    elements,
    image,
    width,
    height,
    live,
    config,
    status,
    busy,
    connect,
    refreshSessions,
    createSession,
    selectSession,
    deleteSession,
    refresh,
    clickAt,
    clickElement,
    doType,
    doKeypress,
    doScroll,
    toggleLive,
    setCaptureInterval,
  };
}

export type DevtoolsStore = ReturnType<typeof useDevtools>;

export const devtoolsKey: InjectionKey<DevtoolsStore> = Symbol('devtools');

/** Convenience for child components: inject the store or throw if missing. */
export function useDevtoolsStore(): DevtoolsStore {
  const store = inject(devtoolsKey);
  if (!store) throw new Error('DevtoolsStore not provided');
  return store;
}

/** Used once at the app root. */
export function provideDevtools(): DevtoolsStore {
  const store = useDevtools();
  provide(devtoolsKey, store);
  return store;
}
