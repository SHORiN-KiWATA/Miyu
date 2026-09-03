//! OpenAI 兼容客户端的测试，按协议与关注点分文件。
//!
//! 原本是一个三千多行的 `mod tests`。三套线路协议（chat / responses / anthropic）
//! 各有各的流式事件形态，混在一起看不出哪条守着哪个契约。

mod anthropic;
mod antigravity;
mod chat_stream;
mod claude_code;
mod codex;
mod endpoint_retry;
mod extra_body;
mod failover;
mod responses;
mod shared;
mod thinking;
