//! 家目录布局(09-10 分层架构阶段 6):目录树仿 Linux。
//!
//! ```text
//! ~/.miyu/
//!   config/            系统配置(/etc)
//!   personas/          管理员发布的共享人格(/usr/share):default/、dev/
//!   extensions/        已装扩展(/usr/lib):skills/、scripts/
//!   state/ cache/ models/   机器运行的东西(/var)
//!   home/<用户名>/     人的东西:conversation.db、profile.md、identities/、
//!                      artifacts/、documents/、pictures/、ledger/、shares/
//! ```
//!
//! 三条规则:用户的进 home、管理员发布的在根只读、机器运行的在 state。
//! 第一个账号(管理员)也进 home,不留例外;成员的家目录在注册时建。
//! 会话库暂时仍是一份(成员的会话靠 owner 列区分),按人拆库是下一步。
//!
//! 搬家复用资源迁移那套「预检 → 日志 → 原子移动 → 标记」;标记文件
//! `.home-layout-v1` 的内容就是管理员的家目录名,以后改系统用户名也不受影响。
//! `miyu layout` 能看计划、手动执行与回滚。

use crate::paths::*;

pub(crate) const HOME_LAYOUT_MARKER: &str = ".home-layout-v1";
pub(crate) const HOME_MIGRATION_JOURNAL: &str = ".home-layout-v1.journal.json";
/// 回滚之后留下的「别再自动搬」标记:同一个二进制下次启动就不会又搬回去。
/// `miyu layout --apply` 会撤掉它。
pub(crate) const HOME_LAYOUT_OPT_OUT: &str = ".home-layout-off";
/// 拿不到合法系统用户名时的家目录名。
pub(crate) const DEFAULT_ADMIN_HOME: &str = "admin";

/// 家目录布局的坐标:根布局 + 管理员家目录名。
#[derive(Clone, Debug)]
pub(crate) struct HomeLayout {
    pub(crate) layout: Layout,
    pub(crate) admin: String,
}

/// 一条搬家计划(给 `miyu layout` 看的):源、目标、源现在在不在。
#[derive(Clone, Debug)]
pub(crate) struct HomeMovePlan {
    pub(crate) source: PathBuf,
    pub(crate) destination: PathBuf,
    pub(crate) present: bool,
}

impl HomeLayout {
    pub(crate) fn marker(&self) -> PathBuf {
        self.layout.root_dir.join(HOME_LAYOUT_MARKER)
    }

    pub(crate) fn journal(&self) -> PathBuf {
        self.layout.root_dir.join(HOME_MIGRATION_JOURNAL)
    }

    pub(crate) fn opt_out_marker(&self) -> PathBuf {
        self.layout.root_dir.join(HOME_LAYOUT_OPT_OUT)
    }

    pub(crate) fn homes_dir(&self) -> PathBuf {
        self.layout.root_dir.join("home")
    }

    pub(crate) fn admin_home(&self) -> PathBuf {
        self.homes_dir().join(&self.admin)
    }

    pub(crate) fn personas_dir(&self) -> PathBuf {
        self.layout.root_dir.join("personas")
    }

    pub(crate) fn extensions_dir(&self) -> PathBuf {
        self.layout.root_dir.join("extensions")
    }

    /// 搬家清单,顺序即执行顺序。`user-identity.md` 先于它所在的 `identities/`
    /// 整目录:文件改名成 profile.md 落到家目录根,其余人格专属身份跟目录走。
    pub(crate) fn entries(&self) -> Vec<ResourceMigrationEntry> {
        let data = &self.layout.data_dir;
        let state = &self.layout.state_dir;
        let home = self.admin_home();
        let mut entries = vec![
            ResourceMigrationEntry {
                source: data.join("personas"),
                destination: self.personas_dir(),
            },
            ResourceMigrationEntry {
                source: data.join("skills"),
                destination: self.extensions_dir().join("skills"),
            },
            ResourceMigrationEntry {
                source: data.join("scripts"),
                destination: self.extensions_dir().join("scripts"),
            },
        ];
        // WAL/SHM 与主库同进退:daemon 崩过的话未 checkpoint 的提交还在 wal 里。
        for name in [
            "conversation.db",
            "conversation.db-wal",
            "conversation.db-shm",
        ] {
            entries.push(ResourceMigrationEntry {
                source: state.join(name),
                destination: home.join(name),
            });
        }
        for (from, to) in [
            ("artifacts", "artifacts"),
            ("documents", "documents"),
            ("pictures", "pictures"),
            ("ledger", "ledger"),
            ("shared", "shares"),
        ] {
            entries.push(ResourceMigrationEntry {
                source: data.join(from),
                destination: home.join(to),
            });
        }
        entries.push(ResourceMigrationEntry {
            source: data.join("identities/user-identity.md"),
            destination: home.join("profile.md"),
        });
        entries.push(ResourceMigrationEntry {
            source: data.join("identities"),
            destination: home.join("identities"),
        });
        entries
    }

    pub(crate) fn plan(&self) -> Result<Vec<HomeMovePlan>> {
        self.entries()
            .into_iter()
            .map(|entry| {
                Ok(HomeMovePlan {
                    present: entry_exists(&entry.source)?,
                    source: entry.source,
                    destination: entry.destination,
                })
            })
            .collect()
    }
}

/// 家目录名规则与账号用户名一致(3–32 位,字母数字 `_ - .`,首字符字母或
/// 数字),目录名直接用它。
pub(crate) fn is_valid_home_name(name: &str) -> bool {
    let count = name.chars().count();
    (3..=32).contains(&count)
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        && name != "."
        && name != ".."
}

/// 迁移时给管理员挑家目录名:`MIYU_ADMIN_USER` → 系统用户名 → `admin`。
pub(crate) fn admin_home_name_from_env() -> String {
    for key in ["MIYU_ADMIN_USER", "USER", "LOGNAME"] {
        if let Some(value) = std::env::var_os(key) {
            let value = value.to_string_lossy().trim().to_string();
            if is_valid_home_name(&value) {
                return value;
            }
        }
    }
    DEFAULT_ADMIN_HOME.to_string()
}

/// 用户回滚过并要求别再自动搬。
pub(crate) fn home_layout_opted_out(root: &Path) -> Result<bool> {
    marker_exists_at(
        &root.join(HOME_LAYOUT_OPT_OUT),
        "home layout opt-out marker",
    )
}

/// 标记里记的管理员家目录名;没标记 = 还是老布局。
pub(crate) fn read_home_layout_admin(root: &Path) -> Result<Option<String>> {
    let marker = root.join(HOME_LAYOUT_MARKER);
    if !marker_exists_at(&marker, "home layout marker")? {
        return Ok(None);
    }
    let raw =
        fs::read_to_string(&marker).with_context(|| format!("reading {}", marker.display()))?;
    let name = raw.trim();
    if !is_valid_home_name(name) {
        bail!(
            "Miyu home layout marker {} does not name a valid admin home ({name:?})",
            marker.display()
        );
    }
    Ok(Some(name.to_string()))
}

fn write_home_marker(layout: &HomeLayout) -> Result<()> {
    let path = layout.marker();
    let temporary = path.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        rand::random::<u64>()
    ));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&temporary)
        .with_context(|| format!("creating {}", temporary.display()))?;
    file.write_all(layout.admin.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(&temporary, &path)
        .with_context(|| format!("installing home layout marker {}", path.display()))?;
    sync_parent(&path)
}

/// 自动搬家(daemon 起来时 / 没 daemon 的 CLI):已搬过就 true;有别的
/// daemon 在跑就 false(下次再试);预检不过报错且零写入。
pub(crate) fn try_migrate_home_layout(
    layout: &HomeLayout,
    current_process_is_daemon: bool,
) -> Result<bool> {
    if read_home_layout_admin(&layout.layout.root_dir)?.is_some() {
        remove_journal_if_present_at(&layout.journal())?;
        return Ok(true);
    }
    ensure_private_dir(&layout.layout.root_dir)?;
    let _lease = acquire_migration_lock(&layout.layout.root_dir)?;
    if read_home_layout_admin(&layout.layout.root_dir)?.is_some() {
        remove_journal_if_present_at(&layout.journal())?;
        return Ok(true);
    }
    let Some(_daemon_guard) =
        try_acquire_resource_daemon_guard(&layout.layout, current_process_is_daemon)?
    else {
        return Ok(false);
    };
    recover_migration_at(&layout.journal())?;
    let mut entries = Vec::new();
    for entry in layout.entries() {
        if entry_exists(&entry.source)? {
            entries.push(entry);
        }
    }
    preflight_entries(&layout.layout.root_dir, &entries)?;
    ensure_private_dir(&layout.homes_dir())?;
    ensure_private_dir(&layout.admin_home())?;
    run_journaled_moves(&layout.journal(), entries)?;
    write_home_marker(layout)?;
    remove_journal_if_present_at(&layout.journal())?;
    Ok(true)
}

/// 回滚:把搬过去的都搬回来,删掉标记。只在没有 daemon 时做。
pub(crate) fn rollback_home_layout(
    layout: &HomeLayout,
    current_process_is_daemon: bool,
) -> Result<bool> {
    if read_home_layout_admin(&layout.layout.root_dir)?.is_none() {
        return Ok(true);
    }
    let _lease = acquire_migration_lock(&layout.layout.root_dir)?;
    let Some(_daemon_guard) =
        try_acquire_resource_daemon_guard(&layout.layout, current_process_is_daemon)?
    else {
        return Ok(false);
    };
    recover_migration_at(&layout.journal())?;
    // 逆序、反向:目录在前文件在后搬过去的,回来时文件先回目录里。
    let mut entries = Vec::new();
    for entry in layout.entries().into_iter().rev() {
        if entry_exists(&entry.destination)? {
            entries.push(ResourceMigrationEntry {
                source: entry.destination,
                destination: entry.source,
            });
        }
    }
    preflight_entries(&layout.layout.root_dir, &entries)?;
    run_journaled_moves(&layout.journal(), entries)?;
    let marker = layout.marker();
    fs::remove_file(&marker).with_context(|| format!("removing {}", marker.display()))?;
    sync_parent(&marker)?;
    remove_journal_if_present_at(&layout.journal())?;
    write_marker(&layout.opt_out_marker())?;
    Ok(true)
}

/// `miyu layout --apply`:撤掉回滚留下的「别再搬」标记。
pub(crate) fn clear_home_layout_opt_out(layout: &HomeLayout) -> Result<()> {
    let marker = layout.opt_out_marker();
    match fs::remove_file(&marker) {
        Ok(()) => sync_parent(&marker),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}
