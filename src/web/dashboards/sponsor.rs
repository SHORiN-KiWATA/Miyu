//! 赞助面板的 HTTP 面:概览 + 榜单 / 明细分页 / 记一笔 / 改备注 / 删一笔。
//! 存储层在 `state/conversation_db/sponsors.rs`,这里只做校验、编排与出口格式化。
//!
//! **金额全程整数分,只在出口格式化一次**。让 JS 自己拿 minor 去除以 100 就等于
//! 把舍入规则复制了一份,两份迟早对不上账;所以每个金额都随身带一个已经排好版
//! 的字符串,前端只负责显示。
//!
//! **面板不联网取汇率**。这是管理员自己的台面:他手上就有那笔钱的人民币数,直接
//! 填进来即可。美元记录没给折算值就落 `unconverted`,榜上不计、明细里挂个牌子说
//! 明——宁可榜上少一笔,也不能把 0 当成"这人赞助了 0 元"糊过去。工具侧(模型在
//! 群里记账)才需要现拉汇率,那是另一条路径。

use crate::state::{NewSponsorRecord, SponsorOrder, SponsorRecord, SponsorSummary, SponsorTotal};
use crate::web::*;

/// 榜单默认长度与硬顶。
const DEFAULT_LEADERBOARD: usize = 20;
const MAX_LEADERBOARD: usize = 100;
/// 明细一页默认条数与硬顶。
const DEFAULT_RECORDS: usize = 50;
const MAX_RECORDS: usize = 200;
/// 一笔金额的区间(分):一分到一亿元。
const MIN_AMOUNT_MINOR: i64 = 1;
const MAX_AMOUNT_MINOR: i64 = 10_000_000_000;
const MAX_NOTE_CHARS: usize = 200;
/// 记账来源标记:面板记的这笔不来自任何会话。与好感度事件的 "dashboard" 同义。
const DASHBOARD_ORIGIN: &str = "dashboard";

#[derive(Deserialize)]
pub(in crate::web) struct OverviewQuery {
    #[serde(default)]
    order: String,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize)]
pub(in crate::web) struct RecordsQuery {
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    sponsor_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::web) struct CreateBody {
    #[serde(default)]
    sponsor_id: String,
    #[serde(default)]
    sponsor_name: String,
    #[serde(default)]
    amount_minor: i64,
    #[serde(default)]
    currency: String,
    /// 折算好的人民币(分)。只对美元有意义,给了就认。
    #[serde(default)]
    cny_minor: Option<i64>,
    #[serde(default)]
    note: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::web) struct PatchBody {
    #[serde(default)]
    note: Option<String>,
    #[serde(default)]
    sponsor_name: Option<String>,
}

async fn blocking<T, F>(work: F) -> std::result::Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(ApiError::internal)?
        .map_err(|error| ApiError::internal(safe_error_message(&error)))
}

fn bad(message: &str) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, message)
}

fn clamp(value: Option<usize>, default: usize, max: usize) -> usize {
    value.filter(|value| *value > 0).unwrap_or(default).min(max)
}

/* ── 出口格式化 ───────────────────────────────────────── */

/// 分 → 带千分位的显示串。整除取整、取余拿分,全程整数:钱一旦过一趟 f64 就有
/// 了自己的舍入史,而这里的数字是要给人对账的。
fn money(minor: i64, currency: &str) -> String {
    let symbol = if currency == "USD" { "$" } else { "¥" };
    let sign = if minor < 0 { "-" } else { "" };
    let absolute = minor.unsigned_abs();
    let digits = (absolute / 100).to_string();
    let mut major = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            major.push(',');
        }
        major.push(digit);
    }
    format!("{sign}{symbol}{major}.{:02}", absolute % 100)
}

/// 按币种分列的合计:键序由 BTreeMap 定,前端拿到的顺序恒定。
fn currency_rows(by_currency: &std::collections::BTreeMap<String, i64>) -> Vec<Value> {
    by_currency
        .iter()
        .map(|(currency, minor)| {
            json!({ "currency": currency, "minor": minor, "text": money(*minor, currency) })
        })
        .collect()
}

/// 一笔的对外形状。`converted=false` 的那笔不进人民币榜,前端必须把它标出来。
fn record_json(record: &SponsorRecord) -> Value {
    let converted = record.fx_source != "unconverted";
    json!({
        "record_id": record.record_id,
        "sponsor_id": record.sponsor_id,
        "sponsor_name": record.sponsor_name,
        "amount_minor": record.amount_minor,
        "amount_text": money(record.amount_minor, &record.currency),
        "currency": record.currency,
        "cny_minor": record.cny_minor,
        "cny_text": money(record.cny_minor, "CNY"),
        "converted": converted,
        "fx_rate": record.fx_rate,
        "fx_source": record.fx_source,
        "note": record.note,
        "recorded_by": record.recorded_by,
        "sponsored_at": record.sponsored_at,
        "updated_at": record.updated_at,
    })
}

fn total_json(rank: usize, total: &SponsorTotal) -> Value {
    json!({
        "rank": rank,
        "sponsor_id": total.sponsor_id,
        "sponsor_name": total.sponsor_name,
        "cny_minor": total.cny_minor,
        "cny_text": money(total.cny_minor, "CNY"),
        "by_currency": currency_rows(&total.by_currency),
        "record_count": total.record_count,
        "first_sponsored_at": total.first_sponsored_at,
        "last_sponsored_at": total.last_sponsored_at,
    })
}

fn summary_json(summary: &SponsorSummary) -> Value {
    json!({
        "sponsor_count": summary.sponsor_count,
        "record_count": summary.record_count,
        "cny_minor": summary.cny_minor,
        "cny_text": money(summary.cny_minor, "CNY"),
        "by_currency": currency_rows(&summary.by_currency),
        "last_sponsored_at": summary.last_sponsored_at,
    })
}

/* ── 校验 ─────────────────────────────────────────────── */

/// 币种只认这两个。大小写随手写,归一化成库里存的大写。
fn parse_currency(raw: &str) -> std::result::Result<String, ApiError> {
    match raw.trim().to_ascii_uppercase().as_str() {
        "" | "CNY" => Ok("CNY".to_string()),
        "USD" => Ok("USD".to_string()),
        other => Err(bad(&format!(
            "unsupported currency {other}; expected CNY or USD"
        ))),
    }
}

/// 备注按字符数截断前先量长度:超了就报错而不是悄悄截短——面板上人正看着自己
/// 打的字,吞掉一半比拒绝更难懂。
fn check_note(note: &str) -> std::result::Result<String, ApiError> {
    let note = note.trim();
    if note.chars().count() > MAX_NOTE_CHARS {
        return Err(bad(&format!(
            "note is too long; keep it under {MAX_NOTE_CHARS} characters"
        )));
    }
    Ok(note.to_string())
}

/// 表单 → 待落库的一笔。整段校验放在一个纯函数里,测试打得到。
fn new_record(body: CreateBody) -> std::result::Result<NewSponsorRecord, ApiError> {
    let sponsor_id = body.sponsor_id.trim().to_string();
    if sponsor_id.is_empty() {
        return Err(bad("sponsor_id is required"));
    }
    if !(MIN_AMOUNT_MINOR..=MAX_AMOUNT_MINOR).contains(&body.amount_minor) {
        return Err(bad(&format!(
            "amount_minor must be between {MIN_AMOUNT_MINOR} and {MAX_AMOUNT_MINOR} minor units"
        )));
    }
    let currency = parse_currency(&body.currency)?;
    let note = check_note(&body.note)?;
    // 人民币不需要折算,cny 就是它自己;调用方即使传了 cny_minor 也不理会——那
    // 是"同一个数的另一种写法",认它只会开出一条两个数不相等的口子。
    let (cny_minor, fx_rate, fx_source) = if currency == "CNY" {
        (body.amount_minor, 0.0, String::new())
    } else {
        match body.cny_minor.filter(|minor| *minor > 0) {
            // 管理员自己填的折算值:反推一个隐含汇率存进去,行里的
            // cny ≈ amount × rate 这条不变式不能因为来源不同就断掉。
            Some(cny_minor) => {
                if cny_minor > MAX_AMOUNT_MINOR {
                    return Err(bad("cny_minor is out of range"));
                }
                (
                    cny_minor,
                    cny_minor as f64 / body.amount_minor as f64,
                    "manual".to_string(),
                )
            }
            None => (0, 0.0, "unconverted".to_string()),
        }
    };
    Ok(NewSponsorRecord {
        platform: DASHBOARD_ORIGIN.to_string(),
        account_id: String::new(),
        sponsor_id,
        sponsor_name: body.sponsor_name.trim().to_string(),
        amount_minor: body.amount_minor,
        currency,
        cny_minor,
        fx_rate,
        fx_source,
        note,
        recorded_by: DASHBOARD_ORIGIN.to_string(),
        sponsored_at: String::new(),
    })
}

/* ── 路由 ─────────────────────────────────────────────── */

pub(in crate::web) async fn dash_sponsors_overview(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(query): Query<OverviewQuery>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_admin(&headers, &state)?;
    let order = SponsorOrder::parse(&query.order);
    let limit = clamp(query.limit, DEFAULT_LEADERBOARD, MAX_LEADERBOARD);
    let store = state.state_store.clone();
    let (summary, totals) = blocking(move || {
        Ok((
            store.sponsor_summary()?,
            store.sponsor_totals(order, limit)?,
        ))
    })
    .await?;
    Ok(Json(json!({
        "summary": summary_json(&summary),
        "leaderboard": totals
            .iter()
            .enumerate()
            .map(|(index, total)| total_json(index + 1, total))
            .collect::<Vec<_>>(),
    })))
}

pub(in crate::web) async fn dash_sponsors_records(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(query): Query<RecordsQuery>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_admin(&headers, &state)?;
    let limit = clamp(query.limit, DEFAULT_RECORDS, MAX_RECORDS);
    let offset = query.offset.unwrap_or(0);
    let sponsor_id = query.sponsor_id.trim().to_string();
    let store = state.state_store.clone();
    // 多要一条来判断"还有没有下一页":存储层没有 count,而为了一个页码去加一条
    // SQL 不值得——LIMIT n+1 是同一次查询里就能拿到的答案。
    let mut records = blocking(move || {
        if sponsor_id.is_empty() {
            store.sponsor_records(limit + 1, offset)
        } else {
            // 按人筛没有 offset 口径,从头取到本页末尾再切,页数本来就很少。
            let mut rows = store.sponsor_records_for(&sponsor_id, offset + limit + 1)?;
            rows.drain(..offset.min(rows.len()));
            Ok(rows)
        }
    })
    .await?;
    let has_more = records.len() > limit;
    records.truncate(limit);
    Ok(Json(json!({
        "records": records.iter().map(record_json).collect::<Vec<_>>(),
        "has_more": has_more,
        "limit": limit,
        "offset": offset,
    })))
}

pub(in crate::web) async fn dash_sponsors_create(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Json(body): Json<CreateBody>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_admin_mutation(&headers, &state)?;
    let new = new_record(body)?;
    let store = state.state_store.clone();
    let record = blocking(move || store.add_sponsor_record(&new)).await?;
    Ok(Json(json!({ "ok": true, "record": record_json(&record) })))
}

pub(in crate::web) async fn dash_sponsors_patch(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(record_id): Path<i64>,
    Json(body): Json<PatchBody>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_admin_mutation(&headers, &state)?;
    if body.note.is_none() && body.sponsor_name.is_none() {
        return Err(bad("give note or sponsor_name"));
    }
    let note = body.note.as_deref().map(check_note).transpose()?;
    let sponsor_name = body
        .sponsor_name
        .as_deref()
        .map(|name| name.trim().to_string());
    let store = state.state_store.clone();
    let updated = blocking(move || {
        store.update_sponsor_record(record_id, note.as_deref(), sponsor_name.as_deref())
    })
    .await?;
    updated
        .map(|record| Json(json!({ "ok": true, "record": record_json(&record) })))
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "sponsorship record not found"))
}

pub(in crate::web) async fn dash_sponsors_delete(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(record_id): Path<i64>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_admin_mutation(&headers, &state)?;
    let store = state.state_store.clone();
    let removed = blocking(move || store.delete_sponsor_record(record_id)).await?;
    if !removed {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "sponsorship record not found",
        ));
    }
    Ok(Json(json!({ "ok": true })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(value: Value) -> CreateBody {
        serde_json::from_value(value).expect("body")
    }

    fn error(value: Value) -> ApiError {
        new_record(body(value)).expect_err("should be refused")
    }

    #[test]
    fn sponsor_id_is_required() {
        for empty in ["", "   "] {
            let error = error(json!({ "sponsor_id": empty, "amount_minor": 100 }));
            assert_eq!(error.status, StatusCode::BAD_REQUEST);
            assert!(error.message.contains("sponsor_id"), "{}", error.message);
        }
    }

    #[test]
    fn amounts_outside_the_range_are_refused() {
        for amount in [0, -1, MAX_AMOUNT_MINOR + 1] {
            let error = error(json!({ "sponsor_id": "10001", "amount_minor": amount }));
            assert_eq!(error.status, StatusCode::BAD_REQUEST);
            assert!(error.message.contains("amount_minor"), "{}", error.message);
        }
        // 边界两端都是合法的一笔。
        for amount in [MIN_AMOUNT_MINOR, MAX_AMOUNT_MINOR] {
            assert!(new_record(body(
                json!({ "sponsor_id": "10001", "amount_minor": amount })
            ))
            .is_ok());
        }
    }

    #[test]
    fn only_cny_and_usd_are_accepted() {
        for good in ["", "cny", "CNY", "usd", "Usd"] {
            let record = new_record(body(
                json!({ "sponsor_id": "1", "amount_minor": 100, "currency": good }),
            ))
            .expect(good);
            assert!(record.currency == "CNY" || record.currency == "USD");
        }
        for bad in ["JPY", "eur", "元"] {
            let error = error(json!({ "sponsor_id": "1", "amount_minor": 100, "currency": bad }));
            assert_eq!(error.status, StatusCode::BAD_REQUEST);
            assert!(error.message.contains("currency"), "{}", error.message);
        }
    }

    #[test]
    fn over_long_notes_are_refused_rather_than_silently_cut() {
        let note: String = "赞".repeat(MAX_NOTE_CHARS + 1);
        let error = error(json!({ "sponsor_id": "1", "amount_minor": 100, "note": note }));
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
        assert!(error.message.contains("note"), "{}", error.message);
        // 正好到顶的一条要收下,别把边界算错一格。
        let edge: String = "赞".repeat(MAX_NOTE_CHARS);
        assert!(new_record(body(
            json!({ "sponsor_id": "1", "amount_minor": 100, "note": edge })
        ))
        .is_ok());
    }

    /// 面板不拉汇率:美元没给折算值就是 unconverted,不能落成 cny=0 的"已折算"。
    #[test]
    fn usd_without_a_rate_is_recorded_unconverted() {
        let record = new_record(body(
            json!({ "sponsor_id": "1", "amount_minor": 500, "currency": "USD" }),
        ))
        .unwrap();
        assert_eq!(record.cny_minor, 0);
        assert_eq!(record.fx_source, "unconverted");
        assert_eq!(record.fx_rate, 0.0);
    }

    #[test]
    fn an_explicit_cny_amount_is_honoured_and_implies_a_rate() {
        let record = new_record(body(json!({
            "sponsor_id": "1", "amount_minor": 1000, "currency": "USD", "cny_minor": 7100
        })))
        .unwrap();
        assert_eq!(record.cny_minor, 7100);
        assert_eq!(record.fx_source, "manual");
        assert!((record.fx_rate - 7.1).abs() < 1e-9, "{}", record.fx_rate);
    }

    /// 人民币那笔的折算值只能是它自己,调用方多塞一个数不该改写这条。
    #[test]
    fn a_cny_record_converts_to_itself() {
        let record = new_record(body(json!({
            "sponsor_id": "1", "amount_minor": 3000, "currency": "CNY", "cny_minor": 9999
        })))
        .unwrap();
        assert_eq!(record.cny_minor, 3000);
        assert_eq!(record.fx_source, "");
    }

    #[test]
    fn money_is_grouped_and_never_touches_a_float() {
        assert_eq!(money(0, "CNY"), "¥0.00");
        assert_eq!(money(5, "CNY"), "¥0.05");
        assert_eq!(money(123_400, "CNY"), "¥1,234.00");
        assert_eq!(money(5000, "USD"), "$50.00");
        assert_eq!(money(MAX_AMOUNT_MINOR, "CNY"), "¥100,000,000.00");
        assert_eq!(money(-250, "CNY"), "-¥2.50");
    }

    #[test]
    fn page_sizes_are_capped_and_zero_falls_back_to_the_default() {
        assert_eq!(clamp(None, DEFAULT_RECORDS, MAX_RECORDS), DEFAULT_RECORDS);
        assert_eq!(
            clamp(Some(0), DEFAULT_RECORDS, MAX_RECORDS),
            DEFAULT_RECORDS
        );
        assert_eq!(clamp(Some(10), DEFAULT_RECORDS, MAX_RECORDS), 10);
        assert_eq!(clamp(Some(9999), DEFAULT_RECORDS, MAX_RECORDS), MAX_RECORDS);
        assert_eq!(
            clamp(Some(9999), DEFAULT_LEADERBOARD, MAX_LEADERBOARD),
            MAX_LEADERBOARD
        );
    }

    #[test]
    fn unknown_body_fields_are_refused() {
        assert!(serde_json::from_value::<CreateBody>(
            json!({ "sponsor_id": "1", "amount_minor": 100, "fx_rate": 7.1 })
        )
        .is_err());
    }
}
