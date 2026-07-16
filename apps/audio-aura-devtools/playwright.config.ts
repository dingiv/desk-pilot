import { defineConfig } from '@playwright/test';

// Uses the system Chromium (no Playwright browser download). The webServer runs the app on its
// own port + a throwaway DB so the test is reproducible and isolated from any dev server.
export default defineConfig({
  testDir: './test',
  timeout: 120_000,
  expect: { timeout: 15_000 },
  fullyParallel: false,
  workers: 1,
  reporter: 'list',
  use: {
    baseURL: 'http://127.0.0.1:8092',
    launchOptions: {
      executablePath: process.env.PW_CHROMIUM ?? '/usr/bin/chromium',
      args: ['--no-sandbox', '--disable-setuid-sandbox', '--disable-dev-shm-usage'],
    },
  },
  webServer: {
    command:
      'NODE_ENV=development VOICE_DB_PATH=./data/test.db VOICE_AUDIO_DIR=./data/test-audio PORT=8092 ./node_modules/.bin/tsx src/server.ts',
    url: 'http://127.0.0.1:8092/api/health',
    timeout: 60_000,
    reuseExistingServer: false,
  },
});
