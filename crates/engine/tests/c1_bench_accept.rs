//! `benches/harness/sql_c1.rs`（TASK-83 の SQL 文字列組み立て）と
//! `benches/harness/env_report.rs`（実行環境記録）の回帰テスト。
//!
//! 対象ビヘイビア: なし（基盤タスク。`sql_c1_bench.rs` は時間依存のためこのテストからは
//! 実行しない。`tests/bench_accept.rs`・`tests/batch_accept.rs` と同様、実測タイマーに
//! 依存しない時間非依存の契約のみを `#[path]` で取り込み `cargo test`（`make ci` 対象）
//! で検証する）。

// `harness` モジュール全体を対象に `dead_code` を許容する（本テストは
// `sql_c1`・`env_report` のみを検証対象とし、`ab`/`protocol`/`rng`/`accept`/`stats` は
// 他のテスト（`tests/bench_harness.rs`・`tests/bench_accept.rs`）が別途検証するため。
// `tests/bench_accept.rs` と同一方針）。
#[allow(dead_code)]
#[path = "../benches/harness/mod.rs"]
mod harness;

use harness::env_report::EnvReport;
use harness::sql_c1::{
    c1_statement, c1_where_statement, render_ab_line, render_p95_line, render_recall_line,
    resolve_verbose, vector_literal, SqlC1Error, MAX_VECTOR_LITERAL_BYTES,
};

use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::parser::parse_vector_literal;

// --- vector_literal ---

#[test]
fn vector_literal_round_trips_through_parse_vector_literal() {
    let values: Vec<f32> = (0..768).map(|i| (i as f32) * 0.01 - 3.84).collect();
    let literal = vector_literal(&values).expect("finite values must produce a literal");
    assert!(literal.as_str().len() < 64 * 1024);

    // `vector_literal` は「`str::parse::<f32>` で元の値へ往復できる」ことを契約として
    // 宣言しているため、許容誤差なしのビット等価比較で検証する（誤差付き比較は契約より
    // 緩く、往復性の劣化を見逃す）。
    let parsed = parse_vector_literal(literal.as_str(), 768).expect("literal must parse back");
    assert_eq!(parsed, values, "vector_literal must round-trip exactly");
}

#[test]
fn vector_literal_rejects_nan() {
    let values = vec![1.0, f32::NAN, 3.0];
    assert_eq!(vector_literal(&values), Err(SqlC1Error::NonFiniteComponent));
}

#[test]
fn vector_literal_rejects_infinity() {
    let values = vec![1.0, f32::INFINITY, 3.0];
    assert_eq!(vector_literal(&values), Err(SqlC1Error::NonFiniteComponent));
}

#[test]
fn vector_literal_empty_dimension_round_trips() {
    let literal = vector_literal(&[]).expect("empty vector must produce a literal");
    assert_eq!(literal.as_str(), "[]");
    let parsed = parse_vector_literal(literal.as_str(), 0).expect("literal must parse back");
    assert!(parsed.is_empty());
}

#[test]
fn max_vector_literal_bytes_matches_parser_boundary() {
    // `harness::sql_c1::MAX_VECTOR_LITERAL_BYTES` は private な
    // `sql::parser::MAX_VECTOR_LITERAL_BYTES` の手動複製値。この境界テストは
    // harness 側の定数を基準に `parse_vector_literal` の受理・拒否境界を突き合わせる
    // ことで、parser 側の定数が将来変更されたときにこのテストが真っ先に落ちる形で
    // ドリフトを検知する。
    let padding_len = MAX_VECTOR_LITERAL_BYTES - 2; // `[` と `]` の 2 バイトを引く
    let padding = "0".repeat(padding_len);

    let literal_at_limit = format!("[{padding}]");
    assert_eq!(literal_at_limit.len(), MAX_VECTOR_LITERAL_BYTES);
    // 中身は数値として不正（"0" の連続）なので InvalidInput になるのは想定内。
    // ここで確認したいのは PayloadTooLarge にならないことだけ。
    if let Err(SqlSurfaceError::PayloadTooLarge { .. }) = parse_vector_literal(&literal_at_limit, 1)
    {
        panic!("literal exactly at MAX_VECTOR_LITERAL_BYTES must not be rejected as too large");
    }

    let literal_over_limit = format!("[{padding}0]");
    assert_eq!(literal_over_limit.len(), MAX_VECTOR_LITERAL_BYTES + 1);
    assert!(matches!(
        parse_vector_literal(&literal_over_limit, 1),
        Err(SqlSurfaceError::PayloadTooLarge { .. })
    ));
}

// --- c1_statement ---

#[test]
fn c1_statement_builds_expected_select() {
    let literal = vector_literal(&[1.0, 0.0, 0.0]).unwrap();
    let sql = c1_statement("documents", "embedding", &literal, 20).unwrap();
    assert_eq!(
        sql,
        "SELECT id FROM documents ORDER BY embedding <=> '[1,0,0]' LIMIT 20"
    );
}

#[test]
fn c1_statement_rejects_empty_table_identifier() {
    let literal = vector_literal(&[1.0]).unwrap();
    assert_eq!(
        c1_statement("", "embedding", &literal, 20),
        Err(SqlC1Error::InvalidIdentifier("table"))
    );
}

#[test]
fn c1_statement_rejects_table_identifier_starting_with_digit() {
    let literal = vector_literal(&[1.0]).unwrap();
    assert_eq!(
        c1_statement("1docs", "embedding", &literal, 20),
        Err(SqlC1Error::InvalidIdentifier("table"))
    );
}

#[test]
fn c1_statement_rejects_column_identifier_with_symbol() {
    let literal = vector_literal(&[1.0]).unwrap();
    assert_eq!(
        c1_statement("documents", "embed;ding", &literal, 20),
        Err(SqlC1Error::InvalidIdentifier("column"))
    );
}

// --- c1_where_statement（Issue #487: 可視比率スイープの WHERE 付き C1）---

#[test]
fn c1_where_statement_builds_expected_select() {
    let literal = vector_literal(&[1.0, 0.0, 0.0]).unwrap();
    let sql = c1_where_statement("documents", "embedding", "bucket", "b0", &literal, 10).unwrap();
    assert_eq!(
        sql,
        "SELECT id FROM documents WHERE bucket = 'b0' ORDER BY embedding <=> '[1,0,0]' LIMIT 10"
    );
}

#[test]
fn c1_where_statement_rejects_invalid_filter_column_identifier() {
    let literal = vector_literal(&[1.0]).unwrap();
    assert_eq!(
        c1_where_statement("documents", "embedding", "buc;ket", "b0", &literal, 10),
        Err(SqlC1Error::InvalidIdentifier("filter_column"))
    );
}

#[test]
fn c1_where_statement_rejects_filter_token_with_sql_symbols() {
    let literal = vector_literal(&[1.0]).unwrap();
    for token in ["b0' OR '1'='1", "b 0", "", "b0;drop"] {
        assert_eq!(
            c1_where_statement("documents", "embedding", "bucket", token, &literal, 10),
            Err(SqlC1Error::InvalidIdentifier("filter_token")),
            "expected {token:?} to be rejected"
        );
    }
}

#[test]
fn c1_where_statement_rejects_invalid_table_and_column_identifiers() {
    let literal = vector_literal(&[1.0]).unwrap();
    assert_eq!(
        c1_where_statement("1docs", "embedding", "bucket", "b0", &literal, 10),
        Err(SqlC1Error::InvalidIdentifier("table"))
    );
    assert_eq!(
        c1_where_statement("documents", "embed;ding", "bucket", "b0", &literal, 10),
        Err(SqlC1Error::InvalidIdentifier("column"))
    );
}

// --- SQL-1 end-to-end: c1_statement の出力が EngineCore::execute_sql に受理され、
//     結果 id 列が CpuScalarProvider の Top-k と一致すること -----------------------

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;

mod e2e {
    use super::temp_db::{unique_db_path, CleanupGuard};
    use super::*;
    use engine::catalog::{ColumnDef, ColumnType, TableSchema};
    use engine::core::EngineCore;
    use engine::kernel::{CpuScalarProvider, SearchInput, SearchProvider};
    use engine::policy::PolicyContext;
    use engine::row_codec::Value;
    use engine::storage::{Storage, Visibility};

    #[test]
    fn c1_statement_output_is_accepted_and_matches_exact_oracle() {
        let path = unique_db_path("c1-bench-accept-e2e");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&TableSchema::new(
                "docs",
                vec![ColumnDef::new("embedding", ColumnType::Vector(3), false)],
            ))
            .expect("create table");

        let corpus: Vec<(u64, [f32; 3])> = vec![
            (1, [1.0, 0.0, 0.0]),
            (2, [0.9, 0.1, 0.0]),
            (3, [0.0, 1.0, 0.0]),
            (4, [0.0, 0.0, 1.0]),
            (5, [0.5, 0.5, 0.0]),
            (6, [-1.0, 0.0, 0.0]),
        ];
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        for (id, emb) in &corpus {
            // TASK-101（RECOVER-10）: 台帳は operation_id ごとに内容ハッシュを持つため、
            // 内容の異なる複数行へ同一 operation_id を使い回すと 2 件目以降が
            // OperationIdContentMismatch で拒否される。行ごとに一意の operation_id を使う。
            let op_id =
                engine::recovery::required_op_id::OperationId::parse(&format!("test-op-{id}"))
                    .expect("valid operation_id");
            engine::tenant::insert_typed_row(
                &storage,
                "docs",
                &ctx,
                *id,
                Visibility::Public,
                &[Value::Vector(emb.to_vec())],
                &op_id,
            )
            .expect("insert row");
        }

        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));

        let query = [1.0f32, 0.0, 0.0];
        let literal = vector_literal(&query).expect("finite query vector");
        let sql = c1_statement("docs", "embedding", &literal, 3).expect("well-formed statement");

        let result = core
            .execute_sql(&ctx, &sql)
            .expect("execute_sql accepts C1 template");
        let actual: Vec<u64> = result.rows.iter().map(|r| r.id).collect();

        let ids: Vec<u64> = corpus.iter().map(|(id, _)| *id).collect();
        let flat: Vec<f32> = corpus.iter().flat_map(|(_, e)| e.iter().copied()).collect();
        let expected: Vec<u64> = CpuScalarProvider
            .search(SearchInput {
                ids: &ids,
                vectors: &flat,
                dim: 3,
                query: &query,
                k: 3,
            })
            .expect("reference search succeeds")
            .into_iter()
            .map(|hit| hit.id)
            .collect();

        // 集合ではなく順序込みで比較する。C1 テンプレートは `ORDER BY <=> ... LIMIT k` の
        // 距離昇順を契約しており、集合比較では順序の退行を検知できないため。上記 corpus は
        // query `[1,0,0]` に対する上位 3 件（id=1・2・5）の距離が互いに異なり同点タイが
        // 発生しないので、SQL 表層経路と参照オラクル経路の順序は一意に定まる。
        assert_eq!(
            actual, expected,
            "C1 template result must match the independent exact oracle's ordered top-k"
        );
    }
}

// --- resolve_verbose・render_* (Issue #278: 実測値 public ログ出力の抑止) ---

mod verbose_opt_in {
    use super::*;
    use std::time::Duration;

    #[test]
    fn resolve_verbose_defaults_to_false_when_unset() {
        assert_eq!(resolve_verbose(None, false), Ok(false));
    }

    #[test]
    fn resolve_verbose_true_on_exact_match_outside_github_actions() {
        assert_eq!(resolve_verbose(Some("1"), false), Ok(true));
    }

    #[test]
    fn resolve_verbose_rejects_non_exact_values() {
        // `BENCH_DEDICATED_ENV` の `.trim() == "1"` より厳格な厳密一致（Issue #278の
        // 設計方針）。空白付き・別表記は verbose 側へ寛容にしない。
        for raw in ["true", "0", " 1 ", "yes", ""] {
            assert_eq!(
                resolve_verbose(Some(raw), false),
                Ok(false),
                "raw={raw:?} must not be treated as verbose"
            );
        }
    }

    #[test]
    fn resolve_verbose_fails_closed_under_github_actions_when_requested() {
        assert_eq!(
            resolve_verbose(Some("1"), true),
            Err(SqlC1Error::VerboseRefusedUnderGitHubActions)
        );
    }

    #[test]
    fn resolve_verbose_unset_under_github_actions_does_not_block_normal_gate_runs() {
        assert_eq!(resolve_verbose(None, true), Ok(false));
    }

    #[test]
    fn render_p95_line_non_verbose_excludes_measured_values() {
        let line = render_p95_line(
            100_000,
            768,
            20,
            Duration::from_millis(12),
            Duration::from_millis(34),
            true,
            false,
        );
        assert!(line.contains("rows=100000"));
        assert!(line.contains("dim=768"));
        assert!(line.contains("k=20"));
        assert!(line.contains("pass=true"));
        assert!(!line.contains(&format!("{:?}", Duration::from_millis(12))));
        assert!(!line.contains(&format!("{:?}", Duration::from_millis(34))));
        assert!(!line.contains("median"));
        assert!(!line.contains("p95="));
    }

    #[test]
    fn render_p95_line_verbose_includes_measured_values() {
        let median = Duration::from_millis(12);
        let p95 = Duration::from_millis(34);
        let line = render_p95_line(100_000, 768, 20, median, p95, true, true);
        assert!(line.contains(&format!("{median:?}")));
        assert!(line.contains(&format!("{p95:?}")));
    }

    #[test]
    fn render_recall_line_non_verbose_excludes_measured_values() {
        let recall_min = 0.987_654;
        let line = render_recall_line(20, 20, recall_min, true, false);
        assert!(line.contains("k=20"));
        assert!(line.contains("queries=20"));
        assert!(line.contains("pass=true"));
        assert!(!line.contains(&format!("{recall_min:.6}")));
        assert!(!line.contains("recall_min"));
    }

    #[test]
    fn render_recall_line_verbose_includes_measured_values() {
        let recall_min = 0.987_654;
        let line = render_recall_line(20, 20, recall_min, true, true);
        assert!(line.contains(&format!("{recall_min:.6}")));
    }

    #[test]
    fn render_ab_line_non_verbose_excludes_measured_values() {
        let a_median = Duration::from_millis(5);
        let b_median = Duration::from_millis(6);
        let median_ratio = 0.8333;
        let line = render_ab_line(a_median, b_median, median_ratio, false);
        assert!(!line.contains(&format!("{a_median:?}")));
        assert!(!line.contains(&format!("{b_median:?}")));
        assert!(!line.contains(&format!("{median_ratio:.4}")));
        assert!(line.contains("available=true"));
        assert!(line.contains("not counted toward pass/fail"));
    }

    #[test]
    fn render_ab_line_verbose_includes_measured_values() {
        let a_median = Duration::from_millis(5);
        let b_median = Duration::from_millis(6);
        let median_ratio = 0.8333;
        let line = render_ab_line(a_median, b_median, median_ratio, true);
        assert!(line.contains(&format!("{a_median:?}")));
        assert!(line.contains(&format!("{b_median:?}")));
        assert!(line.contains(&format!("{median_ratio:.4}")));
    }
}

// --- EnvReport ---

#[test]
fn env_report_capture_does_not_panic_and_renders_logical_cpus_consistently() {
    let report = EnvReport::capture("scalar");
    // `EnvReport::capture` は `std::thread::available_parallelism()` 失敗時に
    // `logical_cpus == 0`（fail-closed で「不明」を表す）を返す契約なので、
    // `>= 1` を固定で要求しない。ここでは capture がパニックしないことと、
    // `Display` が契約どおり `0` を "unavailable" として描画する（それ以外は
    // 実際の論理コア数を描画する）ことを検証する。
    let rendered = format!("{report}");
    assert!(!rendered.is_empty());
    assert!(rendered.contains("os="));
    if report.logical_cpus == 0 {
        assert!(rendered.contains("logical_cpus=unavailable"));
    } else {
        assert!(rendered.contains(&format!("logical_cpus={}", report.logical_cpus)));
    }
}
