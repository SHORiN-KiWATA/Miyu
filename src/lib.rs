//! Miyu 的库入口。
//!
//! 模块声明与启动流程都在这里，`main.rs` 只剩一个薄壳。这么分是为了让拆分
//! 有个可依赖的地基：有了 lib target 之后，集成测试与架构门禁才能按模块路径
//! 引用，而不是只能通过 bin 的私有模块树。
#![allow(dead_code)]

mod agent;
mod alarm;
mod args;
mod cli;
mod clipboard;
mod config;
mod config_tui;
mod daemon;
mod default_kb;
mod default_models;
mod embedding;
mod host_info;
mod i18n;
mod ipc;
mod json_extract;
mod ledger;
mod llm;
mod logging;
mod memory;
mod memory_types;
mod models_cache;
mod notify;
mod paths;
mod persona_hint;
mod platform_types;
mod platforms;
mod prompts;
mod question;
mod question_tui;
mod render;
mod runtime;
mod shell;
mod skills;
mod slash_commands;
mod state;
mod terminal;
mod token_counter;
mod token_estimate;
mod tools;
mod transfer;
#[cfg(feature = "voice")]
pub mod voice;
mod web;

use anyhow::Result;

pub async fn run() -> Result<()> {
    // 趁二进制还在磁盘上，先把自己的路径记下来。daemon 一跑就是几小时，
    // 期间升级安装包或重新编译都会把这个文件换掉，那之后 `/proc/self/exe`
    // 读出来的是 `".../miyu (deleted)"`，再想 spawn 自己就 ENOENT 了
    // （长图渲染器、闹钟、知识库索引都靠这条路）。
    paths::prime_miyu_executable();
    if platforms::plugins::renderer_worker_requested() {
        return platforms::plugins::run_renderer_worker().await;
    }
    if embedding::embedding_worker_requested() {
        return embedding::run_embedding_worker().await;
    }
    let paths = paths::MiyuPaths::new()?;
    let language = config::AppConfig::display_language_hint(&paths);
    i18n::init(language.as_deref().unwrap_or("auto"));
    let cli = cli::parse();
    cli::run(cli, paths).await
}

/// 退出码:`main.rs` 用,见 `cli::exit_code`。
pub fn exit_code_for(error: &anyhow::Error) -> i32 {
    cli::exit_code::exit_code_for(error)
}

/// 错误前缀的本地化文案。`main.rs` 打印失败时要用，而 `i18n` 是私有模块。
pub fn error_label() -> &'static str {
    i18n::text("error", "错误")
}
