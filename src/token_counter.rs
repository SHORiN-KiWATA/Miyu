use anyhow::{ensure, Result};
use fancy_regex::Regex;
use rustc_hash::FxHashMap;
use std::collections::BinaryHeap;
use std::num::NonZeroU64;
use std::sync::OnceLock;
use std::thread;

type Rank = u32;
type RankMap = FxHashMap<Box<[u8]>, Rank>;
const MAX_NUM_THREADS: usize = 128;
const O200K_PATTERN: &str = concat!(
    r#"[^\r\n\p{L}\p{N}]?[\p{Lu}\p{Lt}\p{Lm}\p{Lo}\p{M}]*[\p{Ll}\p{Lm}\p{Lo}\p{M}]+(?i:'s|'t|'re|'ve|'m|'ll|'d)?"#,
    "|",
    r#"[^\r\n\p{L}\p{N}]?[\p{Lu}\p{Lt}\p{Lm}\p{Lo}\p{M}]+[\p{Ll}\p{Lm}\p{Lo}\p{M}]*(?i:'s|'t|'re|'ve|'m|'ll|'d)?"#,
    "|",
    r#"\p{N}{1,3}"#,
    "|",
    r#" ?[^\s\p{L}\p{N}]+[\r\n/]*"#,
    "|",
    r#"\s*[\r\n]+"#,
    "|",
    r#"\s+(?!\S)"#,
    "|",
    r#"\s+"#
);

static COUNTER: OnceLock<CoreBpeCounter> = OnceLock::new();

pub fn count(text: &str) -> usize {
    COUNTER
        .get_or_init(|| CoreBpeCounter::new().expect("embedded o200k vocabulary must be valid"))
        .count_ordinary(text)
}

struct CoreBpeCounter {
    encoder: RankMap,
    regex_tls: Vec<Regex>,
}

impl CoreBpeCounter {
    fn new() -> Result<Self> {
        let data = include_bytes!(concat!(env!("OUT_DIR"), "/o200k_base.bin"));
        let mut encoder = RankMap::default();
        let mut cursor = 0usize;
        let mut rank = 0u32;
        while cursor < data.len() {
            ensure!(cursor + 2 <= data.len(), "truncated o200k token length");
            let len = u16::from_le_bytes([data[cursor], data[cursor + 1]]) as usize;
            cursor += 2;
            let end = cursor + len;
            ensure!(end <= data.len(), "truncated o200k token payload");
            encoder.insert(Box::<[u8]>::from(&data[cursor..end]), rank);
            cursor = end;
            rank += 1;
        }
        ensure!(rank == 199_998, "unexpected o200k vocabulary size");
        ensure!(encoder.len() == rank as usize, "duplicate o200k token");

        let regex = Regex::new(O200K_PATTERN)?;
        Ok(Self {
            encoder,
            regex_tls: (0..MAX_NUM_THREADS).map(|_| regex.clone()).collect(),
        })
    }

    fn count_ordinary(&self, text: &str) -> usize {
        let regex = &self.regex_tls[hash_current_thread() % MAX_NUM_THREADS];
        regex
            .find_iter(text)
            .map(|mat| {
                let piece = mat.unwrap().as_str().as_bytes();
                if self.encoder.contains_key(piece) {
                    1
                } else {
                    byte_pair_count(piece, &self.encoder)
                }
            })
            .sum()
    }
}

struct FakeThreadId(NonZeroU64);

fn hash_current_thread() -> usize {
    const _: [u8; 8] = [0; std::mem::size_of::<std::thread::ThreadId>()];
    const _: [u8; 8] = [0; std::mem::size_of::<FakeThreadId>()];
    let id = unsafe {
        std::mem::transmute::<std::thread::ThreadId, FakeThreadId>(thread::current().id()).0
    };
    u64::from(id) as usize
}

fn rank(ranks: &RankMap, piece: &[u8]) -> Rank {
    ranks.get(piece).copied().unwrap_or(Rank::MAX)
}

fn byte_pair_count(piece: &[u8], ranks: &RankMap) -> usize {
    if piece.len() == 1 {
        return 1;
    }
    if piece.len() < 100 {
        return byte_pair_merge(ranks, piece).len() - 1;
    }
    byte_pair_merge_large(ranks, piece).len()
}

fn byte_pair_merge(ranks: &RankMap, piece: &[u8]) -> Vec<(usize, Rank)> {
    let mut parts = Vec::with_capacity(piece.len() + 1);
    let mut min_rank = (Rank::MAX, usize::MAX);
    for i in 0..piece.len() - 1 {
        let rank = rank(ranks, &piece[i..i + 2]);
        if rank < min_rank.0 {
            min_rank = (rank, i);
        }
        parts.push((i, rank));
    }
    parts.push((piece.len() - 1, Rank::MAX));
    parts.push((piece.len(), Rank::MAX));

    let get_rank = |parts: &Vec<(usize, Rank)>, i: usize| {
        if i + 3 < parts.len() {
            rank(ranks, &piece[parts[i].0..parts[i + 3].0])
        } else {
            Rank::MAX
        }
    };
    while min_rank.0 != Rank::MAX {
        let i = min_rank.1;
        if i > 0 {
            parts[i - 1].1 = get_rank(&parts, i - 1);
        }
        parts[i].1 = get_rank(&parts, i);
        parts.remove(i + 1);

        min_rank = (Rank::MAX, usize::MAX);
        for (i, &(_, rank)) in parts[..parts.len() - 1].iter().enumerate() {
            if rank < min_rank.0 {
                min_rank = (rank, i);
            }
        }
    }
    parts
}

#[derive(Eq, PartialEq, Clone, Copy)]
struct Merge {
    start: usize,
    rank: Rank,
}

impl Ord for Merge {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other
            .rank
            .cmp(&self.rank)
            .then_with(|| other.start.cmp(&self.start))
    }
}

impl PartialOrd for Merge {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

struct State {
    prev: usize,
    end: usize,
    next_end: usize,
    next_rank: Rank,
    cur_rank: Rank,
}

fn byte_pair_merge_large(ranks: &RankMap, piece: &[u8]) -> Vec<Rank> {
    let mut state = Vec::with_capacity(piece.len());
    state.push(State {
        prev: usize::MAX,
        end: 1,
        next_end: 2,
        next_rank: Rank::MAX,
        cur_rank: Rank::MAX,
    });
    let mut heap = BinaryHeap::with_capacity(piece.len());
    for i in 0..piece.len() - 1 {
        let pair_rank = rank(ranks, &piece[i..i + 2]);
        if pair_rank != Rank::MAX {
            heap.push(Merge {
                start: i,
                rank: pair_rank,
            });
            state[i].next_rank = pair_rank;
        }
        state.push(State {
            prev: i,
            end: i + 2,
            next_end: i + 3,
            next_rank: Rank::MAX,
            cur_rank: Rank::MAX,
        });
    }

    let potential_merge =
        |state: &mut Vec<State>, heap: &mut BinaryHeap<Merge>, start: usize, next_end: usize| {
            state[start].next_end = next_end;
            state[start].next_rank = Rank::MAX;
            if next_end <= piece.len() {
                let next_rank = rank(ranks, &piece[start..next_end]);
                if next_rank != Rank::MAX {
                    heap.push(Merge {
                        start,
                        rank: next_rank,
                    });
                    state[start].next_rank = next_rank;
                }
            }
        };

    while let Some(left) = heap.pop() {
        if left.rank == Rank::MAX {
            break;
        }
        if left.rank != state[left.start].next_rank {
            continue;
        }
        let left_start = left.start;
        let right_start = state[left_start].end;
        let right_end = state[left_start].next_end;
        let right_next_end = state[right_start].next_end;
        state[left_start].cur_rank = state[left_start].next_rank;
        state[left_start].end = right_end;
        potential_merge(&mut state, &mut heap, left_start, right_next_end);
        if right_end < state.len() {
            state[right_end].prev = left_start;
        }
        if left_start > 0 {
            let prev_start = state[left_start].prev;
            potential_merge(&mut state, &mut heap, prev_start, right_end);
        }
        state[right_start].next_rank = Rank::MAX;
    }

    let mut result = Vec::new();
    let mut i = 0;
    while i < state.len() {
        result.push(if state[i].cur_rank != Rank::MAX {
            state[i].cur_rank
        } else {
            rank(ranks, &piece[i..state[i].end])
        });
        i = state[i].end;
    }
    result
}
