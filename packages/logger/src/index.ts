/**
 * @vrover/logger — the shared, zero-dependency Logger for every VRover module. Unified line format,
 * a swappable output target ({@link LogSink}), and readable/settable levels with live global
 * defaults. Importable by all packages including the leaves (platform/som/llm/…).
 */
export type { LogLevel } from './levels.js';
export { LOG_LEVELS, levelValue, enabledFor, parseLevel } from './levels.js';

export type { LogRecord, LogFormatter } from './format.js';
export { defaultFormatter, formatArgs } from './format.js';

export type { LogSink } from './sink.js';
export { consoleSink, arraySink } from './sink.js';

export type { Logger, LoggerOptions } from './logger.js';
export {
  createLogger,
  getRootLogger,
  getDefaultLevel,
  setDefaultLevel,
  getDefaultSink,
  setDefaultSink,
} from './logger.js';
