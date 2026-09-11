//! opencode Zen 的客户端识别头。
//!
//! Zen 的服务端按这几个头分桶。缺了它们的请求落进匿名档,额度比带全了小一大截
//! ——第三方客户端报的「key 明明有效还是狂吐 429」就是这么来的。Miyu 在此之前
//! 一个都不发。
//!
//! 头名与取值不是从文档抄的(Zen 没文档化),是本机 opencode 1.18.29 的实测抓包
//! ——把它的 `opencode` 供应商 baseURL 指到本地服务器录下来的,测具在
//! `testkit/opencode-zen/capture_headers.py`。原样:
//!
//! ```text
//! User-Agent: opencode/1.18.29 ai-sdk/provider-utils/4.0.46 runtime/bun/1.4.0
//! x-opencode-client: cli
//! x-opencode-project: global
//! x-opencode-session: ses_f7f57a497ffeztcKAAwwE6ZtFU
//! x-opencode-request: msg_080a85b8d001GbUgeBrzPv21j7
//! ```
//!
//! 三处与网上流传的说法不同,以实测为准:
//! - `x-opencode-project` 不在项目里时是**字面量 `global`**,不是随机 id
//!   (在 git 项目里才是 40 位 hex 的路径散列);
//! - `x-opencode-request` **不是每个 HTTP 请求换**,它是用户消息 id,一个回合内
//!   连着几轮工具调用都用同一个;
//! - User-Agent 是 `opencode/<版本> ai-sdk/... runtime/bun/...`,不是
//!   `opencode/latest/<版本>/cli`。
//!
//! 判定按**端点**而不是供应商 id:用户可以把 Zen 那份配置改名(测试里就有个叫
//! `myopencode` 的),而决定服务端怎么看这次请求的是打到哪个地址。同 `errors`
//! 里的 `zen_upstream_failed`。
//!
//! 端点不止 `/zen/v1` 一个:Console Go 挂在 `/zen/go/v1`,同一套头同一个网关。
//! 09-11 实测(`testkit/opencode-zen/go_endpoint_headers.py`)——Go 那边缺
//! `x-opencode-session` 是**硬 400**(`MissingSessionID`),不像 Zen 只是把你
//! 丢进匿名桶,所以这里按 `/zen` 这一层整段判,新开的兄弟端点自动覆盖到。

use crate::config::ProviderConfig;
use rand::Rng;
use sha1::Digest;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// 抓包里 opencode CLI 报的自己。版本跟着本机装的那份走,升级 opencode 后
/// 这里不会自动变——它只是辅助识别,主识别位是 `x-opencode-client`。
const OPENCODE_USER_AGENT: &str = "opencode/1.18.29 ai-sdk/provider-utils/4.0.46 runtime/bun/1.4.0";

/// opencode 的 `--client` 默认值(它的 TUI 与 `run` 都是 cli)。
const OPENCODE_CLIENT: &str = "cli";

/// 不在某个具体项目里时 opencode 报的值。Miyu 不是按目录组织工作的,恒定报它。
const OPENCODE_PROJECT: &str = "global";

/// Zen 系端点共同的前缀:`/zen/v1`(Zen)与 `/zen/go/v1`(Console Go)都在它下面。
const OPENCODE_ZEN_ROOT: &str = "https://opencode.ai/zen";

/// 这次请求是不是打到 Zen 系端点(Zen 本体或它下面的 Console Go)。
pub(in crate::llm::openai_compatible) fn is_zen_endpoint(provider: &ProviderConfig) -> bool {
    match provider
        .base_url
        .trim_end_matches('/')
        .strip_prefix(OPENCODE_ZEN_ROOT)
    {
        // 只认整段路径:`/zen`、`/zen/v1`、`/zen/go/v1` 算,`/zenith/v1` 不算。
        Some(rest) => rest.is_empty() || rest.starts_with('/'),
        None => false,
    }
}

/// 给发往 Zen 的请求补上识别头;不是 Zen 的原样返回,一个头都不加。
pub(in crate::llm::openai_compatible) fn apply(
    request: reqwest::RequestBuilder,
    provider: &ProviderConfig,
    session: Option<&str>,
    request_id: &str,
) -> reqwest::RequestBuilder {
    if !is_zen_endpoint(provider) {
        return request;
    }
    request
        .header("User-Agent", OPENCODE_USER_AGENT)
        .header("x-opencode-client", OPENCODE_CLIENT)
        .header("x-opencode-project", OPENCODE_PROJECT)
        .header("x-opencode-session", session_id(session))
        .header("x-opencode-request", message_id(request_id))
}

/// 一次对话对应服务端一个会话:Miyu 的会话 id 确定性地映射成 opencode 形状,
/// 同一段对话跨 daemon 重启也是同一个。够不到会话的地方(REPL 直连、配置界面
/// 试连)退回进程级的那个——总比整个 daemon 共用一个、或者每个 HTTP 请求换一
/// 个(看着像刷会话)强。
fn session_id(session: Option<&str>) -> String {
    match session.map(str::trim).filter(|value| !value.is_empty()) {
        Some(session) => format!("ses_{}", digest_suffix(session)),
        None => process_session_id().to_string(),
    }
}

fn process_session_id() -> &'static str {
    static SESSION: OnceLock<String> = OnceLock::new();
    SESSION.get_or_init(|| format!("ses_{}", random_suffix()))
}

/// 把 Miyu 的请求 id(`llm_<毫秒>_<序号>`)换算成 opencode 形状的消息 id。
///
/// 确定性映射:同一次逻辑调用换端点重试时头不变,这与 opencode 那边「一个用户
/// 回合内恒定」的语义对得上。
fn message_id(request_id: &str) -> String {
    format!("msg_{}", digest_suffix(request_id))
}

/// 把任意标识散列成前缀之后那 26 位。十六进制是 base62 的子集,形状仍然对得上。
fn digest_suffix(value: &str) -> String {
    let digest = sha1::Sha1::digest(value.as_bytes());
    format!("{digest:x}")[..SUFFIX_LEN].to_string()
}

/// 抓到的 id 前缀之后恒为 26 位。
const SUFFIX_LEN: usize = 26;

/// 12 位十六进制(时间序)+ 14 位 base62,与抓包里的形状一致。里头的位不做他用,
/// 服务端也不需要解得开它。
fn random_suffix() -> String {
    const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0);
    let mut suffix = format!("{:012x}", millis & 0xffff_ffff_ffff);
    let mut rng = rand::thread_rng();
    for _ in 0..(SUFFIX_LEN - 12) {
        suffix.push(ALPHABET[rng.gen_range(0..ALPHABET.len())] as char);
    }
    suffix
}
