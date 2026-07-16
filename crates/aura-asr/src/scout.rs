//! scout — omni-scout `/audio` client. Streams chunked 16 kHz mono S16LE PCM from a omni-scout
//! daemon (real mic or `--mock-audio`), re-frames the variable-length chunks into fixed-size i16
//! windows for downstream VAD, and auto-reconnects on drop.
//!
//! Wire format (from omni-scout `server.rs`): `GET /audio` → `200 OK`,
//! `Transfer-Encoding: chunked`, then `{hex-size}\r\n{pcm bytes}\r\n` chunks until `0\r\n\r\n`.
//!
//! Implementation: a single raw `read()` loop into an 8 KB buffer. Chunk boundaries are scanned
//! manually (find `\r\n`, parse hex size, read that many bytes). No per-byte loops: PCM i16 pairs
//! are batched via `chunks_exact`. This avoids BufReader/read_line overhead and the O(n) drain that
//! starved the old implementation (only ~1 window/s instead of ~500/s).

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

const READ_BUF: usize = 8 * 1024;

/// A fixed-size i16 window pulled from omni-scout `/audio`. Pure sync IO in a dedicated thread —
/// fine for a single audio stream; wrap in async later if multiple sources are needed.
pub struct ScoutAudioSource {
    addr: String,
    window: usize,
    /// When false, the stream loop does NOT connect to scout (the existing connection, if any,
    /// ends naturally; no reconnect until set true again). This is the "toggle aura's OWN
    /// connection behavior" switch — it does NOT kill the scout process. Shared with the daemon's
    /// POST /api/control/scout handler.
    active: Arc<AtomicBool>,
}

impl ScoutAudioSource {
    pub fn new(addr: impl Into<String>, window: usize) -> Self {
        Self::with_active(addr, window, Arc::new(AtomicBool::new(true)))
    }

    /// Like [`new`](Self::new) but with a shared `active` flag the caller can flip at runtime to
    /// pause/resume the scout connection (takes effect at the reconnect boundary).
    pub fn with_active(addr: impl Into<String>, window: usize, active: Arc<AtomicBool>) -> Self {
        ScoutAudioSource { addr: addr.into(), window, active }
    }

    /// Connect to `/audio` and call `on_window` for each fixed-size i16 window. Blocks the calling
    /// thread. Reconnects after `reconnect_delay` on any stream error. While `active == false`, it
    /// does NOT connect (and logs the pause/resume transitions).
    pub fn stream<F: FnMut(&[i16])>(&self, mut on_window: F, reconnect_delay: Duration) -> ! {
        eprintln!("[scout] ingest started, connecting to {}/audio …", self.addr);
        let mut was_active = self.active.load(Ordering::Relaxed);
        loop {
            let active = self.active.load(Ordering::Relaxed);
            if !active {
                if was_active {
                    eprintln!("[scout] paused — connection toggled off (not connecting)");
                    was_active = false;
                }
                std::thread::sleep(reconnect_delay);
                continue;
            }
            if !was_active {
                eprintln!("[scout] resumed — connecting to {}/audio", self.addr);
                was_active = true;
            }
            if let Err(e) = self.stream_once(&mut on_window) {
                eprintln!("[scout] stream ended ({e}); reconnecting in {reconnect_delay:?}");
            }
            std::thread::sleep(reconnect_delay);
        }
    }

    fn stream_once<F: FnMut(&[i16])>(&self, on_window: &mut F) -> anyhow::Result<()> {
        let mut sock = TcpStream::connect(&self.addr)?;
        sock.set_nodelay(true)?; // minimise latency on small chunks
        let req = format!(
            "GET /audio HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            self.addr
        );
        sock.write_all(req.as_bytes())?;

        // ── read everything into one growable buffer, scan for headers end ──
        let mut buf: Vec<u8> = Vec::with_capacity(READ_BUF);
        let mut tmp = [0u8; READ_BUF];
        let hdr_end = loop {
            if let Some(pos) = find_subseq(&buf, b"\r\n\r\n") {
                break pos + 4; // body starts after the blank line
            }
            let n = sock.read(&mut tmp)?;
            if n == 0 {
                anyhow::bail!("eof in headers");
            }
            buf.extend_from_slice(&tmp[..n]);
        };

        // Move body bytes (already read while scanning headers) to the front.
        let mut body = buf.split_off(hdr_end);

        // ── chunked-body state machine ──
        // We maintain `body` as all unconsumed bytes. At any point we're in one of two states:
        //   A) reading a chunk-size line (hex + \r\n)
        //   B) reading chunk payload (size bytes) + trailing \r\n
        enum State { SizeLine, Payload(usize) }
        let mut state = State::SizeLine;

        // i16 reframe accumulator
        let mut win: Vec<i16> = Vec::with_capacity(self.window);
        let mut odd_byte: Option<u8> = None; // carry a lone byte across reads (odd PCM length)

        loop {
            // If body doesn't have what we need, refill from the socket.
            match state {
                State::SizeLine => {
                    // Need a complete size line ending in \r\n.
                    if find_subseq(&body, b"\r\n").is_none() {
                        let n = sock.read(&mut tmp)?;
                        if n == 0 { break; }
                        body.extend_from_slice(&tmp[..n]);
                        continue;
                    }
                    let nl = find_subseq(&body, b"\r\n").unwrap();
                    let line = std::str::from_utf8(&body[..nl]).unwrap_or("");
                    let size = usize::from_str_radix(line.trim(), 16).unwrap_or(0);
                    body.drain(0..nl + 2); // consume size line + \r\n
                    if size == 0 { break; } // end-of-chunks marker
                    state = State::Payload(size);
                }
                State::Payload(remaining) => {
                    if body.len() < remaining + 2 {
                        // Not enough yet (payload + trailing \r\n): refill.
                        let n = sock.read(&mut tmp)?;
                        if n == 0 { break; }
                        body.extend_from_slice(&tmp[..n]);
                        continue;
                    }
                    // We have the full payload: convert PCM bytes → i16 windows inline.
                    let payload = &body[..remaining];
                    let mut start = 0;
                    if let Some(b) = odd_byte.take() {
                        if !payload.is_empty() {
                            win.push(i16::from_le_bytes([b, payload[0]]));
                            if win.len() == self.window {
                                on_window(&win);
                                win.clear();
                            }
                            start = 1;
                        } else {
                            odd_byte = Some(b);
                        }
                    }
                    let rest = &payload[start..];
                    for chunk in rest.chunks_exact(2) {
                        win.push(i16::from_le_bytes([chunk[0], chunk[1]]));
                        if win.len() == self.window {
                            on_window(&win);
                            win.clear();
                        }
                    }
                    if let Some(&b) = rest.chunks_exact(2).remainder().get(0) {
                        odd_byte = Some(b);
                    }
                    body.drain(0..remaining + 2); // consume payload + \r\n
                    state = State::SizeLine;
                }
            }
        }
        Ok(())
    }
}

/// Find the first occurrence of `needle` in `hay`; returns the byte offset.
fn find_subseq(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn finds_subseq() {
        assert_eq!(find_subseq(b"abc\r\n\r\ndef", b"\r\n\r\n"), Some(3));
        assert_eq!(find_subseq(b"hello", b"lo"), Some(3));
        assert_eq!(find_subseq(b"abc", b"xyz"), None);
    }
    #[test]
    fn constructs() {
        let s = ScoutAudioSource::new("127.0.0.1:7878", 512);
        assert_eq!(s.window, 512);
        assert_eq!(s.addr, "127.0.0.1:7878");
    }
}
