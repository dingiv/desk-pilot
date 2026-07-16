import { useState, useRef, useCallback } from 'react';
import './App.css';

interface StepAction {
  name: string;
  input: Record<string, unknown>;
  result: string;
}
interface AgentStep {
  index: number;
  elements: number;
  actions: StepAction[];
}
interface TaskResult {
  status: 'success' | 'max_steps' | 'error' | 'paused';
  summary?: string;
  steps: AgentStep[];
  error?: string;
}

interface TaskCapture {
  step: number;
  dataUrl: string;
}

interface TaskEvent {
  type: 'step' | 'log' | 'capture' | 'done' | 'error' | 'paused';
  step?: AgentStep;
  text?: string;
  capture?: TaskCapture;
  result?: TaskResult;
}

// Vite statically replaces `import.meta.env` at build time.
const MODE = import.meta.env.MODE as string;
const IS_DEV = Boolean(import.meta.env.DEV);

function formatStep(s: AgentStep): string {
  const acts = s.actions.map((a) => `${a.name} ${JSON.stringify(a.input)} → ${a.result}`);
  return `step ${s.index}  (${s.elements} elements)\n${acts.map((a) => `  ${a}`).join('\n') || '  (no action — model only spoke)'}`;
}

function formatResult(r: TaskResult): string {
  const lines: string[] = [];
  lines.push(`status: ${r.status}  |  steps: ${r.steps.length}`);
  if (r.summary) lines.push(`summary: ${r.summary}`);
  if (r.error) lines.push(`error: ${r.error}`);
  return lines.join('\n');
}

export function App() {
  const [task, setTask] = useState('search a funny news in chrome for me');
  const [running, setRunning] = useState(false);
  const [status, setStatus] = useState('');
  const [statusCls, setStatusCls] = useState('');
  const [streamSteps, setStreamSteps] = useState<AgentStep[]>([]);
  const [captures, setCaptures] = useState<TaskCapture[]>([]);
  const [log, setLog] = useState<string[]>([]);
  const [result, setResult] = useState<TaskResult | null>(null);
  const [errorMsg, setErrorMsg] = useState('');
  const logEndRef = useRef<HTMLDivElement>(null);
  const abortRef = useRef<AbortController | null>(null);

  function setStatusLine(text: string, cls = ''): void {
    setStatus(text);
    setStatusCls(cls);
  }

  const handleKeyDown = useCallback(
    (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
      if (e.key === 'Enter' && (e.metaKey || e.ctrlKey) && !running) {
        e.preventDefault();
        void run();
      }
    },
    [running],
  );

  async function run(): Promise<void> {
    const t = task.trim();
    if (!t) {
      setStatusLine('enter a task first', 'bad');
      return;
    }
    // Abort any in-flight stream.
    abortRef.current?.abort();

    setRunning(true);
    setStreamSteps([]);
    setCaptures([]);
    setLog([]);
    setResult(null);
    setErrorMsg('');
    setStatusLine('connecting…');

    const controller = new AbortController();
    abortRef.current = controller;

    try {
      const url = `/api/run/stream?task=${encodeURIComponent(t)}`;
      const res = await fetch(url, { signal: controller.signal });

      if (!res.ok) {
        const body = await res.json().catch(() => ({})) as { error?: string };
        throw new Error(body.error || `HTTP ${res.status}`);
      }
      if (!res.body) throw new Error('no response body');

      setStatusLine('running…');
      const reader = res.body.getReader();
      const decoder = new TextDecoder();
      let buf = '';

      while (true) {
        const { done, value } = await reader.read();
        if (done) break;

        buf += decoder.decode(value, { stream: true });
        // SSE frames are separated by double newlines.
        const parts = buf.split('\n\n');
        buf = parts.pop() ?? ''; // keep the incomplete last frame

        for (const part of parts) {
          const dataLine = part
            .split('\n')
            .find((l) => l.startsWith('data: '))
            ?.slice(6);
          if (!dataLine) continue;
          try {
            const ev = JSON.parse(dataLine) as TaskEvent;
            applyEvent(ev);
          } catch {
            // skip malformed frames
          }
        }
      }
    } catch (err) {
      if ((err as Error).name === 'AbortError') return;
      setErrorMsg(err instanceof Error ? err.message : String(err));
      setStatusLine('connection error', 'bad');
    } finally {
      setRunning(false);
      abortRef.current = null;
    }
  }

  function applyEvent(ev: TaskEvent): void {
    switch (ev.type) {
      case 'log':
        if (ev.text) setLog((prev) => [...prev, ev.text!]);
        break;
      case 'capture':
        if (ev.capture) {
          setCaptures((prev) => [...prev, ev.capture!]);
          setStatusLine(`captured step ${ev.capture!.step}…`);
        }
        break;
      case 'step':
        if (ev.step) {
          setStreamSteps((prev) => [...prev, ev.step!]);
          setStatusLine(`step ${ev.step!.index}…`);
        }
        break;
      case 'done':
        if (ev.result) {
          setResult(ev.result);
          setStatusLine(
            ev.result.status === 'success' ? 'done ✓' : 'finished',
            ev.result.status === 'success' ? 'ok' : 'bad',
          );
        }
        break;
      case 'error':
        if (ev.result) {
          setResult(ev.result);
          setErrorMsg(ev.result.error ?? 'unknown error');
          setStatusLine('error', 'bad');
        }
        break;
      case 'paused':
        if (ev.result) {
          setResult(ev.result);
          setStatusLine('paused', '');
        }
        break;
    }
    // Auto-scroll the log.
    logEndRef.current?.scrollIntoView({ behavior: 'smooth' });
  }

  function cancel(): void {
    abortRef.current?.abort();
    abortRef.current = null;
    setRunning(false);
    setStatusLine('cancelled');
  }

  return (
    <>
      <header>
        <h1>VRover GUI Agent</h1>
        <span
          className={`badge ${IS_DEV ? 'dev' : 'prod'}`}
          title={`import.meta.env.MODE = ${MODE}`}
        >
          {MODE}
        </span>
      </header>

      <p className="meta">
        A visual agent loop — observe → think → act against an in-memory mock platform.
        {IS_DEV && ' Streams step-by-step via SSE.'}
      </p>

      <textarea
        value={task}
        onChange={(e) => setTask(e.target.value)}
        onKeyDown={handleKeyDown}
        placeholder={`Describe the task, e.g.\nLog in with username 'admin' and password 'hunter2', then submit.\n\n⌘/Ctrl+Enter to run`}
        disabled={running}
      />
      <div className="row">
        {running ? (
          <button className="cancel" onClick={cancel}>
            Cancel
          </button>
        ) : (
          <button onClick={run}>Run</button>
        )}
        <span className={`status ${statusCls}`}>
          {running && <span className="spinner" />}
          {status}
        </span>
      </div>

      {streamSteps.length > 0 && (
        <section>
          <h2>Steps ({streamSteps.length})</h2>
          {streamSteps.map((s) => (
            <pre key={s.index} className="step">
              {formatStep(s)}
            </pre>
          ))}
        </section>
      )}

      {captures.length > 0 && (
        <section>
          <h2>Captures ({captures.length})</h2>
          <div className="captures">
            {captures.map((c) => (
              <figure key={c.step} className="capture">
                <img src={c.dataUrl} alt={`screenshot step ${c.step}`} loading="lazy" />
                <figcaption>step {c.step}</figcaption>
              </figure>
            ))}
          </div>
        </section>
      )}

      <section>
        <h2>Progress log</h2>
        <div className="log-wrap">
          <pre>{log.join('\n')}</pre>
          <div ref={logEndRef} />
        </div>
      </section>

      {result && (
        <section>
          <h2>Result</h2>
          <pre className={result.status === 'success' ? 'ok' : result.status === 'error' ? 'bad' : ''}>
            {formatResult(result)}
          </pre>
        </section>
      )}
      {errorMsg && !result && (
        <section>
          <h2>Error</h2>
          <pre className="bad">{errorMsg}</pre>
        </section>
      )}
    </>
  );
}
