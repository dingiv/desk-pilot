import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

// Dev: `vite` serves the SPA and proxies `/api/*` to the Rust `aura-daemon` (default
// http://127.0.0.1:9091; override with AURA_DAEMON). The TS backend in src/ is dead code —
// NOT started. Prod: `vite build` → `dist/`, which the daemon itself serves (same origin), so
// API_BASE stays empty in both modes and VITE_API_BASE is no longer needed.
const DAEMON = process.env.AURA_DAEMON ?? 'http://127.0.0.1:9091';

export default defineConfig({
  plugins: [react()],
  server: {
    // Proxy REST + SSE (`/api/stream`) to the daemon. http-proxy streams responses, so SSE works.
    proxy: {
      '/api': { target: DAEMON, changeOrigin: true },
    },
    // Don't watch the heavy non-frontend dirs — Rust target/ alone has 10k+ files and blows the
    // kernel inotify limit (ENOSPC). Also raise fs.inotify.max_user_watches on the host.
    watch: {
      ignored: [
        '**/target/**',
        '**/node_modules/**',
        '**/native/**',
        '**/data/**',
        '**/bench/**',
        '**/dist/**',
        '**/web-dist/**',
        '**/src/**', // dead TS backend
        '**/.cargo/**',
        '**/.git/**',
      ],
    },
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
  },
});
