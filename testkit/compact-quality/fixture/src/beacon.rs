//! 信标心跳。

/// 心跳间隔（毫秒）。
pub const BEACON_INTERVAL_MS: u64 = 3700;

/// 发一次心跳，返回本次序号。
pub fn beacon_pulse(sequence: u64) -> u64 {
    sequence.wrapping_add(1)
}

/// 连续丢多少次心跳算掉线。
pub const BEACON_MISS_LIMIT: u32 = 4;
