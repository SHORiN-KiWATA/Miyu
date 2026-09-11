//! 成员子进程沙盒(09-11,Landlock)。
//!
//! 照搬 dsh 的 `landlock-run` 思路:fork 之后、exec 之前给**子进程自己**装一套
//! Landlock 规则集(允许列表),规则随 `execve` 继承,命令和它再起的一切子进程
//! 都受限,daemon 本身不受影响。没有任何依赖——三个裸 syscall 加一次 `prctl`,
//! 内核 5.13+ 自带。
//!
//! 策略由调用方给(回合层按「会话归谁」算):成员的回合与工具桥里,run_command、
//! 后台 job、脚本工具起的进程只能写自己家里的工作区、`/tmp` 与脚本缓存,其余
//! 只读;管理员不套。内核不支持(没编 Landlock / 被禁)就**失败关闭**:成员的
//! 命令一个都不跑,而不是裸奔。
//!
//! 只管文件系统。网络(ABI 4 的 TCP bind/connect)不在 handled 集合里,不受限。

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone, Default)]
pub struct SandboxPolicy {
    /// 只读 + 可执行(目录下的一切)。
    pub read_only: Vec<PathBuf>,
    /// 内核能管的全部文件系统权限。
    pub read_write: Vec<PathBuf>,
    /// 子进程的 HOME(成员的工作区):登录 shell 读 ~/.profile、程序写 ~/.cache
    /// 都落在这里,而不是撞在管理员家门口的 Permission denied 上。
    pub home: Option<PathBuf>,
}

/// 进程内工具(read/edit/glob/grep/print_image/看图……)读路径前过一遍:
/// 有沙盒策略时,路径必须落在只读或可写根之下;没有策略原样放行。
pub fn guard_read(path: &std::path::Path) -> anyhow::Result<()> {
    guard(path, false)
}

/// 同上,写路径:只认可写根。
pub fn guard_write(path: &std::path::Path) -> anyhow::Result<()> {
    guard(path, true)
}

fn guard(path: &std::path::Path, write: bool) -> anyhow::Result<()> {
    let Some(policy) = current_sandbox() else {
        return Ok(());
    };
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        crate::tools::workspace::effective_workdir().join(path)
    };
    let resolved = resolve_existing_prefix(&absolute);
    let allowed = policy
        .read_write
        .iter()
        .chain(if write {
            [].iter()
        } else {
            policy.read_only.iter()
        })
        .any(|root| {
            let root = root.canonicalize().unwrap_or_else(|_| root.clone());
            resolved.starts_with(&root)
        });
    if allowed {
        Ok(())
    } else {
        anyhow::bail!(
            "sandbox: {} is outside your workspace ({} not allowed there)",
            path.display(),
            if write { "writing" } else { "reading" }
        )
    }
}

/// 把路径里已存在的最长前缀 canonicalize(跟符号链接走),剩下的原样接回去——
/// 还不存在的文件也能判在哪个根下,`..` 与软链绕不出去。
fn resolve_existing_prefix(path: &std::path::Path) -> PathBuf {
    let mut existing = path.to_path_buf();
    let mut rest = Vec::new();
    while !existing.exists() {
        match (existing.file_name(), existing.parent()) {
            (Some(name), Some(parent)) => {
                rest.push(name.to_os_string());
                existing = parent.to_path_buf();
            }
            _ => break,
        }
    }
    let mut resolved = existing.canonicalize().unwrap_or(existing);
    for name in rest.into_iter().rev() {
        if name == ".." {
            resolved.pop();
        } else if name != "." {
            resolved.push(name);
        }
    }
    resolved
}

tokio::task_local! {
    static SANDBOX: Option<Arc<SandboxPolicy>>;
}

/// 在这个 future 里起的子进程(经 [`confine`] / [`confine_std`])都套这套策略;
/// `None` = 不套(管理员、终端、平台回合的老路)。
pub async fn with_sandbox<F: std::future::Future>(
    policy: Option<Arc<SandboxPolicy>>,
    future: F,
) -> F::Output {
    SANDBOX.scope(policy, future).await
}

pub fn current_sandbox() -> Option<Arc<SandboxPolicy>> {
    SANDBOX.try_with(|policy| policy.clone()).ok().flatten()
}

/// 有策略在身就给 Command 挂 `pre_exec`(在子进程里装规则再 exec);没有就原样。
pub fn confine(command: &mut tokio::process::Command) {
    if let Some(policy) = current_sandbox() {
        if let Some(home) = &policy.home {
            command.env("HOME", home);
        }
        let rules = Rules::prepare(&policy);
        // SAFETY: 闭包只做裸 syscall / open / close,不碰锁、不分配。
        unsafe {
            command.pre_exec(move || rules.apply());
        }
    }
}

pub fn confine_std(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    if let Some(policy) = current_sandbox() {
        if let Some(home) = &policy.home {
            command.env("HOME", home);
        }
        let rules = Rules::prepare(&policy);
        // SAFETY: 同上。
        unsafe {
            command.pre_exec(move || rules.apply());
        }
    }
}

/// 内核能不能用:`Some(abi)` 能(ABI 号,>= 3 算完整),`None` 不能。启动时记日志用。
pub fn probe() -> Option<i64> {
    let abi = unsafe {
        libc::syscall(
            NR_LANDLOCK_CREATE_RULESET,
            std::ptr::null::<RulesetAttr>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    (abi > 0).then_some(abi as i64)
}

// ── Landlock UAPI,本地定义(内核 ABI 稳定;与 dsh landlock-run 逐字一致) ──

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
}

#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1 << 0;
const LANDLOCK_RULE_PATH_BENEATH: u32 = 1;

const NR_LANDLOCK_CREATE_RULESET: libc::c_long = 444;
const NR_LANDLOCK_ADD_RULE: libc::c_long = 445;
const NR_LANDLOCK_RESTRICT_SELF: libc::c_long = 446;

const FS_EXECUTE: u64 = 1 << 0;
const FS_WRITE_FILE: u64 = 1 << 1;
const FS_READ_FILE: u64 = 1 << 2;
const FS_READ_DIR: u64 = 1 << 3;
const FS_REFER: u64 = 1 << 13; // ABI 2
const FS_TRUNCATE: u64 = 1 << 14; // ABI 3
const FS_IOCTL_DEV: u64 = 1 << 15; // ABI 5
const ABI1_MASK: u64 = FS_REFER - 1;
const MAX_ABI: i64 = 5;

fn fs_mask_for_abi(abi: i64) -> u64 {
    let mut mask = ABI1_MASK;
    if abi >= 2 {
        mask |= FS_REFER;
    }
    if abi >= 3 {
        mask |= FS_TRUNCATE;
    }
    if abi >= 5 {
        mask |= FS_IOCTL_DEV;
    }
    mask
}

/// fork 前就把路径转成 C 字符串:`pre_exec` 里不该再分配。
struct Rules {
    read_only: Vec<CString>,
    read_write: Vec<CString>,
}

impl Rules {
    fn prepare(policy: &SandboxPolicy) -> Self {
        let to_c = |paths: &[PathBuf]| {
            paths
                .iter()
                .filter_map(|path| CString::new(path.as_os_str().as_bytes()).ok())
                .collect::<Vec<_>>()
        };
        Self {
            read_only: to_c(&policy.read_only),
            read_write: to_c(&policy.read_write),
        }
    }

    /// 子进程里跑:建规则集 → 逐条加路径 → no_new_privs → 套到自己身上。
    /// 出错就返回 errno 风格的 io::Error(不分配),spawn 随之失败。
    fn apply(&self) -> std::io::Result<()> {
        let abi = unsafe {
            libc::syscall(
                NR_LANDLOCK_CREATE_RULESET,
                std::ptr::null::<RulesetAttr>(),
                0usize,
                LANDLOCK_CREATE_RULESET_VERSION,
            )
        };
        if abi <= 0 {
            // ENOSYS:内核没编 Landlock;EOPNOTSUPP:编了但没启用。失败关闭。
            return Err(std::io::Error::from_raw_os_error(libc::ENOSYS));
        }
        let handled = fs_mask_for_abi((abi as i64).min(MAX_ABI));
        let attr = RulesetAttr {
            handled_access_fs: handled,
        };
        let ruleset_fd = unsafe {
            libc::syscall(
                NR_LANDLOCK_CREATE_RULESET,
                &attr as *const RulesetAttr,
                std::mem::size_of::<RulesetAttr>(),
                0u32,
            )
        } as libc::c_int;
        if ruleset_fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let read_side = (FS_EXECUTE | FS_READ_FILE | FS_READ_DIR) & handled;
        for path in &self.read_only {
            add_rule(ruleset_fd, path, read_side)?;
        }
        for path in &self.read_write {
            add_rule(ruleset_fd, path, handled)?;
        }
        if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if unsafe { libc::syscall(NR_LANDLOCK_RESTRICT_SELF, ruleset_fd, 0u32) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        unsafe { libc::close(ruleset_fd) };
        Ok(())
    }
}

fn add_rule(ruleset_fd: libc::c_int, path: &CString, mut access: u64) -> std::io::Result<()> {
    let path_fd = unsafe { libc::open(path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
    if path_fd < 0 {
        // 授权根打不开:失败关闭,不静默缩小授权集。
        return Err(std::io::Error::last_os_error());
    }
    // 非目录只能带文件类的权限位(内核对目录专属位报 EINVAL)——`/dev/null` 这类
    // 授权靠它。
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(path_fd, &mut st) } == 0 && (st.st_mode & libc::S_IFMT) != libc::S_IFDIR
    {
        access &= FS_EXECUTE | FS_WRITE_FILE | FS_READ_FILE | FS_TRUNCATE | FS_IOCTL_DEV;
    }
    let attr = PathBeneathAttr {
        allowed_access: access,
        parent_fd: path_fd,
    };
    let rc = unsafe {
        libc::syscall(
            NR_LANDLOCK_ADD_RULE,
            ruleset_fd,
            LANDLOCK_RULE_PATH_BENEATH,
            &attr as *const PathBeneathAttr,
            0u32,
        )
    };
    let error = (rc != 0).then(std::io::Error::last_os_error);
    unsafe { libc::close(path_fd) };
    match error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kernel_has_landlock() -> bool {
        probe().is_some()
    }

    /// 只读根 + 可写 /tmp 目录:目录里能写,别处不能;规则随 exec 继承到 sh。
    #[tokio::test]
    async fn member_policy_confines_shell_writes() {
        if !kernel_has_landlock() {
            eprintln!("skip: kernel without landlock");
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let allowed = temp.path().join("allowed");
        std::fs::create_dir_all(&allowed).unwrap();
        let denied = temp.path().join("denied");
        std::fs::create_dir_all(&denied).unwrap();
        let policy = Arc::new(SandboxPolicy {
            read_only: vec![PathBuf::from("/")],
            read_write: vec![allowed.clone(), PathBuf::from("/dev/null")],
            home: Some(allowed.clone()),
        });
        let script = format!(
            "echo ok > {}/a.txt && ! (echo no > {}/b.txt) 2>/dev/null && cat /etc/hostname >/dev/null",
            allowed.display(),
            denied.display()
        );
        let status = with_sandbox(Some(policy), async move {
            let mut command = tokio::process::Command::new("sh");
            command.arg("-c").arg(script);
            confine(&mut command);
            command.status().await.unwrap()
        })
        .await;
        assert!(status.success(), "sandboxed shell script failed: {status}");
        assert!(allowed.join("a.txt").is_file());
        assert!(!denied.join("b.txt").exists());
    }

    /// 进程内守卫:可写根里能读能写,只读根里只能读,别处都不行;`..` 绕不出去。
    #[tokio::test]
    async fn in_process_guard_follows_the_policy() {
        let temp = tempfile::tempdir().unwrap();
        let rw = temp.path().join("rw");
        let ro = temp.path().join("ro");
        std::fs::create_dir_all(&rw).unwrap();
        std::fs::create_dir_all(&ro).unwrap();
        std::fs::write(ro.join("a.txt"), "a").unwrap();
        let policy = Arc::new(SandboxPolicy {
            read_only: vec![ro.clone()],
            read_write: vec![rw.clone()],
            home: None,
        });
        let outside = temp.path().join("outside.txt");
        let outside_in = outside.clone();
        with_sandbox(Some(policy), async move {
            let outside = outside_in;
            assert!(guard_read(&ro.join("a.txt")).is_ok());
            assert!(guard_write(&ro.join("a.txt")).is_err());
            assert!(guard_read(&rw.join("new.txt")).is_ok());
            assert!(guard_write(&rw.join("new.txt")).is_ok());
            assert!(guard_read(&outside).is_err());
            assert!(guard_write(&rw.join("../outside.txt")).is_err());
            assert!(guard_read(std::path::Path::new("/etc/hostname")).is_err());
        })
        .await;
        assert!(guard_read(&outside).is_ok(), "no policy = no guard");
    }

    #[tokio::test]
    async fn no_policy_means_no_confinement() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("free.txt");
        let mut command = tokio::process::Command::new("sh");
        command
            .arg("-c")
            .arg(format!("echo hi > {}", target.display()));
        confine(&mut command);
        assert!(command.status().await.unwrap().success());
        assert!(target.is_file());
    }
}
