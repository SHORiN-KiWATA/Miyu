//! 分类树。
//!
//! 两层封顶：一级分类（餐饮）＋二级分类（餐饮/外卖）。再深就该用备注或商家
//! 字段了——三层以上的分类树在个人记账里只会让人每次都犹豫该记哪一层。
//!
//! 分类按收支方向分成两棵，「餐饮」不会同时出现在收入里。

use super::types::*;
use super::{new_id, now_rfc3339, LedgerDb};
use anyhow::{bail, Result};
use rusqlite::{params, Connection, OptionalExtension};

const CATEGORY_COLUMNS: &str =
    "category_id, book_id, parent_id, name, direction, icon, sort, archived, created_at, updated_at";

/// 新账本的默认分类。覆盖日常记账的绝大多数场景，用户不必先做一轮配置
/// 就能开始记；不合用的可以归档，缺的可以新建。
const DEFAULT_EXPENSE: &[(&str, &str)] = &[
    ("餐饮", "🍜"),
    ("交通", "🚇"),
    ("购物", "🛍️"),
    ("居住", "🏠"),
    ("娱乐", "🎮"),
    ("医疗", "💊"),
    ("学习", "📚"),
    ("人情", "🎁"),
    ("其他", "📦"),
];
const DEFAULT_INCOME: &[(&str, &str)] = &[
    ("工资", "💰"),
    ("奖金", "🎉"),
    ("理财", "📈"),
    ("兼职", "💼"),
    ("其他", "📦"),
];

fn category_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CategoryRecord> {
    let direction_raw: String = row.get(4)?;
    Ok(CategoryRecord {
        category_id: row.get(0)?,
        book_id: row.get(1)?,
        parent_id: row.get(2)?,
        name: row.get(3)?,
        direction: Direction::parse(&direction_raw).unwrap_or(Direction::Expense),
        icon: row.get(5)?,
        sort: row.get(6)?,
        archived: row.get::<_, i64>(7)? != 0,
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
    })
}

/// 建账本时铺一套默认分类。在建账本的同一个事务里跑：账本建好了却没有
/// 分类是个半成品状态，不该让它有机会出现。
pub(crate) fn seed_default_categories(conn: &Connection, book_id: &str) -> Result<()> {
    let now = now_rfc3339();
    for (direction, items) in [
        (Direction::Expense, DEFAULT_EXPENSE),
        (Direction::Income, DEFAULT_INCOME),
    ] {
        for (sort, (name, icon)) in items.iter().enumerate() {
            conn.execute(
                "INSERT INTO ledger_categories
                 (category_id, book_id, parent_id, name, direction, icon, sort, archived, created_at, updated_at)
                 VALUES (?1, ?2, NULL, ?3, ?4, ?5, ?6, 0, ?7, ?8)",
                params![
                    new_id("ct"),
                    book_id,
                    name,
                    direction.as_str(),
                    icon,
                    sort as i64,
                    now,
                    now
                ],
            )?;
        }
    }
    Ok(())
}

impl LedgerDb {
    pub fn list_categories(
        &self,
        book_id: &str,
        direction: Option<Direction>,
        include_archived: bool,
    ) -> Result<Vec<CategoryRecord>> {
        self.with_conn(|conn| {
            let sql = format!(
                "SELECT {CATEGORY_COLUMNS} FROM ledger_categories
                 WHERE book_id = ?1
                   AND (?2 IS NULL OR direction = ?2)
                   AND (?3 = 1 OR archived = 0)
                 ORDER BY direction, sort, name"
            );
            let mut statement = conn.prepare(&sql)?;
            let rows = statement.query_map(
                params![
                    book_id,
                    direction.map(|value| value.as_str()),
                    i64::from(include_archived)
                ],
                category_from_row,
            )?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// 按名字或 id 找分类。带方向时只在该方向的那棵树里找——「其他」
    /// 收支各有一个，不限定方向必然歧义。
    pub fn resolve_category(
        &self,
        book_id: &str,
        query: &str,
        direction: Option<Direction>,
    ) -> Result<CategoryRecord> {
        let categories = self.list_categories(book_id, direction, false)?;
        super::books::resolve_one(
            query,
            &categories,
            |category| category.category_id.as_str(),
            |category| category.name.as_str(),
            "category",
        )
    }

    pub fn create_category(
        &self,
        book_id: &str,
        name: &str,
        direction: Direction,
        parent: Option<&str>,
        icon: Option<&str>,
    ) -> Result<CategoryRecord> {
        let name = name.trim();
        if name.is_empty() {
            bail!("category name is required");
        }
        if name.chars().count() > 32 {
            bail!("category name must be at most 32 characters");
        }
        // 父级必须同方向，否则会长出「收入/餐饮」这种自相矛盾的枝。
        let parent = match parent.map(str::trim).filter(|value| !value.is_empty()) {
            Some(value) => {
                let parent = self.resolve_category(book_id, value, Some(direction))?;
                if parent.parent_id.is_some() {
                    bail!(
                        "category {:?} is already a sub-category; the tree is two levels deep",
                        parent.name
                    );
                }
                Some(parent)
            }
            None => None,
        };
        let now = now_rfc3339();
        let record = CategoryRecord {
            category_id: new_id("ct"),
            book_id: book_id.to_string(),
            parent_id: parent.as_ref().map(|parent| parent.category_id.clone()),
            name: name.to_string(),
            direction,
            icon: icon.unwrap_or_default().trim().to_string(),
            sort: 1000,
            archived: false,
            created_at: now.clone(),
            updated_at: now,
        };
        self.with_tx(|tx| {
            let clash: Option<String> = tx
                .query_row(
                    "SELECT category_id FROM ledger_categories
                     WHERE book_id = ?1 AND name = ?2 AND direction = ?3
                       AND ((parent_id IS NULL AND ?4 IS NULL) OR parent_id = ?4)",
                    params![
                        record.book_id,
                        record.name,
                        record.direction.as_str(),
                        record.parent_id
                    ],
                    |row| row.get(0),
                )
                .optional()?;
            if clash.is_some() {
                bail!("a category named {:?} already exists here", record.name);
            }
            tx.execute(
                "INSERT INTO ledger_categories
                 (category_id, book_id, parent_id, name, direction, icon, sort, archived, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, ?8, ?9)",
                params![
                    record.category_id,
                    record.book_id,
                    record.parent_id,
                    record.name,
                    record.direction.as_str(),
                    record.icon,
                    record.sort,
                    record.created_at,
                    record.updated_at
                ],
            )?;
            Ok(())
        })?;
        Ok(record)
    }

    /// 归档分类。已经记在这个分类下的账不受影响——历史账目的分类不该
    /// 因为以后不再用这个分类而改变。
    pub fn archive_category(&self, category_id: &str, archived: bool) -> Result<()> {
        self.with_tx(|tx| {
            let changed = tx.execute(
                "UPDATE ledger_categories SET archived = ?2, updated_at = ?3 WHERE category_id = ?1",
                params![category_id, i64::from(archived), now_rfc3339()],
            )?;
            if changed == 0 {
                bail!("category {category_id} not found");
            }
            Ok(())
        })
    }

    pub fn get_category(&self, category_id: &str) -> Result<Option<CategoryRecord>> {
        self.with_conn(|conn| {
            let sql =
                format!("SELECT {CATEGORY_COLUMNS} FROM ledger_categories WHERE category_id = ?1");
            Ok(conn
                .query_row(&sql, params![category_id], category_from_row)
                .optional()?)
        })
    }

    /// 分类的显示名：二级分类带上父级，`餐饮/外卖`。
    pub fn category_display_name(&self, category_id: &str) -> Result<String> {
        let Some(category) = self.get_category(category_id)? else {
            return Ok(String::new());
        };
        match &category.parent_id {
            Some(parent_id) => {
                let parent = self.get_category(parent_id)?;
                Ok(match parent {
                    Some(parent) => format!("{}/{}", parent.name, category.name),
                    None => category.name,
                })
            }
            None => Ok(category.name),
        }
    }
}
