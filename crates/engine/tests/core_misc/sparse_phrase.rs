//! Issue #2114（対象ビヘイビア: SEARCH-12 のポインタ `docs/spec/04-behavior/search.md`、
//! 可視集合の縮約は RLS-8 のポインタ）。位置つき転置索引によるフレーズ一致の受け入れテストと、
//! 位置配列のメモリ増加量の計測（docs `sparse-inverted-index.md` の記録元）。
//!
//! 公開 API のみを使う: `parse_text_query` → `Phrase` → `PhrasePattern::from_phrase` →
//! `SparseIndex::phrase_matches_within`。メモリ計測は決定的な合成コーパスで値を出力し、
//! アサーションは長さに基づく関係に限る（容量に基づく正確な値は固定しない）。
//! 再現: `cargo test -p fandhe-db-engine --test core_misc sparse_phrase -- --nocapture`。

use std::collections::BTreeSet;

use engine::sparse::phrase::PhrasePattern;
use engine::sparse::text_query::{parse_text_query, Clause};
use engine::sparse::{tokenize, DocId, SparseIndex};

/// クエリ文字列の最初のフレーズ節からパターンを作る。
fn first_phrase(query: &str) -> PhrasePattern {
    let q = parse_text_query(query).expect("query within bounds");
    for g in q.groups() {
        for c in g.clauses() {
            if let Clause::Phrase(p) = c {
                return PhrasePattern::from_phrase(p).expect("pattern within bounds");
            }
        }
    }
    panic!("no phrase in query");
}

fn matched(index: &SparseIndex, query: &str, visible: &[DocId]) -> Vec<DocId> {
    let vis: BTreeSet<DocId> = visible.iter().copied().collect();
    index
        .phrase_matches_within(&first_phrase(query), &vis)
        .iter()
        .map(|h| h.doc_id())
        .collect()
}

#[test]
fn phrase_returns_only_documents_with_adjacent_terms() {
    let docs: Vec<(DocId, &str)> = vec![
        (1, "visit the tokyo tower today"),
        (2, "tokyo has a tall tower"),
        (3, "tower tokyo"),
        (4, "Tokyo Tower, tokyo tower"),
    ];
    let index = SparseIndex::build(&docs).unwrap();
    let all = [1, 2, 3, 4];
    assert_eq!(matched(&index, "\"tokyo tower\" cafe", &all), vec![1, 4]);
}

#[test]
fn phrase_handles_cjk_mixed_and_stopword_phrases() {
    let docs: Vec<(DocId, &str)> = vec![
        (1, "東京都の観光"),
        (2, "東京・都の観光"),
        (3, "cafe 東京の店"),
        (4, "cafe x 東京"),
    ];
    let index = SparseIndex::build(&docs).unwrap();
    let all = [1, 2, 3, 4];
    assert_eq!(matched(&index, "\"東京都\"", &all), vec![1]);
    assert_eq!(matched(&index, "\"東京の店\"", &all), vec![3]);
    assert_eq!(matched(&index, "\"cafe 東京\"", &all), vec![3]);
    assert_eq!(matched(&index, "\"cafe の 東京\"", &all), vec![4]);
}

#[test]
fn phrase_never_returns_invisible_documents() {
    let docs: Vec<(DocId, &str)> = vec![(1, "tokyo tower"), (2, "tokyo tower"), (3, "tokyo tower")];
    let index = SparseIndex::build(&docs).unwrap();
    assert_eq!(matched(&index, "\"tokyo tower\"", &[1, 3]), vec![1, 3]);
    assert!(matched(&index, "\"tokyo tower\"", &[]).is_empty());
}

/// xorshift64*（外部クレートなし）。
struct Rng(u64);

impl Rng {
    fn next(&mut self, m: usize) -> usize {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 33) as usize % m
    }
}

fn ascii_corpus(n_docs: usize, words_per_doc: usize) -> Vec<String> {
    let mut rng = Rng(0x1234_5678_9ABC_DEF1);
    let vocab: Vec<String> = (0..5000).map(|i| format!("w{i}")).collect();
    (0..n_docs)
        .map(|_| {
            (0..words_per_doc)
                .map(|_| vocab[rng.next(vocab.len())].as_str())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

fn japanese_corpus(n_docs: usize, chars_per_doc: usize) -> Vec<String> {
    let mut rng = Rng(0x0FED_CBA9_8765_4321);
    let chars: Vec<char> = "東京都大阪府京都市の店を見るはがで人日本語検索結果表示設定情報管理"
        .chars()
        .collect();
    (0..n_docs)
        .map(|_| {
            let mut s = String::new();
            for i in 0..chars_per_doc {
                s.push(chars[rng.next(chars.len())]);
                if i % 17 == 16 {
                    s.push(' ');
                }
            }
            s
        })
        .collect()
}

fn report(label: &str, texts: &[String]) {
    let docs: Vec<(DocId, &str)> = texts
        .iter()
        .enumerate()
        .map(|(i, t)| (i as DocId + 1, t.as_str()))
        .collect();
    let index = SparseIndex::build(&docs).unwrap();
    let tokens: usize = texts.iter().map(|t| tokenize(t).len()).sum();
    let total = index.approx_heap_bytes();
    let pos = index.approx_position_heap_bytes();
    assert!(pos >= 4 * tokens, "positions must hold one u32 per token");
    assert!(total > pos);
    let without = total - pos;
    eprintln!(
        "sparse_phrase memory: corpus={label} docs={} tokens={tokens} \
         approx_heap_bytes={total} position_heap_bytes={pos} without_positions={without} \
         increase_pct={:.1}",
        texts.len(),
        pos as f64 * 100.0 / without as f64
    );
}

#[test]
fn position_memory_overhead_is_reported() {
    report("ascii", &ascii_corpus(2000, 100));
    report("japanese", &japanese_corpus(2000, 100));
}
