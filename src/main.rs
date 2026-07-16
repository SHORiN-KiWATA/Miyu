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

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = cli::parse();
    cli::run(cli).await
}
