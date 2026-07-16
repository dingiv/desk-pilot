import { mkdir, readFile, readdir, unlink, writeFile } from 'node:fs/promises';
import { join } from 'node:path';
import type { MemoryManager, TaskSnapshot } from './types.js';

/**
 * File-based {@link MemoryManager}: one JSON file per task under `dir` (`<id>.json`).
 *
 * A task's history carries screenshot `Buffer`s (PNG). JSON has no native binary, so a
 * replacer/reviver pair encodes any `Buffer` as `{ __buf: true, b64 }` on write and restores it on
 * read — screenshots round-trip correctly and stay compact (base64) on disk.
 *
 * All disk I/O lives here (the impure edge); the agent core never touches the filesystem.
 */
export class FileMemoryManager implements MemoryManager {
  constructor(private readonly dir: string) {}

  async save(snapshot: TaskSnapshot): Promise<void> {
    await mkdir(this.dir, { recursive: true });
    await writeFile(this.path(snapshot.id), JSON.stringify(snapshot, bufferReplacer), 'utf8');
  }

  async load(id: string): Promise<TaskSnapshot | null> {
    try {
      const json = await readFile(this.path(id), 'utf8');
      return JSON.parse(json, bufferReviver) as TaskSnapshot;
    } catch (err) {
      if ((err as NodeJS.ErrnoException).code === 'ENOENT') return null;
      throw err;
    }
  }

  async list(): Promise<string[]> {
    let entries: string[];
    try {
      entries = await readdir(this.dir);
    } catch (err) {
      if ((err as NodeJS.ErrnoException).code === 'ENOENT') return [];
      throw err;
    }
    return entries.filter((f) => f.endsWith('.json')).map((f) => f.slice(0, -'.json'.length));
  }

  async remove(id: string): Promise<void> {
    try {
      await unlink(this.path(id));
    } catch (err) {
      if ((err as NodeJS.ErrnoException).code !== 'ENOENT') throw err;
    }
  }

  /** `<dir>/<id>.json`, rejecting ids that would escape `dir` (path traversal). */
  private path(id: string): string {
    if (id.includes('/') || id.includes('\\') || id.includes('..')) {
      throw new Error(`invalid task id (path traversal): ${id}`);
    }
    return join(this.dir, `${id}.json`);
  }
}

const BUF_TAG = '__buf';
interface EncodedBuffer {
  readonly __buf: true;
  readonly b64: string;
}

/**
 * JSON replacer. Node serializes a `Buffer` by calling its `toJSON` *before* the replacer runs, so
 * by the time we see it it's already `{ type: 'Buffer', data: number[] }`. Re-encode that as a
 * compact base64 sentinel so screenshots don't bloat the file.
 */
function bufferReplacer(_key: string, value: unknown): unknown {
  if (value && typeof value === 'object') {
    const v = value as { type?: unknown; data?: unknown };
    if (v.type === 'Buffer' && Array.isArray(v.data)) {
      return { __buf: true, b64: Buffer.from(v.data).toString('base64') } as EncodedBuffer;
    }
  }
  return value;
}

/** JSON reviver: restores `{ __buf: true, b64 }` back to a `Buffer`. */
function bufferReviver(_key: string, value: unknown): unknown {
  if (value && typeof value === 'object') {
    const v = value as { __buf?: unknown; b64?: unknown };
    if (v.__buf === true && typeof v.b64 === 'string') return Buffer.from(v.b64, 'base64');
  }
  return value;
}
