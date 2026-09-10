//! 被多层共用的命令行参数类型。
//!
//! 这些结构本身只是数据（clap 的 `Args` 派生），没有任何 CLI 执行逻辑，但
//! `web` 与 `daemon` 都要拿它们。放在 `cli.rs` 里会让 daemon 入口反过来依赖
//! CLI——这正是拆分要断的边之一。所以下沉到基础层：谁都能用，它谁都不用。
//!
//! `cli.rs` 仍然 re-export 这些名字，外部按 `cli::WebArgs` 引用不会断。
use clap::Args;

use crate::ipc;

#[derive(Args)]
pub struct WebArgs {
    #[arg(long, default_value_t = ipc::DEFAULT_WEB_PORT)]
    pub port: u16,

    /// WebUI 监听地址；默认 0.0.0.0（所有网卡），127.0.0.1 仅限本机访问。
    #[arg(long, value_name = "ADDR")]
    pub bind: Option<std::net::IpAddr>,

    #[arg(skip)]
    pub port_explicit: bool,
}

impl std::fmt::Debug for WebArgs {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WebArgs")
            .field("port", &self.port)
            .field("bind", &self.bind)
            .field("port_explicit", &self.port_explicit)
            .finish()
    }
}
