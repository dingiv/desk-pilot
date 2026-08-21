//! store — 持久层统一模块:用户模型数据的全部落盘与恢复。
//!
//! 两个组成部分(组合而非继承,各自单一职责):
//! - [`WeightStore`](`sqlite.rs`)— SQLite 表操作:phrases(自生词)/ en_user
//!   (英文自生词)/ recency(recent member 时间戳)/ l0(inputx 用户模型)。
//!   家族持有其 `Arc` 做行内双写(提交时同步落盘)。
//! - [`PersistenceManager`](`manager.rs`)— 启动编排:打开单一连接,
//!   [`warm_all`](PersistenceManager::warm_all) 把全部持久化状态一次性
//!   灌回内存(bigram 链路已于第五轮删除,surrounding 从未消费)。
//!
//! 引擎持有一个 manager;所有家族经 dispatcher 的 `set_store` 拿到同一个
//! `Arc<WeightStore>`。**不在此模块**:lattice 的 `.idx` 磁盘缓存(词典层
//! 自管的缓存,与用户数据无关)。
//!
//! ```
//! use ime_core::store::PersistenceManager;
//! // engine startup: open once, warm everything
//! let pm = PersistenceManager::open("/tmp/swift-ime-docex.db")?;
//! // pm.warm_all(&dispatcher);  // the engine does this in init_store
//! let store = pm.store();
//! # Ok::<(), rusqlite::Error>(())
//! ```

mod manager;
mod sqlite;
pub mod snippet_md;

pub use manager::PersistenceManager;
pub use sqlite::WeightStore;
