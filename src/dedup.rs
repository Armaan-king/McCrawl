//! Dedup at three levels: exact URL (seen-set), exact body (hash) and near-duplicate
//! content (SimHash plus a Manku block index, so lookups never scan every page).

use std::collections::{HashMap, HashSet};
use url::Url;
use xxhash_rust::xxh3::xxh3_64;

/// Fingerprints within this many differing bits are near-duplicates.
pub const NEAR_DUP_BITS: u32 = 3;

/// 64-bit URL key for the seen-set. At 10^8 URLs the collision odds are about 3 in 10^4,
/// and a collision only means one page is skipped.
pub fn url_hash(url: &Url) -> u64 {
    xxh3_64(url.as_str().as_bytes())
}

/// Exact-duplicate key for response bodies (mirrors, `index.html` vs `/`).
pub fn body_hash(body: &[u8]) -> u64 {
    xxh3_64(body)
}

/// Number of differing bits; one `popcnt` instruction.
pub fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

/// 64-bit SimHash (Charikar) over lowercase word 3-shingles. Similar texts land within
/// a few bits of each other; unrelated texts differ in about 32. Empty text → 0.
pub fn simhash(text: &str) -> u64 {
    let words: Vec<u64> = text
        .split_whitespace()
        .map(|w| xxh3_64(w.to_lowercase().as_bytes()))
        .collect();
    if words.is_empty() {
        return 0;
    }
    let mut votes = [0i32; 64];
    for win in words.windows(3.min(words.len())) {
        let mut buf = [0u8; 24];
        for (i, h) in win.iter().enumerate() {
            buf[i * 8..i * 8 + 8].copy_from_slice(&h.to_le_bytes());
        }
        let h = xxh3_64(&buf[..win.len() * 8]);
        for (bit, v) in votes.iter_mut().enumerate() {
            *v += if (h >> bit) & 1 == 1 { 1 } else { -1 };
        }
    }
    votes
        .iter()
        .enumerate()
        .fold(0, |fp, (bit, &v)| if v > 0 { fp | (1 << bit) } else { fp })
}

/// Every URL ever enqueued, stored as its 64-bit hash.
// ponytail: 8 B/URL in RAM (~800 MB at 10^8); Bloom filter or on-disk set in sub-project 2.
#[derive(Default)]
pub struct SeenSet {
    hashes: HashSet<u64>,
}

impl SeenSet {
    /// `true` if the URL is new.
    pub fn insert(&mut self, url: &Url) -> bool {
        self.hashes.insert(url_hash(url))
    }

    pub fn len(&self) -> usize {
        self.hashes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.hashes.is_empty()
    }
}

/// Near-duplicate lookup (Manku, Jain, Das Sarma, WWW 2007). Four tables, each keyed by one
/// 16-bit block of the fingerprint. Pigeonhole: ≤ 3 differing bits leave at least one block
/// identical, so checking the 4 matching buckets finds every match without a full scan.
// ponytail: 4 copies of each fingerprint in RAM; sub-project 2 moves this to sorted on-disk tables.
#[derive(Default)]
pub struct NearDupIndex {
    tables: [HashMap<u16, Vec<(u64, u32)>>; 4],
}

impl NearDupIndex {
    fn block(fp: u64, i: usize) -> u16 {
        (fp >> (i * 16)) as u16
    }

    /// Id of a stored fingerprint within `NEAR_DUP_BITS` of `fp`, if any.
    pub fn find(&self, fp: u64) -> Option<u32> {
        (0..4).find_map(|i| {
            self.tables[i]
                .get(&Self::block(fp, i))?
                .iter()
                .find(|(c, _)| hamming(*c, fp) <= NEAR_DUP_BITS)
                .map(|&(_, id)| id)
        })
    }

    pub fn insert(&mut self, fp: u64, id: u32) {
        for (i, table) in self.tables.iter_mut().enumerate() {
            table.entry(Self::block(fp, i)).or_default().push((fp, id));
        }
    }
}
