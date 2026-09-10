//! 工具面的行为：action 分发、参数校验、返回结构。
//!
//! 这一层薄，但它是模型唯一能碰到的入口，出错的方式和 WebUI 不一样——
//! 模型给的是自然语言里抠出来的字符串，而不是表单填好的字段。
//!
//! 汇率要联网，所以这里只走本币路径；换算逻辑由 `ledger::tests` 与真机
//! 实测覆盖。

use super::*;
use crate::config::AppConfig;
use crate::paths::MiyuPaths;
use serde_json::json;
use tempfile::TempDir;

fn sandbox() -> (TempDir, MiyuPaths, AppConfig) {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let paths = MiyuPaths {
        root_dir: root.to_path_buf(),
        config_dir: root.join("config"),
        config_file: root.join("config/config.jsonc"),
        skills_dir: root.join("config/skills"),
        data_dir: root.join("data"),
        cache_dir: root.join("cache"),
        state_dir: root.join("state"),
        pictures_dir: root.join("pictures"),
        fish_hook_file: root.join("config/fish/conf.d/miyu.fish"),
        bash_hook_file: root.join("config/shell/bash-hook.sh"),
        zsh_hook_file: root.join("config/shell/zsh-hook.zsh"),
        scripts_dir: root.join("data/scripts"),
        system_scripts_dir: root.join("share/scripts"),
    };
    (dir, paths, AppConfig::default())
}

async fn call(args: Value, paths: &MiyuPaths, config: &AppConfig) -> Value {
    let raw = run_ledger(args, paths.clone(), config.clone())
        .await
        .expect("tool call");
    serde_json::from_str(&raw).expect("tool returns json")
}

async fn call_err(args: Value, paths: &MiyuPaths, config: &AppConfig) -> String {
    run_ledger(args, paths.clone(), config.clone())
        .await
        .expect_err("expected an error")
        .to_string()
}

async fn manage(args: Value, paths: &MiyuPaths) -> Value {
    // 记账管理只拿 config 定「谁的账本」;默认配置 = 管理员那份。
    let raw = manage::run(args, paths.clone(), AppConfig::default())
        .await
        .expect("manage call");
    serde_json::from_str(&raw).expect("tool returns json")
}

#[tokio::test]
async fn the_first_entry_creates_a_book_on_the_way() {
    let (_dir, paths, config) = sandbox();
    // 「记一笔」不该先撞上一道配置题。
    let result = call(
        json!({"action": "add", "amount": "35.50", "category": "餐饮", "note": "麦当劳"}),
        &paths,
        &config,
    )
    .await;
    assert_eq!(result["ok"], true);
    assert_eq!(result["entry"]["amount"], "35.50 CNY");
    assert_eq!(result["entry"]["category"], "餐饮");
    assert!(result["entry"]["id"].as_str().unwrap().starts_with("en_"));
}

#[tokio::test]
async fn a_replayed_call_is_refused_until_it_says_force() {
    let (_dir, paths, config) = sandbox();
    let args = json!({"action": "add", "amount": "35.50", "category": "餐饮", "note": "麦当劳"});
    call(args.clone(), &paths, &config).await;

    // 端点抖动时模型会把同一笔重演一遍。
    let again = call(args.clone(), &paths, &config).await;
    assert_eq!(again["ok"], false);
    assert_eq!(again["reason"], "possible_duplicate");
    assert!(again["existing"]["id"].is_string());

    let mut forced = args;
    forced["force"] = json!(true);
    assert_eq!(call(forced, &paths, &config).await["ok"], true);
    let list = call(json!({"action": "list"}), &paths, &config).await;
    assert_eq!(list["total"], 2);
}

#[tokio::test]
async fn budget_shows_up_in_the_result_only_once_it_matters() {
    let (_dir, paths, config) = sandbox();
    call(json!({"action": "add", "amount": "10"}), &paths, &config).await;
    manage(
        json!({"action": "set_budget", "category": "餐饮", "amount": "100"}),
        &paths,
    )
    .await;

    // 没超时结果里干脆没有 budget 字段——没话说的时候不占 token。
    let calm = call(
        json!({"action": "add", "amount": "20", "category": "餐饮", "note": "早饭"}),
        &paths,
        &config,
    )
    .await;
    assert!(calm.get("budget").is_none(), "{calm}");

    let over = call(
        json!({"action": "add", "amount": "200", "category": "餐饮", "note": "聚餐"}),
        &paths,
        &config,
    )
    .await;
    assert_eq!(over["budget"]["state"], "exceeded");
    assert_eq!(over["budget"]["scope"], "category:餐饮");
    assert_eq!(over["budget"]["over"], "120.00");
}

#[tokio::test]
async fn summary_reports_the_month_with_its_top_categories() {
    let (_dir, paths, config) = sandbox();
    call(
        json!({"action": "add", "amount": "35.50", "category": "餐饮"}),
        &paths,
        &config,
    )
    .await;
    call(
        json!({"action": "add", "kind": "income", "amount": "5000", "category": "工资"}),
        &paths,
        &config,
    )
    .await;

    let summary = call(json!({"action": "summary"}), &paths, &config).await;
    assert_eq!(summary["expense"], "35.50");
    assert_eq!(summary["income"], "5000.00");
    assert_eq!(summary["net"], "4964.50");
    assert_eq!(summary["currency"], "CNY");
    assert_eq!(summary["top_expense_categories"][0]["name"], "餐饮");
}

#[tokio::test]
async fn entries_can_be_edited_and_deleted_by_an_id_prefix() {
    let (_dir, paths, config) = sandbox();
    let added = call(
        json!({"action": "add", "amount": "35.50", "note": "麦当劳"}),
        &paths,
        &config,
    )
    .await;
    let id = added["entry"]["id"].as_str().unwrap().to_string();

    // 模型转述 id 时经常只报前几位。
    let edited = call(
        json!({"action": "update", "id": &id[..8], "note": "麦当劳午餐"}),
        &paths,
        &config,
    )
    .await;
    assert_eq!(edited["entry"]["note"], "麦当劳午餐");

    let deleted = call(json!({"action": "delete", "id": &id[..8]}), &paths, &config).await;
    assert_eq!(deleted["ok"], true);
    assert_eq!(
        call(json!({"action": "list"}), &paths, &config).await["total"],
        0
    );
}

#[tokio::test]
async fn bad_arguments_come_back_as_errors_the_model_can_act_on() {
    let (_dir, paths, config) = sandbox();
    for (args, needle) in [
        (json!({"action": "add"}), "amount is required"),
        (
            json!({"action": "add", "amount": "35.555"}),
            "decimal places",
        ),
        (json!({"action": "add", "amount": "-5"}), "must be positive"),
        (
            json!({"action": "add", "amount": "10", "date": "09/08"}),
            "2026-09-08",
        ),
        (json!({"action": "nope"}), "unknown action"),
        (json!({"action": "delete"}), "id is required"),
    ] {
        let error = call_err(args.clone(), &paths, &config).await;
        assert!(error.contains(needle), "{args} -> {error}");
    }
}

#[tokio::test]
async fn a_transfer_needs_two_different_accounts_and_no_category() {
    let (_dir, paths, config) = sandbox();
    manage(
        json!({"action": "create_book", "name": "生活", "currency": "CNY"}),
        &paths,
    )
    .await;
    // 「现金」「银行卡」是新账本自带的默认账户，不用先建。

    let missing = call_err(
        json!({"action": "add", "kind": "transfer", "amount": "100", "account": "现金"}),
        &paths,
        &config,
    )
    .await;
    assert!(missing.contains("both account and to_account"), "{missing}");

    let same = call_err(
        json!({"action": "add", "kind": "transfer", "amount": "100", "account": "现金", "to_account": "现金"}),
        &paths,
        &config,
    )
    .await;
    assert!(same.contains("must differ"), "{same}");

    let categorised = call_err(
        json!({"action": "add", "kind": "transfer", "amount": "100", "account": "现金",
               "to_account": "银行卡", "category": "餐饮"}),
        &paths,
        &config,
    )
    .await;
    assert!(categorised.contains("takes no category"), "{categorised}");

    let ok = call(
        json!({"action": "add", "kind": "transfer", "amount": "100", "account": "现金", "to_account": "银行卡"}),
        &paths,
        &config,
    )
    .await;
    assert_eq!(ok["ok"], true);
}

#[tokio::test]
async fn several_books_force_the_caller_to_say_which_one() {
    let (_dir, paths, config) = sandbox();
    manage(
        json!({"action": "create_book", "name": "生活", "currency": "CNY"}),
        &paths,
    )
    .await;
    manage(
        json!({"action": "create_book", "name": "工作", "currency": "CNY"}),
        &paths,
    )
    .await;

    // 记错账本是几个月后才会发现的错误,不值得为省一次追问去猜。
    let error = call_err(json!({"action": "add", "amount": "10"}), &paths, &config).await;
    assert!(error.contains("several books"), "{error}");
    assert!(error.contains("生活") && error.contains("工作"), "{error}");

    let ok = call(
        json!({"action": "add", "amount": "10", "book": "工作"}),
        &paths,
        &config,
    )
    .await;
    assert_eq!(ok["book"], "工作");
}

#[tokio::test]
async fn income_and_expense_categories_do_not_leak_into_each_other() {
    let (_dir, paths, config) = sandbox();
    manage(
        json!({"action": "create_book", "name": "生活", "currency": "CNY"}),
        &paths,
    )
    .await;

    // 「工资」只在收入树里。拿它记一笔支出要报错,而不是顺手在支出树下
    // 建一个同名的镜像分类——那样分类表里会有两个「工资」,而真正的问题
    // (kind 填错了) 反倒被藏起来。
    let error = call_err(
        json!({"action": "add", "kind": "expense", "amount": "10", "category": "工资"}),
        &paths,
        &config,
    )
    .await;
    assert!(error.contains("is an income category"), "{error}");
    let categories = manage(json!({"action": "list", "what": "categories"}), &paths).await;
    let wages = categories["categories"]
        .as_array()
        .expect("categories is a list")
        .iter()
        .filter(|category| category["name"] == "工资")
        .count();
    assert_eq!(wages, 1, "支出树下不该冒出第二个「工资」");

    let ok = call(
        json!({"action": "add", "kind": "income", "amount": "5000", "category": "工资"}),
        &paths,
        &config,
    )
    .await;
    assert_eq!(ok["entry"]["category"], "工资");
}

#[tokio::test]
async fn an_unknown_category_is_created_on_the_spot() {
    let (_dir, paths, config) = sandbox();
    let first = call(
        json!({"action": "add", "amount": "60.25", "category": "通讯", "note": "电话费"}),
        &paths,
        &config,
    )
    .await;
    assert_eq!(first["ok"], true);
    assert_eq!(first["entry"]["category"], "通讯");
    // 账本里凭空多一个分类,得说出口。
    assert_eq!(first["created_category"], "通讯 (new)");

    // 第二笔复用它,不该再报一次「新建」。
    let second = call(
        json!({"action": "add", "amount": "39.00", "category": "通讯", "note": "宽带"}),
        &paths,
        &config,
    )
    .await;
    assert_eq!(second["entry"]["category"], "通讯");
    assert!(second.get("created_category").is_none(), "{second}");

    let categories = manage(json!({"action": "list", "what": "categories"}), &paths).await;
    let made = categories["categories"]
        .as_array()
        .expect("categories is a list")
        .iter()
        .filter(|category| category["name"] == "通讯")
        .count();
    assert_eq!(made, 1);
}

/// 分类是在重复闸**之后**才建的。建在前面的话，被挡下的那一笔会在账本里
/// 留下一个没有任何账目挂着的分类。
#[tokio::test]
async fn a_blocked_duplicate_leaves_no_new_category_behind() {
    let (_dir, paths, config) = sandbox();
    call(
        json!({"action": "add", "amount": "10", "note": "咖啡"}),
        &paths,
        &config,
    )
    .await;

    // 同额同备注,只多了个没见过的分类名——两笔的分类都还是空,重复闸认得出来。
    let blocked = call(
        json!({"action": "add", "amount": "10", "note": "咖啡", "category": "饮料"}),
        &paths,
        &config,
    )
    .await;
    assert_eq!(blocked["ok"], false);
    assert_eq!(blocked["reason"], "possible_duplicate");

    let categories = manage(json!({"action": "list", "what": "categories"}), &paths).await;
    let leaked = categories["categories"]
        .as_array()
        .expect("categories is a list")
        .iter()
        .any(|category| category["name"] == "饮料");
    assert!(!leaked, "被挡下的那一笔不该留下「饮料」");
}

#[tokio::test]
async fn manage_lists_what_it_creates() {
    let (_dir, paths, _config) = sandbox();
    let book = manage(
        json!({"action": "create_book", "name": "工作", "currency": "JPY"}),
        &paths,
    )
    .await;
    assert_eq!(book["book"]["currency"], "JPY");

    // 名字避开新账本自带的那套默认账户。
    manage(json!({"action": "create_account", "name": "钱包"}), &paths).await;
    let accounts = manage(json!({"action": "list", "what": "accounts"}), &paths).await;
    let wallet = accounts["accounts"]
        .as_array()
        .expect("accounts is a list")
        .iter()
        .find(|account| account["name"] == "钱包")
        .expect("列出刚建的账户");
    // 日元没有小数位,余额不该显示成 0.00。
    assert_eq!(wallet["balance"], "0");
    // 默认账户跟着账本币种走,这本账是 JPY。
    assert_eq!(accounts["accounts"][0]["currency"], "JPY");

    manage(
        json!({"action": "create_category", "name": "健身", "direction": "expense"}),
        &paths,
    )
    .await;
    let categories = manage(
        json!({"action": "list", "what": "categories", "direction": "expense"}),
        &paths,
    )
    .await;
    let names: Vec<&str> = categories["categories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"健身"), "{names:?}");

    let budgets = manage(json!({"action": "list", "what": "budgets"}), &paths).await;
    assert!(budgets["budgets"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn a_budget_belongs_to_its_own_book() {
    let (_dir, paths, _config) = sandbox();
    manage(
        json!({"action": "create_book", "name": "生活", "currency": "CNY"}),
        &paths,
    )
    .await;
    manage(
        json!({"action": "create_book", "name": "工作", "currency": "CNY"}),
        &paths,
    )
    .await;
    let budget = manage(
        json!({"action": "set_budget", "book": "生活", "amount": "3000"}),
        &paths,
    )
    .await;
    let id = budget["budget"]["id"].as_str().unwrap();

    // id 是随机串,但跨账本删除仍然该被挡住。
    let error = manage::run(
        json!({"action": "delete_budget", "book": "工作", "id": id}),
        paths.clone(),
        AppConfig::default(),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("does not belong"), "{error}");
}
