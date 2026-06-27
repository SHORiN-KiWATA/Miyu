mod agent;
mod cli;
mod config;
mod config_tui;
mod llm;
mod paths;
mod prompts;
mod render;
mod shell;
mod state;
mod tools;

use anyhow::Result;
use clap::Parser;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = cli::Cli::parse();
    cli::run(cli).await
}
