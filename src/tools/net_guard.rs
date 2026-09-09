//! 出站抓取的地址闸门:SSRF 防护三件套。
//!
//! 原先长在 `web_images/download.rs` 里,只有搜图那条线用得上。链接卡片要在
//! Web 服务器里发同样性质的出站请求,而这类逻辑**只能有一份**——安全代码复制
//! 一遍就等于埋一个迟早会漂移的分身。
//!
//! 三层依次是:URL 形状(scheme / userinfo / 特殊主机名)、DNS 解析后的地址
//! (每一个解析结果都必须是公网地址,挡住 DNS rebinding)、以及 IP 段判定
//! (私网、回环、链路本地、CGNAT、文档段、IPv4-mapped IPv6 一并算作非公网)。

use anyhow::{bail, Context, Result};
use reqwest::Url;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

pub(crate) fn is_safe_remote_url(url: &Url) -> bool {
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if host == "localhost" || host.ends_with(".localhost") || host.ends_with(".local") {
        return false;
    }
    match host.parse::<IpAddr>() {
        Ok(ip) => is_public_ip(ip),
        Err(_) => true,
    }
}

pub(crate) async fn resolve_public_remote_target(
    url: &Url,
    timeout: Duration,
) -> Result<Option<(String, Vec<SocketAddr>)>> {
    if !is_safe_remote_url(url) {
        bail!("URL is not a safe public URL")
    }
    let host = url.host_str().context("URL has no host")?;
    if host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
        .is_ok()
    {
        return Ok(None);
    }
    let port = url.port_or_known_default().context("URL has no port")?;
    let addresses = tokio::time::timeout(timeout, tokio::net::lookup_host((host, port)))
        .await
        .context("DNS resolution timed out")??
        .collect::<Vec<_>>();
    if addresses.is_empty() || addresses.iter().any(|address| !is_public_ip(address.ip())) {
        bail!("host resolves to a non-public address")
    }
    Ok(Some((host.to_string(), addresses)))
}

pub(crate) fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [first, second, _, _] = ip.octets();
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_broadcast()
                || ip.is_documentation()
                || ip.is_unspecified()
                || ip.is_multicast()
                || first == 0
                || (first == 100 && (64..=127).contains(&second))
                || (first == 198 && matches!(second, 18 | 19))
                || first >= 240)
        }
        IpAddr::V6(ip) => {
            if let Some(mapped) = ip.to_ipv4_mapped() {
                return is_public_ip(IpAddr::V4(mapped));
            }
            let segments = ip.segments();
            !(ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                || ip.is_unique_local()
                || ip.is_unicast_link_local()
                || (segments[0] == 0x2001 && segments[1] == 0x0db8))
        }
    }
}
