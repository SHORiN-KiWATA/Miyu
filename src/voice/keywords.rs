//! 唤醒词编码:中文唤醒词 → KWS 模型的拼音 token 行。
//!
//! wenetspeech KWS 模型的建模单元是"声母 + 带声调韵母"(如
//! `x iǎo m ǐ x iǎo m ǐ @小米小米`)。这里不硬编码汉语音系,而是把
//! 每个字的带调全拼按模型 tokens.txt 里实际存在的单元做最长匹配拆分,
//! tokens.txt 变了也能跟着走。

use anyhow::{bail, Context, Result};
use pinyin::ToPinyin;
use std::collections::HashSet;
use std::path::Path;

/// 声母表,按长度降序保证 zh/ch/sh 先于 z/c/s 命中。
const INITIALS: [&str; 23] = [
    "zh", "ch", "sh", "b", "p", "m", "f", "d", "t", "n", "l", "g", "k", "h", "j", "q", "x", "r",
    "z", "c", "s", "y", "w",
];

/// 把唤醒词编码成 keywords_buf 的一行:`token token … @原文`。
pub fn encode_keyword(keyword: &str, tokens_file: &Path) -> Result<String> {
    let keyword = keyword.trim();
    if keyword.is_empty() {
        bail!("唤醒词为空");
    }
    let tokens = load_tokens(tokens_file)
        .with_context(|| format!("读取 KWS tokens: {}", tokens_file.display()))?;
    let mut parts: Vec<String> = Vec::new();
    for ch in keyword.chars() {
        if ch.is_whitespace() {
            continue;
        }
        let syllable = ch
            .to_pinyin()
            .with_context(|| format!("唤醒词只支持中文字符,「{ch}」无法注音"))?
            .with_tone();
        parts.extend(
            split_syllable(syllable, &tokens)
                .with_context(|| format!("「{ch}」({syllable}) 无法映射到唤醒模型的建模单元"))?,
        );
    }
    if parts.is_empty() {
        bail!("唤醒词没有可编码的内容");
    }
    Ok(format!("{} @{keyword}", parts.join(" ")))
}

fn load_tokens(path: &Path) -> Result<HashSet<String>> {
    let raw = std::fs::read_to_string(path)?;
    Ok(raw
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .map(str::to_string)
        .collect())
}

fn split_syllable(syllable: &str, tokens: &HashSet<String>) -> Result<Vec<String>> {
    for initial in INITIALS {
        if let Some(rest) = syllable.strip_prefix(initial) {
            if !rest.is_empty() && tokens.contains(initial) && tokens.contains(rest) {
                return Ok(vec![initial.to_string(), rest.to_string()]);
            }
        }
    }
    // 零声母音节(如 ài、ér)整体就是一个 token。
    if tokens.contains(syllable) {
        return Ok(vec![syllable.to_string()]);
    }
    bail!("音节 {syllable} 不在模型词表中");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tokens_file(entries: &[&str]) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        for (index, entry) in entries.iter().enumerate() {
            writeln!(file, "{entry} {index}").unwrap();
        }
        file
    }

    #[test]
    fn encodes_initial_final_pairs() {
        let file = tokens_file(&["w", "èi", "y", "ǒu"]);
        let line = encode_keyword("未有未有", file.path()).unwrap();
        assert_eq!(line, "w èi y ǒu w èi y ǒu @未有未有");
    }

    #[test]
    fn zero_initial_uses_whole_syllable() {
        let file = tokens_file(&["ài", "x", "iǎo"]);
        let line = encode_keyword("小爱", file.path()).unwrap();
        assert_eq!(line, "x iǎo ài @小爱");
    }

    #[test]
    fn rejects_non_chinese() {
        let file = tokens_file(&["m", "ǐ"]);
        assert!(encode_keyword("miyu", file.path()).is_err());
    }

    #[test]
    fn rejects_out_of_vocabulary_syllable() {
        let file = tokens_file(&["m"]);
        assert!(encode_keyword("米", file.path()).is_err());
    }
}
