//! 英語ステミングの再現率の実測（Issue #3050・SEARCH-16 のポインタ。`english-stemmer` feature 有効時のみ）。
//!
//! 自作の合成英語コーパス（外部データなし）で、BM25 疎検索の Recall@k と MRR を、ステミング無効と
//! 有効で比べる。各主題の文書は主題語を語形違い（原形・三単現・過去形・進行形）で含み、クエリは
//! 文書に出ない語形で書く。実測値は固定値で assert し、`--nocapture` で出力する
//! （`docs/design/stemming-crate-adoption.md` の実測表の再現手順）。
#![cfg(feature = "english-stemmer")]

use std::sync::Arc;

use engine::sparse::{SparseIndex, TextAnalysis};

const VERBS: [&str; 20] = [
    "walk", "jump", "climb", "paint", "cook", "search", "deliver", "collect", "print", "listen",
    "watch", "match", "load", "play", "talk", "kick", "plant", "fold", "count", "wander",
];
/// 主題語を含まない一般語（文書の雑音。LCG で選ぶ）。
const FILLER: [&str; 24] = [
    "morning", "river", "quiet", "table", "window", "green", "slowly", "garden", "letter",
    "bridge", "silver", "market", "winter", "number", "cloud", "story", "button", "candle",
    "forest", "island", "pocket", "mirror", "ladder", "summer",
];
const DOCS_PER_TOPIC: usize = 6;

fn inflect(verb: &str, form: usize) -> String {
    match form % 4 {
        0 => verb.to_owned(),
        1 => format!("{verb}s"),
        2 => format!("{verb}ed"),
        _ => format!("{verb}ing"),
    }
}

/// 決定的な擬似乱数（再現性のため固定シード）。
fn next(state: &mut u64) -> usize {
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    ((*state >> 33) % FILLER.len() as u64) as usize
}

fn corpus() -> Vec<(u64, String)> {
    let mut st = 0x2545_f491_4f6c_dd1du64;
    let mut docs = Vec::new();
    for (t, verb) in VERBS.iter().enumerate() {
        for k in 0..DOCS_PER_TOPIC {
            let id = (t * DOCS_PER_TOPIC + k) as u64;
            // 主題語は k に応じた語形（原形・三単現・過去形・進行形の順）で含む。原形は一部の文書だけ。
            let form = k;
            let text = format!(
                "the {} {} was {} near the {} {}",
                FILLER[next(&mut st)],
                FILLER[next(&mut st)],
                inflect(verb, form),
                FILLER[next(&mut st)],
                FILLER[next(&mut st)]
            );
            docs.push((id, text));
        }
    }
    docs
}

/// (Recall@5, Recall@10, MRR) を全クエリ平均で返す。クエリは原形（文書側にも出る形のうち
/// 少数だけが一致する形）で書く。
fn measure(analysis: Arc<TextAnalysis>) -> (f64, f64, f64) {
    let docs = corpus();
    let refs: Vec<(u64, &str)> = docs.iter().map(|(i, t)| (*i, t.as_str())).collect();
    let idx = SparseIndex::build_with_analysis(&refs, analysis).expect("build");
    let (mut r5, mut r10, mut mrr) = (0.0, 0.0, 0.0);
    for (t, verb) in VERBS.iter().enumerate() {
        let lo = (t * DOCS_PER_TOPIC) as u64;
        let hi = lo + DOCS_PER_TOPIC as u64;
        let hits = idx.search(&inflect(verb, 0), 10).expect("search");
        let rel = |id: u64| (lo..hi).contains(&id);
        let count = |k: usize| hits.iter().take(k).filter(|h| rel(h.doc_id)).count() as f64;
        r5 += count(5) / DOCS_PER_TOPIC as f64;
        r10 += count(10) / DOCS_PER_TOPIC as f64;
        if let Some(pos) = hits.iter().position(|h| rel(h.doc_id)) {
            mrr += 1.0 / (pos as f64 + 1.0);
        }
    }
    let n = VERBS.len() as f64;
    (r5 / n, r10 / n, mrr / n)
}

#[test]
fn stemming_improves_recall_on_synthetic_english_corpus() {
    let off = measure(Arc::new(TextAnalysis::default()));
    let on = measure(Arc::new(
        TextAnalysis::default()
            .with_english_stemmer()
            .expect("feature on"),
    ));
    println!(
        "stemmer=off recall@5={:.4} recall@10={:.4} mrr={:.4}",
        off.0, off.1, off.2
    );
    println!(
        "stemmer=on  recall@5={:.4} recall@10={:.4} mrr={:.4}",
        on.0, on.1, on.2
    );
    // 実測値の固定（合成コーパスは決定的。回帰検知用）。
    let near = |a: f64, b: f64| (a - b).abs() < 1e-9;
    assert!(
        near(off.0, 1.0 / 3.0) && near(off.1, 1.0 / 3.0) && near(off.2, 1.0),
        "{off:?}"
    );
    assert!(
        near(on.0, 5.0 / 6.0) && near(on.1, 1.0) && near(on.2, 1.0),
        "{on:?}"
    );
}
