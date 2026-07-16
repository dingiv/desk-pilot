/// <reference types="vite/client" />

// `import.meta.env.DEV` / `.MODE` let the SPA distinguish dev (Vite middleware) from prod
// (built static bundle). Vite statically replaces these at build time — server code can't read them.
export {}