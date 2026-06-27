use super::{ToolRegistry, ToolSpec};
use anyhow::{bail, Result};
use base64::Engine;
use blake2::{Blake2b512, Blake2s256};
use serde_json::{json, Value};
use sha1::Digest as Sha1Digest;

pub fn register(registry: &mut ToolRegistry) {
    registry.register(ToolSpec::new("calculate_hash", "Calculate hashes for text/hex/base64 input. Supports md5, sha1, sha224, sha256, sha384, sha512, sha3_224, sha3_256, sha3_384, sha3_512, blake2b, blake2s, b2sum/blake3, crc32, adler32, and all/mainstream.", json!({"type":"object","properties":{"input_text":{"type":"string"},"algorithms":{"type":"string"},"input_format":{"type":"string","enum":["text","hex","base64"]}},"required":["input_text"],"additionalProperties":false}), |args| async move { calculate(args) }));
    registry.register(ToolSpec::new("decode_encoded_text", "Decode base64, hex, url, html, or rot13 encoded text.", json!({"type":"object","properties":{"input_text":{"type":"string"},"input_format":{"type":"string","enum":["base64","hex","url","html","rot13"]},"text_encoding":{"type":"string"}},"required":["input_text","input_format"],"additionalProperties":false}), |args| async move { decode(args) }));
}

fn calculate(args: Value) -> Result<String> {
    let input = args
        .get("input_text")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let fmt = args
        .get("input_format")
        .and_then(Value::as_str)
        .unwrap_or("text");
    let data = bytes(input, fmt)?;
    let algorithms = args
        .get("algorithms")
        .and_then(Value::as_str)
        .unwrap_or("sha256");
    let algs: Vec<&str> =
        if algorithms.trim().is_empty() || algorithms == "all" || algorithms == "mainstream" {
            vec![
                "md5", "sha1", "sha224", "sha256", "sha384", "sha512", "sha3_224", "sha3_256",
                "sha3_384", "sha3_512", "blake2b", "blake2s", "b2sum", "crc32", "adler32",
            ]
        } else {
            algorithms
                .split([',', ' '])
                .filter(|item| !item.is_empty())
                .collect()
        };
    let mut results = serde_json::Map::new();
    for alg in algs {
        let value = match alg.to_lowercase().as_str() {
            "md5" => format!("{:x}", md5_compat::compute(&data)),
            "sha1" => format!("{:x}", sha1::Sha1::digest(&data)),
            "sha224" => format!("{:x}", sha2::Sha224::digest(&data)),
            "sha256" => format!("{:x}", sha2::Sha256::digest(&data)),
            "sha384" => format!("{:x}", sha2::Sha384::digest(&data)),
            "sha512" => format!("{:x}", sha2::Sha512::digest(&data)),
            "sha3_224" | "sha3-224" => format!("{:x}", sha3::Sha3_224::digest(&data)),
            "sha3_256" | "sha3-256" => format!("{:x}", sha3::Sha3_256::digest(&data)),
            "sha3_384" | "sha3-384" => format!("{:x}", sha3::Sha3_384::digest(&data)),
            "sha3_512" | "sha3-512" => format!("{:x}", sha3::Sha3_512::digest(&data)),
            "blake2b" => format!("{:x}", Blake2b512::digest(&data)),
            "blake2s" => format!("{:x}", Blake2s256::digest(&data)),
            "b2sum" | "blake3" => blake3::hash(&data).to_hex().to_string(),
            "crc32" => format!("{:08x}", crc32fast::hash(&data)),
            "adler32" => format!("{:08x}", adler32(&data)),
            other => format!("unsupported algorithm: {other}"),
        };
        results.insert(alg.to_string(), Value::String(value));
    }
    Ok(serde_json::to_string_pretty(
        &json!({"success": true, "byte_length": data.len(), "results": results}),
    )?)
}

fn adler32(data: &[u8]) -> u32 {
    const MOD: u32 = 65521;
    let mut a = 1u32;
    let mut b = 0u32;
    for byte in data {
        a = (a + *byte as u32) % MOD;
        b = (b + a) % MOD;
    }
    (b << 16) | a
}

fn decode(args: Value) -> Result<String> {
    let input = args
        .get("input_text")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let fmt = args
        .get("input_format")
        .and_then(Value::as_str)
        .unwrap_or("base64");
    let output = match fmt {
        "base64" => String::from_utf8_lossy(
            &base64::engine::general_purpose::STANDARD.decode(input.trim())?,
        )
        .to_string(),
        "hex" => String::from_utf8_lossy(&hex::decode(input.trim())?).to_string(),
        "url" => urlencoding::decode(input)?.to_string(),
        "html" => input
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&amp;", "&")
            .replace("&quot;", "\"")
            .replace("&#39;", "'"),
        "rot13" => input.chars().map(rot13).collect(),
        other => bail!("unsupported input_format: {other}"),
    };
    Ok(serde_json::to_string_pretty(
        &json!({"success": true, "decoded_text": output}),
    )?)
}

fn bytes(input: &str, fmt: &str) -> Result<Vec<u8>> {
    Ok(match fmt {
        "text" => input.as_bytes().to_vec(),
        "hex" => hex::decode(input.trim())?,
        "base64" => base64::engine::general_purpose::STANDARD.decode(input.trim())?,
        other => bail!("unsupported input_format: {other}"),
    })
}

fn rot13(ch: char) -> char {
    match ch {
        'a'..='z' => (((ch as u8 - b'a' + 13) % 26) + b'a') as char,
        'A'..='Z' => (((ch as u8 - b'A' + 13) % 26) + b'A') as char,
        _ => ch,
    }
}

mod md5_compat {
    pub use md5::compute;
}
