import { inspect } from 'node:util';
import type { LogLevel } from './levels.js';

/** A single log event, before it is rendered to a line. */
export interface LogRecord {
  level: LogLevel;
  /** Logger name / category, e.g. `scout` or `platform/desktop`. */
  name: string;
  /** The primary message string. */
  message: string;
  /** Trailing structured arguments, rendered alongside the message. */
  args: readonly unknown[];
  /** Epoch milliseconds. */
  timestamp: number;
}

/** Renders a {@link LogRecord} into the unified line format. */
export type LogFormatter = (rec: LogRecord) => string;

// Pre-formatted, width-padded (5) tags so the level column lines up.
const LEVEL_TAG: Record<LogLevel, string> = {
  trace: 'TRACE',
  debug: 'DEBUG',
  info: 'INFO ',
  warn: 'WARN ',
  error: 'ERROR',
  fatal: 'FATAL',
  // Never emitted, but kept total so the lookup is exhaustive.
  silent: '     ',
};

/** Render the trailing structured args the way `console` would (objects, errors, …). */
export function formatArgs(args: readonly unknown[]): string {
  if (args.length === 0) return '';
  const parts = args.map((a) => (typeof a === 'string' ? a : inspect(a, { depth: 6 })));
  return ' ' + parts.join(' ');
}

/** The unified line format: `ISO LEVEL [name] message <args>` (no args segment when empty). */
export const defaultFormatter: LogFormatter = (rec) => {
  const ts = new Date(rec.timestamp).toISOString();
  const tag = LEVEL_TAG[rec.level] ?? rec.level.toUpperCase().padEnd(5);
  return `${ts} ${tag} [${rec.name}] ${rec.message}${formatArgs(rec.args)}`;
};
