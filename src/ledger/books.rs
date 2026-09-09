//! 账本与账户。
//!
//! 「多账本」是这套设计的骨架：工作账与生活账各有自己的目标币种、账户、
//! 分类和预算，互不干扰。只有一本时所有操作默认落它，模型不必每次指定；
//! 有多本而调用方没说清楚时**报错列出候选，绝不替用户猜**。

use super::money::{format_amount, parse_amount, validate_currency};
use super::types::*;
use super::{local_day_now, new_id, now_rfc3339, LedgerDb};
use anyhow::{bail, Result};
use rusqlite::{params, Connection, OptionalExtension};

const BOOK_COLUMNS: &str = "book_id, name, base_currency, archived, created_at, updated_at";
const ACCOUNT_COLUMNS: &str =
    "account_id, book_id, name, kind, currency, opening_minor, archived, created_at, updated_at";

fn book_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<BookRecord> {
    Ok(BookRecord {
        book_id: row.get(0)?,
        name: row.get(1)?,
        base_currency: row.get(2)?,
        archived: row.get::<_, i64>(3)? != 0,
        created_at: row.get(4)?,
        updated_at: row.get(5)?,
    })
}

fn account_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AccountRecord> {
    let kind_raw: String = row.get(3)?;
    Ok(AccountRecord {
        account_id: row.get(0)?,
        book_id: row.get(1)?,
        name: row.get(2)?,
        // CHECK 约束保证了取值合法，解析不出只可能是库被手工改过。
        kind: AccountKind::parse(&kind_raw).unwrap_or(AccountKind::Other),
        currency: row.get(4)?,
        opening_minor: row.get(5)?,
        archived: row.get::<_, i64>(6)? != 0,
        created_at: row.get(7)?,
        updated_at: row.get(8)?,
    })
}

/// 按「精确 id → id 前缀 → 精确名 → 名字包含」四级依次解析，每级都要求
/// 唯一命中。
///
/// 这是账本域里所有 `resolve_*` 的共同形状：模型可能说 id、说 id 的前几位、
/// 说名字、说名字的一部分，四种都要认；但**歧义一律报错并列出候选**，
/// 因为「记到另一本账上」这种错误用户往往几个月后才发现。
pub(crate) fn resolve_one<T: Clone>(
    query: &str,
    items: &[T],
    id_of: impl Fn(&T) -> &str,
    name_of: impl Fn(&T) -> &str,
    what: &str,
) -> Result<T> {
    let query = query.trim();
    if query.is_empty() {
        bail!("{what} is required");
    }
    let lower = query.to_lowercase();

    if let Some(hit) = items.iter().find(|item| id_of(item) == query) {
        return Ok(hit.clone());
    }
    let by_prefix: Vec<&T> = items
        .iter()
        .filter(|item| id_of(item).starts_with(query))
        .collect();
    if by_prefix.len() == 1 {
        return Ok(by_prefix[0].clone());
    }
    let by_name: Vec<&T> = items
        .iter()
        .filter(|item| name_of(item).to_lowercase() == lower)
        .collect();
    if by_name.len() == 1 {
        return Ok(by_name[0].clone());
    }
    let by_contains: Vec<&T> = items
        .iter()
        .filter(|item| name_of(item).to_lowercase().contains(&lower))
        .collect();
    if by_contains.len() == 1 {
        return Ok(by_contains[0].clone());
    }

    // 一个都没命中与命中多个是两回事，报错必须说清是哪一种。把「这是全部
    // 候选」写成「匹配了 9 条」，模型会照着去挑一个本就不存在的东西
    // （09-09 工具层测试抓到）。
    if by_contains.is_empty() {
        let all: Vec<String> = items.iter().map(|item| name_of(item).to_string()).collect();
        if all.is_empty() {
            bail!("no {what} exists yet");
        }
        bail!(
            "no {what} matches {query:?}; the ones that exist: {}",
            all.join(", ")
        );
    }
    let candidates: Vec<String> = by_contains
        .iter()
        .map(|item| name_of(item).to_string())
        .collect();
    bail!(
        "{query:?} matches {} {what} entries: {}. Name one exactly.",
        candidates.len(),
        candidates.join(", ")
    )
}

impl LedgerDb {
    // ── 账本 ────────────────────────────────────────────────

    pub fn create_book(&self, name: &str, base_currency: &str) -> Result<BookRecord> {
        let name = name.trim();
        if name.is_empty() {
            bail!("book name is required");
        }
        if name.chars().count() > 64 {
            bail!("book name must be at most 64 characters");
        }
        let base_currency = validate_currency(base_currency)?;
        let now = now_rfc3339();
        let book = BookRecord {
            book_id: new_id("bk"),
            name: name.to_string(),
            base_currency,
            archived: false,
            created_at: now.clone(),
            updated_at: now,
        };
        self.with_tx(|tx| {
            let existing: Option<String> = tx
                .query_row(
                    "SELECT book_id FROM ledger_books WHERE name = ?1",
                    params![book.name],
                    |row| row.get(0),
                )
                .optional()?;
            if existing.is_some() {
                bail!("a book named {:?} already exists", book.name);
            }
            tx.execute(
                "INSERT INTO ledger_books (book_id, name, base_currency, archived, created_at, updated_at)
                 VALUES (?1, ?2, ?3, 0, ?4, ?5)",
                params![
                    book.book_id,
                    book.name,
                    book.base_currency,
                    book.created_at,
                    book.updated_at
                ],
            )?;
            super::categories::seed_default_categories(tx, &book.book_id)?;
            Ok(())
        })?;
        Ok(book)
    }

    pub fn list_books(&self, include_archived: bool) -> Result<Vec<BookRecord>> {
        self.with_conn(|conn| list_books_conn(conn, include_archived))
    }

    /// 解析调用方给的账本标识；`None` 表示「没说」，此时只有一本账才算数。
    ///
    /// 多本账而调用方没指定时不选默认、不选最近使用——报错让上层把候选
    /// 摆给模型看。省下的这一次追问，换的是不会把工作餐记进生活账。
    pub fn resolve_book(&self, query: Option<&str>) -> Result<BookRecord> {
        let books = self.list_books(false)?;
        match query.map(str::trim).filter(|value| !value.is_empty()) {
            Some(query) => resolve_one(
                query,
                &books,
                |book| book.book_id.as_str(),
                |book| book.name.as_str(),
                "book",
            ),
            None => match books.len() {
                0 => bail!("no ledger book exists yet; create one first"),
                1 => Ok(books[0].clone()),
                _ => bail!(
                    "several books exist ({}); name the one to use",
                    books
                        .iter()
                        .map(|book| book.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            },
        }
    }

    /// 展示场景下的账本选择：返回（选中的账本，指定的那本是否已不存在）。
    ///
    /// 与 [`Self::resolve_book`] 的严格语义分开：面板首屏不该因为一个失效的
    /// 账本 id 就整页白屏——浏览器记住的 id 可能指向已经删掉的账本，或者
    /// 换了一台机器、换了一份数据。这里回退到第一本，但**把回退这件事说
    /// 出来**，让调用方去清记忆并提示；静默换一本账给人看比白屏更糟。
    ///
    /// 记账、改账那些精确操作仍然走 `resolve_book`：猜错账本会把钱记到别处。
    pub fn resolve_book_for_display(&self, requested: Option<&str>) -> Result<(BookRecord, bool)> {
        let fallback = |db: &Self| -> Result<BookRecord> {
            match db.list_books(false)?.into_iter().next() {
                Some(book) => Ok(book),
                None => db.ensure_default_book(),
            }
        };
        match requested.map(str::trim).filter(|value| !value.is_empty()) {
            Some(value) => match self.resolve_book(Some(value)) {
                Ok(book) => Ok((book, false)),
                Err(_) => Ok((fallback(self)?, true)),
            },
            None => Ok((fallback(self)?, false)),
        }
    }

    pub fn rename_book(&self, book_id: &str, name: &str) -> Result<()> {
        let name = name.trim();
        if name.is_empty() {
            bail!("book name is required");
        }
        self.with_tx(|tx| {
            let changed = tx.execute(
                "UPDATE ledger_books SET name = ?2, updated_at = ?3 WHERE book_id = ?1",
                params![book_id, name, now_rfc3339()],
            )?;
            if changed == 0 {
                bail!("book {book_id} not found");
            }
            Ok(())
        })
    }

    /// 归档而不是删除。账本删了流水就跟着 `ON DELETE CASCADE` 一起没了，
    /// 那是没有回头路的操作，不给它一条顺手的路径。
    pub fn archive_book(&self, book_id: &str, archived: bool) -> Result<()> {
        self.with_tx(|tx| {
            let changed = tx.execute(
                "UPDATE ledger_books SET archived = ?2, updated_at = ?3 WHERE book_id = ?1",
                params![book_id, i64::from(archived), now_rfc3339()],
            )?;
            if changed == 0 {
                bail!("book {book_id} not found");
            }
            Ok(())
        })
    }

    // ── 账户 ────────────────────────────────────────────────

    pub fn create_account(
        &self,
        book_id: &str,
        name: &str,
        kind: AccountKind,
        currency: Option<&str>,
        opening: Option<&str>,
    ) -> Result<AccountRecord> {
        let name = name.trim();
        if name.is_empty() {
            bail!("account name is required");
        }
        if name.chars().count() > 64 {
            bail!("account name must be at most 64 characters");
        }
        let book = self.get_book(book_id)?;
        // 账户不指定币种就跟随账本目标币种——绝大多数账户就是本币。
        let currency = match currency.map(str::trim).filter(|value| !value.is_empty()) {
            Some(value) => validate_currency(value)?,
            None => book.base_currency.clone(),
        };
        let opening_minor = match opening.map(str::trim).filter(|value| !value.is_empty()) {
            Some(value) => parse_amount(value, &currency)?,
            None => 0,
        };
        let now = now_rfc3339();
        let account = AccountRecord {
            account_id: new_id("ac"),
            book_id: book.book_id.clone(),
            name: name.to_string(),
            kind,
            currency,
            opening_minor,
            archived: false,
            created_at: now.clone(),
            updated_at: now,
        };
        self.with_tx(|tx| {
            let existing: Option<String> = tx
                .query_row(
                    "SELECT account_id FROM ledger_accounts WHERE book_id = ?1 AND name = ?2",
                    params![account.book_id, account.name],
                    |row| row.get(0),
                )
                .optional()?;
            if existing.is_some() {
                bail!("an account named {:?} already exists in this book", account.name);
            }
            tx.execute(
                "INSERT INTO ledger_accounts
                 (account_id, book_id, name, kind, currency, opening_minor, archived, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7, ?8)",
                params![
                    account.account_id,
                    account.book_id,
                    account.name,
                    account.kind.as_str(),
                    account.currency,
                    account.opening_minor,
                    account.created_at,
                    account.updated_at
                ],
            )?;
            Ok(())
        })?;
        Ok(account)
    }

    pub fn list_accounts(
        &self,
        book_id: &str,
        include_archived: bool,
    ) -> Result<Vec<AccountRecord>> {
        self.with_conn(|conn| {
            let sql = format!(
                "SELECT {ACCOUNT_COLUMNS} FROM ledger_accounts
                 WHERE book_id = ?1 AND (?2 = 1 OR archived = 0)
                 ORDER BY archived, name"
            );
            let mut statement = conn.prepare(&sql)?;
            let rows = statement.query_map(
                params![book_id, i64::from(include_archived)],
                account_from_row,
            )?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn resolve_account(&self, book_id: &str, query: &str) -> Result<AccountRecord> {
        let accounts = self.list_accounts(book_id, false)?;
        resolve_one(
            query,
            &accounts,
            |account| account.account_id.as_str(),
            |account| account.name.as_str(),
            "account",
        )
    }

    pub fn archive_account(&self, account_id: &str, archived: bool) -> Result<()> {
        self.with_tx(|tx| {
            let changed = tx.execute(
                "UPDATE ledger_accounts SET archived = ?2, updated_at = ?3 WHERE account_id = ?1",
                params![account_id, i64::from(archived), now_rfc3339()],
            )?;
            if changed == 0 {
                bail!("account {account_id} not found");
            }
            Ok(())
        })
    }

    /// 账户余额 = 初始余额 + 流入 − 流出，只算该账户自己币种的账目。
    ///
    /// 跨币种的转账不在 v1 的能力范围内（那需要成对记两笔），所以这里
    /// 只统计币种相符的行，避免把日元和人民币直接相加。
    pub fn account_balance_minor(&self, account: &AccountRecord) -> Result<i64> {
        self.with_conn(|conn| {
            let inflow: i64 = conn.query_row(
                "SELECT COALESCE(SUM(amount_minor), 0) FROM ledger_entries
                 WHERE deleted_at IS NULL AND currency = ?2
                   AND ((kind = 'income' AND account_id = ?1)
                        OR (kind = 'transfer' AND to_account_id = ?1))",
                params![account.account_id, account.currency],
                |row| row.get(0),
            )?;
            let outflow: i64 = conn.query_row(
                "SELECT COALESCE(SUM(amount_minor), 0) FROM ledger_entries
                 WHERE deleted_at IS NULL AND currency = ?2
                   AND ((kind = 'expense' AND account_id = ?1)
                        OR (kind = 'transfer' AND account_id = ?1))",
                params![account.account_id, account.currency],
                |row| row.get(0),
            )?;
            Ok(account.opening_minor + inflow - outflow)
        })
    }

    pub fn get_book(&self, book_id: &str) -> Result<BookRecord> {
        self.with_conn(|conn| {
            let sql = format!("SELECT {BOOK_COLUMNS} FROM ledger_books WHERE book_id = ?1");
            conn.query_row(&sql, params![book_id], book_from_row)
                .optional()?
                .ok_or_else(|| anyhow::anyhow!("book {book_id} not found"))
        })
    }

    /// 没有任何账本时建一本默认的，让「记一笔」不必先做一次配置。
    ///
    /// 目标币种取本机地区的常见币种做不了可靠推断，所以固定 CNY——猜错的
    /// 代价是用户改一次账本设置，比默认成别的币种再连累每笔换算要轻。
    pub fn ensure_default_book(&self) -> Result<BookRecord> {
        let books = self.list_books(true)?;
        if let Some(book) = books.iter().find(|book| !book.archived) {
            return Ok(book.clone());
        }
        if let Some(book) = books.first() {
            return Ok(book.clone());
        }
        self.create_book("日常", "CNY")
    }
}

fn list_books_conn(conn: &Connection, include_archived: bool) -> Result<Vec<BookRecord>> {
    let sql = format!(
        "SELECT {BOOK_COLUMNS} FROM ledger_books
         WHERE ?1 = 1 OR archived = 0
         ORDER BY archived, created_at"
    );
    let mut statement = conn.prepare(&sql)?;
    let rows = statement.query_map(params![i64::from(include_archived)], book_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// 账户余额的人类可读形式，面板与工具都用它。
pub(crate) fn format_balance(minor: i64, currency: &str) -> String {
    format!("{} {}", format_amount(minor, currency), currency)
}

/// 今天的本地日期，导出给别的模块用（保持「本地自然日」口径只有一处定义）。
pub(crate) fn today() -> String {
    local_day_now()
}
