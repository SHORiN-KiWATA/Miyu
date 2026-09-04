//! 工具层看平台回合上下文的那一小扇窗。
//!
//! `PlatformToolContext` 定义在 `crate::platform_types`（纯数据 + 契约层），
//! 这里给 `PlatformTurnContext` 实现它。分成两处是为了让工具层只依赖契约，
//! 不依赖几百行的平台运行时结构——改平台的任何东西都不该触发工具层重编。
use super::{PlatformImageData, PlatformPrincipal, PlatformTurnContext};
use anyhow::Result;
/// 工具层只依赖这个 trait，不依赖 `PlatformTurnContext` 本身。
impl crate::platform_types::PlatformToolContext for PlatformTurnContext {
    fn principal(&self) -> PlatformPrincipal {
        PlatformTurnContext::principal(self)
    }

    fn is_admin(&self) -> bool {
        self.is_admin
    }

    fn sender_display_name(&self) -> String {
        self.sender_display_name.clone()
    }

    fn host_tools_allowed(&self) -> bool {
        PlatformTurnContext::host_tools_allowed(self)
    }

    fn message_images_task(
        &self,
        message_id: String,
    ) -> futures_util::future::BoxFuture<'static, Result<Vec<PlatformImageData>>> {
        PlatformTurnContext::message_images_task(self, message_id)
    }

    fn fetch_platform_file_task(
        &self,
        file_ref: crate::platform_types::PlatformContextFileRef,
    ) -> futures_util::future::BoxFuture<'static, Result<crate::platform_types::PlatformFileDownload>>
    {
        PlatformTurnContext::fetch_platform_file_task(self, file_ref)
    }
}
