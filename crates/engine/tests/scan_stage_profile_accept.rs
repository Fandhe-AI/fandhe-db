//! `benches/harness/scan_stage_profile.rs`（Issue #464。`agg_count`／
//! `rls_isolation`／`vector_knn_where` の段別内訳プロファイル）の回帰テスト。
//!
//! `scan_stage_profile_bench.rs` は時間依存のためこのテストからは実行しない
//! （`tests/knn_profile_accept.rs`・`tests/knn_wire_profile_accept.rs` と同様、
//! 実測タイマー・env に依存しない時間非依存の契約のみを `#[path]` で取り込み
//! `cargo test`〔`make ci` 対象〕で検証する）。
//!
//! 中心は [`decode_dim_and_metadata_reimpl`]（A4 段の再実装。embedding の f32
//! 変換を行わず dim・metadata borrowed slice のみを返す）が `engine::storage::
//! Storage`（pub API・正本）と一致することの検証（ドリフト検出）。加えて
//! rounds／scale のパース、段間差分・ノイズ帯判定の数値例、整合性検証関数
//! （行数一致）を固定する。

#[allow(dead_code)]
#[path = "../benches/harness/mod.rs"]
mod harness;

use harness::scan_stage_profile::{
    assert_scan_row_counts_match, bucket_share_pct, build_lang_filter, classify_against_bands,
    classify_hnsw_subset_regime, decode_dim_and_metadata_reimpl, expected_visible_hits,
    lang_for_id, matches_lang_filter, median_of, min_of, parse_rounds, parse_scale,
    parse_selectivity, reference_band, refuse_under_github_actions, scan_scalar_columns,
    stage_diff_ns_per_row, step_ratio_pct, verify_row_key_tenant_reimpl, where_clause_for_arm,
    BandClass, ProfileArm, ScanStageError, DEFAULT_ROUNDS, DEFAULT_SELECTIVITY_DENOMINATOR,
    MAX_ROUNDS, MAX_SCALE, MAX_SELECTIVITY_DENOMINATOR, MIN_ROUNDS, MIN_SELECTIVITY_DENOMINATOR,
    TARGET_LANG,
};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::{encode_scalar_columns, ScalarRef, Value};
use engine::storage::{RowInput, Storage, Visibility};
use engine::tenant;

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};

use std::time::Duration;

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const ROWS_TABLE: TableDefinition<(&str, u64), &[u8]> = TableDefinition::new("rows");

// --- refuse_under_github_actions --------------------------------------------

#[test]
fn refuse_under_github_actions_rejects_when_set() {
    let err = refuse_under_github_actions(true).unwrap_err();
    assert_eq!(err, ScanStageError::RefusedUnderGitHubActions);
}

#[test]
fn refuse_under_github_actions_allows_when_unset() {
    assert!(refuse_under_github_actions(false).is_ok());
}

// --- parse_rounds ------------------------------------------------------------

#[test]
fn parse_rounds_defaults_when_unset() {
    assert_eq!(parse_rounds(None).unwrap(), DEFAULT_ROUNDS);
}

#[test]
fn parse_rounds_accepts_within_bounds() {
    assert_eq!(parse_rounds(Some("10")).unwrap(), 10);
    assert_eq!(
        parse_rounds(Some(&MIN_ROUNDS.to_string())).unwrap(),
        MIN_ROUNDS
    );
    assert_eq!(
        parse_rounds(Some(&MAX_ROUNDS.to_string())).unwrap(),
        MAX_ROUNDS
    );
}

#[test]
fn parse_rounds_rejects_below_minimum() {
    assert!(matches!(
        parse_rounds(Some("1")),
        Err(ScanStageError::InvalidRounds(_))
    ));
}

#[test]
fn parse_rounds_rejects_above_maximum() {
    assert!(matches!(
        parse_rounds(Some("51")),
        Err(ScanStageError::InvalidRounds(_))
    ));
}

#[test]
fn parse_rounds_rejects_non_numeric() {
    assert!(matches!(
        parse_rounds(Some("abc")),
        Err(ScanStageError::InvalidRounds(_))
    ));
}

// --- parse_scale ---------------------------------------------------------------

#[test]
fn parse_scale_defaults_to_one_when_unset() {
    assert_eq!(parse_scale(None).unwrap(), 1);
}

#[test]
fn parse_scale_accepts_within_bounds() {
    assert_eq!(parse_scale(Some("4")).unwrap(), MAX_SCALE);
}

#[test]
fn parse_scale_rejects_above_maximum() {
    assert!(matches!(
        parse_scale(Some("5")),
        Err(ScanStageError::InvalidScale(_))
    ));
}

#[test]
fn parse_scale_rejects_zero() {
    assert!(matches!(
        parse_scale(Some("0")),
        Err(ScanStageError::InvalidScale(_))
    ));
}

// --- min_of / median_of / reference_band / step_ratio_pct / classify_against_bands

#[test]
fn min_of_and_median_of_basic() {
    let samples = vec![
        Duration::from_millis(10),
        Duration::from_millis(30),
        Duration::from_millis(20),
    ];
    assert_eq!(min_of(&samples).unwrap(), Duration::from_millis(10));
    assert_eq!(median_of(&samples).unwrap(), Duration::from_millis(20));
}

#[test]
fn median_of_even_length_averages_middle_two() {
    let samples = vec![
        Duration::from_millis(10),
        Duration::from_millis(20),
        Duration::from_millis(30),
        Duration::from_millis(40),
    ];
    assert_eq!(median_of(&samples).unwrap(), Duration::from_millis(25));
}

#[test]
fn min_of_median_of_reject_empty() {
    let samples: Vec<Duration> = Vec::new();
    assert_eq!(min_of(&samples).unwrap_err(), ScanStageError::EmptySamples);
    assert_eq!(
        median_of(&samples).unwrap_err(),
        ScanStageError::EmptySamples
    );
}

#[test]
fn reference_band_computes_spread_over_min() {
    let medians = vec![
        Duration::from_millis(100),
        Duration::from_millis(110),
        Duration::from_millis(105),
    ];
    let band = reference_band(&medians).unwrap();
    assert!((band - 0.10).abs() < 1e-9);
}

#[test]
fn step_ratio_pct_rejects_zero_from() {
    assert!(matches!(
        step_ratio_pct(Duration::ZERO, Duration::from_millis(1)),
        Err(ScanStageError::DegenerateRatio(_))
    ));
}

#[test]
fn classify_against_bands_within_fixed_band() {
    assert_eq!(classify_against_bands(4.0, 0.0), BandClass::WithinNoiseBand);
    assert_eq!(classify_against_bands(6.0, 0.0), BandClass::AboveNoiseBand);
    // 実測帯が固定帯より広ければそちらを使う。
    assert_eq!(
        classify_against_bands(8.0, 10.0),
        BandClass::WithinNoiseBand
    );
}

// --- stage_diff_ns_per_row: 非単調は Err -------------------------------------

#[test]
fn stage_diff_ns_per_row_rejects_non_monotonic() {
    let err = stage_diff_ns_per_row(
        Duration::from_millis(10),
        Duration::from_millis(5),
        100,
        "A1",
        "A2",
    )
    .unwrap_err();
    assert!(matches!(err, ScanStageError::ConsistencyViolation(_)));
}

#[test]
fn stage_diff_ns_per_row_computes_ns_per_row() {
    let diff = stage_diff_ns_per_row(
        Duration::from_millis(0),
        Duration::from_millis(1),
        1_000_000,
        "A1",
        "A2",
    )
    .unwrap();
    assert!((diff - 1.0).abs() < 1e-6);
}

// --- assert_scan_row_counts_match --------------------------------------------

#[test]
fn assert_scan_row_counts_match_accepts_equal_counts() {
    assert!(assert_scan_row_counts_match(&[("A1", 10), ("A2", 10), ("A3", 10)]).is_ok());
}

#[test]
fn assert_scan_row_counts_match_rejects_mismatch() {
    let err = assert_scan_row_counts_match(&[("A1", 10), ("A2", 9)]).unwrap_err();
    assert!(matches!(err, ScanStageError::ConsistencyViolation(_)));
}

// --- verify_row_key_tenant_reimpl --------------------------------------------

#[test]
fn verify_row_key_tenant_reimpl_accepts_matching_tenant() {
    assert!(verify_row_key_tenant_reimpl("tenant-a", "tenant-a").is_ok());
}

#[test]
fn verify_row_key_tenant_reimpl_rejects_mismatch() {
    assert!(verify_row_key_tenant_reimpl("tenant-a", "tenant-b").is_err());
}

// --- W1/W2: scan_scalar_columns / build_lang_filter / matches_lang_filter ---

fn scalar_schema() -> TableSchema {
    TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(4), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("topic", ColumnType::Text, false),
        ],
    )
}

#[test]
fn scan_scalar_columns_and_lang_filter_roundtrip() {
    let schema = scalar_schema();
    let metadata = encode_scalar_columns(
        &schema,
        &[
            Value::Vector(vec![1.0, 2.0, 3.0, 4.0]),
            Value::Text("ja".to_string()),
            Value::Text("topic-00".to_string()),
        ],
    )
    .expect("encode_scalar_columns");

    let scanned = scan_scalar_columns(&schema, &metadata).expect("scan_scalar_columns");
    assert_eq!(scanned.len(), schema.columns.len());
    assert_eq!(scanned[1], Some(ScalarRef::Text("ja")));
    assert_eq!(scanned[2], Some(ScalarRef::Text("topic-00")));

    let filters = vec![build_lang_filter(&schema, "lang", "ja").expect("build_lang_filter")];
    assert!(matches_lang_filter(&filters, &scanned));

    let non_matching_filters =
        vec![build_lang_filter(&schema, "lang", "en").expect("build_lang_filter")];
    assert!(!matches_lang_filter(&non_matching_filters, &scanned));
}

// --- decode_dim_and_metadata_reimpl: ドリフト検出 ---------------------------

fn seed_storage(path: &std::path::Path) -> Vec<(String, Visibility, Vec<f32>, Vec<u8>)> {
    let storage = Storage::open(path).expect("open storage");
    let fixtures: Vec<(String, Visibility, Vec<f32>, Vec<u8>)> = vec![
        (
            "tenant-a".to_string(),
            Visibility::Public,
            vec![1.0, 2.0, 3.0, 4.0],
            b"meta-a".to_vec(),
        ),
        (
            "tenant-a".to_string(),
            Visibility::Private,
            vec![-1.5, 0.0, 2.25],
            Vec::new(),
        ),
        (
            "tenant-b".to_string(),
            Visibility::Public,
            vec![0.0; 8],
            b"meta-b-longer".to_vec(),
        ),
    ];
    for (i, (tenant, visibility, embedding, metadata)) in fixtures.iter().enumerate() {
        storage
            .put(
                i as u64,
                &RowInput {
                    tenant_id: tenant,
                    visibility: *visibility,
                    embedding,
                    metadata,
                },
            )
            .expect("seed row");
    }
    fixtures
}

#[test]
fn decode_dim_and_metadata_reimpl_matches_storage_scan_for_every_row() {
    let path = unique_db_path("issue464-scan-stage-profile-accept-decode");
    let _guard = CleanupGuard(path.clone());
    let fixtures = seed_storage(&path);

    // 正本: `Storage::scan()`（pub API・完全デコード）。
    let storage = Storage::open(&path).expect("reopen storage for scan");
    let expected = storage.scan().expect("scan must succeed");
    assert_eq!(expected.len(), fixtures.len());
    drop(storage);

    // 再実装: 生 `redb::Database` で再オープンし、本モジュールのデコードを通す。
    let db = Database::open(&path).expect("reopen raw database");
    let read_txn = db.begin_read().expect("begin read txn");
    let table = read_txn.open_table(ROWS_TABLE).expect("open rows table");

    let mut actual_count = 0usize;
    for entry in table.iter().expect("iter rows table") {
        let (_k, v) = entry.expect("iterate row entry");
        let (dim, metadata) = decode_dim_and_metadata_reimpl(v.value()).expect("reimpl decode");
        let expected_row = expected
            .iter()
            .find(|row| row.embedding.len() == dim as usize && row.metadata == metadata)
            .unwrap_or_else(|| {
                panic!(
                    "reimpl decoded row not found in Storage::scan() result \
                     (layout drift suspected): dim={dim} metadata={metadata:?}"
                )
            });
        assert_eq!(expected_row.embedding.len(), dim as usize);
        actual_count += 1;
    }
    assert_eq!(actual_count, fixtures.len());
}

#[test]
fn decode_dim_and_metadata_reimpl_rejects_truncated_buffer() {
    let path = unique_db_path("issue464-scan-stage-profile-accept-truncated");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .put(
            0,
            &RowInput {
                tenant_id: "tenant-a",
                visibility: Visibility::Public,
                embedding: &[1.0, 2.0, 3.0],
                metadata: b"abc",
            },
        )
        .expect("seed row");
    drop(storage);

    let db = Database::open(&path).expect("reopen raw database");
    let read_txn = db.begin_read().expect("begin read txn");
    let table = read_txn.open_table(ROWS_TABLE).expect("open rows table");
    let (_k, v) = table
        .iter()
        .expect("iter rows table")
        .next()
        .expect("at least one row")
        .expect("iterate row entry");
    let full = v.value();
    // 末尾を切り詰めて破損させる。
    let truncated = &full[..full.len().saturating_sub(2)];
    let err = decode_dim_and_metadata_reimpl(truncated).unwrap_err();
    assert!(matches!(err, ScanStageError::Codec(_)));
}

// --- 選択率 opt-in（Issue #653） ---------------------------------------------

#[test]
fn parse_selectivity_defaults_when_unset() {
    assert_eq!(
        parse_selectivity(None).unwrap(),
        DEFAULT_SELECTIVITY_DENOMINATOR
    );
    assert_eq!(
        parse_selectivity(Some("")).unwrap(),
        DEFAULT_SELECTIVITY_DENOMINATOR
    );
    assert_eq!(
        parse_selectivity(Some("  ")).unwrap(),
        DEFAULT_SELECTIVITY_DENOMINATOR
    );
}

#[test]
fn parse_selectivity_accepts_within_bounds() {
    assert_eq!(parse_selectivity(Some("1/3")).unwrap(), 3);
    assert_eq!(
        parse_selectivity(Some(&format!("1/{MIN_SELECTIVITY_DENOMINATOR}"))).unwrap(),
        MIN_SELECTIVITY_DENOMINATOR
    );
    assert_eq!(
        parse_selectivity(Some(&format!("1/{MAX_SELECTIVITY_DENOMINATOR}"))).unwrap(),
        MAX_SELECTIVITY_DENOMINATOR
    );
}

#[test]
fn parse_selectivity_rejects_below_minimum() {
    assert!(matches!(
        parse_selectivity(Some("1/1")),
        Err(ScanStageError::InvalidSelectivity(_))
    ));
    assert!(matches!(
        parse_selectivity(Some("1/0")),
        Err(ScanStageError::InvalidSelectivity(_))
    ));
}

#[test]
fn parse_selectivity_rejects_above_maximum() {
    assert!(matches!(
        parse_selectivity(Some("1/101")),
        Err(ScanStageError::InvalidSelectivity(_))
    ));
}

#[test]
fn parse_selectivity_rejects_malformed_shapes() {
    for raw in ["3", "2/3", "1/", "/3", "1/abc", "1/3/4", "1 / 3"] {
        assert!(
            matches!(
                parse_selectivity(Some(raw)),
                Err(ScanStageError::InvalidSelectivity(_))
            ),
            "expected {raw:?} to be rejected"
        );
    }
}

// --- lang_for_id -------------------------------------------------------------

#[test]
fn lang_for_id_default_denominator_matches_legacy_langs_rotation() {
    // 導入前の `LANGS[(id as usize) % LANGS.len()]`
    // （`LANGS = ["ja","en","fr","de","es"]`）とビット同一であることを固定する。
    const LEGACY_LANGS: &[&str] = &["ja", "en", "fr", "de", "es"];
    for id in 0..1000u64 {
        let expected = LEGACY_LANGS[(id as usize) % LEGACY_LANGS.len()];
        assert_eq!(
            lang_for_id(id, DEFAULT_SELECTIVITY_DENOMINATOR),
            expected,
            "id={id}"
        );
    }
}

#[test]
fn lang_for_id_selects_target_lang_at_the_configured_rate() {
    // denominator=3: id % 3 == 0 のみ "ja"、それ以外は非 "ja"。
    let mut ja_count = 0usize;
    for id in 0..3000u64 {
        if lang_for_id(id, 3) == TARGET_LANG {
            ja_count += 1;
            assert_eq!(id % 3, 0);
        } else {
            assert_ne!(id % 3, 0);
        }
    }
    assert_eq!(ja_count, 1000);
}

#[test]
fn lang_for_id_never_panics_across_the_full_denominator_range() {
    for denominator in MIN_SELECTIVITY_DENOMINATOR..=MAX_SELECTIVITY_DENOMINATOR {
        for id in 0..(denominator as u64 * 2) {
            let _ = lang_for_id(id, denominator);
        }
    }
}

// --- expected_visible_hits ----------------------------------------------------

#[test]
fn expected_visible_hits_counts_matching_ids_only() {
    let ids: Vec<u64> = (0..9).collect();
    // denominator=3 → id % 3 == 0 のみ一致（0,3,6）。
    assert_eq!(expected_visible_hits(&ids, 3), 3);
}

#[test]
fn expected_visible_hits_empty_input_is_zero() {
    assert_eq!(expected_visible_hits(&[], 5), 0);
}

// --- ProfileArm・where_clause_for_arm ------------------------------------------

#[test]
fn where_clause_for_arm_index_is_a_bare_equality() {
    assert_eq!(
        where_clause_for_arm(ProfileArm::Index, "lang", "ja"),
        "WHERE lang = 'ja'"
    );
}

#[test]
fn where_clause_for_arm_plain_appends_a_residual_vector_predicate() {
    // `AND 1 = 1` は束縛時の定数畳み込み（Issue #353）で消去され
    // `classify_scalar_plan` を `PlainScan` へ強制できないため、`VectorRef` を
    // 参照する残余述語（`id_predicate_from_expr` が `None` を返す形状）を使う。
    assert_eq!(
        where_clause_for_arm(ProfileArm::Plain, "lang", "ja"),
        "WHERE lang = 'ja' AND vec_norm(embedding) > 0"
    );
}

#[test]
fn profile_arm_display_matches_stats_line_labels() {
    assert_eq!(ProfileArm::Index.to_string(), "index");
    assert_eq!(ProfileArm::Plain.to_string(), "plain");
}

// --- bucket_share_pct ----------------------------------------------------------

#[test]
fn bucket_share_pct_computes_percentage_of_total() {
    let part = Duration::from_micros(250);
    let total = Duration::from_micros(1000);
    assert!((bucket_share_pct(part, total).unwrap() - 25.0).abs() < 1e-9);
}

#[test]
fn bucket_share_pct_rejects_zero_total() {
    assert!(matches!(
        bucket_share_pct(Duration::from_micros(1), Duration::ZERO),
        Err(ScanStageError::DegenerateRatio(_))
    ));
}

// --- アーム契約テスト（Issue #653） -------------------------------------------
//
// `ScalarIndex` 候補削減（Issue #474）を実際に経由する production 経路
// （`EngineCore::execute_sql` → `sql::exec::execute_statement_with_cache`）
// 上で、[`ProfileArm::Index`]／[`ProfileArm::Plain`] が
// `scalar_index_cache_stats()` に対して意図した契約（索引アームは
// `index_scans` のみ増分、plain アームはいずれのカウンタも不変）を満たし、
// かつ両アームが同一の Top-k id 集合を返すことを固定する
// （`tests/scalar_index_prune.rs::assert_cold_hot_equivalent` と同じ流儀）。

fn arm_test_schema() -> TableSchema {
    TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(4), false),
            ColumnDef::new("lang", ColumnType::Text, false),
        ],
    )
}

#[test]
fn profile_arm_contract_matches_scalar_index_cache_stats() {
    let path = unique_db_path("scan-stage-profile-accept-arm-contract");
    let _guard = CleanupGuard(path.clone());
    let schema = arm_test_schema();
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema).expect("create table");

    let ctx = PolicyContext::with_visibilities("tenant-a", [Visibility::Public])
        .expect("valid tenant id");
    // denominator=3 相当（selectivity ≒ 33%）: id % 3 == 0 のみ "ja"。
    // 索引の選択度閾値（既定 1/2）を超えないよう、十分な行数（30 行・10 一致）
    // を投入する。
    let rows: Vec<(u64, Vec<f32>, &str)> = (0..30u64)
        .map(|id| {
            let lang = lang_for_id(id, 3);
            // `+ 1.0` でゼロベクトル（`id=0`）を避ける。ゼロノルムのベクトルは
            // `<=>` の距離計算・`vec_norm(embedding) > 0` 述語のいずれでも
            // 特別扱いになりうり、本テストの意図（index/plain 両アームの id
            // 集合一致）とは無関係な差異を生むため。
            (id, vec![id as f32 + 1.0, 0.0, 0.0, 0.0], lang)
        })
        .collect();
    for (id, embedding, lang) in &rows {
        let metadata = encode_scalar_columns(
            &schema,
            &[
                Value::Vector(embedding.clone()),
                Value::Text(lang.to_string()),
            ],
        )
        .expect("encode_scalar_columns");
        let op_id = OperationId::parse(&format!("seed-{id}")).expect("valid operation_id");
        tenant::insert_rows(
            &storage,
            "docs",
            &ctx,
            &[(
                *id,
                RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding,
                    metadata: &metadata,
                },
            )],
            &op_id,
        )
        .expect("seed row");
    }
    let expected_match_ids: Vec<u64> = rows
        .iter()
        .filter(|(id, _, _)| lang_for_id(*id, 3) == TARGET_LANG)
        .map(|(id, _, _)| *id)
        .collect();
    assert_eq!(
        expected_visible_hits(&(0..30).collect::<Vec<_>>(), 3),
        expected_match_ids.len()
    );

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));

    let sql_index = format!(
        // `LIMIT` を一致件数（10）以上に取り、Top-k がタイブレーク順序に依存
        // せず一致行集合の全体（部分集合ではなく）になるようにする（クエリ
        // ベクトルが原点のため候補間の距離が同点になり得る）。
        "SELECT id FROM docs {} ORDER BY embedding <=> '[0.0,0.0,0.0,0.0]' LIMIT 20",
        where_clause_for_arm(ProfileArm::Index, "lang", TARGET_LANG)
    );
    let sql_plain = format!(
        // `LIMIT` を一致件数（10）以上に取り、Top-k がタイブレーク順序に依存
        // せず一致行集合の全体（部分集合ではなく）になるようにする（クエリ
        // ベクトルが原点のため候補間の距離が同点になり得る）。
        "SELECT id FROM docs {} ORDER BY embedding <=> '[0.0,0.0,0.0,0.0]' LIMIT 20",
        where_clause_for_arm(ProfileArm::Plain, "lang", TARGET_LANG)
    );

    let result_ids = |result: &engine::sql::exec::QueryResult| -> Vec<u64> {
        result
            .rows
            .iter()
            .map(|row| match row.cells.first() {
                Some(engine::sql::exec::Cell::Integer(v)) => *v,
                other => panic!("unexpected id cell: {other:?}"),
            })
            .collect()
    };

    // 索引アーム: cold（索引を構築）→ hot（索引を消費）の順で 2 回実行し、
    // `index_scans` のみが増分することを固定する。
    let before_index = core.scalar_index_cache_stats();
    let cold_index = core
        .execute_sql(&ctx, &sql_index)
        .expect("execute_sql (index arm, cold)");
    let hot_index = core
        .execute_sql(&ctx, &sql_index)
        .expect("execute_sql (index arm, hot)");
    let after_index = core.scalar_index_cache_stats();
    let mut cold_index_ids = result_ids(&cold_index);
    cold_index_ids.sort_unstable();
    let mut hot_index_ids = result_ids(&hot_index);
    hot_index_ids.sort_unstable();
    assert_eq!(cold_index_ids, hot_index_ids);
    for id in &hot_index_ids {
        assert!(
            expected_match_ids.contains(id),
            "id {id} leaked outside lang='ja'"
        );
    }
    // 非リーク検証（上のループ）だけでは、`hot_index_ids` が空集合や
    // `expected_match_ids` の真部分集合でも通過してしまう（Cursor Bugbot
    // 指摘・PR #663）。`index_scans` の増分・id 集合の cold/hot 一致は
    // 「索引経路を通ったこと」の確認にとどまり「lang='ja' の期待行が
    // 実際に全件返っていること」の確認にはならないため、独立に導出した
    // `expected_match_ids`（10 行）との完全一致を固定して vacuous pass を防ぐ。
    // `expected_match_ids` は `rows` を `id` 昇順に走査して構築しているため
    // 既に昇順であり、`sort_unstable` 済みの `hot_index_ids` と直接比較できる。
    assert_eq!(
        hot_index_ids, expected_match_ids,
        "index arm hot result must return exactly the lang='ja' visible set, not a subset (vacuous-pass guard)"
    );
    assert!(
        after_index.index_scans > before_index.index_scans,
        "index arm must consume ScalarIndex candidate resolution at least once"
    );
    assert_eq!(
        after_index.plain_scan_fallbacks, before_index.plain_scan_fallbacks,
        "index arm must never fall back to plain scan"
    );

    // plain アーム: `PlainScan` 分類のため index_scans／plain_scan_fallbacks の
    // いずれも不変のまま、索引アームと同一の id 集合を返す。
    let before_plain = core.scalar_index_cache_stats();
    let plain_result = core
        .execute_sql(&ctx, &sql_plain)
        .expect("execute_sql (plain arm)");
    let after_plain = core.scalar_index_cache_stats();
    let mut plain_ids = result_ids(&plain_result);
    plain_ids.sort_unstable();
    assert_eq!(plain_ids, hot_index_ids);
    assert_eq!(
        after_plain.index_scans, before_plain.index_scans,
        "plain arm must never consume ScalarIndex candidate resolution"
    );
    assert_eq!(
        after_plain.plain_scan_fallbacks, before_plain.plain_scan_fallbacks,
        "a pure PlainScan-classified query never enters the fallback-recording branch"
    );
}

// --- classify_hnsw_subset_regime（Issue #677） ------------------------------

#[test]
fn classify_hnsw_subset_regime_ann_masked_has_priority() {
    // `subset_searches_delta > 0`（ANN 完走）はどのフィールドが同時に非ゼロ
    // でも最優先で `ann_masked` と分類する。
    assert_eq!(classify_hnsw_subset_regime(0, 1, 0, 0, 0), "ann_masked");
    assert_eq!(classify_hnsw_subset_regime(1, 1, 1, 1, 1), "ann_masked");
}

#[test]
fn classify_hnsw_subset_regime_plain_scan_ratio() {
    assert_eq!(
        classify_hnsw_subset_regime(0, 0, 1, 0, 0),
        "plain_scan_ratio"
    );
}

#[test]
fn classify_hnsw_subset_regime_mask_split() {
    // Issue #676 が候補 id マスク経路（複製なし）へ委譲する対象の一つ
    // （connectivity 検査による分断検知）。
    assert_eq!(
        classify_hnsw_subset_regime(0, 0, 0, 1, 0),
        "plain_scan_mask_split"
    );
}

#[test]
fn classify_hnsw_subset_regime_masked_short() {
    assert_eq!(
        classify_hnsw_subset_regime(0, 0, 0, 0, 1),
        "plain_scan_masked_short"
    );
}

#[test]
fn classify_hnsw_subset_regime_full_visible_hit_fallback() {
    // `Subset` 形状のクエリでは通常到達しないが、`hits_delta` のみが非ゼロの
    // 場合でも panic せず判別可能なラベルを返す（呼び出し元の非 vacuous
    // 検査が 5 カウンタ中いずれか 1 つの非ゼロを要求するため）。
    assert_eq!(
        classify_hnsw_subset_regime(1, 0, 0, 0, 0),
        "full_visible_hit (unexpected for a Subset-shaped query)"
    );
}

#[test]
fn classify_hnsw_subset_regime_all_zero_is_not_ann_masked() {
    assert_eq!(
        classify_hnsw_subset_regime(0, 0, 0, 0, 0),
        "n/a (all counters delta=0)"
    );
}
