use crate::llm::Usage;
use anyhow::Result;
use chrono::{Duration as ChronoDuration, Local, TimeZone};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

fn u64_is_zero(value: &u64) -> bool {
    *value == 0
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn usage_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn write_state(path: &Path, state: &UsageState) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    writeln!(file, "{}", serde_json::to_string_pretty(state)?)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[derive(Default, Serialize, Deserialize)]
struct UsageState {
    requests: u64,
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
    #[serde(default)]
    conversation_tokens: u64,
    /// Cumulative provider-cache accounting (v7 Release 1). cache_read is the
    /// portion of prompt_tokens served from the provider's prompt cache.
    #[serde(default)]
    cache_read_tokens: u64,
    #[serde(default)]
    cache_write_tokens: u64,
    #[serde(default)]
    reasoning_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_conversation_usage: Option<Usage>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub struct UsageSnapshot {
    pub requests: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    pub conversation_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: u64,
    pub last_usage: Option<Usage>,
    pub last_conversation_usage: Option<Usage>,
}

impl From<UsageState> for UsageSnapshot {
    fn from(state: UsageState) -> Self {
        let last_conversation_usage = state
            .last_conversation_usage
            .clone()
            .or_else(|| state.last_usage.clone());
        Self {
            requests: state.requests,
            prompt_tokens: state.prompt_tokens,
            completion_tokens: state.completion_tokens,
            total_tokens: state.total_tokens,
            conversation_tokens: state.conversation_tokens,
            cache_read_tokens: state.cache_read_tokens,
            cache_write_tokens: state.cache_write_tokens,
            reasoning_tokens: state.reasoning_tokens,
            last_usage: state.last_usage,
            last_conversation_usage,
        }
    }
}

pub fn add_usage(path: &Path, usage: &Usage) -> Result<()> {
    add_usage_with_scope(path, usage, true)
}

pub fn add_auxiliary_usage(path: &Path, usage: &Usage) -> Result<()> {
    add_usage_with_scope(path, usage, false)
}

fn add_usage_with_scope(path: &Path, usage: &Usage, is_conversation: bool) -> Result<()> {
    let _guard = usage_lock().lock().unwrap();
    let mut state = if path.exists() {
        let raw = std::fs::read_to_string(path)?;
        serde_json::from_str(&raw).unwrap_or_default()
    } else {
        UsageState::default()
    };
    state.requests += 1;
    state.prompt_tokens += usage.prompt_tokens;
    state.completion_tokens += usage.completion_tokens;
    state.total_tokens += usage.effective_total_tokens();
    state.cache_read_tokens += usage.cache_read_tokens;
    state.cache_write_tokens += usage.cache_write_tokens;
    state.reasoning_tokens += usage.reasoning_tokens;
    if is_conversation {
        state.conversation_tokens += usage.effective_total_tokens();
    }
    state.last_usage = Some(usage.clone());
    if is_conversation {
        state.last_conversation_usage = Some(usage.clone());
    }
    write_state(path, &state)
}

pub fn snapshot(path: &Path) -> Result<UsageSnapshot> {
    let _guard = usage_lock().lock().unwrap();
    if !path.exists() {
        return Ok(UsageSnapshot::default());
    }
    let raw = std::fs::read_to_string(path)?;
    let state = serde_json::from_str::<UsageState>(&raw).unwrap_or_default();
    Ok(state.into())
}

pub fn clear_last_usage(path: &Path) -> Result<()> {
    let _guard = usage_lock().lock().unwrap();
    if !path.exists() {
        return Ok(());
    }
    let raw = std::fs::read_to_string(path)?;
    let mut state = serde_json::from_str::<UsageState>(&raw).unwrap_or_default();
    state.last_usage = None;
    state.last_conversation_usage = None;
    write_state(path, &state)
}

pub fn reset_conversation(path: &Path) -> Result<()> {
    let _guard = usage_lock().lock().unwrap();
    if !path.exists() {
        return Ok(());
    }
    let raw = std::fs::read_to_string(path)?;
    let mut state = serde_json::from_str::<UsageState>(&raw).unwrap_or_default();
    state.conversation_tokens = 0;
    state.last_usage = None;
    state.last_conversation_usage = None;
    write_state(path, &state)
}

// ─────────────── 用量历史(usage-history.jsonl):控制台统计数据源 ───────────────
// 每次 LLM 调用追加一行 JSON(O_APPEND 单行原子),全部请求都入账:
// 主对话、子代理、压缩、记忆整理、平台插件……由 StateStore 的
// add_usage/add_auxiliary_usage 包装统一落账,想漏都难。

/// 账本文件名。`StateStore::usage_history_file` 与 TUI(独立进程,拿不到
/// StateStore)共用这一个真相源。
pub const USAGE_HISTORY_FILE: &str = "usage-history.jsonl";

/// 一次 LLM 调用的历史记录。`src` 标来源:"agent"(终端/WebUI/定时/子代理)
/// 或平台 id(如 "qq");旧记录缺省归 "agent"。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageRecord {
    #[serde(default)]
    pub ts: i64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub src: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub provider: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    #[serde(default)]
    pub prompt: u64,
    #[serde(default)]
    pub completion: u64,
    #[serde(default)]
    pub total: u64,
    #[serde(default, skip_serializing_if = "u64_is_zero")]
    pub cache_read: u64,
    #[serde(default, skip_serializing_if = "u64_is_zero")]
    pub cache_write: u64,
    #[serde(default, skip_serializing_if = "is_false")]
    pub aux: bool,
    /// 细项标签(见 [`UsageMeta::kind`]);老记录没有,空串=主线。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub kind: String,
    /// 归属账号 id(阶段 5 多用户);空串 = 管理员/遗留/平台。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub acct: String,
    /// 计费估算(USD),读取时按当前价目计算,不落盘。
    #[serde(skip_deserializing, skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
}

/// 调用元数据,由各埋点交代。provider/model 拿不到就 None(如缓存保活)。
#[derive(Debug, Clone, Copy, Default)]
pub struct UsageMeta<'a> {
    pub source: &'a str,
    pub provider: Option<&'a str>,
    pub model: Option<&'a str>,
    /// 细项标签:同一来源里再分一层(如 "judge"=主动回复判断)。None=主线
    /// 调用。统计据此在来源下挂出子项,不改变来源本身的归属与合计。
    pub kind: Option<&'a str>,
}

/// 主动回复判断(QQ 每条消息都跑)的细项标签。
pub const USAGE_KIND_JUDGE: &str = "judge";
/// 好感度更新。
pub const USAGE_KIND_AFFECTION: &str = "affection";
/// 入群审批。
pub const USAGE_KIND_GROUP_JOIN: &str = "group_join";

pub fn record_usage(path: &Path, usage: &Usage, meta: UsageMeta<'_>, aux: bool) -> Result<()> {
    record_usage_at(path, usage, meta, aux, chrono::Utc::now().timestamp())
}

/// 同 [`record_usage`],记到某个账号名下(空串 = 管理员/遗留)。
pub fn record_usage_for_account(
    path: &Path,
    usage: &Usage,
    meta: UsageMeta<'_>,
    aux: bool,
    account: &str,
) -> Result<()> {
    record_usage_for_account_at(
        path,
        usage,
        meta,
        aux,
        account,
        chrono::Utc::now().timestamp(),
    )
}

/// 持 `usage_lock` 只挡住同进程的并发写:`rename_provider` 要整文件重写,
/// 重写期间的追加会被新文件盖掉。跨进程(`MIYU_DIRECT=1` 直连模式的另一个
/// 进程、TUI 改名)仍有理论上的竞态,接受——账本是统计口径,不是账务。
fn record_usage_at(
    path: &Path,
    usage: &Usage,
    meta: UsageMeta<'_>,
    aux: bool,
    ts: i64,
) -> Result<()> {
    record_usage_for_account_at(path, usage, meta, aux, "", ts)
}

fn record_usage_for_account_at(
    path: &Path,
    usage: &Usage,
    meta: UsageMeta<'_>,
    aux: bool,
    account: &str,
    ts: i64,
) -> Result<()> {
    let _guard = usage_lock().lock().unwrap();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let record = UsageRecord {
        ts,
        src: if meta.source.is_empty() {
            "agent".to_string()
        } else {
            meta.source.to_string()
        },
        provider: meta.provider.unwrap_or_default().to_string(),
        model: meta.model.unwrap_or_default().to_string(),
        prompt: usage.prompt_tokens,
        completion: usage.completion_tokens,
        total: usage.effective_total_tokens(),
        cache_read: usage.cache_read_tokens,
        cache_write: usage.cache_write_tokens,
        aux,
        kind: meta.kind.unwrap_or_default().to_string(),
        acct: account.to_string(),
        cost: None,
    };
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{}", serde_json::to_string(&record)?)?;
    Ok(())
}

fn load_records(path: &Path) -> Result<Vec<UsageRecord>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = std::fs::read_to_string(path)?;
    // 容错逐行解析:坏行跳过,不让一条脏数据废掉整个统计。
    Ok(raw
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<UsageRecord>(line).ok())
        .collect())
}

/// 供应商改名:把账本里 `provider == old` 的行改成 `new`,返回改了几行。
///
/// 账本只追加、从不回写,`provider` 存的是当时配置里的 id 裸字符串。改了 id
/// 而账本不动,统计页会把同一个供应商拆成新旧两行,旧行还因为查不到 base_url
/// 而算不出费用。
///
/// 逐行按 `serde_json::Value` 改再写回:解析不了的脏行原样留着
/// (`load_records` 也是跳过它们,不该被这里顺手清掉)。一行都没改就不落盘,
/// 免得为一次无关保存白白重写几十 MB。
pub fn rename_provider(path: &Path, old: &str, new: &str) -> Result<usize> {
    if old == new || old.is_empty() || !path.exists() {
        return Ok(0);
    }
    let _guard = usage_lock().lock().unwrap();
    let raw = std::fs::read_to_string(path)?;
    let mut renamed = 0usize;
    let mut out = String::with_capacity(raw.len());
    for line in raw.lines() {
        match serde_json::from_str::<serde_json::Value>(line) {
            Ok(mut value) if value.get("provider").and_then(|id| id.as_str()) == Some(old) => {
                value["provider"] = serde_json::Value::String(new.to_string());
                out.push_str(&serde_json::to_string(&value)?);
                renamed += 1;
            }
            _ => out.push_str(line),
        }
        out.push('\n');
    }
    if renamed == 0 {
        return Ok(0);
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(out.as_bytes())?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(renamed)
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct UsageAggregate {
    pub requests: u64,
    pub prompt: u64,
    pub completion: u64,
    pub cache_read: u64,
    pub total: u64,
    /// 计费估算(USD),按 models.dev 单价 × 用量;只累计查得到价的请求。
    pub cost: f64,
    /// 参与计费估算的请求数。< requests 说明部分记录没有价格数据
    /// (自定义中转、目录未收录的模型),前端据此标注估算覆盖率。
    pub costed_requests: u64,
}

impl UsageAggregate {
    fn absorb(&mut self, record: &UsageRecord, cost: Option<f64>) {
        self.requests += 1;
        self.prompt += record.prompt;
        self.completion += record.completion;
        self.cache_read += record.cache_read;
        self.total += record.total;
        if let Some(cost) = cost {
            self.cost += cost;
            self.costed_requests += 1;
        }
    }
}

/// 一个本地自然日的汇总(热力图与柱状图共用)。
#[derive(Debug, Clone, Serialize)]
pub struct DailyUsage {
    pub date: String,
    pub requests: u64,
    pub prompt: u64,
    pub completion: u64,
    pub cache_read: u64,
    pub total: u64,
    /// 计费估算(USD),只含查得到价的请求。
    pub cost: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelUsage {
    pub provider: String,
    pub model: String,
    #[serde(flatten)]
    pub aggregate: UsageAggregate,
}

/// 来源内的细项(如主动回复判断),已含在来源合计里,不重复计数。
/// `models` 让前端把细项摊进饼图与模型表——只给一个合计数字,细项就只能
/// 当页脚摆着(08-26 用户点名)。
#[derive(Debug, Clone, Serialize)]
pub struct KindUsage {
    pub kind: String,
    #[serde(flatten)]
    pub aggregate: UsageAggregate,
    pub models: Vec<ModelUsage>,
}

/// 一个来源(智能体/某平台)在选定范围内的汇总与模型构成。
#[derive(Debug, Clone, Serialize)]
pub struct SourceUsage {
    pub src: String,
    #[serde(flatten)]
    pub aggregate: UsageAggregate,
    pub models: Vec<ModelUsage>,
    /// 细项拆解(见 [`KindUsage`]);无标签记录不产生条目。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kinds: Vec<KindUsage>,
}

/// 一个账号在选定范围内的汇总(阶段 5,按人拆总表)。`acct` 空串 = 管理员/遗留/平台。
#[derive(Debug, Clone, Serialize)]
pub struct AccountUsage {
    pub acct: String,
    #[serde(flatten)]
    pub aggregate: UsageAggregate,
}

#[derive(Debug, Clone, Serialize)]
pub struct UsageStats {
    pub range: String,
    pub totals: UsageAggregate,
    /// 上一个等长窗口(环比基线);"至今" 无基线为 None。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prev_totals: Option<UsageAggregate>,
    /// 最近 364 个本地自然日(含今天),供热力图与柱状图切片。
    pub daily: Vec<DailyUsage>,
    pub sources: Vec<SourceUsage>,
    /// 范围内按账号拆分;只按某个账号过滤时为空(拆分没有意义)。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accounts: Vec<AccountUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_ts: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageRange {
    /// 滚动近 24 小时(非日历日)。
    LastDay,
    Days(u32),
    All,
}

impl UsageRange {
    pub fn parse(value: &str) -> Self {
        match value {
            "1d" | "24h" | "today" => Self::LastDay,
            "7d" => Self::Days(7),
            "30d" => Self::Days(30),
            _ => Self::All,
        }
    }

    fn label(&self) -> String {
        match self {
            Self::LastDay => "1d".to_string(),
            Self::Days(n) => format!("{n}d"),
            Self::All => "all".to_string(),
        }
    }
}

fn local_day_start(ts: i64) -> Option<chrono::DateTime<Local>> {
    let time = Local.timestamp_opt(ts, 0).single()?;
    time.date_naive()
        .and_hms_opt(0, 0, 0)?
        .and_local_timezone(Local)
        .single()
}

/// 计价器:按 (provider, model) 给出单价;None = 无价格数据。
pub type PriceFn<'a> = &'a dyn Fn(&str, &str) -> Option<crate::models_cache::ApiCost>;

pub fn usage_stats(path: &Path, range: UsageRange, price: PriceFn<'_>) -> Result<UsageStats> {
    usage_stats_for_account(path, range, price, None)
}

/// 同 [`usage_stats`];`account` 为 Some 时只看该账号的记录(空串 = 管理员/遗留)。
pub fn usage_stats_for_account(
    path: &Path,
    range: UsageRange,
    price: PriceFn<'_>,
    account: Option<&str>,
) -> Result<UsageStats> {
    let mut records = load_records(path)?;
    if let Some(account) = account {
        records.retain(|record| record.acct == account);
    }
    // 单价按 (provider, model) 记忆化:每条记录都查一次目录锁太浪费。
    let mut price_cache =
        std::collections::HashMap::<(String, String), Option<crate::models_cache::ApiCost>>::new();
    let mut record_cost = |record: &UsageRecord| -> Option<f64> {
        price_cache
            .entry((record.provider.clone(), record.model.clone()))
            .or_insert_with(|| price(&record.provider, &record.model))
            .map(|c| {
                c.estimate(
                    record.prompt,
                    record.completion,
                    record.cache_read,
                    record.cache_write,
                )
            })
    };
    let now = chrono::Utc::now().timestamp();
    // 范围窗口按本地自然日对齐:今天=本地零点起;7d/30d=含今天往前 n 天。
    let today_start = local_day_start(now).map(|t| t.timestamp()).unwrap_or(now);
    let (start, prev_start) = match range {
        UsageRange::LastDay => (Some(now - 86_400), Some(now - 2 * 86_400)),
        UsageRange::Days(n) => {
            let start = today_start - i64::from(n - 1) * 86_400;
            (Some(start), Some(start - i64::from(n) * 86_400))
        }
        UsageRange::All => (None, None),
    };

    let mut totals = UsageAggregate::default();
    let mut prev_totals = UsageAggregate::default();
    let mut daily = BTreeMap::<String, DailyUsage>::new();
    #[allow(clippy::type_complexity)]
    let mut sources = BTreeMap::<
        String,
        (
            UsageAggregate,
            BTreeMap<(String, String), UsageAggregate>,
            BTreeMap<String, (UsageAggregate, BTreeMap<(String, String), UsageAggregate>)>,
        ),
    >::new();
    let daily_floor = today_start - 363 * 86_400;
    let mut first_ts: Option<i64> = None;
    let mut accounts = BTreeMap::<String, UsageAggregate>::new();

    for record in &records {
        first_ts = Some(first_ts.map_or(record.ts, |t| t.min(record.ts)));
        let cost = record_cost(record);
        let in_range = start.map_or(true, |s| record.ts >= s);
        if in_range {
            totals.absorb(record, cost);
            if account.is_none() {
                accounts
                    .entry(record.acct.clone())
                    .or_default()
                    .absorb(record, cost);
            }
            let src = if record.src.is_empty() {
                "agent"
            } else {
                record.src.as_str()
            };
            let (agg, models, kinds) = sources.entry(src.to_string()).or_default();
            agg.absorb(record, cost);
            models
                .entry((record.provider.clone(), record.model.clone()))
                .or_default()
                .absorb(record, cost);
            if !record.kind.is_empty() {
                let (kind_agg, kind_models) = kinds.entry(record.kind.clone()).or_default();
                kind_agg.absorb(record, cost);
                kind_models
                    .entry((record.provider.clone(), record.model.clone()))
                    .or_default()
                    .absorb(record, cost);
            }
        } else if let (Some(s), Some(p)) = (start, prev_start) {
            if record.ts >= p && record.ts < s {
                prev_totals.absorb(record, cost);
            }
        }
        if record.ts >= daily_floor {
            if let Some(day) = local_day_start(record.ts) {
                let key = day.format("%Y-%m-%d").to_string();
                let entry = daily.entry(key.clone()).or_insert_with(|| DailyUsage {
                    date: key,
                    requests: 0,
                    prompt: 0,
                    completion: 0,
                    cache_read: 0,
                    total: 0,
                    cost: 0.0,
                });
                entry.requests += 1;
                entry.prompt += record.prompt;
                entry.completion += record.completion;
                entry.cache_read += record.cache_read;
                entry.total += record.total;
                entry.cost += cost.unwrap_or(0.0);
            }
        }
    }

    // 补齐 364 天里没有记录的日子(前端网格要连续)。
    if let Some(today) = local_day_start(now) {
        for offset in 0i64..364 {
            let day = today - ChronoDuration::days(363 - offset);
            let key = day.format("%Y-%m-%d").to_string();
            daily.entry(key.clone()).or_insert(DailyUsage {
                date: key,
                requests: 0,
                prompt: 0,
                completion: 0,
                cache_read: 0,
                total: 0,
                cost: 0.0,
            });
        }
    }
    let daily: Vec<DailyUsage> = daily.into_values().collect();
    let daily = daily
        .into_iter()
        .rev()
        .take(364)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();

    let sources = sources
        .into_iter()
        .map(|(src, (aggregate, models, kinds))| {
            let mut models: Vec<ModelUsage> = models
                .into_iter()
                .map(|((provider, model), aggregate)| ModelUsage {
                    provider,
                    model,
                    aggregate,
                })
                .collect();
            models.sort_by(|a, b| b.aggregate.total.cmp(&a.aggregate.total));
            let mut kinds: Vec<KindUsage> = kinds
                .into_iter()
                .map(|(kind, (aggregate, kind_models))| {
                    let mut models: Vec<ModelUsage> = kind_models
                        .into_iter()
                        .map(|((provider, model), aggregate)| ModelUsage {
                            provider,
                            model,
                            aggregate,
                        })
                        .collect();
                    models.sort_by(|a, b| b.aggregate.total.cmp(&a.aggregate.total));
                    KindUsage {
                        kind,
                        aggregate,
                        models,
                    }
                })
                .collect();
            kinds.sort_by(|a, b| b.aggregate.total.cmp(&a.aggregate.total));
            SourceUsage {
                src,
                aggregate,
                models,
                kinds,
            }
        })
        .collect::<Vec<_>>();
    // 智能体排最前,平台按名称序。
    let mut sources = sources;
    sources.sort_by(|a, b| {
        let rank = |s: &SourceUsage| if s.src == "agent" { 0 } else { 1 };
        rank(a).cmp(&rank(b)).then_with(|| a.src.cmp(&b.src))
    });

    let mut accounts: Vec<AccountUsage> = accounts
        .into_iter()
        .map(|(acct, aggregate)| AccountUsage { acct, aggregate })
        .collect();
    accounts.sort_by(|a, b| b.aggregate.total.cmp(&a.aggregate.total));

    Ok(UsageStats {
        range: range.label(),
        totals,
        prev_totals: (range != UsageRange::All).then_some(prev_totals),
        daily,
        sources,
        accounts,
        first_ts,
    })
}

/// 最近 `limit` 条调用记录,新的在前;可按来源/模型过滤。按 ts 排序而非
/// 文件顺序:追加序通常就是时间序,但时钟回拨或手工并档后也要给出正确的"最近"。
pub fn usage_details(
    path: &Path,
    limit: usize,
    src: Option<&str>,
    model: Option<&str>,
    price: PriceFn<'_>,
) -> Result<Vec<UsageRecord>> {
    usage_details_for_account(path, limit, src, model, price, None)
}

/// 同 [`usage_details`];`account` 为 Some 时只看该账号的记录。
pub fn usage_details_for_account(
    path: &Path,
    limit: usize,
    src: Option<&str>,
    model: Option<&str>,
    price: PriceFn<'_>,
    account: Option<&str>,
) -> Result<Vec<UsageRecord>> {
    let mut records = load_records(path)?;
    if let Some(account) = account {
        records.retain(|record| record.acct == account);
    }
    if let Some(src) = src.filter(|value| !value.is_empty()) {
        records.retain(|record| {
            let record_src = if record.src.is_empty() {
                "agent"
            } else {
                record.src.as_str()
            };
            record_src == src
        });
    }
    if let Some(model) = model.filter(|value| !value.is_empty()) {
        records.retain(|record| record.model == model);
    }
    records.sort_by_key(|record| std::cmp::Reverse(record.ts));
    records.truncate(limit);
    let mut price_cache =
        std::collections::HashMap::<(String, String), Option<crate::models_cache::ApiCost>>::new();
    for record in &mut records {
        record.cost = price_cache
            .entry((record.provider.clone(), record.model.clone()))
            .or_insert_with(|| price(&record.provider, &record.model))
            .map(|c| {
                c.estimate(
                    record.prompt,
                    record.completion,
                    record.cache_read,
                    record.cache_write,
                )
            });
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 计费估算:有价的记录累计 cost/costed_requests,无价的只计用量。
    #[test]
    fn usage_stats_estimates_cost_with_resolver() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("usage-history.jsonl");
        let usage = Usage {
            prompt_tokens: 2_000_000,
            completion_tokens: 1_000_000,
            total_tokens: 3_000_000,
            cache_read_tokens: 1_000_000,
            ..Usage::default()
        };
        record_usage(
            &path,
            &usage,
            UsageMeta {
                source: "agent",
                provider: Some("priced"),
                model: Some("m"),
                kind: None,
            },
            false,
        )
        .unwrap();
        record_usage(
            &path,
            &usage,
            UsageMeta {
                source: "agent",
                provider: Some("unknown"),
                model: Some("m"),
                kind: None,
            },
            false,
        )
        .unwrap();
        let price = |provider: &str, _model: &str| {
            (provider == "priced").then_some(crate::models_cache::ApiCost {
                input: 1.0,
                output: 2.0,
                cache_read: Some(0.1),
                cache_write: None,
            })
        };
        let stats = usage_stats(&path, UsageRange::All, &price).unwrap();
        assert_eq!(stats.totals.requests, 2);
        assert_eq!(stats.totals.costed_requests, 1);
        // 未命中 100 万×1 + 命中 100 万×0.1 + 输出 100 万×2 = 3.1
        assert!(
            (stats.totals.cost - 3.1).abs() < 1e-9,
            "{}",
            stats.totals.cost
        );
        let day_cost: f64 = stats.daily.iter().map(|d| d.cost).sum();
        assert!((day_cost - 3.1).abs() < 1e-9);
        let details = usage_details(&path, 10, None, None, &price).unwrap();
        let priced: Vec<_> = details.iter().filter(|r| r.cost.is_some()).collect();
        assert_eq!(priced.len(), 1);
        assert!((priced[0].cost.unwrap() - 3.1).abs() < 1e-9);
    }

    #[test]
    fn records_and_clears_last_usage() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("usage.json");
        let usage = Usage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
            ..Usage::default()
        };

        add_usage(&path, &usage).unwrap();
        let usage_snapshot = snapshot(&path).unwrap();
        assert_eq!(usage_snapshot.last_usage.unwrap().total_tokens, 15);
        assert_eq!(
            usage_snapshot
                .last_conversation_usage
                .unwrap()
                .prompt_tokens,
            10
        );

        clear_last_usage(&path).unwrap();
        let usage_snapshot = snapshot(&path).unwrap();
        assert_eq!(usage_snapshot.total_tokens, 15);
        assert!(usage_snapshot.last_usage.is_none());
        assert!(usage_snapshot.last_conversation_usage.is_none());
    }

    #[test]
    fn auxiliary_usage_does_not_replace_conversation_usage() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("usage.json");

        add_usage(
            &path,
            &Usage {
                prompt_tokens: 100,
                completion_tokens: 20,
                total_tokens: 120,
                ..Usage::default()
            },
        )
        .unwrap();
        add_auxiliary_usage(
            &path,
            &Usage {
                prompt_tokens: 5,
                completion_tokens: 2,
                total_tokens: 7,
                ..Usage::default()
            },
        )
        .unwrap();

        let snapshot = snapshot(&path).unwrap();
        assert_eq!(snapshot.total_tokens, 127);
        assert_eq!(snapshot.last_usage.unwrap().prompt_tokens, 5);
        assert_eq!(snapshot.last_conversation_usage.unwrap().prompt_tokens, 100);
    }

    #[test]
    fn total_tokens_falls_back_to_prompt_plus_completion() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("usage.json");

        add_usage(
            &path,
            &Usage {
                prompt_tokens: 7,
                completion_tokens: 3,
                total_tokens: 0,
                ..Usage::default()
            },
        )
        .unwrap();

        let snapshot = snapshot(&path).unwrap();
        assert_eq!(snapshot.total_tokens, 10);
        assert_eq!(snapshot.conversation_tokens, 10);
    }

    #[test]
    fn usage_history_records_and_aggregates_by_source() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("usage-history.jsonl");
        let now = chrono::Utc::now().timestamp();
        let usage = |prompt: u64, completion: u64, cache: u64| Usage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: prompt + completion,
            cache_read_tokens: cache,
            ..Usage::default()
        };
        let meta = |source, model| UsageMeta {
            source,
            provider: Some("prov"),
            model: Some(model),
            kind: None,
        };

        record_usage_at(&path, &usage(100, 20, 60), meta("agent", "m-a"), false, now).unwrap();
        record_usage_at(
            &path,
            &usage(50, 10, 0),
            meta("qq", "m-b"),
            false,
            now - 3_600,
        )
        .unwrap();
        record_usage_at(
            &path,
            &usage(30, 5, 10),
            meta("qq", "m-b"),
            true,
            now - 40 * 86_400,
        )
        .unwrap();

        let all = usage_stats(&path, UsageRange::All, &|_, _| None).unwrap();
        assert_eq!(all.totals.requests, 3);
        assert_eq!(all.totals.prompt, 180);
        assert_eq!(all.totals.cache_read, 70);
        assert!(all.prev_totals.is_none());
        assert_eq!(all.daily.len(), 364);
        assert_eq!(all.sources.len(), 2);
        assert_eq!(all.sources[0].src, "agent"); // 智能体排最前
        assert_eq!(all.sources[1].src, "qq");
        assert_eq!(all.sources[1].aggregate.requests, 2);
        assert_eq!(all.sources[1].models[0].model, "m-b");

        let week = usage_stats(&path, UsageRange::Days(7), &|_, _| None).unwrap();
        assert_eq!(week.totals.requests, 2); // 40 天前的那条不在窗口
        assert!(week.prev_totals.is_some());

        let details = usage_details(&path, 2, None, None, &|_, _| None).unwrap();
        assert_eq!(details.len(), 2);
        assert_eq!(details[0].model, "m-a"); // 新的在前
        assert!(!details[0].aux);

        let only_qq = usage_details(&path, 10, Some("qq"), None, &|_, _| None).unwrap();
        assert_eq!(only_qq.len(), 2);
        assert!(only_qq.iter().all(|record| record.src == "qq"));
        let only_model = usage_details(&path, 10, None, Some("m-b"), &|_, _| None).unwrap();
        assert_eq!(only_model.len(), 2);
    }

    #[test]
    fn usage_history_tolerates_corrupt_lines_and_defaults_source() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("usage-history.jsonl");
        std::fs::write(
            &path,
            format!(
                "{{\"ts\":{ts},\"prompt\":10,\"completion\":2,\"total\":12}}\nnot-json\n",
                ts = chrono::Utc::now().timestamp()
            ),
        )
        .unwrap();
        let stats = usage_stats(&path, UsageRange::All, &|_, _| None).unwrap();
        assert_eq!(stats.totals.requests, 1);
        assert_eq!(stats.sources[0].src, "agent"); // 旧记录缺 src 归智能体
    }

    #[test]
    fn reset_conversation_preserves_global_total() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("usage.json");
        add_usage(
            &path,
            &Usage {
                prompt_tokens: 7,
                completion_tokens: 3,
                total_tokens: 10,
                ..Usage::default()
            },
        )
        .unwrap();

        reset_conversation(&path).unwrap();
        let snapshot = snapshot(&path).unwrap();
        assert_eq!(snapshot.total_tokens, 10);
        assert_eq!(snapshot.conversation_tokens, 0);
        assert!(snapshot.last_conversation_usage.is_none());
    }

    fn seed_history(path: &Path, providers: &[&str]) {
        for provider in providers {
            record_usage(
                path,
                &Usage {
                    prompt_tokens: 10,
                    completion_tokens: 5,
                    total_tokens: 15,
                    ..Usage::default()
                },
                UsageMeta {
                    source: "agent",
                    provider: Some(provider),
                    model: Some("m"),
                    kind: None,
                },
                false,
            )
            .unwrap();
        }
    }

    /// 改名只动 provider 对得上的行,别的行一个字节不碰。
    #[test]
    fn rename_provider_rewrites_matching_rows_only() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(USAGE_HISTORY_FILE);
        seed_history(&path, &["old", "old", "other"]);

        assert_eq!(rename_provider(&path, "old", "new").unwrap(), 2);

        let records = load_records(&path).unwrap();
        assert_eq!(records.len(), 3);
        let providers: Vec<&str> = records.iter().map(|item| item.provider.as_str()).collect();
        assert_eq!(providers, ["new", "new", "other"]);
        assert!(records.iter().all(|item| item.total == 15));
    }

    /// 解析不了的脏行原样留着——load_records 也是跳过它们,不该被改名顺手清掉。
    #[test]
    fn rename_provider_keeps_unparseable_lines() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(USAGE_HISTORY_FILE);
        seed_history(&path, &["old"]);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(file, "not json").unwrap();
        drop(file);
        seed_history(&path, &["old"]);

        assert_eq!(rename_provider(&path, "old", "new").unwrap(), 2);

        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(raw.lines().count(), 3);
        assert!(raw.lines().any(|line| line == "not json"));
        assert_eq!(load_records(&path).unwrap().len(), 2);
    }

    /// 没有一行匹配就不落盘:账本几十 MB,不为一次无关保存白白重写。
    #[test]
    fn rename_provider_without_matches_does_not_touch_the_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(USAGE_HISTORY_FILE);
        seed_history(&path, &["other"]);
        let before = std::fs::read_to_string(&path).unwrap();
        let stamp = std::fs::metadata(&path).unwrap().modified().unwrap();

        assert_eq!(rename_provider(&path, "missing", "new").unwrap(), 0);
        // 同名、空旧名、文件不存在都直接返回 0。
        assert_eq!(rename_provider(&path, "other", "other").unwrap(), 0);
        assert_eq!(rename_provider(&path, "", "new").unwrap(), 0);
        assert_eq!(
            rename_provider(&temp.path().join("absent.jsonl"), "other", "new").unwrap(),
            0
        );

        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), stamp);
    }
}

#[cfg(test)]
mod history_scaling_probe {
    use super::*;

    /// 造一份 `n` 条的历史，形状照抄真实文件（151 字节/条）。
    pub(super) fn write_history_for_probe(path: &Path, records: usize) -> std::io::Result<()> {
        write_history(path, records)
    }

    fn write_history(path: &Path, records: usize) -> std::io::Result<()> {
        let mut body = String::with_capacity(records * 160);
        let base = Local::now().timestamp() - records as i64 * 30;
        for index in 0..records {
            let ts = base + index as i64 * 30;
            body.push_str(&format!(
                r#"{{"ts":{ts},"src":"agent","provider":"opencodego","model":"deepseek-v4-flash","prompt":7638,"completion":2842,"total":10480,"cache_read":7552,"aux":true}}"#
            ));
            body.push('\n');
        }
        std::fs::write(path, body)
    }

    /// 量尺：`cargo test --lib history_scaling_probe -- --ignored --nocapture`
    ///
    /// `usage-history.jsonl` 只增不轮转，而每次查询都 `read_to_string` 整读整
    /// 解析。本机实测 14,511 条 / 2.2 MB 是 **5.7 天**攒出来的（约 387 KB/天），
    /// 所以下面这几档分别对应 5.7 天 / 1 个月 / 半年 / 1 年。
    #[test]
    #[ignore]
    fn usage_stats_scales_with_history_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage-history.jsonl");
        println!(
            "\n  {:<16}{:>10}{:>10}{:>12}",
            "条数", "文件", "耗时", "折合时长"
        );
        for (records, span) in [
            (14_511usize, "5.7 天（当前）"),
            (76_000, "1 个月"),
            (460_000, "半年"),
            (930_000, "1 年"),
        ] {
            write_history(&path, records).unwrap();
            let bytes = std::fs::metadata(&path).unwrap().len();
            let started = std::time::Instant::now();
            let stats = usage_stats(&path, crate::state::UsageRange::parse("1d"), &|_, _| None);
            let elapsed = started.elapsed();
            assert!(stats.is_ok());
            println!(
                "  {records:<16}{:>8.1}MB{:>9.1}ms{:>14}",
                bytes as f64 / 1048576.0,
                elapsed.as_secs_f64() * 1000.0,
                span
            );
        }
    }
}

#[cfg(test)]
mod runtime_freeze_probe {
    use super::history_scaling_probe::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// 量尺：`cargo test --lib runtime_freeze_probe -- --ignored --nocapture`
    ///
    /// 要证明的不是「统计变快了」（活儿一样多），而是**统计期间别的异步任务还
    /// 转不转**。
    ///
    /// 关键坑（C17 那次差点栽在这上面）：被测的活儿必须用 `tokio::spawn` 丢到
    /// worker 上。写成 `runtime.block_on(...)` 的话它跑在**调用线程**，而 ticker
    /// 在 worker 上，两边根本不抢同一个线程，量出来两种写法一样快，结论正好
    /// 反过来。
    #[test]
    #[ignore]
    fn stats_query_does_not_stall_other_tasks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage-history.jsonl");
        write_history_for_probe(&path, 76_000).unwrap();

        println!("\n  单 worker 运行时上，统计跑着的时候 ticker 还能跳几次");
        for (label, blocking) in [
            ("同步调用（改前）", false),
            ("spawn_blocking（改后）", true),
        ] {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .unwrap();
            let ticks = Arc::new(AtomicUsize::new(0));
            let counter = ticks.clone();
            let path = path.clone();
            runtime.block_on(async move {
                let ticker = tokio::spawn(async move {
                    for _ in 0..200 {
                        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                        counter.fetch_add(1, Ordering::Relaxed);
                    }
                });
                let work = tokio::spawn(async move {
                    let range = crate::state::UsageRange::parse("1d");
                    if blocking {
                        tokio::task::spawn_blocking(move || {
                            super::usage_stats(&path, range, &|_, _| None)
                        })
                        .await
                        .unwrap()
                        .unwrap();
                    } else {
                        super::usage_stats(&path, range, &|_, _| None).unwrap();
                    }
                });
                work.await.unwrap();
                let during = ticks.load(Ordering::Relaxed);
                ticker.abort();
                println!("  {label:<26}{during:>4} 次");
            });
        }
    }
}
