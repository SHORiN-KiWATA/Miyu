//! 链接的识别与终端呈现。
//!
//! 三件事，都是「模型写的其实是纯文本，人看到的却该是链接」：
//!
//! · **裸地址**——`见 https://a.com。` 里的地址此前一点颜色都没有，句尾的句号
//!   也不属于地址（成对括号要留下：维基/GitHub 的地址自己就带括号）。
//! · **`标题 (地址)` 独占一行**——模型给参考资料就是这么写的，标题是纯文本，
//!   于是只有地址那半截像链接。整行命中时标题跟着一起上色、一起成链。
//! · **OSC 8 超链接**——`ESC]8;;url ESC\ 文本 ESC]8;; ESC\`，kitty/foot/wezterm/
//!   Konsole/VTE 都认。`file://` 也走这条，本地路径在这些终端里点得开。
//!
//! 规则跟 web/app.js 的 `bareUrlAt`/`trimUrlTail` 是同一套，两端不该对同一段
//! 正文给出不同的判断。

use crate::render::style::{LINK_LABEL_STYLE, RESET, URL_STYLE};
use std::sync::OnceLock;

/// 认得的协议。`file://` 在里面是因为模型会用它指本地路径（MCP 服务器目录之类）。
const SCHEMES: [&str; 3] = ["https://", "http://", "file://"];

/// 句尾标点：跟在地址后面时一律不属于地址。
const TAIL_TRIM: &str = "。，、；：！？…～\"'`,.;:!?’”»›|*_~";

/// 成对的收尾符号：只有在地址里配不上开头符号时才算句读。
fn paired_opener(ch: char) -> Option<char> {
    match ch {
        ')' => Some('('),
        ']' => Some('['),
        '}' => Some('{'),
        '》' => Some('《'),
        '」' => Some('「'),
        '』' => Some('『'),
        '】' => Some('【'),
        _ => None,
    }
}

/// 地址正文允许的字符。中日韩标点一个都不能进：「…archlinux.org、AUR」里顿号
/// 后面还跟着字母，只在末尾修剪碰不到它，整段会被当成域名的一部分。
fn is_url_body(ch: char) -> bool {
    if ch.is_whitespace() || matches!(ch, '<' | '>' | '"' | '\'' | '`' | '\u{00a0}') {
        return false;
    }
    !matches!(ch as u32, 0x2000..=0x206f | 0x3000..=0x303f | 0xff00..=0xffef)
}

/// `text` 是不是以认得的协议开头，是的话返回协议长度（全是 ASCII，字节=字符）。
pub(crate) fn scheme_len(text: &str) -> Option<usize> {
    // 按字节比：`text` 的开头可能是多字节汉字，切片会撞非字符边界。
    SCHEMES
        .iter()
        .find(|scheme| {
            text.as_bytes()
                .get(..scheme.len())
                .is_some_and(|head| head.eq_ignore_ascii_case(scheme.as_bytes()))
        })
        .map(|scheme| scheme.len())
}

fn scheme_len_at(chars: &[char], index: usize) -> Option<usize> {
    SCHEMES.iter().copied().find_map(|scheme| {
        let width = scheme.len();
        let end = index.checked_add(width)?;
        if end > chars.len() {
            return None;
        }
        chars[index..end]
            .iter()
            .zip(scheme.chars())
            .all(|(left, right)| left.eq_ignore_ascii_case(&right))
            .then_some(width)
    })
}

fn trim_url_tail(raw: &str) -> String {
    let mut value: Vec<char> = raw.chars().collect();
    while let Some(&last) = value.last() {
        if let Some(opener) = paired_opener(last) {
            let opens = value.iter().filter(|ch| **ch == opener).count();
            let closes = value.iter().filter(|ch| **ch == last).count();
            if closes <= opens {
                break;
            }
            value.pop();
            continue;
        }
        if TAIL_TRIM.contains(last) {
            value.pop();
            continue;
        }
        break;
    }
    value.into_iter().collect()
}

/// `chars[index]` 处是不是一个裸地址的开头，是的话返回地址原文。
/// 前一个字符是字母数字就不认：避开 `xhttps://` 这类粘连。
pub(crate) fn bare_url_at(chars: &[char], index: usize) -> Option<String> {
    if index > 0 && chars[index - 1].is_ascii_alphanumeric() {
        return None;
    }
    let width = scheme_len_at(chars, index)?;
    let mut end = index + width;
    while end < chars.len() && is_url_body(chars[end]) {
        end += 1;
    }
    let raw: String = chars[index..end].iter().collect();
    let trimmed = trim_url_tail(&raw);
    // 光有个协议头不算地址。
    (trimmed.chars().count() > width).then_some(trimmed)
}

/// 终端认不认 OSC 8。颜色本来就是无条件发的，所以这里只拦真的会把转义序列
/// 打成乱码的那两个（`dumb` 与 Linux 文本控制台）。
fn hyperlinks_supported() -> bool {
    if cfg!(test) {
        return false;
    }
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        !matches!(
            std::env::var("TERM").unwrap_or_default().as_str(),
            "dumb" | "linux"
        )
    })
}

/// OSC 8 的字节形态。跟能力判定分开，测试里才能验序列本身
/// （`hyperlinks_supported` 在 cfg(test) 下恒为假，见 AGENTS §5.3）。
pub(crate) fn osc8(url: &str, body: &str) -> String {
    format!("\x1b]8;;{url}\x1b\\{body}\x1b]8;;\x1b\\")
}

/// 把已经上好色的 `body` 挂到 `url` 上。终端不认就原样返回。
pub(crate) fn hyperlink(url: &str, body: &str) -> String {
    if !hyperlinks_supported() || url.contains(['\x1b', '\x07']) {
        return body.to_string();
    }
    osc8(url, body)
}

/// 上色 + 成链的裸地址。
pub(crate) fn render_bare_url(url: &str) -> String {
    hyperlink(url, &format!("{URL_STYLE}{url}{RESET}"))
}

/// 整行是「标题 (地址)」时的渲染。标题跟地址同属一个链接——模型给参考资料就
/// 这么写，只把括号里那半截标蓝，读起来像标题和链接是两码事。
pub(crate) fn render_title_url_line(line: &str) -> Option<String> {
    let trimmed = line.trim_end();
    let body_start = trimmed.len() - trimmed.trim_start().len();
    let (indent, body) = trimmed.split_at(body_start);
    let close = body.chars().last()?;
    let open = match close {
        ')' => '(',
        '）' => '（',
        _ => return None,
    };
    let open_index = body.rfind(open)?;
    let url = body[open_index + open.len_utf8()..body.len() - close.len_utf8()].trim();
    if scheme_len(url).is_none() || url.chars().any(|ch| !is_url_body(ch)) {
        return None;
    }
    let label_raw = &body[..open_index];
    let label = label_raw.trim_end();
    // 标题里再有地址就不是「标题 (地址)」；结尾是 `]` 说明这其实是
    // `[label](url)`，那条有自己的分支——不拦住的话整条 Markdown 会被当成标题
    // 原样漏出来（09-09 走查抓到）。
    if label.is_empty() || label.contains("://") || label.ends_with(']') {
        return None;
    }
    let gap = &label_raw[label.len()..];
    let inner =
        format!("{LINK_LABEL_STYLE}{label}{RESET}{gap}{open}{URL_STYLE}{url}{RESET}{close}");
    Some(format!("{indent}{}", hyperlink(url, &inner)))
}
