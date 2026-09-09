//! 账本：多账本的个人收支记录。
//!
//! 数据落在独立的 `data/ledger/ledger.db`（选址理由见 [`schema`]）。这一层
//! 只管数据，不认识工具、也不认识 WebUI——`tools/ledger.rs` 与
//! `web/dashboards/ledger.rs` 都调用这里，彼此不互相依赖。
//!
//! 三条贯穿全模块的约定：
//!
//! 1. **钱是整数**。所有 `*_minor` 字段是最小货币单位的 `i64`，浮点只在
//!    汇率换算里出现一次（[`money::convert_minor`]）。
//! 2. **汇率是快照**。换算用的汇率连同来源与时刻一起冻结在行里，永不重算。
//! 3. **删除是软的**。`deleted_at` 置位而非删行。

pub(crate) mod books;
pub(crate) mod categories;
pub(crate) mod csv;
pub(crate) mod entries;
pub(crate) mod money;
pub(crate) mod rates;
pub(crate) mod schema;
pub(crate) mod stats;
pub(crate) mod types;

#[cfg(test)]
mod tests;

use crate::paths::MiyuPaths;
use anyhow::{Context, Result};
use chrono::{Local, Utc};
use rusqlite::Connection;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// 账本库的句柄。
///
/// 单连接 + `Mutex`，与主库 `ConversationDb` 同款：账本一天写几十次，
/// 连接池是纯粹的复杂度。所有方法都是同步阻塞的，调用方（daemon 跑在
/// `current_thread` runtime 上）必须包 `spawn_blocking`。
pub struct LedgerDb {
    conn: Mutex<Connection>,
}

impl std::fmt::Debug for LedgerDb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LedgerDb").finish_non_exhaustive()
    }
}

/// 已做过完整体检的库路径。
///
/// `quick_check` 与迁移前备份只在进程内首次打开某个库时做一次：账本操作
/// 是「每次调用现开连接」的形态，每次都体检既慢又会把备份刷成每笔一次。
static CHECKED: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();

impl LedgerDb {
    pub fn open(paths: &MiyuPaths) -> Result<Self> {
        Self::open_at(&Self::db_path(paths))
    }

    pub fn db_path(paths: &MiyuPaths) -> PathBuf {
        paths.data_dir.join("ledger").join("ledger.db")
    }

    /// 直接按路径打开，测试与导入导出用。
    pub fn open_at(db_path: &Path) -> Result<Self> {
        let first_time = {
            let checked = CHECKED.get_or_init(|| Mutex::new(HashSet::new()));
            let mut checked = checked.lock().unwrap();
            checked.insert(db_path.to_path_buf())
        };
        if first_time {
            preflight(db_path)?;
        }
        let conn = schema::open_database(db_path)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub(crate) fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self.conn.lock().unwrap();
        f(&conn)
    }

    pub(crate) fn with_tx<T>(
        &self,
        f: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let value = f(&tx)?;
        tx.commit()?;
        Ok(value)
    }
}

/// 迁移前的体检与备份，照主库 `ConversationDb::open` 的同一套判据。
///
/// 顺序是刻意的：**体检不过就不备份**。坏库上跑 `VACUUM` 会把失配放大成
/// 截断损坏（08-21 取证），而那一刻恰恰是上一份好备份最有价值的时候——
/// 所以先导到暂存文件、成功了才改名顶上，失败则原样保留旧备份。
fn preflight(db_path: &Path) -> Result<()> {
    if !db_path.exists() {
        return Ok(());
    }
    let conn = Connection::open(db_path).with_context(|| {
        format!(
            "opening ledger database for preflight: {}",
            db_path.display()
        )
    })?;
    let check: String = conn
        .query_row("PRAGMA quick_check(1)", [], |row| row.get(0))
        .unwrap_or_else(|_| "unreadable".to_string());
    if check != "ok" {
        tracing::error!(
            db = %db_path.display(),
            %check,
            "ledger.db 未通过完整性体检;请备份后用 sqlite3 .recover 重建,期间不要继续记账"
        );
        return Ok(());
    }

    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version >= schema::SCHEMA_VERSION {
        return Ok(());
    }
    let has_tables: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='ledger_entries')",
        [],
        |row| row.get(0),
    )?;
    if !has_tables {
        return Ok(());
    }

    let bak = with_extension(db_path, "bak");
    let staging = with_extension(db_path, "bak.new");
    let _ = std::fs::remove_file(&staging);
    match conn.execute("VACUUM INTO ?1", [staging.to_string_lossy().as_ref()]) {
        Ok(_) => {
            if let Err(error) = std::fs::rename(&staging, &bak) {
                let _ = std::fs::remove_file(&staging);
                tracing::error!(%error, "ledger.db 迁移前备份改名失败(旧备份保留)");
            }
        }
        Err(error) => {
            let _ = std::fs::remove_file(&staging);
            tracing::error!(%error, "ledger.db 迁移前备份失败(旧备份保留,继续迁移)");
        }
    }
    Ok(())
}

fn with_extension(db_path: &Path, suffix: &str) -> PathBuf {
    let mut name = db_path.file_name().unwrap_or_default().to_os_string();
    name.push(".");
    name.push(suffix);
    db_path.with_file_name(name)
}

/// 当前时刻的 RFC3339 UTC 串。与主库的时间戳口径一致。
pub(crate) fn now_rfc3339() -> String {
    Utc::now().to_rfc3339()
}

/// 当前的**本地**自然日 `YYYY-MM-DD`。
///
/// 存储一律 UTC，唯独「哪一天的账」必须按用户所在时区算——东八区晚上十点
/// 吃的饭，UTC 已经是次日，算进明天的账用户会觉得账本坏了。
pub(crate) fn local_day_now() -> String {
    Local::now().format("%Y-%m-%d").to_string()
}

/// 把 RFC3339 时刻转成本地自然日。
pub(crate) fn local_day_of(rfc3339: &str) -> Result<String> {
    let parsed = chrono::DateTime::parse_from_rfc3339(rfc3339)
        .with_context(|| format!("invalid timestamp {rfc3339:?}, expected RFC3339"))?;
    Ok(parsed.with_timezone(&Local).format("%Y-%m-%d").to_string())
}

/// 生成带前缀的短 id。48 位随机，账本这个量级碰撞概率可忽略；插入时
/// 主键冲突还会再兜一层。
pub(crate) fn new_id(prefix: &str) -> String {
    format!("{prefix}_{:012x}", rand::random::<u64>() & 0xffff_ffff_ffff)
}
