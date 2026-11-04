//! 疎索引テーブル世代整合キャッシュ（Issue #357・`sql::sparse_cache::SparseIndexCache`）
//! 導入後の Recall 非劣化・RRF 同点順位規約の不変を SQL 表層（`EngineCore::
//! execute_sql`）経由で検証する回帰テスト（Issue #358）。
//!
//! **既存 Recall ゲートとの関係**: `tests/hybrid_recall.rs`・`tests/rerank_recall.rs`・
//! `tests/query_planning_recall.rs` は `SparseIndex::build` + `hybrid::hybrid_search`
//! を直接呼び出し、SQL 表層の `execute_statement`（`SparseIndexCache` が結線されている
//! 唯一の経路）を通らない。したがってこれら既存ゲートはキャッシュ導入に対して
//! 構造的に不変であり、それ自体はキャッシュ経路の非劣化を証明しない。本ファイルは
//! その空白を埋める SQL 表層専用の検証であり、既存ゲートを置き換えない
//! （`docs/design/sparse-index-cache-verification.md` 参照）。
//!
//! **検証の核心**: 「cold（クエリごとに `EngineCore` を再オープンしキャッシュを
//! 素通りさせる = キャッシュ導入前と同じ build-per-query 経路）」と「hot（単一
//! `EngineCore` でクエリを連続実行しキャッシュヒットさせる）」で Top-K の id 列が
//! 完全一致することを検証する。このとき `sparse_index_cache_stats()` で
//! hot 側が実際に `hits > 0` を記録していることも同時にアサートする——cold/hot の
//! 結果が一致するだけでは、キャッシュが一度も参照されず退化した比較（例えば
//! いずれも 0 件ヒット）でも通ってしまう vacuous pass になりうるため
//! （Issue #281 で回帰実測モデルの vacuous pass を問題視した教訓と同方針）。
//!
//! **フィクスチャの複製について**: `tests/hybrid_recall.rs` の決定的コーパス
//! 生成器（`Xorshift64`・トピック語彙・Zipf 分布）を共有 fixture へ切り出す案を
//! 検討したが、同ファイルの層 A 固定値アサーション（qa 60 件・hits20=182 等）が
//! 生成過程の 1 ビットの変化にも敏感な決定的乱数ストリームに依存しており、
//! 切り出しによる巻き添え変化のリスクがそれ自体の価値（コード重複の削減）を
//! 上回ると判断した。本ファイルは `Xorshift64`・トピック語彙のごく最小部分集合を
//! 独立に複製する（`tests/hybrid_recall.rs`・`query_planning_recall.rs` が
//! 既に同一実装を複製し合っている前例と同方針）。生成規則自体は簡略化しており
//! `hybrid_recall.rs` の Recall 実測値とは無関係（本ファイルの目的は Recall の
//! 絶対値ではなく cold/hot 等価性の検証）。
//!
//! **Issue #393 での拡張**: 転置索引化（Issue #386 Phase 1・#388〜#392）後も上記の
//! cold/hot 等価性が維持されることを、(a) 可視集合が全体の一部になる RLS ケース、
//! (b) 未知語のみのクエリ、(c) 空クエリの 3 ケースで追加検証する
//! （`docs/design/sparse-inverted-index-recall-verification.md` 参照。既存ケースは
//! 無変更のまま追加する）。
//!
//! 対応 spec ビヘイビア（ポインタのみ・本文非転記）: SEARCH-1, SEARCH-2, SEARCH-3。

use std::collections::BTreeSet;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::exec::QueryResult;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

// ---------- 決定的擬似乱数（`tests/hybrid_recall.rs::Xorshift64` の複製。複製理由は
// 本ファイル冒頭のドキュメント参照） ----------

struct Xorshift64 {
    state: u64,
}

impl Xorshift64 {
    fn new(seed: u64) -> Self {
        Self { state: seed.max(1) }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn next_range(&mut self, n: usize) -> usize {
        assert!(n > 0, "next_range(0) は無効な呼び出し");
        (self.next_u64() % n as u64) as usize
    }
}

/// トピック `idx` のトークン（`sparse::tokenize` の ASCII 単語境界規則の下で必ず
/// 1 トークンになる形式。`hybrid_recall.rs::topic_token` と同一の命名規約だが、
/// 生成対象コーパスは互いに独立であり数値の互換性は不要）。
fn topic_token(idx: usize) -> String {
    format!("kw_{idx:04}")
}

/// クエリ・本文文字列に連結する前に、生成器が作った識別子が想定形式
/// （`kw_` 接頭辞 + 4 桁数字）から逸脱していないことを検証する（SQL 文字列
/// 組み立てへ未検証入力を連結しない方針の防御的複製。本テストの生成源は
/// 生成器自身で untrusted ではないが、`benches/harness/hybrid_profile.rs::
/// validate_query_text` と同じ検証してから連結する作法を踏襲する）。
fn validate_identifier_token(token: &str) -> &str {
    let rest = token
        .strip_prefix("kw_")
        .unwrap_or_else(|| panic!("unexpected token format: {token}"));
    assert!(
        rest.len() == 4 && rest.chars().all(|c| c.is_ascii_digit()),
        "unexpected token format: {token}"
    );
    token
}

const FILLER_WORDS: [&str; 6] = ["the", "a", "of", "for", "and", "note"];

struct Doc {
    id: u64,
    text: String,
    vector: Vec<f32>,
    keywords: BTreeSet<usize>,
}

struct QaCase {
    query_text: String,
    query_vector: Vec<f32>,
    correct: BTreeSet<u64>,
}

fn one_hot_sum(vocab_size: usize, indices: impl IntoIterator<Item = usize>) -> Vec<f32> {
    let mut v = vec![0.0f32; vocab_size];
    for idx in indices {
        if let Some(slot) = v.get_mut(idx) {
            *slot = 1.0;
        }
    }
    v
}

fn vector_literal(vec: &[f32]) -> String {
    let mut s = String::from("[");
    for (i, x) in vec.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!("{x:.1}"));
    }
    s.push(']');
    s
}

/// 決定的コーパス生成（`hybrid_recall.rs` の簡略版）。各文書へ 1〜2 個のトピック
/// キーワードを割り当て、疎チャネル（`text`）・密チャネル（`vector`）双方へ
/// 反映する（ドロップアウト・デコイは実装しない。本ファイルの目的は Recall の
/// 絶対値ではなく cold/hot 等価性の検証であるため、`hybrid_recall.rs` のような
/// 現実的な非対称分布は不要と判断した）。
fn generate_corpus(seed: u64, num_docs: usize, vocab_size: usize) -> Vec<Doc> {
    let mut rng = Xorshift64::new(seed);
    let mut docs = Vec::with_capacity(num_docs);
    for id in 1..=num_docs as u64 {
        let num_kw = 1 + rng.next_range(2);
        let mut keywords = BTreeSet::new();
        while keywords.len() < num_kw {
            keywords.insert(rng.next_range(vocab_size));
        }
        let mut text = String::new();
        for &kw in &keywords {
            let token = topic_token(kw);
            text.push_str(validate_identifier_token(&token));
            text.push(' ');
            text.push_str(validate_identifier_token(&token));
            text.push(' ');
        }
        for _ in 0..3 {
            text.push_str(FILLER_WORDS[rng.next_range(FILLER_WORDS.len())]);
            text.push(' ');
        }
        let vector = one_hot_sum(vocab_size, keywords.iter().copied());
        docs.push(Doc {
            id,
            text: text.trim_end().to_string(),
            vector,
            keywords,
        });
    }
    docs
}

/// AND 交差（2 語）で正解集合が空にならないクエリ対のみを採用する QA セット生成。
fn generate_qa_set(
    rng: &mut Xorshift64,
    docs: &[Doc],
    vocab_size: usize,
    num_queries: usize,
) -> Vec<QaCase> {
    let mut qa = Vec::with_capacity(num_queries);
    let mut attempts = 0usize;
    while qa.len() < num_queries && attempts < num_queries * 50 {
        attempts += 1;
        let a = rng.next_range(vocab_size);
        let b = rng.next_range(vocab_size);
        if a == b {
            continue;
        }
        let correct: BTreeSet<u64> = docs
            .iter()
            .filter(|d| d.keywords.contains(&a) && d.keywords.contains(&b))
            .map(|d| d.id)
            .collect();
        if correct.is_empty() {
            continue;
        }
        let query_text = format!(
            "{} {}",
            validate_identifier_token(&topic_token(a)),
            validate_identifier_token(&topic_token(b)),
        );
        let query_vector = one_hot_sum(vocab_size, [a, b]);
        qa.push(QaCase {
            query_text,
            query_vector,
            correct,
        });
    }
    qa
}

fn create_docs_table(storage: &Storage, vocab_size: usize) {
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(vocab_size as u32), false),
                ColumnDef::new("body", ColumnType::Text, true),
            ],
        ))
        .expect("create table");
}

fn insert_corpus(storage: &Storage, tenant: &str, docs: &[Doc]) {
    let ctx = PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant");
    for doc in docs {
        engine::tenant::insert_typed_row(
            storage,
            "docs",
            &ctx,
            doc.id,
            Visibility::Public,
            &[
                Value::Vector(doc.vector.clone()),
                Value::Text(doc.text.clone()),
            ],
            &engine::recovery::required_op_id::OperationId::parse(&format!(
                "sparse-cache-recall-seed-{tenant}-{}",
                doc.id
            ))
            .expect("valid operation_id"),
        )
        .expect("insert row");
    }
}

fn hybrid_sql(qa: &QaCase, limit: usize) -> String {
    format!(
        "SELECT id FROM docs ORDER BY hybrid_rrf(embedding, '{}', body, '{}') LIMIT {limit}",
        vector_literal(&qa.query_vector),
        qa.query_text,
    )
}

fn result_ids(result: &QueryResult) -> Vec<u64> {
    result.rows.iter().map(|r| r.id).collect()
}

fn open_storage(path: &std::path::Path) -> Storage {
    Storage::open(path).expect("open storage")
}

fn open_core(path: &std::path::Path) -> EngineCore {
    EngineCore::with_provider(path, Box::new(CpuScalarProvider)).expect("open core")
}

// --- 2.1: cold（クエリごと再オープン）/ hot（単一 EngineCore 連続実行）の完全一致 ---

const SMALL_NUM_DOCS: usize = 400;
const SMALL_VOCAB_SIZE: usize = 60;
const SMALL_NUM_QUERIES: usize = 40;
const SMALL_SEED: u64 = 0x5350_4152_5345_4331;

#[test]
fn cold_and_hot_hybrid_results_are_identical_and_cache_is_actually_hit() {
    let path = unique_db_path("sparse-cache-recall-small");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    create_docs_table(&storage, SMALL_VOCAB_SIZE);

    let docs = generate_corpus(SMALL_SEED, SMALL_NUM_DOCS, SMALL_VOCAB_SIZE);
    let mut qa_rng = Xorshift64::new(SMALL_SEED ^ 0xA5A5_A5A5_A5A5_A5A5);
    let qa = generate_qa_set(&mut qa_rng, &docs, SMALL_VOCAB_SIZE, SMALL_NUM_QUERIES);
    assert!(
        qa.len() >= SMALL_NUM_QUERIES / 2,
        "QA generation must find enough non-empty AND queries, got {}",
        qa.len()
    );
    insert_corpus(&storage, "tenant-recall", &docs);
    drop(storage);

    // --- cold: クエリごとに EngineCore を再オープンし、キャッシュを毎回素通りさせる
    // （#377 導入前と同一の build-per-query 経路の代替）。
    let mut cold_ids = Vec::with_capacity(qa.len());
    for case in &qa {
        let core = open_core(&path);
        let ctx = PolicyContext::new("tenant-recall").expect("valid tenant");
        let sql = hybrid_sql(case, 20);
        let result = core.execute_sql(&ctx, &sql).expect("cold hybrid query ok");
        let stats = core.sparse_index_cache_stats();
        assert_eq!(stats.misses, 1, "cold query must be a fresh-cache miss");
        assert_eq!(stats.hits, 0, "cold query must not hit a stale cache");
        cold_ids.push(result_ids(&result));
    }

    // --- hot: 単一 EngineCore で全 QA を連続実行し、2 件目以降はキャッシュヒットする。
    let core = open_core(&path);
    let ctx = PolicyContext::new("tenant-recall").expect("valid tenant");
    let mut hot_ids = Vec::with_capacity(qa.len());
    let mut hot_hits20 = 0usize;
    for case in &qa {
        let sql = hybrid_sql(case, 20);
        let result = core.execute_sql(&ctx, &sql).expect("hot hybrid query ok");
        let ids = result_ids(&result);
        hot_hits20 += ids.iter().filter(|id| case.correct.contains(id)).count();
        hot_ids.push(ids);
    }
    let final_stats = core.sparse_index_cache_stats();
    assert_eq!(
        final_stats.misses, 1,
        "hot run must build the sparse index exactly once"
    );
    assert_eq!(
        final_stats.hits,
        (qa.len() - 1) as u64,
        "hot run's queries after the first must all be cache hits \
         (a 0-hit run would make cold==hot a vacuous pass)"
    );
    assert_eq!(final_stats.stale_evictions, 0);

    // --- cold と hot の Top-20 id 列が全ケースで順序込み完全一致すること。
    for (i, (cold, hot)) in cold_ids.iter().zip(hot_ids.iter()).enumerate() {
        assert_eq!(cold, hot, "case {i} diverged between cold and hot runs");
    }

    // 非劣化の回帰トラッキング（実測して固定。生成規則を変えない限り決定的）。
    // hits20 の絶対値自体は本ファイル独自のフィクスチャに基づくものであり、
    // `hybrid_recall.rs`（TASK-104 層 A/B）の Recall 実測値とは対応しない。
    println!(
        "=== sparse_cache_recall small-scale: qa={} hot_hits20={hot_hits20} ===",
        qa.len()
    );
    // 実測して固定した回帰値（qa=40 件・SMALL_SEED 固定の下で決定的）。生成規則・
    // 検索カーネルの変更で値が動いた場合は意図した変化か確認する。
    assert_eq!(
        hot_hits20,
        46,
        "hot run hits20 regressed from the recorded baseline (qa={})",
        qa.len()
    );
}

// --- 2.2: RRF 同点順位規約（`TieRank::GroupEnd`・境界同点グループ完全化）が
// キャッシュヒット経路でも不変であること。`tests/sql_surface.rs::
// sql4_hybrid_tie_group_across_limit_boundary_is_deterministic` と同じ同点誘発
// コーパスを、cold（再オープン）/ hot（キャッシュヒット）双方で検証する。

#[test]
fn cold_and_hot_hybrid_tie_group_across_limit_boundary_match() {
    let path = unique_db_path("sparse-cache-recall-tie");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("body", ColumnType::Text, true),
            ],
        ))
        .expect("create table");

    // `sql_surface.rs::sql4_hybrid_tie_group_across_limit_boundary_is_deterministic`
    // と同一の同点誘発コーパス（密ランク・疎ランクを入れ替えた 2 行が完全同点になる
    // 構成。同ファイルのコメント参照）。
    let rows: [(u64, [f32; 2], Option<&str>); 5] = [
        // 密側は `<=>`（コサイン距離。Issue #1991）で順位付けされるため、向きの近さで
        // 行 A（問い合わせ [1,0] と同方向）> 行 B > 埋め草の順になるよう角度を変えてある。
        (1, [1.0, 0.0], Some("anchor filler filler")),
        (2, [0.9, 0.3], Some("anchor anchor anchor")),
        (90, [0.5, 1.0], None),
        (91, [0.2, 1.0], None),
        (92, [0.0, 1.0], None),
    ];
    let ctx_seed =
        PolicyContext::with_visibilities("tenant-tie", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    for (id, emb, body) in rows {
        let value = match body {
            Some(b) => Value::Text(b.to_string()),
            None => Value::Null,
        };
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx_seed,
            id,
            Visibility::Public,
            &[Value::Vector(emb.to_vec()), value],
            &engine::recovery::required_op_id::OperationId::parse(&format!(
                "sparse-cache-recall-tie-{id}"
            ))
            .expect("valid operation_id"),
        )
        .expect("insert row");
    }
    drop(storage);

    let sql_for = |limit: usize, kw_syntax: bool| -> String {
        let func = if kw_syntax { "HYBRID" } else { "hybrid_rrf" };
        format!(
            "SELECT * FROM docs ORDER BY {func}(embedding, '[1.0,0.0]', body, 'anchor') LIMIT {limit}"
        )
    };

    // cold: LIMIT 1 / 2 / 3 それぞれを新規 EngineCore（キャッシュ未ヒット）で実行。
    let mut cold_by_limit = Vec::new();
    for limit in [1usize, 2, 3] {
        let core = open_core(&path);
        let ctx = PolicyContext::new("tenant-tie").expect("valid tenant");
        let result = core
            .execute_sql(&ctx, &sql_for(limit, false))
            .expect("cold hybrid_rrf tie query ok");
        let stats = core.sparse_index_cache_stats();
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.hits, 0);
        cold_by_limit.push(result_ids(&result));
    }

    // hot: 単一 EngineCore で LIMIT 1 → 2 → 3 → HYBRID(...) 構文形の順に連続実行。
    let core = open_core(&path);
    let ctx = PolicyContext::new("tenant-tie").expect("valid tenant");
    let mut hot_by_limit = Vec::new();
    for (i, limit) in [1usize, 2, 3].into_iter().enumerate() {
        let result = core
            .execute_sql(&ctx, &sql_for(limit, false))
            .expect("hot hybrid_rrf tie query ok");
        let stats = core.sparse_index_cache_stats();
        if i == 0 {
            assert_eq!(stats.misses, 1);
            assert_eq!(stats.hits, 0);
        } else {
            assert_eq!(stats.misses, 1);
            assert_eq!(stats.hits, i as u64, "case {i} must be a cache hit");
        }
        hot_by_limit.push(result_ids(&result));
    }
    let result_kw = core
        .execute_sql(&ctx, &sql_for(1, true))
        .expect("hot HYBRID() tie query ok");
    let final_stats = core.sparse_index_cache_stats();
    assert_eq!(final_stats.hits, 3, "HYBRID() form must also hit the cache");
    assert_eq!(final_stats.stale_evictions, 0);

    for (i, (cold, hot)) in cold_by_limit.iter().zip(hot_by_limit.iter()).enumerate() {
        assert_eq!(
            cold, hot,
            "LIMIT case {i} diverged between cold and hot runs"
        );
    }
    assert_eq!(
        cold_by_limit[0],
        vec![1u64],
        "LIMIT 1 cuts the tie group after id=1"
    );
    assert_eq!(
        cold_by_limit[1],
        vec![1u64, 2u64],
        "LIMIT 2 must include the full tie group in id order"
    );
    assert_eq!(result_ids(&result_kw), hot_by_limit[0]);
}

// --- 2.3（Issue #393）: RLS 部分可視ケース。可視集合が全体の一部になる場合にも
// cold/hot が一致し、不可視行が結果・統計へ漏えいしないこと。

const RLS_VOCAB_SIZE: usize = 50;
const RLS_GROUP_SIZE: usize = 60;

/// `docs` の各 id を `offset` だけ平行移動する（グループ間で id 帯域を分けるため）。
fn offset_docs(mut docs: Vec<Doc>, offset: u64) -> Vec<Doc> {
    for d in &mut docs {
        d.id += offset;
    }
    docs
}

/// tenant-b の Private 群専用フィクスチャ: どの 2 語対クエリに対しても高密度に
/// 一致するよう、ほぼ全語彙対を均等にカバーする文書群を決定的に生成する
/// （不可視行の内容・df・N が可視文書の統計へ漏えいした場合に強く順位へ影響する
/// 構成。本モジュール冒頭ドキュメント「統計縮約オラクル」参照）。
fn dense_keyword_docs(offset: u64, vocab_size: usize, count: usize) -> Vec<Doc> {
    let mut docs = Vec::with_capacity(count);
    for i in 0..count {
        let a = i % vocab_size;
        let b = (i / vocab_size + 1) % vocab_size;
        let keywords: BTreeSet<usize> = if a == b {
            BTreeSet::from([a])
        } else {
            [a, b].into()
        };
        let mut text = String::new();
        for &kw in &keywords {
            let token = topic_token(kw);
            text.push_str(validate_identifier_token(&token));
            text.push(' ');
            text.push_str(validate_identifier_token(&token));
            text.push(' ');
        }
        let vector = one_hot_sum(vocab_size, keywords.iter().copied());
        docs.push(Doc {
            id: offset + i as u64 + 1,
            text: text.trim_end().to_string(),
            vector,
            keywords,
        });
    }
    docs
}

/// 指定したテナント・可視性で `docs` を投入する（`insert_corpus` の単一テナント・
/// 両可視性前提を、tenant/visibility を個別指定できる形へ一般化したもの）。
fn insert_group(storage: &Storage, tenant: &str, visibility: Visibility, docs: &[Doc], tag: &str) {
    let ctx = PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant");
    for doc in docs {
        engine::tenant::insert_typed_row(
            storage,
            "docs",
            &ctx,
            doc.id,
            visibility,
            &[
                Value::Vector(doc.vector.clone()),
                Value::Text(doc.text.clone()),
            ],
            &engine::recovery::required_op_id::OperationId::parse(&format!(
                "sparse-cache-recall-rls-{tag}-{}",
                doc.id
            ))
            .expect("valid operation_id"),
        )
        .expect("insert row");
    }
}

#[test]
fn cold_and_hot_hybrid_results_match_under_partial_visibility_and_never_leak_invisible_rows() {
    let path = unique_db_path("sparse-cache-recall-rls");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    create_docs_table(&storage, RLS_VOCAB_SIZE);

    // 4 群（id 帯域を分けて重複しないようにする）。
    let a_pub = generate_corpus(0xA1, RLS_GROUP_SIZE, RLS_VOCAB_SIZE); // id 1..=60
    let a_priv = offset_docs(generate_corpus(0xA2, RLS_GROUP_SIZE, RLS_VOCAB_SIZE), 1000); // id 1001..=1060
    let b_pub = offset_docs(generate_corpus(0xB1, RLS_GROUP_SIZE, RLS_VOCAB_SIZE), 2000); // id 2001..=2060
    let b_priv = dense_keyword_docs(3000, RLS_VOCAB_SIZE, RLS_GROUP_SIZE); // id 3001..=3060（高密度）

    insert_group(&storage, "tenant-a", Visibility::Public, &a_pub, "a-pub");
    insert_group(&storage, "tenant-a", Visibility::Private, &a_priv, "a-priv");
    insert_group(&storage, "tenant-b", Visibility::Public, &b_pub, "b-pub");
    insert_group(&storage, "tenant-b", Visibility::Private, &b_priv, "b-priv");
    drop(storage);

    // クエリは a-pub の語彙から生成する（a-pub は両文脈で必ず可視）。
    let mut qa_rng = Xorshift64::new(0xA1 ^ 0xA5A5_A5A5_A5A5_A5A5);
    let qa = generate_qa_set(&mut qa_rng, &a_pub, RLS_VOCAB_SIZE, 10);
    assert!(
        !qa.is_empty(),
        "RLS ケースの QA generation が非空の AND クエリを見つけられなかった"
    );

    let visible_ctx1: BTreeSet<u64> = a_pub.iter().chain(b_pub.iter()).map(|d| d.id).collect();
    let invisible_ctx1: BTreeSet<u64> = a_priv.iter().chain(b_priv.iter()).map(|d| d.id).collect();
    let visible_ctx2: BTreeSet<u64> = a_pub
        .iter()
        .chain(a_priv.iter())
        .chain(b_pub.iter())
        .map(|d| d.id)
        .collect();
    let invisible_ctx2: BTreeSet<u64> = b_priv.iter().map(|d| d.id).collect();

    // (i)+(ii): 文脈ごとに cold/hot が完全一致し、結果が可視集合の部分集合であること。
    for (label, ctx, visible, invisible) in [
        (
            "ctx1-public-only",
            PolicyContext::new("tenant-a").expect("valid tenant"),
            &visible_ctx1,
            &invisible_ctx1,
        ),
        (
            "ctx2-with-a-private",
            PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
                .expect("valid tenant"),
            &visible_ctx2,
            &invisible_ctx2,
        ),
    ] {
        let mut cold_ids = Vec::with_capacity(qa.len());
        for case in &qa {
            let core = open_core(&path);
            let sql = hybrid_sql(case, 20);
            let result = core.execute_sql(&ctx, &sql).expect("cold hybrid query ok");
            cold_ids.push(result_ids(&result));
        }

        let core = open_core(&path);
        let mut hot_ids = Vec::with_capacity(qa.len());
        for case in &qa {
            let sql = hybrid_sql(case, 20);
            let result = core.execute_sql(&ctx, &sql).expect("hot hybrid query ok");
            hot_ids.push(result_ids(&result));
        }

        for (i, (cold, hot)) in cold_ids.iter().zip(hot_ids.iter()).enumerate() {
            assert_eq!(cold, hot, "{label} case {i}: cold と hot が乖離した");
        }
        for ids in cold_ids.iter().chain(hot_ids.iter()) {
            for id in ids {
                assert!(
                    visible.contains(id),
                    "{label}: 可視集合外の id {id} が結果に混入した"
                );
                assert!(
                    !invisible.contains(id),
                    "{label}: 不可視 id {id} が結果へ漏えいした"
                );
            }
        }
    }

    // (iii): 単一 EngineCore で 2 文脈を連続実行すると文脈ごとに別キャッシュエントリ
    // になる（misses == 2）。1 件目は各文脈とも miss、2 件目以降は hit。
    let ctx1 = PolicyContext::new("tenant-a").expect("valid tenant");
    let ctx2 =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let core = open_core(&path);
    let sql = hybrid_sql(&qa[0], 20);
    let _ = core.execute_sql(&ctx1, &sql).expect("ctx1 1st query ok");
    let _ = core.execute_sql(&ctx2, &sql).expect("ctx2 1st query ok");
    let stats_after_first = core.sparse_index_cache_stats();
    assert_eq!(
        stats_after_first.misses, 2,
        "文脈（PolicyContext）ごとに別キャッシュエントリになるはず"
    );
    assert_eq!(stats_after_first.hits, 0);
    let _ = core.execute_sql(&ctx1, &sql).expect("ctx1 2nd query ok");
    let _ = core.execute_sql(&ctx2, &sql).expect("ctx2 2nd query ok");
    let stats_after_second = core.sparse_index_cache_stats();
    assert_eq!(stats_after_second.misses, 2);
    assert_eq!(
        stats_after_second.hits, 2,
        "各文脈の 2 件目以降はキャッシュヒットするはず"
    );
    // 以降で同一パスを再オープンするため、ここで明示的に閉じる（redb は同一パスの
    // 二重オープンを許さない）。
    drop(core);

    // (iv) 統計縮約オラクル: 不可視行を物理的に含まない対照 DB（ctx1 の可視行
    // 〔a-pub ∪ b-pub〕のみを同一 id・tenant・visibility で投入）で同じクエリを
    // 実行し、元 DB の ctx1 結果と完全一致することを確認する。不一致は不可視行の
    // 統計（df・N・avgdl）漏えいを意味するため fail-closed に扱う（本 Issue では
    // production コードを修正せず、乖離があれば原因調査へ差し戻す）。
    let oracle_path = unique_db_path("sparse-cache-recall-rls-oracle");
    let _oracle_guard = CleanupGuard(oracle_path.clone());
    let oracle_storage = open_storage(&oracle_path);
    create_docs_table(&oracle_storage, RLS_VOCAB_SIZE);
    insert_group(
        &oracle_storage,
        "tenant-a",
        Visibility::Public,
        &a_pub,
        "oracle-a-pub",
    );
    insert_group(
        &oracle_storage,
        "tenant-b",
        Visibility::Public,
        &b_pub,
        "oracle-b-pub",
    );
    drop(oracle_storage);

    let oracle_core = open_core(&oracle_path);
    let main_core = open_core(&path);
    for (i, case) in qa.iter().enumerate() {
        let sql = hybrid_sql(case, 20);
        let main_result = main_core
            .execute_sql(&ctx1, &sql)
            .expect("main hybrid query ok");
        let oracle_result = oracle_core
            .execute_sql(&ctx1, &sql)
            .expect("oracle hybrid query ok");
        assert_eq!(
            result_ids(&main_result),
            result_ids(&oracle_result),
            "case {i}: 不可視行を含まない対照 DB と結果が一致しない（統計漏えいの疑い）"
        );
    }
}

// --- 2.4（Issue #393）: 未知語のみのクエリ。疎チャネルが無信号でも密のみへ
// 縮退し、cold/hot が一致すること・純密クエリと同じ順位になること。

#[test]
fn cold_and_hot_hybrid_results_match_for_unknown_terms_only_query() {
    let path = unique_db_path("sparse-cache-recall-unknown");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    create_docs_table(&storage, SMALL_VOCAB_SIZE);
    let docs = generate_corpus(SMALL_SEED, SMALL_NUM_DOCS, SMALL_VOCAB_SIZE);
    insert_corpus(&storage, "tenant-unknown", &docs);
    drop(storage);

    // コーパス語彙（`kw_NNNN`）に存在しない ASCII トークン。密ベクトルは通常の
    // QA と同様に語彙 2 語の one-hot 和（疎側が無信号でも密側は有意な近傍を持つ）。
    let query_vector = one_hot_sum(SMALL_VOCAB_SIZE, [3usize, 7]);
    let query_text = "zzunknowna zzunknownb".to_string();
    for ch in query_text.chars() {
        assert!(
            ch.is_ascii_lowercase() || ch == ' ',
            "unexpected unknown-term query char: {ch}"
        );
    }
    let sql = format!(
        "SELECT id FROM docs ORDER BY hybrid_rrf(embedding, '{}', body, '{}') LIMIT 20",
        vector_literal(&query_vector),
        query_text,
    );
    let dense_only_sql = format!(
        "SELECT id FROM docs ORDER BY embedding <=> '{}' LIMIT 20",
        vector_literal(&query_vector)
    );

    let ctx = PolicyContext::new("tenant-unknown").expect("valid tenant");
    let cold_ids = {
        // redb は同一パスへの二重オープンを許さないため、cold 側の `EngineCore` は
        // hot 側を開く前にスコープを抜けて閉じる。
        let core_cold = open_core(&path);
        let cold_result = core_cold
            .execute_sql(&ctx, &sql)
            .expect("cold unknown-term hybrid query ok");
        let cold_stats = core_cold.sparse_index_cache_stats();
        assert_eq!(cold_stats.misses, 1);
        assert_eq!(cold_stats.hits, 0);
        result_ids(&cold_result)
    };

    let core_hot = open_core(&path);
    let hot_result_1 = core_hot
        .execute_sql(&ctx, &sql)
        .expect("hot unknown-term hybrid query ok (1st)");
    let hot_result_2 = core_hot
        .execute_sql(&ctx, &sql)
        .expect("hot unknown-term hybrid query ok (2nd)");
    let hot_stats = core_hot.sparse_index_cache_stats();
    assert_eq!(hot_stats.misses, 1);
    assert_eq!(hot_stats.hits, 1, "2 件目はキャッシュヒットするはず");

    assert_eq!(
        cold_ids,
        result_ids(&hot_result_1),
        "cold と hot(1st) が乖離した"
    );
    assert_eq!(
        result_ids(&hot_result_1),
        result_ids(&hot_result_2),
        "hot の 1st と 2nd（キャッシュヒット後）が乖離した"
    );
    assert_eq!(
        cold_ids.len(),
        20,
        "疎側無信号でも密のみで LIMIT 件数を満たすはず"
    );

    // 疎チャネルが無信号（RRF 単一チャネル）の場合、融合順位は密順位の単調写像に
    // なるはずであり、純密クエリと同一の Top-20 id 列になることを確認する
    // （不一致の場合は縮退契約の原因を調査し doc へ記録する。アサーションは
    // 弱めない）。
    let dense_only_result = core_hot
        .execute_sql(&ctx, &dense_only_sql)
        .expect("dense-only query ok");
    assert_eq!(
        cold_ids,
        result_ids(&dense_only_result),
        "未知語のみクエリの hybrid 結果は純密クエリの Top-20 と一致するはず"
    );
}

// --- 2.5（Issue #393）: 空クエリ文字列。実測した契約は `Ok`（`sparse.rs::tokenize`
// が空トークン列 → 疎側無信号 → 密のみへ縮退）であり、本テストは cold/hot で
// 同一の結果になることを固定する。契約は `Ok` 必須として `expect` し、`Err` への
// 退行（拒否側への契約変更）が起きた場合はテスト失敗で検出する（codex-review
// 指摘・PR #428: cold/hot 双方が同一 `Err` を返せば成功してしまう分岐は削除した）。
// あわせて `sparse_index_cache_stats()` で 1 回目 miss・2 回目 hit を確認し、
// 空クエリがキャッシュ経路を実際に迂回していないこと（vacuous pass 防止）も
// 固定する。さらに結果件数（LIMIT 20 を満たす）と純密クエリの Top-20 id 列との
// 一致を明示的にアサートし、「dense-only ランキングへのフォールバック」自体を
// 固定する（codex-review 指摘・PR #428: Empty query result not pinned。空の
// `Ok`（0 件）でも通ってしまう分岐を解消した）。

#[test]
fn cold_and_hot_hybrid_results_match_for_empty_query_text() {
    let path = unique_db_path("sparse-cache-recall-empty");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    create_docs_table(&storage, SMALL_VOCAB_SIZE);
    let docs = generate_corpus(SMALL_SEED, SMALL_NUM_DOCS, SMALL_VOCAB_SIZE);
    insert_corpus(&storage, "tenant-empty-query", &docs);
    drop(storage);

    let query_vector = one_hot_sum(SMALL_VOCAB_SIZE, [3usize, 7]);
    let sql = format!(
        "SELECT id FROM docs ORDER BY hybrid_rrf(embedding, '{}', body, '') LIMIT 20",
        vector_literal(&query_vector)
    );
    let dense_only_sql = format!(
        "SELECT id FROM docs ORDER BY embedding <=> '{}' LIMIT 20",
        vector_literal(&query_vector)
    );
    let ctx = PolicyContext::new("tenant-empty-query").expect("valid tenant");

    let cold_ids = {
        // redb は同一パスへの二重オープンを許さないため、cold 側の `EngineCore` は
        // hot 側を開く前にスコープを抜けて閉じる。
        let core_cold = open_core(&path);
        let cold_result = core_cold
            .execute_sql(&ctx, &sql)
            .expect("empty-query hybrid query must stay Ok (dense-only fallback)");
        let cold_stats = core_cold.sparse_index_cache_stats();
        assert_eq!(
            cold_stats.misses, 1,
            "空クエリ: cold は 1 回目で miss するはず"
        );
        assert_eq!(
            cold_stats.hits, 0,
            "空クエリ: cold は 1 回目で hit しないはず"
        );
        result_ids(&cold_result)
    };

    let core_hot = open_core(&path);
    let hot_result_1 = core_hot
        .execute_sql(&ctx, &sql)
        .expect("empty-query hybrid query must stay Ok (dense-only fallback, hot 1st)");
    let hot_result_2 = core_hot
        .execute_sql(&ctx, &sql)
        .expect("empty-query hybrid query must stay Ok (dense-only fallback, hot 2nd)");
    let hot_stats = core_hot.sparse_index_cache_stats();
    assert_eq!(
        hot_stats.misses, 1,
        "空クエリ: hot は 1 回目のみ miss するはず"
    );
    assert_eq!(
        hot_stats.hits, 1,
        "空クエリ: 2 件目はキャッシュヒットするはず（キャッシュ迂回の退行検出）"
    );

    assert_eq!(
        cold_ids,
        result_ids(&hot_result_1),
        "空クエリ: cold と hot(1st) の結果 id 列が乖離した"
    );
    assert_eq!(
        result_ids(&hot_result_1),
        result_ids(&hot_result_2),
        "空クエリ: hot の 1st と 2nd（キャッシュヒット後）が乖離した"
    );

    // `Ok`・cold/hot 一致のみでは空の `Ok`（0 件）でも通ってしまい、コメントで
    // 主張している「密のみへの縮退」自体は固定されない（codex-review 指摘・
    // PR #428: Empty query result not pinned）。件数と、純密クエリの Top-20 id 列
    // との一致を明示的にアサートし、dense-only フォールバックの契約を固定する。
    assert_eq!(
        cold_ids.len(),
        20,
        "空クエリ: 疎側無信号でも密のみで LIMIT 件数を満たすはず"
    );
    let dense_only_result = core_hot
        .execute_sql(&ctx, &dense_only_sql)
        .expect("dense-only query ok");
    assert_eq!(
        cold_ids,
        result_ids(&dense_only_result),
        "空クエリ: hybrid 結果は純密クエリの Top-20 と一致するはず（dense-only フォールバック）"
    );
}

// --- 大規模段: 数万件規模での cold/hot 等価性（Issue #358 検証設計 2.1 の大規模段。
// `cargo test`（debug）での所要時間が長いため既定では実行しない。
// `make sparse-cache-recall-large` で実行する（`bench-hybrid-profile` と同じ
// opt-in 方針）。

const LARGE_NUM_DOCS: usize = 20_000;
const LARGE_VOCAB_SIZE: usize = 800;
const LARGE_NUM_QUERIES: usize = 30;
const LARGE_SEED: u64 = 0x4C41_5247_4531_3031;

#[test]
#[ignore = "数万件規模のためデフォルト実行対象外。make sparse-cache-recall-large で実行する"]
fn cold_and_hot_hybrid_results_are_identical_large_scale() {
    let path = unique_db_path("sparse-cache-recall-large");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    create_docs_table(&storage, LARGE_VOCAB_SIZE);

    let docs = generate_corpus(LARGE_SEED, LARGE_NUM_DOCS, LARGE_VOCAB_SIZE);
    let mut qa_rng = Xorshift64::new(LARGE_SEED ^ 0xA5A5_A5A5_A5A5_A5A5);
    let qa = generate_qa_set(&mut qa_rng, &docs, LARGE_VOCAB_SIZE, LARGE_NUM_QUERIES);
    assert!(
        !qa.is_empty(),
        "large-scale QA generation must find non-empty AND queries"
    );
    insert_corpus(&storage, "tenant-recall-large", &docs);
    drop(storage);

    let mut cold_ids = Vec::with_capacity(qa.len());
    for case in &qa {
        let core = open_core(&path);
        let ctx = PolicyContext::new("tenant-recall-large").expect("valid tenant");
        let sql = hybrid_sql(case, 20);
        let result = core.execute_sql(&ctx, &sql).expect("cold hybrid query ok");
        cold_ids.push(result_ids(&result));
    }

    let core = open_core(&path);
    let ctx = PolicyContext::new("tenant-recall-large").expect("valid tenant");
    let mut hot_ids = Vec::with_capacity(qa.len());
    for case in &qa {
        let sql = hybrid_sql(case, 20);
        let result = core.execute_sql(&ctx, &sql).expect("hot hybrid query ok");
        hot_ids.push(result_ids(&result));
    }
    let final_stats = core.sparse_index_cache_stats();
    assert_eq!(final_stats.misses, 1);
    assert_eq!(final_stats.hits, (qa.len() - 1) as u64);

    for (i, (cold, hot)) in cold_ids.iter().zip(hot_ids.iter()).enumerate() {
        assert_eq!(
            cold, hot,
            "large-scale case {i} diverged between cold and hot runs"
        );
    }
}
