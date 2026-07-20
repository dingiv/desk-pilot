/// <reference types="vite/client" />

// API base for the devtools frontend.
// Dev: empty (Vite dev server proxies /api → aura-daemon). Prod: empty (daemon serves dist/).
// Override only for non-standard setups: VITE_API_BASE=http://host:port
export const API_BASE: string = import.meta.env.VITE_API_BASE ?? '';
