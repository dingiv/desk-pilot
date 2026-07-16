// API base for the devtools frontend. Empty = same-origin (the TS backend on :8080).
// Set VITE_API_BASE=http://127.0.0.1:9090 to point the devtools at the Rust daemon (voice-core).
export const API_BASE: string = import.meta.env.VITE_API_BASE ?? '';
