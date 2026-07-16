//! ingest.rs — Stage1 audio ingest. Streams omni-scout `GET /audio` (chunked 16k mono S16LE),
//! de-chunks, re-frames to 20ms, runs VAD+ASR (audio-aura-asr). Each finalized utterance drives the
//! existing Stage2 pipeline (`handle_turn`), so ASR text flows through 整流+路由 like an injected turn.
//! Runs on its own std::thread (blocking I/O + ONNX); `rt` schedules the async pipeline call.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::Handle;
use audio_aura_asr::sherpa::SherpaAsr;
use audio_aura_asr::{SpeechEventKind, VadConfig, VadSegmenter};

use crate::{pipeline, AppState};

pub fn run_ingest(
    state: Arc<AppState>,
    rt: Handle,
    scout_addr: String,
    model: String,
    tokens: String,
) -> anyhow::Result<()> {
    let asr = SherpaAsr::new(&model, &tokens)?;
    eprintln!("[ingest] SenseVoice ready — connecting to omni-scout {scout_addr}/audio");
    let mut seg = VadSegmenter::new(VadConfig::default(), asr);
    let mut byte_buf: Vec<u8> = Vec::with_capacity(4096);
    let mut frame: Vec<i16> = Vec::with_capacity(320);

    loop {
        let r = connect_stream(&scout_addr, &mut |bytes: &[u8]| {
            byte_buf.extend_from_slice(bytes);
            // drain complete i16 samples, emit 20ms (320-sample) frames to the segmenter
            let pairs = byte_buf.len() / 2;
            for i in 0..pairs {
                let s = i16::from_le_bytes([byte_buf[i * 2], byte_buf[i * 2 + 1]]);
                frame.push(s);
                if frame.len() == 320 {
                    for ev in seg.push_frame(&frame) {
                        if ev.kind == SpeechEventKind::Final {
                            if let Some(text) = ev.text.as_ref().map(|t| t.trim().to_string()) {
                                if !text.is_empty() {
                                    eprintln!("[ingest] utterance ({:.1}s): {text}", ev.duration_ms / 1000.0);
                                    dispatch_turn(&state, &rt, text);
                                }
                            }
                        }
                    }
                    frame.clear();
                }
            }
            byte_buf.drain(0..pairs * 2);
        });
        if let Err(e) = r {
            eprintln!("[ingest] audio stream ended ({e}); reconnecting in 2s");
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// Push a finalized utterance through the Stage2 pipeline (same as an injected turn).
fn dispatch_turn(state: &Arc<AppState>, rt: &Handle, text: String) {
    let st = state.clone();
    rt.spawn(async move {
        let _ = pipeline::handle_turn(
            st,
            pipeline::TurnInput {
                raw_text: text,
                start_time: None,
                end_time: None,
                topic_id: None,
                audio_base64: None,
                audio_mime: None,
                duration_ms: None,
            },
        )
        .await;
    });
}

/// One connection to `GET /audio`: parse headers, then de-chunk the body, calling `on_bytes` with
/// each PCM payload. Returns when the stream closes (chunked terminator or EOF).
fn connect_stream(addr: &str, on_bytes: &mut impl FnMut(&[u8])) -> anyhow::Result<()> {
    let mut stream = TcpStream::connect(addr)?;
    stream.write_all(
        format!("GET /audio HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes(),
    )?;
    let mut reader = BufReader::new(stream);

    // headers until blank line
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            anyhow::bail!("eof in headers");
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
    }

    // chunked body: {hex-size}\r\n{payload}\r\n ... 0\r\n\r\n
    loop {
        let mut size_line = String::new();
        if reader.read_line(&mut size_line)? == 0 {
            break;
        }
        let size = usize::from_str_radix(size_line.trim(), 16).unwrap_or(0);
        if size == 0 {
            break;
        }
        let mut buf = vec![0u8; size];
        reader.read_exact(&mut buf)?;
        on_bytes(&buf);
        let mut crlf = [0u8; 2];
        let _ = reader.read_exact(&mut crlf); // trailing CRLF
    }
    Ok(())
}
