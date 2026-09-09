//! CSV 导入导出。
//!
//! 导出的是**原始数据**：用户实际花的金额与币种在前，换算结果与汇率快照
//! 在后。这样一份导出既能给人看，也能原样导回来而不丢信息。
//!
//! 自己实现 RFC 4180 的读写而不是引一个 crate：格式就这么点（引号包裹、
//! 双引号转义、字段内可含换行），而账本的依赖越少越好。

use super::types::*;
use super::LedgerDb;
use anyhow::{bail, Result};

/// 导出的列顺序，也是导入认的表头。
pub const HEADER: &[&str] = &[
    "date",
    "kind",
    "amount",
    "currency",
    "category",
    "account",
    "note",
    "merchant",
    "converted",
    "base_currency",
    "rate",
];

/// 导入时认的一行。后面三列（converted / base_currency / rate）是导出附带的
/// 参考信息，导入时忽略——汇率按导入当时重新取，免得把别处算的数当权威。
#[derive(Clone, Debug, Default)]
pub struct ImportRow {
    pub date: String,
    pub kind: String,
    pub amount: String,
    pub currency: String,
    pub category: String,
    pub account: String,
    pub note: String,
    pub merchant: String,
}

fn escape(field: &str) -> String {
    if field.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field.to_string()
    }
}

impl LedgerDb {
    /// 导出一段时间的流水。软删除的条目不导出——它们在账面上已经不存在。
    pub fn export_csv(&self, book: &BookRecord, from_day: &str, to_day: &str) -> Result<String> {
        let filter = super::entries::EntryFilter {
            book_id: book.book_id.clone(),
            from_day: Some(from_day.to_string()),
            to_day: Some(to_day.to_string()),
            include_deleted: false,
            offset: 0,
            limit: 500,
            ..Default::default()
        };
        let mut out = String::new();
        out.push_str(&HEADER.join(","));
        out.push('\n');

        // 分页取满为止：一次 500 行，几年的账也就几十次查询。
        let mut offset = 0;
        loop {
            let page = super::entries::EntryFilter {
                offset,
                ..filter.clone()
            };
            let (entries, total) = self.list_entries(&page)?;
            if entries.is_empty() {
                break;
            }
            for entry in &entries {
                let category = match &entry.category_id {
                    Some(id) => self.category_display_name(id)?,
                    None => String::new(),
                };
                let account = match &entry.account_id {
                    Some(id) => self.account_name(id)?,
                    None => String::new(),
                };
                let converted = entry
                    .base_amount_minor
                    .map(|minor| super::money::format_amount(minor, &entry.base_currency))
                    .unwrap_or_default();
                let row = [
                    entry.occurred_day.clone(),
                    entry.kind.as_str().to_string(),
                    super::money::format_amount(entry.amount_minor, &entry.currency),
                    entry.currency.clone(),
                    category,
                    account,
                    entry.note.clone(),
                    entry.merchant.clone(),
                    converted,
                    entry.base_currency.clone(),
                    entry.rate.clone().unwrap_or_default(),
                ];
                out.push_str(
                    &row.iter()
                        .map(|field| escape(field))
                        .collect::<Vec<_>>()
                        .join(","),
                );
                out.push('\n');
            }
            offset += entries.len() as i64;
            if offset >= total {
                break;
            }
        }
        Ok(out)
    }

    fn account_name(&self, account_id: &str) -> Result<String> {
        self.with_conn(|conn| {
            use rusqlite::OptionalExtension;
            Ok(conn
                .query_row(
                    "SELECT name FROM ledger_accounts WHERE account_id = ?1",
                    rusqlite::params![account_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
                .unwrap_or_default())
        })
    }

    /// 库里已经有几条与这一行内容相同的导入记录。
    ///
    /// 返回的是**数量**而不是存在与否，因为「幂等」和「同一批里两笔一样的
    /// 账」是两个都要满足的目标：一份写了两趟同样车费的文件，第一次导入
    /// 该进两笔；再导一次则一笔都不该进。只看存在与否会把第二趟车费永久
    /// 挡在门外（09-09 真机实测发现）。调用方按「文件里出现几次、库里已有
    /// 几条」取差额。
    ///
    /// 只数 `source = 'import'` 的行：手记过一笔同样的账，不该让文件里
    /// 那笔导不进来。
    pub fn count_import_rows(
        &self,
        book_id: &str,
        day: &str,
        kind: EntryKind,
        amount_minor: i64,
        currency: &str,
        note: &str,
    ) -> Result<i64> {
        self.with_conn(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM ledger_entries
                 WHERE book_id = ?1 AND occurred_day = ?2 AND kind = ?3
                   AND amount_minor = ?4 AND currency = ?5 AND note = ?6
                   AND source = 'import' AND deleted_at IS NULL",
                rusqlite::params![book_id, day, kind.as_str(), amount_minor, currency, note],
                |row| row.get(0),
            )?)
        })
    }
}

/// 解析 CSV 文本。表头必须以 `date,kind,amount` 开头，多余的列忽略。
pub fn parse_csv(text: &str) -> Result<Vec<ImportRow>> {
    let mut records = split_records(text);
    if records.is_empty() {
        bail!("the file is empty");
    }
    let header: Vec<String> = records
        .remove(0)
        .into_iter()
        .map(|field| field.trim().to_lowercase())
        .collect();
    let index = |name: &str| header.iter().position(|column| column == name);
    let (Some(date_at), Some(kind_at), Some(amount_at)) =
        (index("date"), index("kind"), index("amount"))
    else {
        bail!("the header must contain at least date, kind and amount columns");
    };
    let currency_at = index("currency");
    let category_at = index("category");
    let account_at = index("account");
    let note_at = index("note");
    let merchant_at = index("merchant");

    let pick = |fields: &[String], at: Option<usize>| -> String {
        at.and_then(|at| fields.get(at))
            .map(|value| value.trim().to_string())
            .unwrap_or_default()
    };

    let mut rows = Vec::new();
    for fields in records {
        // 尾部空行、以及只有分隔符的行，直接跳过而不是报错。
        if fields.iter().all(|field| field.trim().is_empty()) {
            continue;
        }
        rows.push(ImportRow {
            date: pick(&fields, Some(date_at)),
            kind: pick(&fields, Some(kind_at)),
            amount: pick(&fields, Some(amount_at)),
            currency: pick(&fields, currency_at),
            category: pick(&fields, category_at),
            account: pick(&fields, account_at),
            note: pick(&fields, note_at),
            merchant: pick(&fields, merchant_at),
        });
    }
    Ok(rows)
}

/// 按 RFC 4180 切成记录与字段：引号内的逗号与换行都属于字段内容。
fn split_records(text: &str) -> Vec<Vec<String>> {
    let mut records = Vec::new();
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        if quoted {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    quoted = false;
                }
            } else {
                field.push(c);
            }
            continue;
        }
        match c {
            '"' => quoted = true,
            ',' => fields.push(std::mem::take(&mut field)),
            '\r' => {}
            '\n' => {
                fields.push(std::mem::take(&mut field));
                records.push(std::mem::take(&mut fields));
            }
            _ => field.push(c),
        }
    }
    if !field.is_empty() || !fields.is_empty() {
        fields.push(field);
        records.push(fields);
    }
    // BOM 会粘在第一个表头上，让 "date" 匹配不上。
    if let Some(first) = records.first_mut() {
        if let Some(head) = first.first_mut() {
            *head = head.trim_start_matches('\u{feff}').to_string();
        }
    }
    records
}
