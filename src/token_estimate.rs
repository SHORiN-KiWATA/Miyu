//! token 估算：优先使用 OpenAI-family `o200k_base` BPE，失败时回退到字符规则。

const CHARS_PER_TOKEN_LATIN: usize = 4;
const CHARS_PER_TOKEN_CJK: usize = 2;

/// 估算单段文本的 token 数（非空文本至少为 1，空串为 0）。
pub fn estimate_tokens(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    tiktoken_tokens(text)
        .unwrap_or_else(|| text_tokens(text))
        .max(1)
}

/// 估算多段文本合计 token 数。
#[allow(dead_code)]
pub fn estimate_texts_tokens(texts: &[&str]) -> u64 {
    let combined: String = texts.iter().copied().collect();
    estimate_tokens(&combined) as u64
}

fn text_tokens(text: &str) -> usize {
    let mut cjk = 0usize;
    let mut latin = 0usize;
    for ch in text.chars() {
        if is_cjk(ch) {
            cjk += 1;
        } else {
            latin += 1;
        }
    }
    cjk.div_ceil(CHARS_PER_TOKEN_CJK) + latin.div_ceil(CHARS_PER_TOKEN_LATIN)
}

fn tiktoken_tokens(text: &str) -> Option<usize> {
    std::panic::catch_unwind(|| crate::token_counter::count(text)).ok()
}

fn is_cjk(ch: char) -> bool {
    let code = ch as u32;
    (0x4E00..=0x9FFF).contains(&code)
        || (0x3400..=0x4DBF).contains(&code)
        || (0x20000..=0x2A6DF).contains(&code)
        || (0x3040..=0x30FF).contains(&code)
        || (0xAC00..=0xD7AF).contains(&code)
        || (0xFF00..=0xFFEF).contains(&code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latin_uses_bpe_tokenizer() {
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcdefgh"), 1);
        assert!(estimate_tokens("hello world") >= 2);
    }

    #[test]
    fn cjk_uses_bpe_tokenizer() {
        assert_eq!(estimate_tokens("你好"), 1);
        assert_eq!(estimate_tokens("你好世界"), 2);
        assert_eq!(estimate_tokens("你好世"), 2);
    }

    #[test]
    fn mixed_text_counts_non_empty_tokens() {
        assert_eq!(estimate_tokens("abcd你好"), 2);
        assert!(estimate_tokens("abc你好世") >= 2);
    }

    #[test]
    fn empty_is_zero() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_texts_tokens(&[]), 0);
    }

    #[test]
    fn counts_match_official_tiktoken_vectors() {
        let vectors = [
            ("", 0),
            ("hello world", 2),
            ("你好世界", 2),
            ("Rust + 中文 + emoji 🚀\nsecond line", 10),
            (" \t\n\r\n punctuation: !@#$%^&*()[]{}", 13),
            ("<|endoftext|><|endofprompt|>", 13),
            (include_str!("prompts/miyu.md"), 3174),
            (include_str!("prompts/compact.md"), 230),
            (include_str!("prompts/plan.md"), 85),
            (include_str!("prompts/chat.md"), 29),
            (include_str!("../README.md"), 2808),
        ];

        for (text, expected) in vectors {
            assert_eq!(estimate_tokens(text), expected);
        }
    }

    #[test]
    fn count_only_encoder_matches_full_encoder() {
        use sha2::{Digest, Sha256};

        let atoms = [
            "a",
            "Z",
            " token",
            "你好",
            "世界",
            "かな",
            "한글",
            "🚀",
            "🙂",
            "\n",
            "\r\n",
            "\t",
            "123",
            "0001",
            "_",
            "'s",
            "—",
            "<|endoftext|>",
        ];
        let full = tiktoken_rs::o200k_base().unwrap();
        let mut digest = Sha256::new();
        let mut total = 0usize;

        for seed in 0..8192usize {
            let mut text = String::new();
            for step in 0..(seed % 64 + 1) {
                let index = seed.wrapping_mul(17).wrapping_add(step.wrapping_mul(31)) % atoms.len();
                text.push_str(atoms[index]);
            }
            let count = crate::token_counter::count(&text);
            assert_eq!(count, full.count_ordinary(&text));
            total += count;
            digest.update((count as u64).to_le_bytes());
        }

        assert_eq!(total, 383_718);
        assert_eq!(
            hex::encode(digest.finalize()),
            "64735b07c444d71dcd8977a257c8c029160793f46044ec257187a4707dd9def1"
        );
    }

    #[test]
    #[ignore]
    fn benchmark_count_only_encoder() {
        let atoms = [
            "a",
            "Z",
            " token",
            "你好",
            "世界",
            "かな",
            "한글",
            "🚀",
            "🙂",
            "\n",
            "\r\n",
            "\t",
            "123",
            "0001",
            "_",
            "'s",
            "—",
            "<|endoftext|>",
        ];
        let corpus = (0..8192usize)
            .map(|seed| {
                let mut text = String::new();
                for step in 0..(seed % 64 + 1) {
                    let index =
                        seed.wrapping_mul(17).wrapping_add(step.wrapping_mul(31)) % atoms.len();
                    text.push_str(atoms[index]);
                }
                text
            })
            .collect::<Vec<_>>();
        std::hint::black_box(crate::token_counter::count(&corpus[0]));

        let started = std::time::Instant::now();
        let total = corpus
            .iter()
            .map(|text| crate::token_counter::count(text))
            .sum::<usize>();
        let elapsed = started.elapsed();

        assert_eq!(total, 383_718);
        eprintln!(
            "tokenizer_benchmark elapsed_ns={} corpus_bytes={} tokens={total}",
            elapsed.as_nanos(),
            corpus.iter().map(String::len).sum::<usize>()
        );
    }
}
