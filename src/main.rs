#![allow(dead_code)]

mod agent;
mod alarm;
mod cli;
mod clipboard;
mod config;
mod config_tui;
mod daemon;
mod default_kb;
mod default_models;
mod host_info;
mod i18n;
mod ipc;
mod llm;
mod logging;
mod memory;
mod models_cache;
mod notify;
mod paths;
mod platforms;
mod prompts;
mod question;
mod question_tui;
mod render;
mod shell;
mod skills;
mod state;
mod sys;
mod token_counter;
mod token_estimate;
mod tools;
mod transfer;
mod web;

use anyhow::Result;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("{}: {error:#}", i18n::text("error", "错误"));
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    if platforms::plugins::renderer_worker_requested() {
        return platforms::plugins::run_renderer_worker().await;
    }
    let paths = paths::MiyuPaths::new()?;
    let language = config::AppConfig::display_language_hint(&paths);
    i18n::init(language.as_deref().unwrap_or("auto"));
    let cli = cli::parse();
    cli::run(cli, paths).await
}
