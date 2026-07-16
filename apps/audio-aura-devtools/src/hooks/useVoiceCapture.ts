import { useCallback, useRef, useState } from 'react';

/**
 * useVoiceCapture — 浏览器端语音采集。协调两件事：
 *  - Web Speech (webkitSpeechRecognition, zh-CN, 连续+中间结果) 做实时 ASR + 静音断句；
 *  - 每句话并行用 MediaRecorder 录一段 Opus(webm)，随该句一起回调（音频溯源，best-effort）。
 * 每检测到一个"最终结果"(isFinal) 即触发 onTurn(一整句)。容器无麦克风时降级：supported/err 提示。
 */

export interface CapturedTurn {
  raw_text: string;
  audio_base64: string | null;
  audio_mime: string | null;
  start_time: number;
  end_time: number;
  duration_ms: number | null;
}

interface SpeechRecognitionLike {
  lang: string;
  continuous: boolean;
  interimResults: boolean;
  start(): void;
  stop(): void;
  onresult: ((e: SpeechResultEvent) => void) | null;
  onerror: ((e: { error?: string }) => void) | null;
  onend: (() => void) | null;
}
interface SpeechResultEvent {
  resultIndex: number;
  results: ArrayLike<ArrayLike<{ transcript: string }> & { isFinal: boolean }>;
}

function getSR(): (new () => SpeechRecognitionLike) | null {
  const w = window as unknown as {
    SpeechRecognition?: new () => SpeechRecognitionLike;
    webkitSpeechRecognition?: new () => SpeechRecognitionLike;
  };
  return w.SpeechRecognition ?? w.webkitSpeechRecognition ?? null;
}

function pickMime(): string {
  const cands = ['audio/webm;codecs=opus', 'audio/webm', 'audio/ogg;codecs=opus'];
  if (typeof MediaRecorder !== 'undefined' && MediaRecorder.isTypeSupported) {
    for (const c of cands) if (MediaRecorder.isTypeSupported(c)) return c;
  }
  return '';
}

function blobToBase64(blob: Blob): Promise<string> {
  return new Promise((resolve, reject) => {
    const r = new FileReader();
    r.onloadend = () => {
      const s = typeof r.result === 'string' ? r.result : '';
      const comma = s.indexOf(',');
      resolve(comma >= 0 ? s.slice(comma + 1) : '');
    };
    r.onerror = () => reject(r.error);
    r.readAsDataURL(blob);
  });
}

export function useVoiceCapture(onTurn: (t: CapturedTurn) => void) {
  const supported = getSR() !== null;
  const [listening, setListening] = useState(false);
  const [interim, setInterim] = useState('');
  const [error, setError] = useState<string | null>(null);

  const onTurnRef = useRef(onTurn);
  onTurnRef.current = onTurn;
  const recRef = useRef<SpeechRecognitionLike | null>(null);
  const streamRef = useRef<MediaStream | null>(null);
  const mrRef = useRef<MediaRecorder | null>(null);
  const chunksRef = useRef<Blob[]>([]);
  const segStartRef = useRef<number>(0);
  const activeRef = useRef(false);
  const listeningRef = useRef(false);

  const startSegment = useCallback(() => {
    const stream = streamRef.current;
    if (!stream) return;
    const mime = pickMime();
    try {
      const mr = mime ? new MediaRecorder(stream, { mimeType: mime }) : new MediaRecorder(stream);
      chunksRef.current = [];
      mr.ondataavailable = (e) => {
        if (e.data && e.data.size) chunksRef.current.push(e.data);
      };
      mr.start();
      mrRef.current = mr;
      segStartRef.current = Date.now();
    } catch {
      mrRef.current = null;
    }
  }, []);

  const stopSegment = useCallback((): Promise<{ base64: string; mime: string; durationMs: number } | null> => {
    const mr = mrRef.current;
    mrRef.current = null;
    if (!mr) return Promise.resolve(null);
    return new Promise((resolve) => {
      mr.onstop = () => {
        void (async () => {
          try {
            const mime = mr.mimeType || 'audio/webm';
            const blob = new Blob(chunksRef.current, { type: mime });
            const base64 = await blobToBase64(blob);
            resolve(base64 ? { base64, mime, durationMs: Date.now() - segStartRef.current } : null);
          } catch {
            resolve(null);
          }
        })();
      };
      try {
        mr.stop();
      } catch {
        resolve(null);
      }
    });
  }, []);

  const start = useCallback(async () => {
    const SR = getSR();
    if (!SR) {
      setError('浏览器不支持语音识别（需 Chrome 系浏览器 + 中文语言包）');
      return;
    }
    setError(null);
    try {
      streamRef.current = await navigator.mediaDevices.getUserMedia({ audio: true });
    } catch {
      streamRef.current = null; // 无麦克风：STT 可能仍可用，音频溯源关闭
    }

    const rec = new SR();
    rec.lang = 'zh-CN';
    rec.continuous = true;
    rec.interimResults = true;
    rec.onresult = (e) => {
      let interimText = '';
      let finalText = '';
      for (let i = e.resultIndex; i < e.results.length; i++) {
        const res = e.results[i];
        if (!res) continue;
        const alt = res[0];
        const txt = alt ? alt.transcript : '';
        if (res.isFinal) finalText += txt;
        else interimText += txt;
      }
      if (interimText && !activeRef.current) {
        activeRef.current = true;
        startSegment();
      }
      setInterim(interimText);
      if (finalText.trim()) {
        const raw = finalText.trim();
        const start_time = segStartRef.current || Date.now();
        activeRef.current = false;
        setInterim('');
        void stopSegment().then((audio) => {
          onTurnRef.current({
            raw_text: raw,
            audio_base64: audio?.base64 ?? null,
            audio_mime: audio?.mime ?? null,
            start_time,
            end_time: Date.now(),
            duration_ms: audio?.durationMs ?? null,
          });
        });
      }
    };
    rec.onerror = (e) => {
      if (e?.error && e.error !== 'no-speech' && e.error !== 'aborted') setError(String(e.error));
    };
    rec.onend = () => {
      if (listeningRef.current) {
        try {
          rec.start();
        } catch {
          /* already restarting */
        }
      }
    };
    recRef.current = rec;
    listeningRef.current = true;
    setListening(true);
    try {
      rec.start();
    } catch {
      /* already started */
    }
  }, [startSegment, stopSegment]);

  const stop = useCallback(() => {
    listeningRef.current = false;
    setListening(false);
    setInterim('');
    activeRef.current = false;
    try {
      recRef.current?.stop();
    } catch {
      /* */
    }
    void stopSegment();
    streamRef.current?.getTracks().forEach((t) => t.stop());
    streamRef.current = null;
  }, [stopSegment]);

  return { supported, listening, interim, error, start, stop };
}
