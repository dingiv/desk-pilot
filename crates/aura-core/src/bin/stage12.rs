//! stage12 — Stage1 (SenseVoice ASR) + Stage2 (Qwen3-1.7B 整流+意图路由, GPU) bench + report.
//!
//! Run via the cargo alias:  `cargo benchvoice`  (no args → scans the default material dirs)
//!   or  `cargo benchvoice <file.wav|dir> ...`    (add material = drop WAVs in a dir, or pass paths)
//! Writes a Markdown report to `bench/report-<epoch>.md` (+ `bench/latest.md`):
//! per-clip Stage1/Stage2 latency and both stages' recognition results.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use audio_aura_asr::onnx::{AsrConfig, OnnxRuntimeManager};
use audio_aura_asr::Asr;
use audio_aura_router::{parse_decision, RouterEngine};

const BASE: &str = "/workspaces/gui_agent/audio-aura/native";
const REPORT_DIR: &str = "/workspaces/gui_agent/audio-aura/bench";
const VAD_ENDPOINT_MS: f64 = 550.0; // fixed VAD silence wait before a turn commits (perceived latency)

fn ms(d: std::time::Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Read a PCM WAV → (mono i16 samples, sample_rate). Downmixes stereo.
fn read_wav(path: &Path) -> (Vec<i16>, u32) {
    let b = fs::read(path).expect("read wav");
    let fmt = b.windows(4).position(|w| w == b"fmt ").map(|p| p + 8).unwrap_or(12);
    let channels = u16::from_le_bytes([b[fmt + 2], b[fmt + 3]]);
    let sr = u32::from_le_bytes([b[fmt + 4], b[fmt + 5], b[fmt + 6], b[fmt + 7]]);
    let data = b.windows(4).position(|w| w == b"data").map(|p| p + 8).unwrap_or(44);
    let mut pcm: Vec<i16> = b[data..].chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect();
    if channels == 2 {
        pcm = pcm.chunks_exact(2).map(|c| ((c[0] as i32 + c[1] as i32) / 2) as i16).collect();
    }
    (pcm, sr)
}

fn scan_dir(dir: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().map(|x| x == "wav").unwrap_or(false) {
                out.push(p);
            }
        }
    }
}

/// Resolve the material list: given files/dirs, else the default material dirs (drop WAVs there to add).
fn collect_wavs(args: Vec<String>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if args.is_empty() {
        scan_dir(Path::new(&format!("{BASE}/models/testwavs")), &mut out);
        scan_dir(Path::new(&format!("{BASE}/models/sensevoice/test_wavs")), &mut out);
    } else {
        for a in args {
            let p = PathBuf::from(&a);
            if p.is_dir() {
                scan_dir(&p, &mut out);
            } else if p.extension().map(|x| x == "wav").unwrap_or(false) {
                out.push(p);
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

struct Row {
    name: String,
    dur: f64,
    asr_ms: f64,
    router_ms: f64,
    raw: String,
    intent: String,
    calibrated: String,
    reply: String,
}

fn main() -> anyhow::Result<()> {
    eprintln!("[load] Stage1 SenseVoice + Stage2 Qwen3-1.7B (GPU if built --features cuda) …");
    let mgr = OnnxRuntimeManager::builder()
        .asr(AsrConfig {
            model: format!("{BASE}/models/sensevoice/model.int8.onnx"),
            tokens: format!("{BASE}/models/sensevoice/tokens.txt"),
            ..Default::default()
        })
        .build()?;
    mgr.warm();
    let asr = mgr.asr().expect("asr configured");
    let router = RouterEngine::load(&format!("{BASE}/models"), "Qwen3-1.7B-Q8_0.gguf")?;
    let _ = router.route_blocking("你好", None, &[]); // warmup
    eprintln!("[ready]");

    let wavs = collect_wavs(std::env::args().skip(1).collect());
    if wavs.is_empty() {
        eprintln!("no .wav material found — pass files/dirs or drop WAVs in native/models/testwavs/");
        return Ok(());
    }
    eprintln!("[run] {} clips\n", wavs.len());

    let mut rows: Vec<Row> = Vec::new();
    for w in &wavs {
        let name = w.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let (pcm, sr) = read_wav(w);
        let dur = pcm.len() as f64 / sr as f64;

        let t = Instant::now();
        let raw = asr.recognize(&pcm, sr)?;
        let asr_ms = ms(t.elapsed());

        let t = Instant::now();
        let out = router.route_blocking(&raw, None, &[])?;
        let d = parse_decision(&out, &raw);
        let router_ms = ms(t.elapsed());

        println!(
            "● {name} ({dur:.1}s)  ASR {asr_ms:.0}ms | 路由 {router_ms:.0}ms | 合计 {:.0}ms",
            asr_ms + router_ms
        );
        println!("   Stage1: {raw}");
        println!("   Stage2: [{}] {}\n", d.intent, d.calibrated_text);
        rows.push(Row {
            name,
            dur,
            asr_ms,
            router_ms,
            raw,
            intent: d.intent,
            calibrated: d.calibrated_text,
            reply: d.reply,
        });
    }

    let report = build_report(&rows);
    fs::create_dir_all(REPORT_DIR).ok();
    let epoch = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let path = format!("{REPORT_DIR}/report-{epoch}.md");
    fs::write(&path, &report)?;
    fs::write(format!("{REPORT_DIR}/latest.md"), &report)?;
    println!("报告已写入 {path}  (+ bench/latest.md)");
    Ok(())
}

fn build_report(rows: &[Row]) -> String {
    let n = rows.len().max(1) as f64;
    let (sa, sr, sd) = rows.iter().fold((0.0, 0.0, 0.0), |(a, r, d), x| {
        (a + x.asr_ms, r + x.router_ms, d + x.dur)
    });
    let epoch = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);

    let mut s = String::new();
    s.push_str("# Stage1+2 语音识别基准报告\n\n");
    s.push_str(&format!("- 生成时间 (unix): `{epoch}`\n"));
    s.push_str("- Stage1 = SenseVoice int8 (sherpa-onnx) · Stage2 = Qwen3-1.7B Q8 整流+意图路由 (GPU sm_120)\n");
    s.push_str(&format!("- 用例: {} 条\n\n", rows.len()));

    // ── 延迟汇总 ──
    s.push_str("## 延迟汇总\n\n");
    s.push_str("| 指标 | 值 |\n|---|---|\n");
    s.push_str(&format!("| Stage1 ASR 平均 | {:.0} ms |\n", sa / n));
    s.push_str(&format!("| Stage2 路由 平均 | {:.0} ms |\n", sr / n));
    s.push_str(&format!("| 合计计算 平均 | {:.0} ms |\n", (sa + sr) / n));
    s.push_str(&format!("| ASR 实时倍数 | {:.0}× (RTF {:.3}) |\n", sd / (sa / 1000.0).max(1e-6), sa / 1000.0 / sd.max(1e-6)));
    s.push_str(&format!("| 端到端感知 (含 VAD 断句 {VAD_ENDPOINT_MS:.0}ms) | ~{:.0} ms/句 |\n\n", VAD_ENDPOINT_MS + (sa + sr) / n));

    // ── 逐条耗时 ──
    s.push_str("## 逐条耗时\n\n");
    s.push_str("| 素材 | 时长(s) | Stage1 ASR(ms) | Stage2 路由(ms) | 合计(ms) | ×实时 | 意图 |\n");
    s.push_str("|---|---:|---:|---:|---:|---:|---|\n");
    for r in rows {
        let total = r.asr_ms + r.router_ms;
        s.push_str(&format!(
            "| {} | {:.1} | {:.0} | {:.0} | {:.0} | {:.0}× | {} |\n",
            r.name, r.dur, r.asr_ms, r.router_ms, total, r.dur / (total / 1000.0).max(1e-6), r.intent
        ));
    }
    s.push('\n');

    // ── 识别结果（供人工判断准确度）──
    s.push_str("## 识别结果\n\n");
    for r in rows {
        s.push_str(&format!("### {}  ({:.1}s)\n\n", r.name, r.dur));
        s.push_str(&format!("- **Stage1 原文**: {}\n", r.raw));
        s.push_str(&format!("- **Stage2 整流** `[{}]`: {}\n", r.intent, r.calibrated));
        s.push_str(&format!("- **秘书回应**: {}\n\n", r.reply));
    }
    s
}
