//! Integration test: the aura bridge (swift-ime) against a MOCK aura server.
//!
//! Verifies the 联调 seam end-to-end (no mic): the `AuraAgent` SDK drives the connection
//! (health probe + snapshot fetch + `/api/asr_stream` SSE), the bridge drains its events into
//! the AsrBuffer (live → live, final → #1), which the `#asr` Voice flow reads. The mock speaks
//! the daemon's HTTP contract per path; `/api/asr_stream` scripts interim → calibrated → final.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use ime_core::asr_buffer::AsrBuffer;
use swift_ime::bridge::spawn_aura_client;

/// A minimal AuraStateView snapshot (the agent fetches `/api/state` on connect).
const STATE_JSON: &str = r#"{"connected":true,"stage3_on":true,"config":{"asr_backend":"qwen3-asr","asr_kind":"local","asr_provider":"cpu","llm_kind":"local","model":"qwen2.5-3b-instruct-q4_k_m.gguf","vad":{"threshold":0.5,"min_silence":1.0,"merge_gap":2.5}},"hotwords":[],"corrections":[]}"#;

/// Mock aura-daemon: accepts connections in a loop, dispatching by request path.
/// - `/health` / `/api/state` → plain 200s (the agent probes these).
/// - `/api/asr_stream` → SSE: interim → calibrated_interim → final, then holds ~2s.
fn mock_aura_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut stream) = conn else { break };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            // Read the HTTP request line (first line: METHOD PATH HTTP/x.y).
            let mut req = [0u8; 4096];
            let n = stream.read(&mut req).unwrap_or(0);
            let req = String::from_utf8_lossy(&req[..n]);
            let path = req.lines().next().unwrap_or("").split_whitespace().nth(1).unwrap_or("/").to_string();
            if path.starts_with("/api/asr_stream") {
                let resp = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
                stream.write_all(resp).unwrap();
                stream.write_all(b"data: {\"type\":\"interim\",\"seq\":1,\"partial\":\"ni\",\"at_s\":0}\n\n").unwrap();
                stream.write_all("data: {\"type\":\"calibrated_interim\",\"seq\":1,\"calibrated\":\"你好\"}\n\n".as_bytes()).unwrap();
                stream.write_all("data: {\"type\":\"final\",\"seq\":1,\"raw_text\":\"你好\",\"streaming_text\":\"\",\"calibrated\":\"你好世界\",\"intent\":\"chat\",\"reply\":\"\",\"route_ms\":12}\n\n".as_bytes()).unwrap();
                stream.flush().unwrap();
                thread::sleep(Duration::from_secs(2)); // keep open so bytes are flushed before close
            } else if path.starts_with("/api/state") {
                let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", STATE_JSON.len(), STATE_JSON);
                stream.flush().unwrap();
            } else {
                // /health etc.
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                stream.flush().unwrap();
            }
        }
    });
    format!("127.0.0.1:{}", addr.port())
}

#[test]
fn bridge_feeds_segments_into_asr_buffer() {
    let addr = mock_aura_server();
    let buf = Arc::new(AsrBuffer::new());
    spawn_aura_client(Arc::clone(&buf), Some(&addr));

    // Wait for the final to land in the buffer (becomes candidate #1).
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let (finals, _live) = buf.voice_candidates();
        if finals.iter().any(|f| f == "你好世界") {
            break; // success
        }
        if Instant::now() >= deadline {
            let (finals, live) = buf.voice_candidates();
            panic!("bridge did not feed the final in time. finals={finals:?} live={live:?}");
        }
        thread::sleep(Duration::from_millis(100));
    }

    // Live streaming should also have been observed at some point (calibrated_interim → live).
    // The final clears live, so we only assert the final here; the live path is unit-tested in ime-core.
    let (finals, _) = buf.voice_candidates();
    assert_eq!(finals.first(), Some(&"你好世界".to_string()), "final is #1");
}
