use mccrawl::dedup::{NEAR_DUP_BITS, NearDupIndex, SeenSet, hamming, simhash};
use url::Url;

/// Deterministic pseudo-text: `n` words drawn from a 1009-word vocabulary.
fn words(n: usize, prefix: &str) -> Vec<String> {
    (0..n)
        .map(|i| format!("{prefix}{}", (i * 7919) % 1009))
        .collect()
}

#[test]
fn seen_set_dedups() {
    let mut s = SeenSet::default();
    let u = Url::parse("https://a.com/x").unwrap();
    assert!(s.insert(&u));
    assert!(!s.insert(&u));
    assert_eq!(s.len(), 1);
}

#[test]
fn simhash_near_and_far() {
    let w = words(2000, "w");
    let a = w.join(" ");
    let mut w2 = w.clone();
    w2[1000] = "changed".into();
    let b = w2.join(" ");
    let c = words(2000, "z").join(" ");
    assert_eq!(simhash(&a), simhash(&a));
    assert!(hamming(simhash(&a), simhash(&b)) <= NEAR_DUP_BITS);
    assert!(hamming(simhash(&a), simhash(&c)) > 10);
    assert_eq!(simhash(""), 0);
}

#[test]
fn near_dup_index_uses_blocks_then_verifies_distance() {
    let mut idx = NearDupIndex::default();
    let fp = 0xDEAD_BEEF_1234_5678u64;
    idx.insert(fp, 7);
    assert_eq!(idx.find(fp), Some(7));
    assert_eq!(idx.find(fp ^ (1 | 1 << 20 | 1 << 40)), Some(7)); // 3 bits, 3 different blocks
    assert_eq!(idx.find(fp ^ 0b1111), None); // 4 bits: candidate found, then rejected by distance
    assert_eq!(idx.find(!fp), None);
}
