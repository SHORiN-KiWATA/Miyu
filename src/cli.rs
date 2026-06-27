use crate::agent::{Agent, AgentEvent};
use crate::config::AppConfig;
use crate::llm::OpenAiCompatibleClient;
use crate::paths::MiyuPaths;
use crate::render;
use crate::shell;
use crate::state::StateStore;
use crate::tools;
use anyhow::{bail, Result};
use clap::{Args, Parser, Subcommand};
use crossterm::cursor::{self, Hide, MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::style::{Attribute, Print, SetAttribute};
use crossterm::terminal::{self, Clear, ClearType};
use crossterm::{execute, queue};
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "miyu", version, about = "Miyu CLI AI Agent")]
pub struct Cli {
    #[arg(long, hide = true)]
    pub shell_intercept: bool,

    #[arg(long, hide = true)]
    pub shell: Option<String>,

    #[command(subcommand)]
    pub command: Option<Command>,

    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub message: Vec<String>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    Ask(MessageArgs),
    Init,
    Paths,
    Config(ConfigArgs),
    Providers(ProvidersArgs),
    FishInit,
    History(HistoryArgs),
    Kb(KbArgs),
    Reset,
}

#[derive(Debug, Args)]
pub struct MessageArgs {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub message: Vec<String>,
}

#[derive(Debug, Args)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub command: Option<ConfigCommand>,
}

#[derive(Debug, Args)]
pub struct HistoryArgs {
    #[arg(short, long, default_value_t = 20)]
    pub limit: usize,

    #[arg(long)]
    pub raw: bool,

    #[arg(long)]
    pub no_thinking: bool,
}

#[derive(Debug, Args)]
pub struct ProvidersArgs {
    pub index: Option<usize>,
}

#[derive(Debug, Args)]
pub struct KbArgs {
    #[command(subcommand)]
    pub command: KbCommand,
}

#[derive(Debug, Subcommand)]
pub enum KbCommand {
    Add(KbAddArgs),
    List,
    Search(KbSearchArgs),
    Find(KbFindArgs),
    Read(KbReadArgs),
    Remove(KbRemoveArgs),
    Reindex,
    Stats,
    Embed(KbEmbedArgs),
}

#[derive(Debug, Args)]
pub struct KbAddArgs {
    pub path: PathBuf,
    #[arg(
        short,
        long,
        help = "Compatibility flag; directories are recursive by default"
    )]
    pub recursive: bool,
}

#[derive(Debug, Args)]
pub struct KbSearchArgs {
    pub query: Vec<String>,
    #[arg(short, long)]
    pub limit: Option<usize>,
}

#[derive(Debug, Args)]
pub struct KbFindArgs {
    pub query: Vec<String>,
    #[arg(short, long)]
    pub limit: Option<usize>,
}

#[derive(Debug, Args)]
pub struct KbReadArgs {
    pub file: String,
    #[arg(long, default_value_t = 1)]
    pub start: usize,
    #[arg(long)]
    pub lines: Option<usize>,
}

#[derive(Debug, Args)]
pub struct KbRemoveArgs {
    pub file: String,
}

#[derive(Debug, Args)]
pub struct KbEmbedArgs {
    #[command(subcommand)]
    pub command: KbEmbedCommand,
}

#[derive(Debug, Subcommand)]
pub enum KbEmbedCommand {
    Reindex(KbEmbedReindexArgs),
}

#[derive(Debug, Args)]
pub struct KbEmbedReindexArgs {
    #[arg(long)]
    pub quiet: bool,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    Validate,
    Paths,
    #[command(hide = true)]
    PromptSource,
}

pub async fn run(cli: Cli) -> Result<()> {
    let paths = MiyuPaths::new()?;

    if cli.shell_intercept {
        let shell_name = cli.shell.as_deref().unwrap_or("fish");
        let message = join_message(cli.message);
        return run_shell_intercept(&paths, shell_name, message).await;
    }

    match cli.command {
        Some(Command::Ask(args)) => run_chat(&paths, join_message(args.message)).await,
        Some(Command::Init) => {
            AppConfig::init_files(&paths)?;
            StateStore::new(&paths)?.init_files()?;
            println!("initialized Miyu at {}", paths.config_dir.display());
            Ok(())
        }
        Some(Command::Paths) => {
            paths.print();
            Ok(())
        }
        Some(Command::Config(args)) => run_config(&paths, args).await,
        Some(Command::Providers(args)) => run_providers(&paths, args),
        Some(Command::FishInit) => shell::fish::install(&paths),
        Some(Command::History(args)) => run_history(&paths, args),
        Some(Command::Kb(args)) => run_kb(&paths, args).await,
        Some(Command::Reset) => run_reset(&paths),
        None => {
            let message = join_message(cli.message);
            if message.is_empty() {
                run_repl(&paths).await
            } else {
                run_chat(&paths, message).await
            }
        }
    }
}

fn run_providers(paths: &MiyuPaths, args: ProvidersArgs) -> Result<()> {
    let mut config = AppConfig::load(paths)?;
    let choices = config.provider_model_choices();
    if choices.is_empty() {
        bail!("no available providers");
    }
    if let Some(index) = args.index {
        if index == 0 || index > choices.len() {
            bail!("provider index out of range: {index}");
        }
        let choice = &choices[index - 1];
        let provider_id = choice.provider_id.clone();
        let model = choice.model.clone();
        let label = choice.label();
        config.set_active_provider_model(&provider_id, &model)?;
        config.save(paths)?;
        println!("active provider: {index}. {label}");
        return Ok(());
    }
    if io::stdout().is_terminal() && io::stdin().is_terminal() {
        if let Some(index) = inline_fuzzy_select(
            &choices
                .iter()
                .map(|choice| choice.label())
                .collect::<Vec<_>>(),
        )? {
            let choice = &choices[index];
            let provider_id = choice.provider_id.clone();
            let model = choice.model.clone();
            let label = choice.label();
            config.set_active_provider_model(&provider_id, &model)?;
            config.save(paths)?;
            println!("active provider: {}. {label}", index + 1);
        }
        return Ok(());
    }
    for (index, choice) in choices.iter().enumerate() {
        let active = config
            .provider(None)
            .map(|provider| {
                provider.id == choice.provider_id && provider.default_model == choice.model
            })
            .unwrap_or(false);
        let marker = if active { "*" } else { " " };
        println!("{marker} {}. {}", index + 1, choice.label());
    }
    Ok(())
}

fn inline_fuzzy_select(items: &[String]) -> Result<Option<usize>> {
    let menu_lines = inline_fuzzy_lines(items.len());
    reserve_inline_fuzzy_space(menu_lines)?;
    let mut session = InlineRawMode::start()?;
    let matcher = SkimMatcherV2::default();
    let mut query = String::new();
    let mut selected = 0usize;
    let (_, cursor_y) = cursor::position().unwrap_or((0, menu_lines.saturating_sub(1)));
    let anchor_y = cursor_y.saturating_sub(menu_lines.saturating_sub(1));
    loop {
        let matches = fuzzy_matches(&matcher, items, &query);
        if selected >= matches.len() {
            selected = matches.len().saturating_sub(1);
        }
        draw_inline_fuzzy(
            &mut session.stdout,
            anchor_y,
            menu_lines,
            &query,
            items,
            &matches,
            selected,
        )?;
        if let Event::Key(KeyEvent {
            code, modifiers, ..
        }) = event::read()?
        {
            match code {
                KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                    clear_inline_fuzzy(&mut session.stdout, anchor_y, menu_lines)?;
                    return Ok(None);
                }
                KeyCode::Esc => {
                    clear_inline_fuzzy(&mut session.stdout, anchor_y, menu_lines)?;
                    return Ok(None);
                }
                KeyCode::Char('q') if query.is_empty() => {
                    clear_inline_fuzzy(&mut session.stdout, anchor_y, menu_lines)?;
                    return Ok(None);
                }
                KeyCode::Enter => {
                    clear_inline_fuzzy(&mut session.stdout, anchor_y, menu_lines)?;
                    return Ok(matches.get(selected).map(|(_, index)| *index));
                }
                KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => {
                    selected = (selected + 1).min(matches.len().saturating_sub(1));
                }
                KeyCode::Backspace => {
                    query.pop();
                    selected = 0;
                }
                KeyCode::Char(ch) if !modifiers.contains(KeyModifiers::CONTROL) => {
                    query.push(ch);
                    selected = 0;
                }
                _ => {}
            }
        }
    }
}

fn fuzzy_matches(matcher: &SkimMatcherV2, items: &[String], query: &str) -> Vec<(i64, usize)> {
    let mut matches = items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            if query.trim().is_empty() {
                Some((0, index))
            } else {
                matcher.fuzzy_match(item, query).map(|score| (score, index))
            }
        })
        .collect::<Vec<_>>();
    if !query.trim().is_empty() {
        matches.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    }
    matches
}

fn draw_inline_fuzzy(
    stdout: &mut io::Stdout,
    anchor_y: u16,
    menu_lines: u16,
    query: &str,
    items: &[String],
    matches: &[(i64, usize)],
    selected: usize,
) -> Result<()> {
    let (cols, _) = terminal::size().unwrap_or((80, 24));
    let width = cols.saturating_sub(2).max(24) as usize;
    let visible = matches.len().min(menu_lines.saturating_sub(2) as usize);
    queue!(stdout, Hide)?;
    for row in 0..menu_lines {
        queue!(
            stdout,
            MoveTo(0, anchor_y + row),
            Clear(ClearType::CurrentLine)
        )?;
    }
    queue!(
        stdout,
        MoveTo(0, anchor_y),
        Print(truncate_display(&format!("> {query}"), width)),
    )?;
    if matches.is_empty() {
        queue!(stdout, MoveTo(0, anchor_y + 1), Print("  no matches"))?;
    } else {
        for (row, (_, item_index)) in matches.iter().take(visible).enumerate() {
            let marker = if row == selected { ">" } else { " " };
            let line = truncate_display(&format!("{marker} {}", items[*item_index]), width);
            queue!(stdout, MoveTo(0, anchor_y + row as u16 + 1))?;
            if row == selected {
                queue!(
                    stdout,
                    SetAttribute(Attribute::Reverse),
                    Print(line),
                    SetAttribute(Attribute::Reset)
                )?;
            } else {
                queue!(stdout, Print(line))?;
            }
        }
    }
    queue!(
        stdout,
        MoveTo(0, anchor_y + menu_lines.saturating_sub(1)),
        Print(truncate_display(
            "[type] search  [j/k] move  [enter] select  [esc/q] cancel",
            width
        ))
    )?;
    stdout.flush()?;
    Ok(())
}

fn clear_inline_fuzzy(stdout: &mut io::Stdout, anchor_y: u16, lines: u16) -> Result<()> {
    for row in 0..lines {
        queue!(
            stdout,
            MoveTo(0, anchor_y + row),
            Clear(ClearType::CurrentLine)
        )?;
    }
    queue!(stdout, MoveTo(0, anchor_y), Show)?;
    stdout.flush()?;
    Ok(())
}

fn reserve_inline_fuzzy_space(lines: u16) -> Result<()> {
    for _ in 1..lines {
        println!();
    }
    io::stdout().flush()?;
    Ok(())
}

fn inline_fuzzy_lines(item_count: usize) -> u16 {
    ((item_count.min(10) + 2) as u16).max(3)
}

fn truncate_display(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        value.to_string()
    } else {
        format!(
            "{}…",
            value
                .chars()
                .take(max.saturating_sub(1))
                .collect::<String>()
        )
    }
}

struct InlineRawMode {
    stdout: io::Stdout,
}

impl InlineRawMode {
    fn start() -> Result<Self> {
        terminal::enable_raw_mode()?;
        Ok(Self {
            stdout: io::stdout(),
        })
    }
}

impl Drop for InlineRawMode {
    fn drop(&mut self) {
        let _ = execute!(self.stdout, Show);
        let _ = terminal::disable_raw_mode();
    }
}

async fn run_config(paths: &MiyuPaths, args: ConfigArgs) -> Result<()> {
    match args.command {
        Some(ConfigCommand::Validate) => {
            AppConfig::load(paths)?;
            println!("config is valid: {}", paths.config_file.display());
            Ok(())
        }
        Some(ConfigCommand::Paths) => {
            paths.print();
            Ok(())
        }
        Some(ConfigCommand::PromptSource) => {
            let config = AppConfig::load(paths)?;
            let persona = config.prompt.active_persona.trim();
            let identity = config.prompt.active_identity.trim();
            println!(
                "base_prompt_source: {}",
                if persona.is_empty() {
                    "built-in"
                } else {
                    "persona"
                }
            );
            println!(
                "active_persona: {}",
                if persona.is_empty() { "Miyu" } else { persona }
            );
            if !persona.is_empty() {
                println!(
                    "active_persona_file: {}",
                    config.persona_path(paths, persona).display()
                );
            }
            println!(
                "active_identity: {}",
                if identity.is_empty() {
                    "(none)"
                } else {
                    identity
                }
            );
            println!("prompts_dir: {}", config.prompts_dir_path(paths).display());
            println!(
                "identities_dir: {}",
                config.identities_dir_path(paths).display()
            );
            let system_prompt = config.system_prompt(paths)?;
            println!(
                "system_prompt_first_line: {}",
                system_prompt.lines().next().unwrap_or("")
            );
            println!("system_prompt_chars: {}", system_prompt.chars().count());
            Ok(())
        }
        None => crate::config_tui::run(paths),
    }
}

async fn run_shell_intercept(paths: &MiyuPaths, shell_name: &str, message: String) -> Result<()> {
    if shell_name != "fish" {
        bail!("unsupported shell: {shell_name}");
    }
    if message.is_empty() || !shell::looks_like_natural_language(&message) {
        bail!("not a natural language command");
    }
    run_chat_with_options(paths, message, Some(false), true).await
}

async fn run_chat(paths: &MiyuPaths, message: String) -> Result<()> {
    run_chat_with_options(paths, message, None, false).await
}

async fn run_chat_with_options(
    paths: &MiyuPaths,
    message: String,
    show_reasoning: Option<bool>,
    plain: bool,
) -> Result<()> {
    if message.is_empty() {
        return run_repl(paths).await;
    }
    let config = AppConfig::load(paths)?;
    let state = StateStore::new(paths)?;
    state.init_files()?;
    let client = OpenAiCompatibleClient::from_config(&config, paths)?;
    let registry = build_tool_registry(&config, paths)?;
    let reasoning_mode = if show_reasoning == Some(false) {
        render::ReasoningDisplayMode::Hidden
    } else {
        render::ReasoningDisplayMode::from_config(&config.display.reasoning)
    };
    let tool_call_mode = if plain {
        render::ToolCallDisplayMode::Hidden
    } else {
        render::ToolCallDisplayMode::from_config(&config.display.tool_calls)
    };
    let mut agent = Agent::new(config, paths, state, client, registry)?;
    let mut renderer = render::StreamRenderer::new(reasoning_mode, tool_call_mode, plain);
    let _answer = agent
        .chat_stream(&message, |event| handle_agent_event(&mut renderer, event))
        .await?;
    renderer.finish()?;
    Ok(())
}

async fn run_repl(paths: &MiyuPaths) -> Result<()> {
    let config = AppConfig::load(paths)?;
    let state = StateStore::new(paths)?;
    state.init_files()?;
    let client = OpenAiCompatibleClient::from_config(&config, paths)?;
    let registry = build_tool_registry(&config, paths)?;
    let reasoning_mode = render::ReasoningDisplayMode::from_config(&config.display.reasoning);
    let tool_call_mode = render::ToolCallDisplayMode::from_config(&config.display.tool_calls);
    let mut agent = Agent::new(config, paths, state, client, registry)?;

    println!("Miyu REPL. Type exit or quit to leave.");
    let mut editor = rustyline::DefaultEditor::new()?;
    loop {
        let input = match editor.readline("> ") {
            Ok(input) => input,
            Err(
                rustyline::error::ReadlineError::Interrupted | rustyline::error::ReadlineError::Eof,
            ) => break,
            Err(err) => return Err(err.into()),
        };
        let input = input.trim();
        if input.eq_ignore_ascii_case("exit") || input.eq_ignore_ascii_case("quit") {
            break;
        }
        if input.is_empty() {
            continue;
        }
        let _ = editor.add_history_entry(input);
        let mut renderer = render::StreamRenderer::new(reasoning_mode, tool_call_mode, false);
        let _answer = agent
            .chat_stream(input, |event| handle_agent_event(&mut renderer, event))
            .await?;
        renderer.finish()?;
    }
    Ok(())
}

fn run_history(paths: &MiyuPaths, args: HistoryArgs) -> Result<()> {
    let state = StateStore::new(paths)?;
    for entry in state.history(args.limit)? {
        if args.raw {
            println!("{}", serde_json::to_string(&entry)?);
            continue;
        }
        println!("{} {}", entry.timestamp, entry.role);
        if entry.role == "assistant" {
            let response = crate::llm::ChatResult {
                content: entry.content,
                reasoning: if args.no_thinking {
                    None
                } else {
                    entry.reasoning
                },
                usage: None,
                tool_calls: Vec::new(),
            };
            render::print_assistant_response(&response, !args.no_thinking)?;
        } else {
            println!("{}", entry.content);
        }
        println!();
    }
    Ok(())
}

async fn run_kb(paths: &MiyuPaths, args: KbArgs) -> Result<()> {
    let config = AppConfig::load(paths)?;
    let kb = tools::knowledge_base::KnowledgeBase::new(config, paths.clone())?;
    match args.command {
        KbCommand::Add(args) => {
            let added = kb.add_path(&args.path).await?;
            for path in added {
                println!("added {path}");
            }
        }
        KbCommand::List => {
            for file in kb.list()? {
                println!("{}\t{} bytes", file.name, file.size_bytes);
            }
        }
        KbCommand::Search(args) => {
            let query = args.query.join(" ");
            println!("{}", kb.search(&query, args.limit).await?);
        }
        KbCommand::Find(args) => {
            let query = args.query.join(" ");
            println!("{}", kb.find_by_name(&query, args.limit)?);
        }
        KbCommand::Read(args) => {
            println!("{}", kb.read_file(&args.file, args.start, args.lines)?);
        }
        KbCommand::Remove(args) => {
            kb.remove(&args.file)?;
            println!("removed {}", args.file);
        }
        KbCommand::Reindex => {
            let files = kb.list()?;
            println!(
                "keyword index is rebuilt on demand; files tracked: {}",
                files.len()
            );
        }
        KbCommand::Stats => {
            println!("{}", kb.stats()?);
        }
        KbCommand::Embed(args) => match args.command {
            KbEmbedCommand::Reindex(args) => {
                kb.reindex_embeddings(args.quiet).await?;
            }
        },
    }
    Ok(())
}

fn run_reset(paths: &MiyuPaths) -> Result<()> {
    StateStore::new(paths)?.reset_conversation()?;
    println!("cleared current conversation history");
    Ok(())
}

fn join_message(parts: Vec<String>) -> String {
    parts.join(" ").trim().to_string()
}

fn build_tool_registry(config: &AppConfig, paths: &MiyuPaths) -> Result<tools::ToolRegistry> {
    let mut registry = if config.tools.enabled {
        tools::builtin_registry(config, paths)
    } else {
        tools::ToolRegistry::new()
    };
    if config.tools.enabled && config.skills.enabled {
        tools::register_skills(&mut registry, paths, config.skills.allow_command_execution)?;
    }
    Ok(registry)
}

fn handle_agent_event(renderer: &mut render::StreamRenderer, event: AgentEvent) -> Result<()> {
    match event {
        AgentEvent::Chunk(chunk) => renderer.write_chunk(chunk),
        AgentEvent::ToolCall { name, arguments } => renderer.write_tool_call(&name, &arguments),
        AgentEvent::ToolResult { name, ok, output } => {
            renderer.write_tool_result(&name, ok, &output)
        }
    }
}
