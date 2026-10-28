//! SHA-512（`tls::sha512`。TASK-228・WIRE-9・HTTP-10 ポインタ。Issue #960・
//! 親 #941）の公開 API だけを使う結合テスト。単体テストの内部関数照合とは
//! 別に、外部から見える契約（`Sha512::new`／`update`／`finalize`・`digest`）が
//! FIPS 180-4／RFC 6234 の公開テストベクタと一致し、ストリーミング分割が
//! 一括呼び出しと同じ結果になることを固定する。

use wire_server::tls::sha512::{digest, Sha512, BLOCK_LEN, DIGEST_LEN};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// RFC 6234 §8.5 TEST1。
#[test]
fn digest_matches_rfc6234_test1_abc() {
    let d = digest(b"abc");
    assert_eq!(
        hex(&d),
        "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
    );
    assert_eq!(d.len(), DIGEST_LEN);
}

// RFC 6234 §8.5 TEST2_2（112 バイト・2 ブロックに収まる境界の入力）。
#[test]
fn digest_matches_rfc6234_test2_2() {
    let input = concat!(
        "abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmn",
        "hijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu",
    );
    assert_eq!(input.len(), 112);
    let d = digest(input.as_bytes());
    assert_eq!(
        hex(&d),
        "8e959b75dae313da8cf4f72814fc143f8f7779c6eb9f7fa17299aeadb6889018501d289e4900f7e4331b99dec4b5433ac7d329eeb6dd26545e96e55b874be909"
    );
}

// RFC 6234 §8.5 TEST3（"a" を 1,000,000 回）。
#[test]
fn digest_matches_rfc6234_test3_million_a() {
    let input = vec![b'a'; 1_000_000];
    let d = digest(&input);
    assert_eq!(
        hex(&d),
        "e718483d0ce769644e2e42c7bc15b4638e1f98b13b2044285632a803afa973ebde0ff244877ea60a4cb0432ce577c31beb009c5c2c49aa2e4eadb217ad8cc09b"
    );
}

// FIPS 180-4／NIST 公開計算例: 空入力。
#[test]
fn digest_matches_known_digest_for_empty_input() {
    let d = digest(b"");
    assert_eq!(
        hex(&d),
        "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e"
    );
}

// パディング境界長（111/112/113/127/128/129/239/240/256 バイト。`0x80` と
// 16 バイト長フィールドが 1 ブロックに収まるか次ブロックへ溢れるかの分かれ目）
// での既知解照合。期待値は `'a'` を n バイト並べた入力に対する独立実装
// （coreutils `sha512sum`・Python `hashlib.sha512`。両者一致を確認済み）の
// 出力で、内部の参照実装とは独立に一括・1 バイト刻みの双方を固定する。
#[test]
fn digest_matches_independent_known_answers_at_block_boundary_lengths() {
    const CASES: [(usize, &str); 9] = [
        (
            111,
            "fa9121c7b32b9e01733d034cfc78cbf67f926c7ed83e82200ef86818196921760b4beff48404df811b953828274461673c68d04e297b0eb7b2b4d60fc6b566a2",
        ),
        (
            112,
            "c01d080efd492776a1c43bd23dd99d0a2e626d481e16782e75d54c2503b5dc32bd05f0f1ba33e568b88fd2d970929b719ecbb152f58f130a407c8830604b70ca",
        ),
        (
            113,
            "55ddd8ac210a6e18ba1ee055af84c966e0dbff091c43580ae1be703bdb85da31acf6948cf5bd90c55a20e5450f22fb89bd8d0085e39f85a86cc46abbca75e24d",
        ),
        (
            127,
            "828613968b501dc00a97e08c73b118aa8876c26b8aac93df128502ab360f91bab50a51e088769a5c1eff4782ace147dce3642554199876374291f5d921629502",
        ),
        (
            128,
            "b73d1929aa615934e61a871596b3f3b33359f42b8175602e89f7e06e5f658a243667807ed300314b95cacdd579f3e33abdfbe351909519a846d465c59582f321",
        ),
        (
            129,
            "4f681e0bd53cda4b5a2041cc8a06f2eabde44fb16c951fbd5b87702f07aeab611565b19c47fde30587177ebb852e3971bbd8d3fd30da18d71037dfbd98420429",
        ),
        (
            239,
            "52c853cb8d907f3d4d6b889beb027985d7c273486d75f8baf26f80d24e90c74c6c3de3e22131582380a7d14d43f2941a31385439cd6ddc469f628015e50bf286",
        ),
        (
            240,
            "4c296d90c61052a62ffb1dd196f1b7b09373b1f93e71836baebf89690546b7595684dbe9467a8e484fa0d1094272b4344a7c24f5fee8daedeb0bf549c985ab5f",
        ),
        (
            256,
            "6a9169eb662f136d87374070e8828b3e615a7eca32a89446e9225b02832709be095e635c824a2bb70213ba2ea0ababac0809827843992c851903b7ac0c136699",
        ),
    ];
    for (len, expected) in CASES {
        let input = vec![b'a'; len];
        assert_eq!(
            hex(&digest(&input)),
            expected,
            "one-shot mismatch at len={len}"
        );

        let mut hasher = Sha512::new();
        for byte in input.chunks(1) {
            hasher.update(byte);
        }
        assert_eq!(
            hex(&hasher.finalize()),
            expected,
            "byte-wise mismatch at len={len}"
        );
    }
}

// パディング境界長（111/112/127/128。1 ブロックに収まるか 2 ブロックに
// なるかの分かれ目）で、あらゆる分割点の `update` が一括 `digest` と一致
// することを固定する。
#[test]
fn streaming_split_update_matches_one_shot_digest_at_boundary_lengths() {
    for len in [111usize, 112, 127, 128] {
        let input: Vec<u8> = (0..len).map(|i| (i % 256) as u8).collect();
        let expected = digest(&input);
        for split in 0..=len {
            let mut hasher = Sha512::new();
            hasher.update(&input[..split]);
            hasher.update(&input[split..]);
            assert_eq!(
                hasher.finalize(),
                expected,
                "mismatch at len={len}, split={split}"
            );
        }
    }
}

#[test]
fn streaming_split_update_matches_one_shot_for_various_chunk_sizes() {
    let input: Vec<u8> = (0..300).map(|i| (i % 256) as u8).collect();
    let expected = digest(&input);
    for chunk_size in [1usize, 3, 7, 16, 64, 111, 112, 127, 128, 129, 200] {
        let mut hasher = Sha512::new();
        for chunk in input.chunks(chunk_size) {
            hasher.update(chunk);
        }
        assert_eq!(
            hasher.finalize(),
            expected,
            "mismatch at chunk_size={chunk_size}"
        );
    }
}

#[test]
fn block_len_constant_matches_fips_180_4() {
    assert_eq!(BLOCK_LEN, 128);
}

#[test]
fn debug_output_does_not_leak_internal_state() {
    let mut hasher = Sha512::new();
    hasher.update(b"secret-input");
    let debug_str = format!("{hasher:?}");
    assert!(debug_str.contains("redacted"));
    assert!(!debug_str.contains("secret-input"));
}
