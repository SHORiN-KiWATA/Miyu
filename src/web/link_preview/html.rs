//! 从 HTML 的 `<head>` 里挖出做卡片要的那几样东西。
//!
//! 刻意不建 DOM：这里只要 title / description / image / site_name / favicon，
//! 而这些全在 `<head>` 的 meta 与 link 标签里。为几百字节的元数据把整页塞进
//! 解析器，代价（内存、时间、畸形 HTML 的边界）和收益不成比例。
//!
//! 扫描是纯函数，好测；网络那半在 `super`。

/// 扫描上限。调用方已经按 `</head>` 截过一刀，这里只是兜底——但不能比
/// `<head>` 常见的实际大小还小：YouTube 的 og 标签在第 70 万字节（09-09 实测）。
pub(super) const HEAD_SCAN_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct HeadMetadata {
    pub(super) title: String,
    pub(super) description: String,
    pub(super) image: String,
    pub(super) site_name: String,
    pub(super) icon: String,
}

/// 一个标签的属性表（键统一小写，值已解码常见实体）。
fn attributes(tag: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let bytes: Vec<char> = tag.chars().collect();
    let mut index = 0usize;
    // 跳过标签名。
    while index < bytes.len() && !bytes[index].is_whitespace() {
        index += 1;
    }
    while index < bytes.len() {
        while index < bytes.len() && (bytes[index].is_whitespace() || bytes[index] == '/') {
            index += 1;
        }
        let start = index;
        while index < bytes.len()
            && !bytes[index].is_whitespace()
            && bytes[index] != '='
            && bytes[index] != '/'
        {
            index += 1;
        }
        if index == start {
            break;
        }
        let key: String = bytes[start..index]
            .iter()
            .collect::<String>()
            .to_lowercase();
        while index < bytes.len() && bytes[index].is_whitespace() {
            index += 1;
        }
        if index >= bytes.len() || bytes[index] != '=' {
            out.push((key, String::new()));
            continue;
        }
        index += 1;
        while index < bytes.len() && bytes[index].is_whitespace() {
            index += 1;
        }
        let value = if index < bytes.len() && (bytes[index] == '"' || bytes[index] == '\'') {
            let quote = bytes[index];
            index += 1;
            let start = index;
            while index < bytes.len() && bytes[index] != quote {
                index += 1;
            }
            let value: String = bytes[start..index].iter().collect();
            index += 1;
            value
        } else {
            let start = index;
            while index < bytes.len() && !bytes[index].is_whitespace() {
                index += 1;
            }
            bytes[start..index].iter().collect()
        };
        out.push((key, decode_entities(&value)));
    }
    out
}

/// 只解常见的几个命名实体和数字实体。页面标题里出现的基本就这些。
fn decode_entities(value: &str) -> String {
    if !value.contains('&') {
        return value.to_string();
    }
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(position) = rest.find('&') {
        out.push_str(&rest[..position]);
        rest = &rest[position..];
        let Some(end) = rest[..rest.len().min(12)].find(';') else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..end];
        let replacement = match entity {
            "amp" => Some("&".to_string()),
            "lt" => Some("<".to_string()),
            "gt" => Some(">".to_string()),
            "quot" => Some("\"".to_string()),
            "apos" | "#39" => Some("'".to_string()),
            "nbsp" => Some(" ".to_string()),
            other => other
                .strip_prefix('#')
                .and_then(|digits| match digits.strip_prefix(['x', 'X']) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok(),
                    None => digits.parse::<u32>().ok(),
                })
                .and_then(char::from_u32)
                .map(String::from),
        };
        match replacement {
            Some(text) => {
                out.push_str(&text);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn attribute<'a>(attributes: &'a [(String, String)], name: &str) -> Option<&'a str> {
    attributes
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

/// 已有值不覆盖：标签在文档里的顺序就是优先级，og: 那批一般排在前面。
fn fill(slot: &mut String, value: Option<&str>) {
    if !slot.is_empty() {
        return;
    }
    let Some(value) = value.map(str::trim) else {
        return;
    };
    if !value.is_empty() {
        *slot = value.to_string();
    }
}

pub(super) fn extract(html: &str) -> HeadMetadata {
    let scope = &html[..html.len().min(HEAD_SCAN_BYTES)];
    let mut meta = HeadMetadata::default();
    let mut document_title = String::new();
    let mut twitter_title = String::new();
    let mut twitter_description = String::new();
    let mut twitter_image = String::new();
    let mut plain_description = String::new();
    // favicon 按「越明确越好」排序，最后才退到 /favicon.ico。
    let mut apple_icon = String::new();

    let lower = scope.to_lowercase();
    if let (Some(open), Some(close)) = (lower.find("<title"), lower.find("</title>")) {
        if let Some(text_start) = scope[open..close].find('>').map(|offset| open + offset + 1) {
            document_title = decode_entities(scope[text_start..close].trim());
        }
    }

    let mut cursor = 0usize;
    while let Some(offset) = lower[cursor..].find('<') {
        let start = cursor + offset;
        let Some(length) = scope[start..].find('>') else {
            break;
        };
        let tag = &scope[start + 1..start + length];
        cursor = start + length + 1;
        let name = tag
            .split(|character: char| character.is_whitespace() || character == '/')
            .next()
            .unwrap_or_default()
            .to_lowercase();
        // </head> 之后就是正文，正文里的 meta 与卡片无关。
        if name == "/head" || name == "body" {
            break;
        }
        if name != "meta" && name != "link" {
            continue;
        }
        let attrs = attributes(tag);
        if name == "link" {
            let rel = attribute(&attrs, "rel").unwrap_or_default().to_lowercase();
            let href = attribute(&attrs, "href");
            if rel.split_whitespace().any(|value| value == "icon") {
                fill(&mut meta.icon, href);
            } else if rel.contains("apple-touch-icon") {
                fill(&mut apple_icon, href);
            }
            continue;
        }
        let key = attribute(&attrs, "property")
            .or_else(|| attribute(&attrs, "name"))
            .unwrap_or_default()
            .to_lowercase();
        let content = attribute(&attrs, "content");
        match key.as_str() {
            "og:title" => fill(&mut meta.title, content),
            "og:description" => fill(&mut meta.description, content),
            "og:image" | "og:image:url" | "og:image:secure_url" => fill(&mut meta.image, content),
            "og:site_name" => fill(&mut meta.site_name, content),
            "twitter:title" => fill(&mut twitter_title, content),
            "twitter:description" => fill(&mut twitter_description, content),
            "twitter:image" | "twitter:image:src" => fill(&mut twitter_image, content),
            "description" => fill(&mut plain_description, content),
            _ => {}
        }
    }

    fill(&mut meta.title, Some(&twitter_title));
    fill(&mut meta.title, Some(&document_title));
    fill(&mut meta.description, Some(&twitter_description));
    fill(&mut meta.description, Some(&plain_description));
    fill(&mut meta.image, Some(&twitter_image));
    fill(&mut meta.icon, Some(&apple_icon));
    meta
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_graph_wins_over_the_document_title() {
        let html = r#"<html><head>
            <title>Fallback &amp; Co</title>
            <meta property="og:title" content="Register - Roogoo">
            <meta property="og:description" content="A borderless payment platform.">
            <meta property="og:image" content="/static/card.png">
            <meta property="og:site_name" content="Roogoo">
            <link rel="icon" href="/favicon.png">
        </head><body><meta property="og:title" content="正文里的不算"></body></html>"#;
        let meta = extract(html);
        assert_eq!(meta.title, "Register - Roogoo");
        assert_eq!(meta.description, "A borderless payment platform.");
        assert_eq!(meta.image, "/static/card.png");
        assert_eq!(meta.site_name, "Roogoo");
        assert_eq!(meta.icon, "/favicon.png");
    }

    #[test]
    fn falls_back_through_twitter_and_plain_tags() {
        let html = r#"<head>
            <title>Arch Wiki &#8212; Fcitx5</title>
            <meta name="description" content='输入法配置'>
            <meta name=twitter:image content=/tw.png>
            <link rel="apple-touch-icon" href="/touch.png">
        </head>"#;
        let meta = extract(html);
        assert_eq!(meta.title, "Arch Wiki — Fcitx5");
        assert_eq!(meta.description, "输入法配置");
        assert_eq!(meta.image, "/tw.png");
        assert_eq!(meta.icon, "/touch.png");
    }

    #[test]
    fn a_page_without_metadata_yields_nothing_but_a_title() {
        let meta = extract("<html><head><title>  Bare  </title></head><body>x</body></html>");
        assert_eq!(meta.title, "Bare");
        assert!(meta.description.is_empty());
        assert!(meta.image.is_empty());
    }

    #[test]
    fn malformed_markup_does_not_panic() {
        for html in [
            "<meta property=og:title content=",
            "<<<>>><meta",
            "<link rel=icon href=",
            "<title>unclosed",
            "&#xZZ; &notanentity; &",
        ] {
            let _ = extract(html);
        }
    }
}
