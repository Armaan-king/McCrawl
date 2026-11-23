use mccrawl::dedup::{NEAR_DUP_BITS, NearDupIndex, SeenSet, body_hash, hamming, simhash, url_hash};
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

#[test]
fn seen_set_tells_urls_apart() {
    let mut s = SeenSet::default();
    assert!(s.is_empty());
    assert!(s.insert(&Url::parse("https://a.com/x").unwrap()));
    assert!(s.insert(&Url::parse("https://a.com/x?").unwrap())); // the seen-set doesn't normalize
    assert!(s.insert(&Url::parse("http://a.com/x").unwrap()));
    assert_eq!((s.len(), s.is_empty()), (3, false));
}

#[test]
fn simhash_ignores_case_and_whitespace_and_handles_short_texts() {
    assert_eq!(simhash("Hello World Foo"), simhash("hello\n\tworld   FOO"));
    assert_eq!(simhash("   \n\t "), 0); // whitespace only counts as empty
    let (a, b) = (simhash("a"), simhash("b"));
    assert!(a != 0 && b != 0 && a != b); // one or two words still get a fingerprint
    assert_ne!(simhash("a b"), simhash("b a")); // word order matters
    assert_ne!(simhash("Straße über"), 0);
    assert_eq!(simhash("STRASSE"), simhash("strasse"));
}

#[test]
fn fingerprints_are_stable_across_runs_and_versions() {
    // Golden values: fingerprints are saved with every page and compared across crawls,
    // so a refactor or optimization must never change them.
    let text = "the quick brown fox jumps over the lazy dog";
    let url = Url::parse("https://a.com/x").unwrap();
    assert_eq!(
        (simhash(text), url_hash(&url), body_hash(b"hello")),
        // body_hash("hello") is the reference XXH3-64 value, so Python's xxhash agrees.
        (
            0x75a5_b714_549a_617a,
            0x12fa_457f_8110_da38,
            0x9555_e855_5c62_dcfd
        )
    );
    // 4 words = 2 shingles, so about half the bits get tied votes (+1 −1): pins tie handling.
    assert_eq!(simhash("one two three four"), 0x0008_1482_2800_0280);
}

#[test]
fn block_index_finds_everything_within_3_bits_and_nothing_further() {
    // The pigeonhole guarantee, checked on 2,000 random fingerprints with 0..=6 random bits flipped.
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    let mut rand = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    for _ in 0..2_000 {
        let fp = rand();
        let mut idx = NearDupIndex::default();
        idx.insert(fp, 1);
        for k in 0..=6 {
            let mut mask = 0u64;
            while mask.count_ones() < k {
                mask |= 1 << (rand() % 64);
            }
            let found = idx.find(fp ^ mask).is_some();
            assert_eq!(found, k <= NEAR_DUP_BITS, "fp {fp:#x}, mask {mask:#x}");
        }
    }
}

#[test]
fn hamming_basics() {
    assert_eq!(hamming(0, 0), 0);
    assert_eq!(hamming(0, u64::MAX), 64);
    assert_eq!(hamming(0b1010, 0b0110), 2);
    assert_eq!(hamming(5, 9), hamming(9, 5));
}

#[test]
fn near_dup_index_edges() {
    let mut idx = NearDupIndex::default();
    assert_eq!(idx.find(42), None); // empty index
    let fp = 0x0123_4567_89AB_CDEFu64;
    idx.insert(fp, 1);
    idx.insert(fp ^ 0xF000, 2); // 4 bits away from id 1; shares blocks 1-3 with it
    // Distance 5 to id 1 and 1 to id 2: the shared bucket holds both, and the far one is skipped.
    assert_eq!(idx.find(fp ^ 0xF001), Some(2));
    assert_eq!(idx.find(fp ^ 0b111), Some(1)); // 3 bits inside one block
    let one_bit_per_block = 1 | 1 << 16 | 1 << 32 | 1 << 48;
    assert_eq!(idx.find(fp ^ one_bit_per_block), None); // 4 bits, and no block matches
    let edges = 1 << 63 | 1 << 47 | 1 << 31; // top bit of blocks 3, 2, 1
    assert_eq!(idx.find(fp ^ edges), Some(1));
}
