//! `miyu pm`:装 / 卸 / 升 / 搜 / 列 / tap。`miyupm` 是同一套(argv[0] 识别)。

use crate::cli::*;
use crate::pm::{self, PackageSource};

#[derive(Debug, Args)]
pub struct PmArgs {
    #[command(subcommand)]
    pub command: PmCommand,
}

#[derive(Debug, Subcommand)]
pub enum PmCommand {
    /// 装一个包:包名(查 tap 索引)、owner/repo[@ref]、GitHub URL 或本地目录
    #[command(alias = "add", alias = "i")]
    Install(PmInstallArgs),
    /// 卸载
    #[command(alias = "rm", alias = "uninstall")]
    Remove { name: String },
    /// 升级(不给名字 = 全部)
    #[command(alias = "up", alias = "update")]
    Upgrade {
        name: Option<String>,
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// 在 tap 索引里搜
    Search { query: String },
    /// 已装的包
    #[command(alias = "ls")]
    List,
    /// 第三方索引仓库
    Tap(TapArgs),
}

#[derive(Debug, Args)]
pub struct PmInstallArgs {
    pub spec: String,
    /// git 分支/标签/commit(缺省仓库默认分支)
    #[arg(long = "ref", value_name = "REF")]
    pub reference: Option<String>,
    /// 覆盖不是本包装的同名文件
    #[arg(long)]
    pub force: bool,
    /// 不问直接装
    #[arg(short = 'y', long)]
    pub yes: bool,
}

#[derive(Debug, Args)]
pub struct TapArgs {
    #[command(subcommand)]
    pub command: TapCommand,
}

#[derive(Debug, Subcommand)]
pub enum TapCommand {
    /// 加一个 owner/repo(根上要有 index.json)
    Add { repo: String },
    #[command(alias = "rm")]
    Remove { repo: String },
    #[command(alias = "ls")]
    List,
}

fn confirm(question: &str, yes: bool) -> Result<bool> {
    if yes || !io::stdin().is_terminal() {
        return Ok(true);
    }
    print!("{question} [y/N] ");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "是"))
}

async fn resolve_source(
    paths: &MiyuPaths,
    spec: &str,
    reference: Option<&str>,
) -> Result<PackageSource> {
    match PackageSource::parse_spec(spec, reference)? {
        Some(source) => Ok(source),
        None => pm::resolve_from_taps(paths, spec).await,
    }
}

async fn notify_daemon(paths: &MiyuPaths) {
    // 脚本目录一变下一回合自动重扫;通知 daemon 重载配置顺带刷新技能目录指纹。
    if let Err(error) = reload_daemon_if_running(paths).await {
        eprintln!(
            "{}",
            owned(
                format!(
                    "note: daemon did not reload ({error:#}); restart it to pick up the change"
                ),
                format!("提示:daemon 没有重载({error:#}),重启后生效")
            )
        );
    }
}

pub(in crate::cli) async fn run_pm(paths: &MiyuPaths, args: PmArgs) -> Result<()> {
    let config = AppConfig::load_or_default(paths)?;
    match args.command {
        PmCommand::Install(install) => {
            let source = resolve_source(paths, &install.spec, install.reference.as_deref()).await?;
            let fetched = pm::fetch(&source).await?;
            let plan = pm::plan_install(&config, paths, &fetched.root)?;
            let name = plan.manifest.package.name.clone();
            println!(
                "{} {} {} ({}){}",
                t("Package", "包"),
                name,
                plan.manifest.package.version,
                plan.manifest.package.kind.as_str(),
                if plan.manifest.package.description.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", plan.manifest.package.description)
                }
            );
            println!("{}: {}", t("source", "来源"), source.describe());
            if let Some(commit) = &fetched.commit {
                println!("{}: {}", t("commit", "提交"), commit);
            }
            println!("{}:", t("files", "文件"));
            for line in plan.summary_lines(&paths.root_dir) {
                println!("  {line}");
            }
            if !confirm(t("Install?", "装吗?"), install.yes)? {
                println!("{}", t("Cancelled.", "已取消。"));
                return Ok(());
            }
            let installed = pm::install(paths, &plan, &source, fetched.commit, install.force)?;
            println!(
                "{}",
                owned(
                    format!("Installed {name} ({} files).", installed.files.len()),
                    format!("已安装 {name}({} 个文件)。", installed.files.len())
                )
            );
            notify_daemon(paths).await;
            Ok(())
        }
        PmCommand::Remove { name } => {
            let removed = pm::remove(paths, &name)?;
            println!(
                "{}",
                owned(
                    format!("Removed {name} ({} files).", removed.files.len()),
                    format!("已卸载 {name}({} 个文件)。", removed.files.len())
                )
            );
            notify_daemon(paths).await;
            Ok(())
        }
        PmCommand::Upgrade { name, yes } => {
            let lock = pm::load_lock(paths)?;
            let targets: Vec<String> = match name {
                Some(name) => {
                    if !lock.packages.contains_key(&name) {
                        bail!("package {name:?} is not installed");
                    }
                    vec![name]
                }
                None => lock.packages.keys().cloned().collect(),
            };
            if targets.is_empty() {
                println!("{}", t("Nothing installed.", "还没装任何包。"));
                return Ok(());
            }
            let mut changed = false;
            for name in targets {
                let installed = &lock.packages[&name];
                let source =
                    resolve_source(paths, &installed.source, installed.reference.as_deref())
                        .await?;
                let fetched = pm::fetch(&source).await?;
                let plan = pm::plan_install(&config, paths, &fetched.root)?;
                if plan.manifest.package.name != name {
                    bail!(
                        "{} now serves package {:?}, not {name:?}",
                        source.describe(),
                        plan.manifest.package.name
                    );
                }
                if pm::is_up_to_date(installed, &plan, fetched.commit.as_deref())? {
                    println!("{name}: {}", t("up to date", "已是最新"));
                    continue;
                }
                println!(
                    "{name}: {} -> {}",
                    installed.version, plan.manifest.package.version
                );
                if !confirm(t("Upgrade?", "升级吗?"), yes)? {
                    continue;
                }
                pm::install(paths, &plan, &source, fetched.commit, false)?;
                changed = true;
                println!(
                    "{}",
                    owned(format!("Upgraded {name}."), format!("已升级 {name}。"))
                );
            }
            if changed {
                notify_daemon(paths).await;
            }
            Ok(())
        }
        PmCommand::Search { query } => {
            let query = query.trim().to_lowercase();
            let mut found = 0usize;
            for tap in pm::load_taps(paths)? {
                let index = match pm::fetch_tap_index(&tap).await {
                    Ok(index) => index,
                    Err(error) => {
                        eprintln!("{tap}: {error:#}");
                        continue;
                    }
                };
                for (name, entry) in index.packages {
                    if query.is_empty()
                        || name.contains(&query)
                        || entry.description.to_lowercase().contains(&query)
                    {
                        found += 1;
                        println!(
                            "{name:<24} {:<10} {}  [{tap}] {}",
                            entry.kind.map(|kind| kind.as_str()).unwrap_or("-"),
                            entry.repo,
                            entry.description
                        );
                    }
                }
            }
            if found == 0 {
                println!("{}", t("No packages matched.", "没有匹配的包。"));
            }
            Ok(())
        }
        PmCommand::List => {
            let lock = pm::load_lock(paths)?;
            if lock.packages.is_empty() {
                println!("{}", t("Nothing installed.", "还没装任何包。"));
                return Ok(());
            }
            for (name, installed) in &lock.packages {
                println!(
                    "{name:<24} {:<10} {:<10} {}{}  {}",
                    installed.version,
                    installed.kind.as_str(),
                    installed.source,
                    installed
                        .commit
                        .as_deref()
                        .map(|sha| format!("@{}", &sha[..sha.len().min(7)]))
                        .unwrap_or_default(),
                    installed.description
                );
            }
            Ok(())
        }
        PmCommand::Tap(tap) => match tap.command {
            TapCommand::Add { repo } => {
                let (owner, name) = pm::validate_repo_slug(&repo)?;
                let slug = format!("{owner}/{name}");
                let mut taps = pm::load_taps(paths)?;
                if taps.contains(&slug) {
                    println!("{}", t("Already added.", "已经在列。"));
                    return Ok(());
                }
                // 先验一下索引读得到
                pm::fetch_tap_index(&slug).await?;
                taps.push(slug.clone());
                pm::save_taps(paths, &taps)?;
                println!(
                    "{}",
                    owned(format!("Added tap {slug}."), format!("已加 tap {slug}。"))
                );
                Ok(())
            }
            TapCommand::Remove { repo } => {
                let mut taps = pm::load_taps(paths)?;
                let before = taps.len();
                taps.retain(|tap| tap != repo.trim());
                if taps.len() == before {
                    bail!("tap {repo:?} is not in the list");
                }
                pm::save_taps(paths, &taps)?;
                println!(
                    "{}",
                    owned(
                        format!("Removed tap {repo}."),
                        format!("已去掉 tap {repo}。")
                    )
                );
                Ok(())
            }
            TapCommand::List => {
                for tap in pm::load_taps(paths)? {
                    println!(
                        "{tap}{}",
                        if tap == pm::OFFICIAL_TAP {
                            t("  (official)", "  (官方)")
                        } else {
                            ""
                        }
                    );
                }
                Ok(())
            }
        },
    }
}
