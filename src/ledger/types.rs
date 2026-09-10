//! 账本领域类型。
//!
//! 所有金额字段都是最小货币单位的 `i64`（见 [`super::money`]），所有时间戳
//! 都是 RFC3339 UTC 字符串（与主库口径一致）。唯一的例外是 `occurred_day`：
//! 那是**本地自然日**的 `YYYY-MM-DD`，见 [`EntryRecord::occurred_day`]。

use anyhow::{bail, Result};
use serde::Serialize;

/// 一笔账的性质。转账不计收支，只在账户之间搬钱。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    Expense,
    Income,
    Transfer,
}

impl EntryKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Expense => "expense",
            Self::Income => "income",
            Self::Transfer => "transfer",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self> {
        match value.trim() {
            "expense" => Ok(Self::Expense),
            "income" => Ok(Self::Income),
            "transfer" => Ok(Self::Transfer),
            other => bail!("unknown entry kind {other:?}; expected expense, income or transfer"),
        }
    }
}

/// 分类的收支方向。分类树按方向分成两棵，避免「餐饮」既能收又能支。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Expense,
    Income,
}

impl Direction {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Expense => "expense",
            Self::Income => "income",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self> {
        match value.trim() {
            "expense" => Ok(Self::Expense),
            "income" => Ok(Self::Income),
            other => bail!("unknown category direction {other:?}; expected expense or income"),
        }
    }
}

/// 换算状态。`Pending` 是网络取汇率失败时的降级形态——账照记，数字待补，
/// 统计里单独列出而不是拿个猜的汇率凑进去。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RateStatus {
    /// 已按快照汇率换算。
    Ok,
    /// 原币种与账本目标币种相同，无需换算。
    Same,
    /// 取汇率失败，`base_amount_minor` 为空，等待补算。
    Pending,
}

impl RateStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Same => "same",
            Self::Pending => "pending",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self> {
        match value.trim() {
            "ok" => Ok(Self::Ok),
            "same" => Ok(Self::Same),
            "pending" => Ok(Self::Pending),
            other => bail!("unknown rate status {other:?}"),
        }
    }
}

/// 账目从哪来。审计用，也让 CSV 导入的批次可被整批回滚。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EntrySource {
    /// 对话里记的。
    Chat,
    /// WebUI 面板里记的。
    Webui,
    /// CSV 导入的。
    Import,
}

impl EntrySource {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Webui => "webui",
            Self::Import => "import",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self> {
        match value.trim() {
            "chat" => Ok(Self::Chat),
            "webui" => Ok(Self::Webui),
            "import" => Ok(Self::Import),
            other => bail!("unknown entry source {other:?}"),
        }
    }
}

/// 账户类型。只影响面板上的图标与分组，不参与任何计算。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AccountKind {
    Cash,
    Bank,
    Ewallet,
    Credit,
    Other,
}

impl AccountKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Cash => "cash",
            Self::Bank => "bank",
            Self::Ewallet => "ewallet",
            Self::Credit => "credit",
            Self::Other => "other",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self> {
        match value.trim() {
            "cash" => Ok(Self::Cash),
            "bank" => Ok(Self::Bank),
            "ewallet" => Ok(Self::Ewallet),
            "credit" => Ok(Self::Credit),
            "other" | "" => Ok(Self::Other),
            other => bail!("unknown account kind {other:?}"),
        }
    }
}

/// 一本账。多本账互相独立：目标币种、账户、分类、预算都不共享。
#[derive(Clone, Debug, Serialize)]
pub struct BookRecord {
    pub book_id: String,
    pub name: String,
    /// 汇总口径币种。所有统计都换算到它。
    pub base_currency: String,
    pub archived: bool,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct AccountRecord {
    pub account_id: String,
    pub book_id: String,
    pub name: String,
    pub kind: AccountKind,
    /// 账户自身的币种，可以与账本目标币种不同。
    pub currency: String,
    /// 建账户时的初始余额，参与余额计算但不算收支。
    pub opening_minor: i64,
    pub archived: bool,
    pub created_at: String,
    pub updated_at: String,
}

/// 一个账户的余额，连同「有几笔没能算进来」。
///
/// 未折算的笔数跟着余额一起走，而不是悄悄丢掉：余额少了一截却没人说一声，
/// 是这套账本最不该出现的那种错。
#[derive(Clone, Debug, Serialize)]
pub struct AccountBalance {
    /// 账户币种的最小单位。
    pub minor: i64,
    pub currency: String,
    pub unconverted_count: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct CategoryRecord {
    pub category_id: String,
    pub book_id: String,
    /// 二级分类的父级；一级分类为空。只做两层，再深就该用标签了。
    pub parent_id: Option<String>,
    pub name: String,
    pub direction: Direction,
    /// 面板上的 emoji，可空。
    pub icon: String,
    pub sort: i64,
    pub archived: bool,
    pub created_at: String,
    pub updated_at: String,
}

/// 一笔流水。
///
/// 双币存储：`amount_minor`/`currency` 是**用户实际花的钱**（3000 JPY），
/// `base_amount_minor`/`base_currency` 是换算到账本口径的结果（147.00 CNY）。
/// 汇率连同来源与时刻一起冻结在 `rate*` 字段里，**永不重算**——历史汇率
/// 后来怎么变，都不该改动已经记下的账。
#[derive(Clone, Debug, Serialize)]
pub struct EntryRecord {
    pub entry_id: String,
    pub book_id: String,
    pub kind: EntryKind,
    pub amount_minor: i64,
    pub currency: String,
    /// 待换算时为空。
    pub base_amount_minor: Option<i64>,
    pub base_currency: String,
    /// 汇率快照，字符串保全精度。
    pub rate: Option<String>,
    pub rate_source: Option<String>,
    pub rate_at: Option<String>,
    pub rate_status: RateStatus,
    /// 支出/收入的账户；转账时是转出方。
    pub account_id: Option<String>,
    /// 仅转账使用：转入方。
    pub to_account_id: Option<String>,
    /// 转账没有分类。
    pub category_id: Option<String>,
    /// 业务发生时刻，RFC3339 UTC。昨天的饭今天补记，统计要按这个算。
    pub occurred_at: String,
    /// 发生时刻对应的**本地自然日**（`YYYY-MM-DD`）。
    ///
    /// 冗余列，但必要：`occurred_at` 是 UTC，SQLite 的 `date()` 直接取会把
    /// 东八区的深夜算进前一天。写入时按本地时区算好一次，查询就退化成
    /// 纯字符串比较，既走索引又不会错位。
    pub occurred_day: String,
    pub note: String,
    pub merchant: String,
    pub source: EntrySource,
    /// 乐观并发：WebUI 与模型同时改一笔时不静默覆盖。
    pub revision: i64,
    /// 软删除。财务数据不做硬删除。
    pub deleted_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// 月度预算。`category_id` 为空表示整本账的总预算。
#[derive(Clone, Debug, Serialize)]
pub struct BudgetRecord {
    pub budget_id: String,
    pub book_id: String,
    pub category_id: Option<String>,
    /// 金额以账本目标币种计。
    pub amount_minor: i64,
    pub active: bool,
    pub created_at: String,
    pub updated_at: String,
}

/// 预算的执行情况，随记账结果一起返回给模型。
#[derive(Clone, Debug, Serialize)]
pub struct BudgetStatus {
    /// `total` 或 `category:<名字>`。
    pub scope: String,
    /// `YYYY-MM`。
    pub period: String,
    pub limit_minor: i64,
    pub used_minor: i64,
    pub currency: String,
    pub state: BudgetState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BudgetState {
    /// 用量 < 80%，不值得打扰。
    Ok,
    /// 用量 ≥ 80% 但未超。
    Near,
    Exceeded,
}

impl BudgetState {
    pub(crate) fn from_usage(used_minor: i64, limit_minor: i64) -> Self {
        if limit_minor <= 0 {
            return Self::Ok;
        }
        if used_minor > limit_minor {
            Self::Exceeded
        } else if used_minor * 100 >= limit_minor * 80 {
            Self::Near
        } else {
            Self::Ok
        }
    }
}
