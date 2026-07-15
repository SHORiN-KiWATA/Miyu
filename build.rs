use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

use base64::{engine::general_purpose, Engine as _};

const PROMPT_MASK: &[u8] = b"MiyuPromptMask";

fn main() {
    println!("cargo:rerun-if-changed=src/prompts/miyu.md");
    println!("cargo:rerun-if-changed=src/prompts/plan.md");
    println!("cargo:rerun-if-changed=src/prompts/chat.md");
    println!("cargo:rerun-if-changed=assets/o200k_base.tiktoken");

    let prompt = fs::read("src/prompts/miyu.md").expect("read src/prompts/miyu.md");
    let encoded = prompt
        .into_iter()
        .enumerate()
        .map(|(index, byte)| byte ^ PROMPT_MASK[index % PROMPT_MASK.len()])
        .collect::<Vec<_>>();
    let encoded = base64_encode(&encoded);
    let out_dir = env::var("OUT_DIR").expect("OUT_DIR is set by cargo");
    let dest = Path::new(&out_dir).join("default_miyu_prompt.rs");
    fs::write(
        dest,
        format!(
            "const PROMPT_MASK: &[u8] = b\"MiyuPromptMask\";\nconst OBFUSCATED_DEFAULT_SYSTEM_PROMPT: &str = \"{encoded}\";\n"
        ),
    )
    .expect("write generated prompt asset");

    build_o200k_vocab();
}

fn build_o200k_vocab() {
    let source =
        fs::read_to_string("assets/o200k_base.tiktoken").expect("read o200k_base vocabulary");
    let mut output = Vec::with_capacity(source.len() / 2);
    let mut tokens = HashSet::with_capacity(199_998);
    let mut count = 0usize;
    for (expected_rank, line) in source.lines().enumerate() {
        let mut parts = line.split(' ');
        let token = general_purpose::STANDARD
            .decode(parts.next().expect("vocabulary token"))
            .expect("decode vocabulary token");
        assert!(tokens.insert(token.clone()), "duplicate o200k token");
        let rank = parts
            .next()
            .expect("vocabulary rank")
            .parse::<usize>()
            .expect("parse vocabulary rank");
        assert_eq!(rank, expected_rank, "o200k ranks must be sequential");
        let len = u16::try_from(token.len()).expect("token length fits in u16");
        output.extend_from_slice(&len.to_le_bytes());
        output.extend_from_slice(&token);
        count += 1;
    }
    assert_eq!(count, 199_998, "unexpected o200k vocabulary size");
    assert_eq!(tokens.len(), count, "o200k tokens must be unique");

    let destination =
        PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR")).join("o200k_base.bin");
    fs::write(destination, output).expect("write compact o200k vocabulary");
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        output.push(TABLE[(first >> 2) as usize] as char);
        output.push(TABLE[(((first & 0b0000_0011) << 4) | (second >> 4)) as usize] as char);
        if chunk.len() > 1 {
            output.push(TABLE[(((second & 0b0000_1111) << 2) | (third >> 6)) as usize] as char);
        } else {
            output.push('=');
        }
        if chunk.len() > 2 {
            output.push(TABLE[(third & 0b0011_1111) as usize] as char);
        } else {
            output.push('=');
        }
    }
    output
}
