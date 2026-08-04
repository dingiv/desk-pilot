//! Integration test: the aura bridge (swift-ime) against a MOCK aura data-plane SSE server.
//!
//! Verifies the 联调 seam end-to-end (no mic): the SDK parses `AsrSegment` frames off the SSE
//! stream and the bridge maps them to the AsrBuffer (interim/calibrated → live, final → #1),
//! which the `#asr` Voice flow reads. The real aura-daemon speaks the same `/api/asr_stream`
//! contract; this mock just scripts it.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use ime_core::asr_buffer::AsrBuffer;
use swift_ime::bridge::spawn_aura_client;

/// Spawn a one-shot mock `/api/asr_stream` that emits interim → calibrated_interim → final,
/// then holds the connection ~2s before closing (the SDK reconnects after, harmlessly).
fn mock_asr_stream() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
        // Drain the HTTP request line.
        let mut req = [0u8; 1024];
        let _ = stream.read(&mut req);
        let resp = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
        stream.write_all(resp).unwrap();
        stream.write_all(b"data: {\"type\":\"interim\",\"seq\":1,\"partial\":\"ni\",\"at_s\":0}\n\n").unwrap();
        stream.write_all("data: {\"type\":\"calibrated_interim\",\"seq\":1,\"calibrated\":\"你好\"}\n\n".as_bytes()).unwrap();
        stream.write_all("data: {\"type\":\"final\",\"seq\":1,\"raw_text\":\"你好\",\"streaming_text\":\"\",\"calibrated\":\"你好世界\",\"intent\":\"chat\",\"reply\":\"\",\"route_ms\":12}\n\n".as_bytes()).unwrap();
        stream.flush().unwrap();
        thread::sleep(Duration::from_secs(2)); // keep open so bytes are flushed before close
    });
    format!("127.0.0.1:{}", addr.port())
}

#[test]
fn bridge_feeds_segments_into_asr_buffer() {
    let addr = mock_asr_stream();
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
