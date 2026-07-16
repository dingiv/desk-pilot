import type { LogLevel } from './levels.js';
import type { LogRecord } from './format.js';

/**
 * Where a formatted log line goes — the *replaceable output target*. Receives both the structured
 * record and the pre-rendered line, so a sink may choose structured (`rec`) or textual (`formatted`)
 * handling (e.g. a JSON sink uses `rec`, the console sink prints `formatted`).
 */
export type LogSink = (rec: LogRecord, formatted: string) => void;

const CONSOLE_METHOD: Record<LogLevel, 'debug' | 'info' | 'warn' | 'error' | 'log'> = {
  trace: 'debug',
  debug: 'debug',
  info: 'info',
  warn: 'warn',
  error: 'error',
  fatal: 'error',
  // Never invoked (silent never emits), kept total for an exhaustive map.
  silent: 'log',
};

/** Default sink: routes by severity to the matching `console` method. */
export const consoleSink: LogSink = (rec, formatted) => {
  const method = CONSOLE_METHOD[rec.level] ?? 'log';
  console[method](formatted);
};

/** A capturing sink for tests / in-memory consumers. Pushes the structured record into `into`. */
export function arraySink(into: LogRecord[] = []): { sink: LogSink; records: LogRecord[] } {
  return {
    records: into,
    sink: (rec) => {
      into.push(rec);
    },
  };
}
