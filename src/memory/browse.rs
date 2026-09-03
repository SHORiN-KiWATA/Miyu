//! 记忆浏览(WebUI dashboard):分页列出、按词过滤、单条删除。
//!
//! 只读查询走 `data_conn_existing`——库不存在就是"这个人格还没有记忆",
//! 不该因为有人打开面板就建一个空库出来。

use super::*;

/// 可浏览的两张表。列名只在这里白名单化,外面传来的表名不拼 SQL。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowseTable {
    Facts,
    Episodes,
}

impl BrowseTable {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "facts" => Some(Self::Facts),
            "episodes" => Some(Self::Episodes),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Facts => "facts",
            Self::Episodes => "episodes",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct BrowseQuery {
    pub text: String,
    pub status: String,
    pub limit: usize,
    pub offset: usize,
}

pub struct BrowsePage {
    pub items: Vec<Value>,
    pub total: i64,
}

impl MemoryStore {
    pub fn browse(&self, table: BrowseTable, query: &BrowseQuery) -> Result<BrowsePage> {
        if !self.data_db.exists() {
            return Ok(BrowsePage {
                items: Vec::new(),
                total: 0,
            });
        }
        let conn = self.data_conn_existing()?;
        let mut clauses = Vec::new();
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        let text = query.text.trim();
        if !text.is_empty() {
            clauses.push("content LIKE ? ESCAPE '\\'");
            params.push(Box::new(format!("%{}%", escape_like(text))));
        }
        let status = query.status.trim();
        if !status.is_empty() && status != "all" {
            clauses.push("status = ?");
            params.push(Box::new(status.to_string()));
        }
        let where_sql = if clauses.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", clauses.join(" AND "))
        };
        let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let total: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM {}{where_sql}", table.name()),
            param_refs.as_slice(),
            |row| row.get(0),
        )?;
        let extra = match table {
            BrowseTable::Facts => "confidence, '' AS retention",
            BrowseTable::Episodes => "1.0 AS confidence, COALESCE(retention, '') AS retention",
        };
        let limit = query.limit.clamp(1, 500) as i64;
        let mut stmt = conn.prepare(&format!(
            "SELECT id, content, source, status, strength, recall_count, created_at, updated_at,
                    visibility, owner_display_name, subjects, {extra}
             FROM {}{where_sql}
             ORDER BY updated_at DESC, id DESC
             LIMIT {limit} OFFSET {}",
            table.name(),
            query.offset as i64
        ))?;
        let items = stmt
            .query_map(param_refs.as_slice(), |row| {
                let subjects: String = row.get(10)?;
                Ok(json!({
                    "id": row.get::<_, i64>(0)?,
                    "content": row.get::<_, String>(1)?,
                    "source": row.get::<_, String>(2)?,
                    "status": row.get::<_, String>(3)?,
                    "strength": row.get::<_, f64>(4)?,
                    "recall_count": row.get::<_, i64>(5)?,
                    "created_at": row.get::<_, String>(6)?,
                    "updated_at": row.get::<_, String>(7)?,
                    "visibility": row.get::<_, String>(8)?,
                    "owner": row.get::<_, String>(9)?,
                    "subjects": serde_json::from_str::<Value>(&subjects).unwrap_or(Value::Array(Vec::new())),
                    "confidence": row.get::<_, f64>(11)?,
                    "retention": row.get::<_, String>(12)?,
                }))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(BrowsePage { items, total })
    }

    pub fn delete_item(&self, table: BrowseTable, id: i64) -> Result<bool> {
        if !self.data_db.exists() {
            return Ok(false);
        }
        let conn = self.data_conn_existing()?;
        let affected = conn.execute(
            &format!("DELETE FROM {} WHERE id = ?1", table.name()),
            rusqlite::params![id],
        )?;
        Ok(affected == 1)
    }
}

fn escape_like(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}
