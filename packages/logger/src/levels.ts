/**
 * Severity-ordered log levels. `trace` is the most verbose, `fatal` the most severe;
 * `silent` is a threshold that suppresses everything.
 */
export type LogLevel = 'trace' | 'debug' | 'info' | 'warn' | 'error' | 'fatal' | 'silent';

/** All level names (lowest → highest severity), for parsing/validation. */
export const LOG_LEVELS: readonly LogLevel[] = [
  'trace',
  'debug',
  'info',
  'warn',
  'error',
  'fatal',
  'silent',
];

const ORDER: Record<LogLevel, number> = {
  trace: 10,
  debug: 20,
  info: 30,
  warn: 40,
  error: 50,
  fatal: 60,
  // `silent` ranks above everything so any comparison against it yields "do not emit".
  silent: Number.POSITIVE_INFINITY,
};

/** Numeric weight of a level (higher = more severe). */
export function levelValue(level: LogLevel): number {
  return ORDER[level] ?? Number.POSITIVE_INFINITY;
}

/** Would a message at `msg` severity pass the `active` threshold? `silent` never emits. */
export function enabledFor(active: LogLevel, msg: LogLevel): boolean {
  if (active === 'silent' || msg === 'silent') return false;
  return levelValue(msg) >= levelValue(active);
}

/** Parse a level name (case-insensitive); falls back when absent or unrecognized. */
export function parseLevel(raw: string | undefined, fallback: LogLevel): LogLevel {
  if (raw == null || raw === '') return fallback;
  const norm = raw.trim().toLowerCase();
  return (LOG_LEVELS as readonly string[]).includes(norm) ? (norm as LogLevel) : fallback;
}
