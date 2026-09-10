//! 成员回合 / 工具桥的作用域(09-11):工作区落在成员家里,子进程套 Landlock。
//!
//! 会话归成员(归属键非空、账号不是管理员)就给一份 [`MemberScope`]:
//! - 工作区 = `home/<用户>/workspace`(不看会话记录里的 workspace——成员改不了,
//!   也不该把 daemon 的 cwd 当工作区);
//! - 沙盒策略 = 整机只读 + 可写 {工作区, /tmp, /dev/null, 脚本缓存目录}。
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
    let policy = SandboxPolicy {
        read_only: vec![PathBuf::from("/")],
        read_write: vec![
            workspace.clone(),
            PathBuf::from("/tmp"),
            PathBuf::from("/dev/null"),
            paths.cache_dir.clone(),
        ],
    };
    Some(MemberScope {
        workspace,
        policy: Arc::new(policy),
    })
}
