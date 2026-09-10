//! `miyu layout`:家目录布局(阶段 6)的查看、手动执行与回滚。
//!
//! 搬家本身在 daemon 起来时自动做;这里给用户一个能提前看计划(干跑)、
//! 在没 daemon 时手动搬、以及后悔了搬回去的入口。

use crate::cli::*;
use crate::paths::{
    admin_home_name_from_env, clear_home_layout_opt_out, current_process_is_daemon,
    home_layout_opted_out, read_home_layout_admin, rollback_home_layout, try_migrate_home_layout,
    HomeLayout, Layout,
};

#[derive(Debug, Args)]
pub struct LayoutArgs {
    /// 立刻搬家(daemon 必须已停);不带任何参数只打印计划
    #[arg(long, conflicts_with = "rollback")]
    pub apply: bool,
    /// 搬回老布局并去掉标记(daemon 必须已停)
    #[arg(long)]
    pub rollback: bool,
    /// 管理员家目录名(仅首次搬家时生效;缺省取系统用户名)
    #[arg(long, value_name = "NAME")]
    pub admin: Option<String>,
}

fn home_layout_for(paths: &MiyuPaths, admin_override: Option<&str>) -> Result<HomeLayout> {
    let layout = Layout {
        root_dir: paths.root_dir.clone(),
        config_dir: paths.config_dir.clone(),
        data_dir: paths.data_dir.clone(),
        cache_dir: paths.cache_dir.clone(),
        state_dir: paths.state_dir.clone(),
    };
    let admin = match read_home_layout_admin(&paths.root_dir)? {
        Some(admin) => admin,
        None => match admin_override {
            Some(name) if crate::paths::is_valid_home_name(name) => name.to_string(),
            Some(name) => bail!(
                "{}",
                owned(
                    format!("invalid admin home name: {name}"),
                    format!("管理员家目录名不合法:{name}")
                )
            ),
            None => admin_home_name_from_env(),
        },
    };
    Ok(HomeLayout { layout, admin })
}

pub(in crate::cli) fn run_layout(paths: &MiyuPaths, args: LayoutArgs) -> Result<()> {
    let layout = home_layout_for(paths, args.admin.as_deref())?;
    let migrated = read_home_layout_admin(&paths.root_dir)?.is_some();
    if args.rollback {
        if !migrated {
            println!(
                "{}",
                t(
                    "Already on the legacy layout; nothing to roll back.",
                    "本来就是老布局,没有可回滚的。"
                )
            );
            return Ok(());
        }
        if !rollback_home_layout(&layout, current_process_is_daemon())? {
            bail!(
                "{}",
                t(
                    "a Miyu daemon is running; stop it (miyu daemon stop) and retry",
                    "daemon 还在跑;先 miyu daemon stop 再试"
                )
            );
        }
        println!(
            "{}",
            t(
                "Rolled back to the legacy layout (data/ and state/); automatic migration stays off until `miyu layout --apply`.",
                "已搬回老布局(data/ 与 state/);自动搬家已关,`miyu layout --apply` 再开。"
            )
        );
        return Ok(());
    }
    if args.apply {
        if migrated {
            println!(
                "{}",
                t(
                    "Already on the home layout; nothing to do.",
                    "已经是家目录布局,不用再搬。"
                )
            );
            return Ok(());
        }
        clear_home_layout_opt_out(&layout)?;
        if !try_migrate_home_layout(&layout, current_process_is_daemon())? {
            bail!(
                "{}",
                t(
                    "a Miyu daemon is running; stop it (miyu daemon stop) and retry",
                    "daemon 还在跑;先 miyu daemon stop 再试"
                )
            );
        }
        println!(
            "{}",
            owned(
                format!(
                    "Moved into the home layout; admin home is {}",
                    layout.admin_home().display()
                ),
                format!(
                    "已搬成家目录布局;管理员家目录 {}",
                    layout.admin_home().display()
                )
            )
        );
        return Ok(());
    }
    let opted_out = home_layout_opted_out(&paths.root_dir)?;
    println!(
        "{}",
        if migrated {
            owned(
                format!(
                    "Layout: home (admin home {})",
                    layout.admin_home().display()
                ),
                format!(
                    "当前布局:家目录(管理员家目录 {})",
                    layout.admin_home().display()
                ),
            )
        } else if opted_out {
            owned(
                format!(
                    "Layout: legacy (rolled back; automatic migration is off). `miyu layout --apply` would move (admin home {}):",
                    layout.admin_home().display()
                ),
                format!(
                    "当前布局:老布局(已回滚,自动搬家已关)。`miyu layout --apply` 会搬(管理员家目录 {}):",
                    layout.admin_home().display()
                ),
            )
        } else {
            owned(
                format!(
                    "Layout: legacy (data/ + state/). `miyu layout --apply` would move (admin home {}):",
                    layout.admin_home().display()
                ),
                format!(
                    "当前布局:老布局(data/ + state/)。`miyu layout --apply` 会搬(管理员家目录 {}):",
                    layout.admin_home().display()
                ),
            )
        }
    );
    for step in layout.plan()? {
        let (from, to) = if migrated {
            (step.destination, step.source)
        } else {
            (step.source, step.destination)
        };
        let present = if migrated {
            from.exists()
        } else {
            step.present
        };
        println!(
            "  {} {} -> {}",
            if present { "*" } else { " " },
            from.display(),
            to.display()
        );
    }
    println!(
        "{}",
        t(
            "(* = present; the move runs automatically when the daemon next starts)",
            "(* = 存在;daemon 下次启动时自动搬)"
        )
    );
    Ok(())
}
