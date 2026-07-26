mod agent;
mod alarm;
mod cli;
mod clipboard;
mod config;
mod config_tui;
mod default_kb;
mod default_models;
mod i18n;
mod llm;
mod logging;
mod memory;
mod models_cache;
mod paths;
mod prompts;
mod question;
mod question_tui;
mod render;
mod shell;
mod state;
mod token_counter;
mod token_estimate;
mod tools;
mod web;

use anyhow::Result;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("{}: {error:#}", i18n::text("error", "错误"));
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let paths = paths::MiyuPaths::new()?;
    let language = config::AppConfig::display_language_hint(&paths);
    i18n::init(language.as_deref().unwrap_or("auto"));
    let cli = cli::parse();
    cli::run(cli, paths).await
}
