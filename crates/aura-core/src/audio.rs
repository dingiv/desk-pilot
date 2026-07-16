//! audio.rs — save/read compressed audio (Opus/webm) for Stage1 provenance. Mirrors src/audio.ts.

use anyhow::Result;
use base64::{engine::general_purpose::STANDARD, Engine};
use tokio::fs;

fn ext_for(mime: &str) -> &'static str {
    match mime.to_lowercase().as_str() {
        "audio/webm" | "audio/webm;codecs=opus" => "webm",
        "audio/ogg" | "audio/ogg;codecs=opus" => "ogg",
        "audio/mp4" => "mp4",
        "audio/mpeg" => "mp3",
        "audio/wav" => "wav",
        _ => "webm",
    }
}

pub async fn save_audio(dir: &str, chunk_id: &str, b64: &str, mime: &str) -> Result<String> {
    fs::create_dir_all(dir).await.ok();
    let data = STANDARD.decode(b64)?;
    let path = format!("{dir}/{chunk_id}.{}", ext_for(mime));
    fs::write(&path, data).await?;
    Ok(path)
}

pub async fn read_audio(path: &str) -> Result<Vec<u8>> {
    Ok(fs::read(path).await?)
}
