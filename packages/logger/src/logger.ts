import { enabledFor, parseLevel, type LogLevel } from './levels.js';
import { defaultFormatter, type LogFormatter, type LogRecord } from './format.js';
import { consoleSink, type LogSink } from './sink.js';

export interface LoggerOptions {
  /** Per-logger level override; omit to follow the live global default. */
  level?: LogLevel;
  /** Per-logger sink override; omit to follow the live global default. */
  sink?: LogSink;
  /** Per-logger formatter override; defaults to the unified {@link defaultFormatter}. */
  formatter?: LogFormatter;
}

export interface Logger {
  readonly name: string;
  /** Effective severity threshold (override, else the live global default). */
  getLevel(): LogLevel;
  /** Set a per-logger level override. */
  setLevel(level: LogLevel): void;
  /** Effective sink (override, else the live global default). */
  getSink(): LogSink;
  /** Set a per-logger sink override. */
  setSink(sink: LogSink): void;
  /** Would a message at `level` be emitted right now? */
  isLevelActive(level: LogLevel): boolean;
  trace(message: string, ...args: unknown[]): void;
  debug(message: string, ...args: unknown[]): void;
  info(message: string, ...args: unknown[]): void;
  warn(message: string, ...args: unknown[]): void;
  error(message: string, ...args: unknown[]): void;
  fatal(message: string, ...args: unknown[]): void;
  /** A nested logger with name `${this.name}/${name}`, sharing this logger's sink/formatter. */
  child(name: string): Logger;
}

// --- live global defaults ---
// Holders only — NOTHING is computed or opened at module load: no env read, no instance
// creation, no file open. Both defaults resolve lazily on first use (the level from `LOG_LEVEL`,
// like `loadConfig`), so importing this module can never crash its host before it has booted.
let defaultLevel: LogLevel | undefined; // undefined = not yet resolved from LOG_LEVEL
let defaultSink: LogSink | undefined; // undefined = not yet resolved (→ consoleSink)

/** Resolve the global default level from `LOG_LEVEL` on first use and cache it. */
function resolveDefaultLevel(): LogLevel {
  return (defaultLevel ??= parseLevel(process.env['LOG_LEVEL'], 'info'));
}

/** Resolve the global default sink on first use (→ consoleSink) and cache it. */
function resolveDefaultSink(): LogSink {
  return (defaultSink ??= consoleSink);
}

/** The current global default level (used by loggers without an explicit level). */
export function getDefaultLevel(): LogLevel {
  return resolveDefaultLevel();
}

/** Reconfigure the global default level — applies live to loggers without an explicit level. */
export function setDefaultLevel(level: LogLevel): void {
  defaultLevel = level;
}

/** The current global default sink. */
export function getDefaultSink(): LogSink {
  return resolveDefaultSink();
}

/** Reconfigure the global default sink — applies live to loggers without an explicit sink. */
export function setDefaultSink(sink: LogSink): void {
  defaultSink = sink;
}

function makeLogger(name: string, opts: LoggerOptions): Logger {
  const formatter = opts.formatter ?? defaultFormatter;
  // `undefined` means "follow the live global default at emit time".
  let levelOverride: LogLevel | undefined = opts.level;
  let sinkOverride: LogSink | undefined = opts.sink;

  const effectiveLevel = (): LogLevel => levelOverride ?? resolveDefaultLevel();
  const effectiveSink = (): LogSink => sinkOverride ?? resolveDefaultSink();

  const emit = (level: LogLevel, message: string, args: unknown[]): void => {
    if (!enabledFor(effectiveLevel(), level)) return;
    const record: LogRecord = { level, name, message, args, timestamp: Date.now() };
    const sink = effectiveSink();
    sink(record, formatter(record));
  };

  return {
    name,
    getLevel: effectiveLevel,
    setLevel: (level) => {
      levelOverride = level;
    },
    getSink: effectiveSink,
    setSink: (sink) => {
      sinkOverride = sink;
    },
    isLevelActive: (level) => enabledFor(effectiveLevel(), level),
    trace: (m, ...a) => emit('trace', m, a),
    debug: (m, ...a) => emit('debug', m, a),
    info: (m, ...a) => emit('info', m, a),
    warn: (m, ...a) => emit('warn', m, a),
    error: (m, ...a) => emit('error', m, a),
    fatal: (m, ...a) => emit('fatal', m, a),
    // Child inherits the parent's formatter + current overrides (snapshot); its level is then
    // resolved independently — later changes to the parent do not propagate.
    child: (childName) =>
      makeLogger(`${name}/${childName}`, { formatter, level: levelOverride, sink: sinkOverride }),
  };
}

/**
 * Create a named Logger. Without explicit `level`/`sink`, it follows the live global defaults
 * (env-derived level + console sink) — so {@link setDefaultLevel}/{@link setDefaultSink}
 * reconfigure it on the fly.
 */
export function createLogger(name: string, options: LoggerOptions = {}): Logger {
  return makeLogger(name, options);
}

let _rootLogger: Logger | undefined;

/**
 * Lazily-created, cached top-level logger (`vrover`). Created on first call rather than at module
 * load — same lifecycle as `loadConfig`, so importing `@vrover/logger` performs no work.
 */
export function getRootLogger(): Logger {
  return (_rootLogger ??= createLogger('vrover'));
}
