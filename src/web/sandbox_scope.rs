//! 成员回合 / 工具桥的作用域(09-11):工作区落在成员家里,子进程套 Landlock。
//!
//! 会话归成员(归属键非空、账号不是管理员)就给一份 [`MemberScope`]:
//! - 工作区 = `home/<用户>/workspace`(不看会话记录里的 workspace——成员改不了,
//!   也不该把 daemon 的 cwd 当工作区);
//! - 沙盒策略(09-11 用户拍板:沙盒外的读取也禁):可写 {工作区, /tmp, /dev/null,
//!   脚本缓存目录};只读只给跑程序必需的系统目录(/usr /etc /proc …)、内置与已装
//!   脚本目录、miyu 自己的二进制;管理员的家、~/.miyu 的配置与库都摸不到。
//!
//! 管理员、终端、平台回合拿到 `None`:老路不变。

use crate::tools::sandbox::SandboxPolicy;
use crate::web::*;

pub(in crate::web) struct MemberScope {
    pub(in crate::web) workspace: PathBuf,
    pub(in crate::web) policy: Arc<SandboxPolicy>,
}

pub(in crate::web) fn member_scope(
    paths: &MiyuPaths,
    admin_store: &StateStore,
    stores: &StoreRegistry,
    session_id: &str,
) -> Option<MemberScope> {
    let owner = stores.owner_of_session(session_id)?;
    if owner.is_empty() {
        return None;
    }
    let account = admin_store.account_by_id(&owner).ok().flatten()?;
    if account.is_admin() {
        return None;
    }
    let workspace = paths.user_home_dir(&account.username).join("workspace");
    if let Err(error) = crate::paths::ensure_private_dir(&workspace) {
        tracing::warn!(error = %error, path = %workspace.display(), "member workspace dir");
    }
    let mut read_only: Vec<PathBuf> = [
        "/usr", "/bin", "/sbin", "/lib", "/lib64", "/etc", "/proc", "/sys", "/dev", "/run", "/opt",
        "/var",
    ]
    .iter()
    .map(PathBuf::from)
    .collect();
    read_only.push(paths.scripts_dir.clone());
    read_only.push(paths.system_scripts_dir.clone());
    if let Ok(exe) = std::env::current_exe() {
        read_only.push(exe);
    }
    // Landlock 对打不开的授权根是失败关闭:不存在的目录先剔掉。
    read_only.retain(|path| path.exists());
    let policy = SandboxPolicy {
        read_only,
        read_write: vec![
            workspace.clone(),
            PathBuf::from("/tmp"),
            PathBuf::from("/dev/null"),
            paths.cache_dir.clone(),
        ],
        home: Some(workspace.clone()),
    };
    Some(MemberScope {
        workspace,
        policy: Arc::new(policy),
    })
}
