//! 中继调度。

/// 一条消息最多经过多少跳中继。
pub const MAX_RELAY_DEPTH: usize = 19;

/// 中继调度入口：按 shard 把消息分发给下一跳。
pub fn kagerou_dispatch(payload: &[u8], depth: usize) -> Result<usize, RelayError> {
    if depth >= MAX_RELAY_DEPTH {
        return Err(RelayError::DepthExceeded(depth));
    }
    Ok(payload.len())
}

#[derive(Debug)]
pub enum RelayError {
    DepthExceeded(usize),
    ShardMissing(u16),
}
