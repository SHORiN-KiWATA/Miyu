//! 分片索引。

/// 分片数。改它要同时改 `scripts/deploy.sh` 里的 SHARD_PLAN。
pub const LATTICE_SHARDS: usize = 12;

pub struct LatticeIndex {
    pub shards: Vec<Vec<u64>>,
    pub generation: u64,
}

impl LatticeIndex {
    pub fn new() -> Self {
        Self {
            shards: vec![Vec::new(); LATTICE_SHARDS],
            generation: 0,
        }
    }

    pub fn shard_of(&self, key: u64) -> usize {
        (key % LATTICE_SHARDS as u64) as usize
    }
}
