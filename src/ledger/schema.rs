//! 建库与迁移。
//!
//! 账本是独立的 `data/ledger.db`，不挂主库。理由是写入频率：主库每轮对话都在
//! 写，账本一天几笔——SQLite 损坏几乎都发生在写入过程中，写得越少越安全，
//! 而且主库真出事时账本不陪葬。代价是这里要自建迁移，所以主库的两道保护
//! 一并搬过来：打开时 `quick_check`（[`super::open_ledger_db`]），迁移前
//! `VACUUM INTO` 备份。
//!
//! `SCHEMA_VERSION` 只增不减，[`migrate`] 从任意旧版本一路走到当前版本。
//! 与主库同规矩：只追加、纯增量，不回填不删列。

use anyhow::{bail, Context, Result};
use rusqlite::Connection;
use std::path::Path;

pub(crate) const SCHEMA_VERSION: i64 = 1;

/// 打开连接并跑迁移。PRAGMA 套餐照 `message_history` 的成熟配方。
pub(crate) fn open_database(db_path: &Path) -> Result<Connection> {
    if let Some(parent) = db_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating ledger directory: {}", parent.display()))?;
    }
    let conn = Connection::open(db_path)
        .with_context(|| format!("opening ledger database: {}", db_path.display()))?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = FULL;
         PRAGMA foreign_keys = ON;
         PRAGMA wal_autocheckpoint = 200;
         PRAGMA cache_size = -2048;
         PRAGMA mmap_size = 0;",
    )?;
    migrate(&conn)?;
    Ok(conn)
}

/// `synchronous = FULL` 是这里与别处不同的一处刻意选择。
///
/// 主库和消息历史用 `NORMAL`（WAL 下断电可能丢最后几个事务，换写入吞吐）。
/// 账本一天写几十次，吞吐毫无意义，而「记完账断电丢了这笔」是用户完全
/// 察觉不到的数据损失——用不上的性能换确定性，划算。
pub(crate) fn migrate(conn: &Connection) -> Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version > SCHEMA_VERSION {
        bail!(
            "ledger database schema {version} is newer than this build supports ({SCHEMA_VERSION})"
        );
    }
    if version == SCHEMA_VERSION {
        return Ok(());
    }

    if version < 1 {
        conn.execute_batch(
            "BEGIN IMMEDIATE;
             CREATE TABLE IF NOT EXISTS ledger_books (
                 book_id       TEXT PRIMARY KEY,
                 name          TEXT NOT NULL,
                 base_currency TEXT NOT NULL,
                 archived      INTEGER NOT NULL DEFAULT 0 CHECK (archived IN (0, 1)),
                 created_at    TEXT NOT NULL,
                 updated_at    TEXT NOT NULL
             );
             CREATE UNIQUE INDEX IF NOT EXISTS idx_books_name
                 ON ledger_books(name);

             CREATE TABLE IF NOT EXISTS ledger_accounts (
                 account_id    TEXT PRIMARY KEY,
                 book_id       TEXT NOT NULL REFERENCES ledger_books(book_id) ON DELETE CASCADE,
                 name          TEXT NOT NULL,
                 kind          TEXT NOT NULL
                     CHECK (kind IN ('cash', 'bank', 'ewallet', 'credit', 'other')),
                 currency      TEXT NOT NULL,
                 opening_minor INTEGER NOT NULL DEFAULT 0,
                 archived      INTEGER NOT NULL DEFAULT 0 CHECK (archived IN (0, 1)),
                 created_at    TEXT NOT NULL,
                 updated_at    TEXT NOT NULL
             );
             CREATE UNIQUE INDEX IF NOT EXISTS idx_accounts_book_name
                 ON ledger_accounts(book_id, name);

             CREATE TABLE IF NOT EXISTS ledger_categories (
                 category_id TEXT PRIMARY KEY,
                 book_id     TEXT NOT NULL REFERENCES ledger_books(book_id) ON DELETE CASCADE,
                 parent_id   TEXT REFERENCES ledger_categories(category_id) ON DELETE SET NULL,
                 name        TEXT NOT NULL,
                 direction   TEXT NOT NULL CHECK (direction IN ('expense', 'income')),
                 icon        TEXT NOT NULL DEFAULT '',
                 sort        INTEGER NOT NULL DEFAULT 0,
                 archived    INTEGER NOT NULL DEFAULT 0 CHECK (archived IN (0, 1)),
                 created_at  TEXT NOT NULL,
                 updated_at  TEXT NOT NULL
             );
             -- 同名分类在不同父级下可以共存(「餐饮/早餐」与「零食/早餐」),
             -- 所以唯一键带上父级。NULL 在 SQLite 里彼此不相等,一级分类
             -- 靠下面那条部分索引兜住。
             CREATE UNIQUE INDEX IF NOT EXISTS idx_categories_child_name
                 ON ledger_categories(book_id, parent_id, name)
                 WHERE parent_id IS NOT NULL;
             CREATE UNIQUE INDEX IF NOT EXISTS idx_categories_root_name
                 ON ledger_categories(book_id, name, direction)
                 WHERE parent_id IS NULL;

             CREATE TABLE IF NOT EXISTS ledger_entries (
                 entry_id          TEXT PRIMARY KEY,
                 book_id           TEXT NOT NULL REFERENCES ledger_books(book_id) ON DELETE CASCADE,
                 kind              TEXT NOT NULL
                     CHECK (kind IN ('expense', 'income', 'transfer')),
                 amount_minor      INTEGER NOT NULL CHECK (amount_minor > 0),
                 currency          TEXT NOT NULL,
                 base_amount_minor INTEGER,
                 base_currency     TEXT NOT NULL,
                 rate              TEXT,
                 rate_source       TEXT,
                 rate_at           TEXT,
                 rate_status       TEXT NOT NULL
                     CHECK (rate_status IN ('ok', 'same', 'pending')),
                 account_id        TEXT REFERENCES ledger_accounts(account_id) ON DELETE SET NULL,
                 to_account_id     TEXT REFERENCES ledger_accounts(account_id) ON DELETE SET NULL,
                 category_id       TEXT REFERENCES ledger_categories(category_id) ON DELETE SET NULL,
                 occurred_at       TEXT NOT NULL,
                 occurred_day      TEXT NOT NULL,
                 note              TEXT NOT NULL DEFAULT '',
                 merchant          TEXT NOT NULL DEFAULT '',
                 source            TEXT NOT NULL CHECK (source IN ('chat', 'webui', 'import')),
                 revision          INTEGER NOT NULL DEFAULT 1,
                 deleted_at        TEXT,
                 created_at        TEXT NOT NULL,
                 updated_at        TEXT NOT NULL,
                 -- 转账两端必须都在、且不能是同一个账户;收支则不许有转入方。
                 CHECK (
                     (kind = 'transfer' AND to_account_id IS NOT NULL
                      AND account_id IS NOT NULL AND account_id <> to_account_id
                      AND category_id IS NULL)
                     OR (kind <> 'transfer' AND to_account_id IS NULL)
                 ),
                 -- 待换算时不该留下半截汇率;换算完成时金额必须在。
                 CHECK (
                     (rate_status = 'pending' AND base_amount_minor IS NULL)
                     OR (rate_status <> 'pending' AND base_amount_minor IS NOT NULL)
                 )
             );
             -- 主查询路径:某本账按日期倒序翻页(面板流水、月度统计都走它)。
             CREATE INDEX IF NOT EXISTS idx_entries_book_day
                 ON ledger_entries(book_id, occurred_day DESC, entry_id DESC)
                 WHERE deleted_at IS NULL;
             CREATE INDEX IF NOT EXISTS idx_entries_book_category_day
                 ON ledger_entries(book_id, category_id, occurred_day DESC)
                 WHERE deleted_at IS NULL;
             CREATE INDEX IF NOT EXISTS idx_entries_book_account_day
                 ON ledger_entries(book_id, account_id, occurred_day DESC)
                 WHERE deleted_at IS NULL;
             -- 补算待换算账目时扫这一条,平时是空集。
             CREATE INDEX IF NOT EXISTS idx_entries_pending
                 ON ledger_entries(book_id, rate_status)
                 WHERE rate_status = 'pending' AND deleted_at IS NULL;
             -- 重复记账闸按创建时刻回看最近几分钟。
             CREATE INDEX IF NOT EXISTS idx_entries_book_created
                 ON ledger_entries(book_id, created_at DESC)
                 WHERE deleted_at IS NULL;

             CREATE TABLE IF NOT EXISTS ledger_budgets (
                 budget_id    TEXT PRIMARY KEY,
                 book_id      TEXT NOT NULL REFERENCES ledger_books(book_id) ON DELETE CASCADE,
                 category_id  TEXT REFERENCES ledger_categories(category_id) ON DELETE CASCADE,
                 amount_minor INTEGER NOT NULL CHECK (amount_minor > 0),
                 active       INTEGER NOT NULL DEFAULT 1 CHECK (active IN (0, 1)),
                 created_at   TEXT NOT NULL,
                 updated_at   TEXT NOT NULL
             );
             -- 一本账最多一条总预算,每个分类最多一条分类预算。
             CREATE UNIQUE INDEX IF NOT EXISTS idx_budgets_total
                 ON ledger_budgets(book_id)
                 WHERE category_id IS NULL;
             CREATE UNIQUE INDEX IF NOT EXISTS idx_budgets_category
                 ON ledger_budgets(book_id, category_id)
                 WHERE category_id IS NOT NULL;

             -- 汇率按天缓存:同一天同一对币种只问一次网络。
             CREATE TABLE IF NOT EXISTS ledger_rates (
                 day        TEXT NOT NULL,
                 base       TEXT NOT NULL,
                 target     TEXT NOT NULL,
                 rate       TEXT NOT NULL,
                 source     TEXT NOT NULL,
                 fetched_at TEXT NOT NULL,
                 PRIMARY KEY (day, base, target)
             ) WITHOUT ROWID;

             PRAGMA user_version = 1;
             COMMIT;",
        )
        .context("creating ledger schema")?;
    }
    Ok(())
}
