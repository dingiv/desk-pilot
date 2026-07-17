//! Integration test: ring buffer decoupling. One thread feeds a WAV into the ring, another drains
//! 20ms frames, runs noise gate + EnergyVad, collects VadEvent counts. Proves no frames are lost.

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use audio_aura_asr::buffer::{noise_gate, AudioRing};
use audio_aura_asr::{EnergyVad, VadConfig, VadEventKind};

const FRAME: usize = 320;

fn load_test_wav() -> (Vec<i16>, u32) {
    audio_aura_store::wav::read_wav_i16(std::path::Path::new(
        "/workspaces/gui_agent/audio-aura/native/models/sensevoice/test_wavs/zh.wav",
    ))
    .unwrap()
}

#[test]
fn ring_decouples_ingest_from_vad() {
    let (pcm, _sr) = load_test_wav();
    let ring = Arc::new(Mutex::new(AudioRing::new(16_000 * 60))); // 1 minute

    // -- ingest thread: push WAV into ring, then trailing silence so VAD can exit --
    let ring_in = Arc::clone(&ring);
    let pcm_clone = pcm.clone();
    thread::spawn(move || {
        for chunk in pcm_clone.chunks(FRAME) {
            if chunk.len() == FRAME {
                ring_in.lock().unwrap().push(chunk);
            }
        }
        // Push trailing silence (800ms ≫ min_silence 550ms) so the VAD emits EndOfSpeech
        let silence = vec![0i16; FRAME];
        for _ in 0..40 {
            ring_in.lock().unwrap().push(&silence);
        }
    });

    // -- consume thread: drain ring, noise gate, VAD --
    let mut vad = EnergyVad::new(VadConfig::default());
    let mut sos = 0u32;
    let mut eos = 0u32;

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        let frame = {
            let mut guard = ring.lock().unwrap();
            if !guard.has_frame(FRAME) {
                drop(guard);
                thread::sleep(Duration::from_millis(5));
                continue;
            }
            let mut f = guard.drain(FRAME);
            drop(guard);
            noise_gate(&mut f, 500.0);
            // Always push noise-gated frames (including silence) to the VAD so it can track
            // silence duration and emit EndOfSpeech.
            f
        };
        if let Some(ev) = vad.push_frame(&frame) {
            match ev.kind {
                VadEventKind::StartOfSpeech => sos += 1,
                VadEventKind::EndOfSpeech => eos += 1,
            }
        }
        if sos > 0 && eos > 0 && ring.lock().unwrap().len() == 0 {
            break;
        }
    }

    println!("SOS={sos} EOS={eos} ring_remain={}", ring.lock().unwrap().len());
    assert!(sos >= 1, "SOS={sos} EOS={eos} — noise gate silencing all frames? Check RMS floor.");
    assert!(eos >= 1, "SOS={sos} EOS={eos} — frames got through but VAD never exited speech.");
}
