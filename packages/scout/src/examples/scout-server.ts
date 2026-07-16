/**
 * Start the Visual Scout server as a standalone process and keep it running.
 *
 *   pnpm --filter @vrover/scout example   # uses SCOUT_HOST / SCOUT_PORT (default 127.0.0.1:7878)
 *
 * The server needs no API key — it only exposes UI operations + grounding over a
 * custom TCP protocol. Each connecting client handshakes and gets its own session
 * (and its own backend terminal). Ctrl+C to stop.
 */
import { MultiScreenPlatform } from '@vrover/platform';
import { startScoutServer } from '@vrover/scout';

const server = await startScoutServer({
  backendFactory: () => new MultiScreenPlatform(),
  backendName: 'multi-screen',
  log: (m) => console.log(m),
});

console.log(`\nVisual Scout server ready at ${server.host}:${server.port} (TCP, custom binary protocol)`);
console.log('  each client handshakes → gets a fresh session + isolated backend terminal');
console.log('\nPress Ctrl+C to stop.');

process.on('SIGINT', async () => {
  console.log('\nShutting down…');
  await server.close();
  process.exit(0);
});
