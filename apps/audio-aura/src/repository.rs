//! 持久化/数据访问层(repository)—— Storage 监督器的构建与窄读接口。
//! 落盘细节(date-named WAV / per-turn day log / recent ring / 过期清理)全在
//! aura-core 的 [`Storage`] + [`AudioArchive`];本层只负责:
//! ① 从配置 + FileLoader 命名空间解析目录并构建(含启动索引重建/清理/周期 flusher);
//! ② 对 service/router 暴露三个只读接口 + pipeline assemble 需要的原始句柄。

use std::sync::Arc;

use audio_aura_core::archive::{ArchiveConfig, AudioArchive, ClipMeta};
use audio_aura_core::hub::{Storage, TurnRecord};
use tracing::info;

/// 持久化仓库(Clone = 句柄共享)。目录优先级:aura.yaml `recordings_dir` 覆盖 >
/// `DATA::` 命名空间(dev: apps/audio-aura/data/,prod: ~/.desk-pilot/data/)> 相对路径兜底。
#[derive(Clone)]
pub(crate) struct DataStore {
    storage: Arc<Storage>,
}

impl DataStore {
    /// 构建 + 启动恢复:`init()` 从磁盘重建索引(重启不丢历史)并立即清理过期
    /// 录音/turn 日志,再 spawn 周期 flusher。
    pub(crate) fn build(recordings_dir: Option<String>, retention_days: u32) -> DataStore {
        let data = shared::loader!();
        let rec_dir = recordings_dir.map(std::path::PathBuf::from).unwrap_or_else(|| {
            data.resolve("DATA::recordings")
                .unwrap_or_else(|| std::path::PathBuf::from("data/recordings"))
        });
        let turns_dir = data
            .resolve("DATA::turns")
            .unwrap_or_else(|| std::path::PathBuf::from("data/turns"));
        let retention_days = retention_days.max(1);
        info!(
            recordings = %rec_dir.display(),
            turns = %turns_dir.display(),
            retention_days,
            "storage ready (periodic flush + daily expired cleanup)"
        );
        let archive = Arc::new(AudioArchive::new(ArchiveConfig {
            dir: rec_dir,
            retention_days,
            ..Default::default()
        }));
        let storage = Arc::new(Storage::new(archive, turns_dir, retention_days));
        let cleaned = storage.init();
        if cleaned > 0 {
            info!(cleaned, "expired recordings/turn-logs cleaned at startup");
        }
        // 周期 flusher 线程:句柄随手 drop = detach,线程随进程常驻(无需 join)。
        let _ = storage.audio.spawn_flusher();
        DataStore { storage }
    }

    /// 原始句柄:pipeline assemble 的落盘出口(ParagraphCalibration 时自动
    /// record_final = archive + day log + ring)。
    pub(crate) fn storage(&self) -> Arc<Storage> {
        Arc::clone(&self.storage)
    }

    /// 段落 WAV 回放字节(hot tier 优先,磁盘文件兜底,透明解析)。
    pub(crate) fn wav(&self, paragraph_id: u64) -> Option<Vec<u8>> {
        self.storage.audio.wav(paragraph_id)
    }

    /// 全部已知 clip(hot + flushed,seq 升序)。
    pub(crate) fn recordings(&self) -> Vec<ClipMeta> {
        self.storage.recordings()
    }

    /// 最近定稿 turn(最旧 → 最新)—— `/api/results` 的数据源。
    pub(crate) fn recent(&self) -> Vec<TurnRecord> {
        self.storage.recent()
    }
}
