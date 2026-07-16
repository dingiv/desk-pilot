import { defineConfig } from 'vite';
import vue from '@vitejs/plugin-vue';

/**
 * The Scout DevTools UI is a client-rendered Vue 3 SPA. Browsers can't speak scout's raw
 * TCP protocol, so in dev (`vite`) and preview (`vite preview`) we proxy `/api` to the
 * scout **devtools service** (the in-process HTTP/SSE surface on the scout server). The SPA
 * therefore calls only relative `/api/...` URLs — single origin, no CORS, and SSE streams
 * cleanly. Point the proxy at a non-default scout with:
 *
 *   SCOUT_DEVTOOLS_API=http://host:port pnpm --filter @vrover/omni-scout-devtools dev
 */
const scoutApi = process.env.SCOUT_DEVTOOLS_API ?? 'http://127.0.0.1:7881';

const proxy = {
  '/api': {
    target: scoutApi,
    changeOrigin: true,
    // SSE (`/api/sessions/:id/stream`) must stream without buffering.
    ws: false,
  },
};

export default defineConfig({
  plugins: [vue()],
  server: { port: 9090, proxy },
  preview: { port: 9090, proxy },
});
