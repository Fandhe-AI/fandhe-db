//! 拡張クエリプロトコルの `$n` パラメータ束縛（Issue #935・WIRE-12・TASK-217）の
//! engine 層結合テスト。`EngineCore::parse_sql_prepared`（Parse）・
//! `EngineCore::bind_prepared`（Bind）・`EngineCore::describe_prepared_in_session`
//! （Describe）が、SQL テキスト経由の簡易クエリ（`parse_sql`／`execute_sql_in_session`／
//! `describe_parsed_in_session`）と完全に同一の判定順序・実行結果・RLS 境界を
//! 持つことを固定する（第 2 の実行器を作らない設計の証明。詳細・受理位置・
//! スコープ外は `docs/design/wire-extended-query-param-binding.md` 参照）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn new_core_with_documents_table(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("body", ColumnType::Text, false),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

/// ENUM 列（`mood`）を持つ `documents` テーブル（PR #1012 Cursor Bugbot 指摘の
/// 回帰専用。`tests/enum_column.rs` と同じ流儀で名前付き型を先に登録する）。
fn new_core_with_enum_documents_table(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    let mood = storage
        .create_enum_type(
            "mood",
            vec![
                "happy".to_string(),
                "sad".to_string(),
                "neutral".to_string(),
            ],
        )
        .expect("create enum type");
    storage
        .create_table(&TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("body", ColumnType::Text, false),
                ColumnDef::new("lang", ColumnType::Text, false),
                ColumnDef::new("mood", ColumnType::Enum(mood), true),
            ],
        ))
        .expect("create table");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn some(s: &str) -> Option<Vec<u8>> {
    Some(s.as_bytes().to_vec())
}

// --- リテラル同値性（束縛結果が「同じ値を正しくエスケープしたリテラルで
// 書いた SQL」の `parse_sql` 結果と完全に一致する） --------------------------

#[test]
fn bind_prepared_where_equality_matches_literal_form() {
    let path = unique_db_path("prepared-where-equality-parity");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);

    let prepared = core
        .parse_sql_prepared("SELECT id, body FROM documents WHERE lang = $1 LIMIT 5")
        .expect("parse_sql_prepared should succeed");
    assert_eq!(prepared.param_count(), 1);

    let bound = core
        .bind_prepared(&prepared, &[some("ja")])
        .expect("bind_prepared should succeed");
    let literal = core
        .parse_sql("SELECT id, body FROM documents WHERE lang = 'ja' LIMIT 5")
        .expect("parse_sql should succeed");
    assert_eq!(bound, literal);
}

/// PR #1217 レビュー指摘: portal の保持量は同一 `$n` の出現回数を掛けた
/// 展開後の量で数える。
#[test]
fn expanded_bound_bytes_counts_each_placeholder_occurrence() {
    let path = unique_db_path("prepared-expanded-bytes");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);

    let prepared = core
        .parse_sql_prepared("SELECT id FROM documents WHERE lang = $1 AND body = $1 LIMIT 5")
        .expect("parse_sql_prepared should succeed");
    assert_eq!(prepared.expanded_bound_bytes(&[some("abcd")]), 8);
}

#[test]
fn bind_prepared_vector_distance_matches_literal_form() {
    let path = unique_db_path("prepared-vector-distance-parity");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);

    let prepared = core
        .parse_sql_prepared("SELECT id FROM documents ORDER BY embedding <=> $1 LIMIT 5")
        .expect("parse_sql_prepared should succeed");
    let bound = core
        .bind_prepared(&prepared, &[some("[0.1,0.2,0.3]")])
        .expect("bind_prepared should succeed");
    let literal = core
        .parse_sql("SELECT id FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5")
        .expect("parse_sql should succeed");
    assert_eq!(bound, literal);
}

#[test]
fn bind_prepared_insert_values_matches_literal_form() {
    let path = unique_db_path("prepared-insert-values-parity");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);

    let prepared = core
        .parse_sql_prepared(
            "INSERT INTO documents (id, embedding, body, lang) VALUES (1, $1, $2, $3) USING OPERATION_ID $4",
        )
        .expect("parse_sql_prepared should succeed");
    assert_eq!(prepared.param_count(), 4);
    let bound = core
        .bind_prepared(
            &prepared,
            &[
                some("[0.1,0.2,0.3]"),
                some("hello"),
                some("en"),
                some("op-0001"),
            ],
        )
        .expect("bind_prepared should succeed");
    let literal = core
        .parse_sql(
            "INSERT INTO documents (id, embedding, body, lang) VALUES (1, '[0.1,0.2,0.3]', 'hello', 'en') USING OPERATION_ID 'op-0001'",
        )
        .expect("parse_sql should succeed");
    assert_eq!(bound, literal);
}

#[test]
fn bind_prepared_using_plan_matches_literal_form() {
    let path = unique_db_path("prepared-using-plan-parity");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);

    let prepared = core
        .parse_sql_prepared("SELECT id FROM documents USING PLAN($1) LIMIT 5")
        .expect("parse_sql_prepared should succeed");
    let bound = core
        .bind_prepared(&prepared, &[some("find the handler")])
        .expect("bind_prepared should succeed");
    let literal = core
        .parse_sql("SELECT id FROM documents USING PLAN('find the handler') LIMIT 5")
        .expect("parse_sql should succeed");
    assert_eq!(bound, literal);
}

#[test]
fn bind_prepared_using_operation_id_matches_literal_form_for_truncate() {
    let path = unique_db_path("prepared-truncate-operation-id-parity");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);

    let prepared = core
        .parse_sql_prepared("TRUNCATE TABLE documents USING OPERATION_ID $1")
        .expect("parse_sql_prepared should succeed");
    let bound = core
        .bind_prepared(&prepared, &[some("op-trunc-0001")])
        .expect("bind_prepared should succeed");
    let literal = core
        .parse_sql("TRUNCATE TABLE documents USING OPERATION_ID 'op-trunc-0001'")
        .expect("parse_sql should succeed");
    assert_eq!(bound, literal);
}

// --- インジェクション耐性 ----------------------------------------------------

#[test]
fn bind_prepared_treats_sql_metacharacters_as_a_single_opaque_literal() {
    let path = unique_db_path("prepared-injection-resistance");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    // 挿入した自分自身の行を読み戻すため Private も許可する（RLS-11・TASK-195。
    // `PolicyContext::new` の既定 `Public` のみでは自テナントの書き込みも
    // 読み戻せない）。
    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let mut session = SessionState::default();

    core.execute_sql_in_session(
        &ctx,
        &mut session,
        "INSERT INTO documents (id, embedding, body, lang) VALUES (1, '[0.1,0.2,0.3]', 'hello', 'ja') USING OPERATION_ID 'seed-0001'",
    )
    .expect("seed insert should succeed");
    core.execute_sql_in_session(
        &ctx,
        &mut session,
        "INSERT INTO documents (id, embedding, body, lang) VALUES (2, '[0.4,0.5,0.6]', 'world', 'en') USING OPERATION_ID 'seed-0002'",
    )
    .expect("seed insert should succeed");

    let malicious = "ja' OR '1'='1";
    let prepared = core
        .parse_sql_prepared("SELECT id FROM documents WHERE lang = $1 LIMIT 5")
        .expect("parse_sql_prepared should succeed");
    let bound = core
        .bind_prepared(&prepared, &[some(malicious)])
        .expect("bind_prepared should succeed");
    let outcome = core
        .execute_parsed_in_session(&ctx, &mut session, &bound)
        .expect("execute should succeed");
    match outcome {
        SqlOutcome::Query(result) => {
            // 値は 1 個の文字列リテラルとしてのみ扱われるため、`lang = "ja' OR
            // '1'='1"` に完全一致する行は無く、文の形も変わらない
            // （両方の行を返す `OR` インジェクションが成立していないことの証明）。
            assert_eq!(result.rows.len(), 0);
        }
        other => panic!("expected SqlOutcome::Query, got {other:?}"),
    }

    // 改行・カンマ・`--`・別の `$n` らしき文字列を含む値でも、単一リテラルの
    // ままであることを重ねて確認する。
    let tricky_values = [
        "'); DROP TABLE documents; --",
        "a\nb",
        "$2, $3",
        "line1\r\nline2",
    ];
    for value in tricky_values {
        let prepared = core
            .parse_sql_prepared("SELECT id FROM documents WHERE lang = $1 LIMIT 5")
            .expect("parse_sql_prepared should succeed");
        let bound = core
            .bind_prepared(&prepared, &[some(value)])
            .expect("bind_prepared should succeed");
        let outcome = core
            .execute_parsed_in_session(&ctx, &mut session, &bound)
            .expect("execute should succeed for opaque literal value");
        match outcome {
            SqlOutcome::Query(result) => assert_eq!(result.rows.len(), 0),
            other => panic!("expected SqlOutcome::Query, got {other:?}"),
        }
    }

    // テーブルは無傷（`DROP TABLE` が実行されていれば以降の SELECT が
    // `42P01` になる）。
    let sanity = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT id FROM documents WHERE lang = 'ja' LIMIT 5",
        )
        .expect("table must still exist and be queryable");
    match sanity {
        SqlOutcome::Query(result) => assert_eq!(result.rows.len(), 1),
        other => panic!("expected SqlOutcome::Query, got {other:?}"),
    }
}

// --- 許可位置外の拒否（`42601`） ---------------------------------------------

#[test]
fn parse_sql_prepared_rejects_limit_placeholder() {
    let path = unique_db_path("prepared-rejects-limit");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);

    let err = core
        .parse_sql_prepared(
            "SELECT id FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT $1",
        )
        .expect_err("LIMIT $n must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn parse_sql_prepared_rejects_using_mode_placeholder() {
    let path = unique_db_path("prepared-rejects-using-mode");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);

    let err = core
        .parse_sql_prepared(
            "SELECT id FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5 USING MODE $1",
        )
        .expect_err("USING MODE $n must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn parse_sql_prepared_rejects_set_search_mode_placeholder() {
    let path = unique_db_path("prepared-rejects-set-search-mode");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);

    let err = core
        .parse_sql_prepared("SET search_mode = $1")
        .expect_err("SET search_mode = $n must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn parse_sql_prepared_rejects_hybrid_function_argument_placeholder() {
    let path = unique_db_path("prepared-rejects-hybrid-arg");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);

    let err = core
        .parse_sql_prepared(
            "SELECT id FROM documents ORDER BY HYBRID_RRF(embedding, $1, body, 'q') LIMIT 5",
        )
        .expect_err("hybrid function argument $n must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

// PR #1012 Cursor Bugbot 指摘の回帰: `GROUP BY` 以降の `<col> = $n` は WHERE
// 等価述語の許可位置（`sql::params` パターン 4）に含まれず、他の範囲外
// パラメータと同じ `42601` で拒否されなければならない。
#[test]
fn parse_sql_prepared_rejects_placeholder_after_group_by() {
    let path = unique_db_path("prepared-rejects-after-group-by");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);

    let err = core
        .parse_sql_prepared(
            "SELECT lang, COUNT(*) AS n FROM documents WHERE lang = 'ja' GROUP BY lang = $1",
        )
        .expect_err("$n after GROUP BY must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

// --- 字句・パラメータ番号上限 -------------------------------------------------

#[test]
fn parse_sql_prepared_rejects_zero_and_leading_zero_and_ambiguous_suffix() {
    let path = unique_db_path("prepared-rejects-malformed-lexemes");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);

    for sql in [
        "SELECT id FROM documents WHERE lang = $0 LIMIT 5",
        "SELECT id FROM documents WHERE lang = $01 LIMIT 5",
        "SELECT id FROM documents WHERE lang = $1a LIMIT 5",
    ] {
        let err = core
            .parse_sql_prepared(sql)
            .expect_err("malformed placeholder must be rejected");
        assert_eq!(err.wire_code(), "42601", "sql = {sql}");
    }
}

#[test]
fn parse_sql_prepared_rejects_parameter_number_over_max() {
    let path = unique_db_path("prepared-rejects-over-max-param");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);

    let err = core
        .parse_sql_prepared("SELECT id FROM documents WHERE lang = $65 LIMIT 5")
        .expect_err("parameter number over MAX_PARAMS must be rejected");
    assert_eq!(err.wire_code(), "54000");
}

// --- 値検証（NULL は `22000`・形式不正は `22P02`） -------------------------------------------------------

#[test]
fn bind_prepared_rejects_null_value() {
    let path = unique_db_path("prepared-rejects-null-value");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);

    let prepared = core
        .parse_sql_prepared("SELECT id FROM documents WHERE lang = $1 LIMIT 5")
        .expect("parse_sql_prepared should succeed");
    let err = core
        .bind_prepared(&prepared, &[None])
        .expect_err("NULL bind value must be rejected");
    assert_eq!(err.wire_code(), "22000");
}

#[test]
fn bind_prepared_rejects_non_utf8_value() {
    let path = unique_db_path("prepared-rejects-non-utf8-value");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);

    let prepared = core
        .parse_sql_prepared("SELECT id FROM documents WHERE lang = $1 LIMIT 5")
        .expect("parse_sql_prepared should succeed");
    let err = core
        .bind_prepared(&prepared, &[Some(vec![0xff, 0xfe])])
        .expect_err("non-UTF-8 bind value must be rejected");
    // WIRE-12: 値の形式不正は 22P02。
    assert_eq!(err.wire_code(), "22P02");
}

#[test]
fn bind_prepared_rejects_missing_operation_id_via_empty_string_value() {
    let path = unique_db_path("prepared-rejects-empty-operation-id");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);

    let prepared = core
        .parse_sql_prepared("TRUNCATE TABLE documents USING OPERATION_ID $1")
        .expect("parse_sql_prepared should succeed");
    let err = core
        .bind_prepared(&prepared, &[some("")])
        .expect_err("empty operation_id value must be rejected");
    assert_eq!(err.wire_code(), "23502");
}

// PR #1012 codex 指摘の確認: `docs/design/wire-extended-query-param-binding.md`
// 「NULL パラメータ値」節が案内する「`$n` を使わず SQL テキストへ
// `USING OPERATION_ID NULL` を直接書く」形は、構文としては受理され（`42601`
// にはならない）、句の省略と同じ扱い（台帳あり構成では必須化違反 `23502`）に
// なることを固定する（RECOVER-1・TASK-92 の既存契約をそのまま継承する）。
#[test]
fn parse_sql_prepared_treats_literal_null_operation_id_as_omitted_clause() {
    let path = unique_db_path("prepared-literal-null-operation-id");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);

    for sql in [
        "TRUNCATE TABLE documents USING OPERATION_ID NULL",
        "TRUNCATE TABLE documents",
    ] {
        let err = core
            .parse_sql_prepared(sql)
            .expect_err("omitted operation_id must be rejected in ledgered mode");
        assert_eq!(err.wire_code(), "23502", "sql = {sql}");
    }
}

#[test]
fn bind_prepared_rejects_value_count_mismatch() {
    let path = unique_db_path("prepared-rejects-value-count-mismatch");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);

    let prepared = core
        .parse_sql_prepared("SELECT id FROM documents WHERE lang = $1 LIMIT 5")
        .expect("parse_sql_prepared should succeed");
    let err = core
        .bind_prepared(&prepared, &[])
        .expect_err("value count mismatch must be rejected");
    assert_eq!(err.wire_code(), "22000");

    let err = core
        .bind_prepared(&prepared, &[some("ja"), some("en")])
        .expect_err("value count mismatch must be rejected");
    assert_eq!(err.wire_code(), "22000");
}

// --- 簡易クエリ経路は引き続き `$n` を拒否する --------------------------------

#[test]
fn execute_sql_in_session_still_rejects_dollar_placeholder_in_simple_query() {
    let path = unique_db_path("prepared-simple-query-still-rejects-dollar");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();

    let err = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT id FROM documents WHERE lang = $1",
        )
        .expect_err("simple query must still reject $n");
    assert_eq!(err.wire_code(), "42601");
}

// --- RLS 暗黙適用（第 2 の実行器を作らない設計の証明） ------------------------

#[test]
fn bind_prepared_search_applies_rls_implicitly_across_tenants() {
    let path = unique_db_path("prepared-rls-implicit");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    // ctx_a は自テナントの書き込みを読み戻すため Private も許可する
    // （RLS-11・TASK-195）。ctx_b は既定 Public のみで十分（他テナント境界の
    // 確認が目的）。
    let ctx_a =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let ctx_b = PolicyContext::new("tenant-b").expect("valid tenant");
    let mut session = SessionState::default();

    core.execute_sql_in_session(
        &ctx_a,
        &mut session,
        "INSERT INTO documents (id, embedding, body, lang) VALUES (1, '[0.1,0.2,0.3]', 'secret-a', 'ja') USING OPERATION_ID 'rls-0001'",
    )
    .expect("tenant-a insert should succeed");

    let prepared = core
        .parse_sql_prepared("SELECT id FROM documents WHERE lang = $1 LIMIT 5")
        .expect("parse_sql_prepared should succeed");
    let bound_for_b = core
        .bind_prepared(&prepared, &[some("ja")])
        .expect("bind_prepared should succeed");
    let outcome = core
        .execute_parsed_in_session(&ctx_b, &mut session, &bound_for_b)
        .expect("execute should succeed");
    match outcome {
        SqlOutcome::Query(result) => {
            assert_eq!(result.rows.len(), 0, "tenant-b must not see tenant-a's row");
        }
        other => panic!("expected SqlOutcome::Query, got {other:?}"),
    }

    let bound_for_a = core
        .bind_prepared(&prepared, &[some("ja")])
        .expect("bind_prepared should succeed");
    let outcome = core
        .execute_parsed_in_session(&ctx_a, &mut session, &bound_for_a)
        .expect("execute should succeed");
    match outcome {
        SqlOutcome::Query(result) => assert_eq!(result.rows.len(), 1),
        other => panic!("expected SqlOutcome::Query, got {other:?}"),
    }
}

// --- Describe（Bind 前・値未確定でも呼べる） ----------------------------------

#[test]
fn describe_prepared_matches_literal_form_describe() {
    let path = unique_db_path("prepared-describe-parity");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let session = SessionState::default();

    let prepared = core
        .parse_sql_prepared("SELECT id, body FROM documents WHERE lang = $1 LIMIT 5")
        .expect("parse_sql_prepared should succeed");
    let described = core
        .describe_prepared_in_session(&session, &prepared)
        .expect("describe_prepared_in_session should succeed");

    let literal = core
        .parse_sql("SELECT id, body FROM documents WHERE lang = 'ja' LIMIT 5")
        .expect("parse_sql should succeed");
    let literal_described = core
        .describe_parsed_in_session(&session, &literal)
        .expect("describe_parsed_in_session should succeed");

    assert_eq!(described, literal_described);
}

// PR #1012 Cursor Bugbot 指摘の回帰: `ORDER BY <vec列> <=> $n` を含む文の
// Describe（Bind 前・値未確定）は、`parse_sql_prepared` が構造検証専用の
// 固定ダミー値へ全 `$n` を置換するため、ダミー値がベクトルとして不正でも
// 結果列だけは導出できなければならない（`sql::parser::
// bind_projection_for_describe` がランキングのリテラル実パースを省略する
// ことで実現）。結果列は投影列にのみ依存するため、実リテラルで Describe
// した場合と完全に一致する契約を固定する。
#[test]
fn describe_prepared_vector_distance_order_by_succeeds_before_bind() {
    let path = unique_db_path("prepared-describe-vector-distance");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let session = SessionState::default();

    let prepared = core
        .parse_sql_prepared("SELECT id FROM documents ORDER BY embedding <=> $1 LIMIT 5")
        .expect("parse_sql_prepared should succeed");
    let described = core
        .describe_prepared_in_session(&session, &prepared)
        .expect("describe_prepared_in_session should succeed for vector distance ORDER BY");

    let literal = core
        .parse_sql("SELECT id FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5")
        .expect("parse_sql should succeed");
    let literal_described = core
        .describe_parsed_in_session(&session, &literal)
        .expect("describe_parsed_in_session should succeed");

    assert_eq!(described, literal_described);
}

// PR #1012 codex/Cursor Bugbot 指摘の回帰: `describe_prepared_in_session` は
// 文中の `$n` の位置に関係なく検証省略していたわけではなく、`ORDER BY` の
// ベクトルリテラルが実際に `$n` に由来するダミー値の場合に限って実パースを
// 省略しなければならない。`$n` を含まない `ORDER BY` の不正な実ベクトル
// リテラル（次元不一致）は、通常の `describe_parsed_in_session` と同じく
// Describe 時点（Bind 前）で `22000` として検出されなければならない
// （以前は `skip_vector_literal_validation` が常に `true` だったため、この
// 不正値の検出が Execute まで遅延していた）。
#[test]
fn describe_prepared_rejects_real_invalid_vector_literal_without_dollar_param() {
    let path = unique_db_path("prepared-describe-real-invalid-vector-literal");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let session = SessionState::default();

    // `$n` を一切含まない文（`documents.embedding` の次元と一致しない不正な
    // 実リテラル）。
    let prepared = core
        .parse_sql_prepared("SELECT id FROM documents ORDER BY embedding <=> '[0.1,0.2]' LIMIT 5")
        .expect("parse_sql_prepared should succeed (structural validation only)");

    let err = core
        .describe_prepared_in_session(&session, &prepared)
        .expect_err(
            "describe_prepared_in_session must reject an invalid real vector literal, \
             not defer detection to Execute",
        );
    assert_eq!(err.wire_code(), "22000");

    // 通常の Describe（SQL テキスト直接）と同一のエラーになることを固定する。
    let literal = core
        .parse_sql("SELECT id FROM documents ORDER BY embedding <=> '[0.1,0.2]' LIMIT 5")
        .expect("parse_sql should succeed");
    let literal_err = core
        .describe_parsed_in_session(&session, &literal)
        .expect_err("describe_parsed_in_session should reject the same invalid literal");
    assert_eq!(err.wire_code(), literal_err.wire_code());
}

// 同上（PR #1012 指摘の複合形）: `WHERE` 節の `$n` と `ORDER BY` の不正な
// 実ベクトルリテラルが同一文に共存する場合でも、`$n` の存在自体が Describe
// 全体の検証を無効化してはならない。
#[test]
fn describe_prepared_rejects_real_invalid_vector_literal_alongside_unrelated_dollar_param() {
    let path = unique_db_path("prepared-describe-mixed-param-and-invalid-literal");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let session = SessionState::default();

    let prepared = core
        .parse_sql_prepared(
            "SELECT id FROM documents WHERE lang = $1 ORDER BY embedding <=> '[0.1,0.2]' LIMIT 5",
        )
        .expect("parse_sql_prepared should succeed (structural validation only)");

    let err = core
        .describe_prepared_in_session(&session, &prepared)
        .expect_err(
            "an unrelated $n in WHERE must not suppress ORDER BY vector literal validation",
        );
    assert_eq!(err.wire_code(), "22000");
}

// --- ENUM 列の WHERE 等価述語（Issue #935 PR #1012 Cursor Bugbot 指摘の回帰）---
//
// `describe_prepared_in_session` は `dummy_parsed` 上で `bind_where_predicates`
// を走らせるため、ENUM 列の WHERE 等価 `$n` では固定ダミー文字列 "0" が
// 語彙外ラベルとして扱われ、Bind 前の Describe が常に `22P02` で失敗して
// いた。ダミー値へ置換された位置に限り語彙照合を省略し、実リテラル・Bind 後の
// 不正値検出はいずれも従来どおり機能することを固定する。

#[test]
fn describe_prepared_enum_where_equality_with_dollar_param_succeeds_before_bind() {
    let path = unique_db_path("prepared-describe-enum-dollar-param");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_enum_documents_table(&path);
    let session = SessionState::default();

    let prepared = core
        .parse_sql_prepared("SELECT id, mood FROM documents WHERE mood = $1 LIMIT 5")
        .expect("parse_sql_prepared should succeed (structural validation only)");

    let described = core
        .describe_prepared_in_session(&session, &prepared)
        .expect(
            "describe_prepared_in_session must not reject a dummy-substituted ENUM \
             equality value before Bind (PR #1012 Cursor Bugbot regression)",
        );

    // 通常の Describe（実リテラル。語彙に含まれる値）と同一の結果列になることを
    // 固定する（列の形はどの値を束縛しても変わらない契約）。
    let literal = core
        .parse_sql("SELECT id, mood FROM documents WHERE mood = 'happy' LIMIT 5")
        .expect("parse_sql should succeed");
    let literal_described = core
        .describe_parsed_in_session(&session, &literal)
        .expect("describe_parsed_in_session should succeed for a valid enum literal");
    assert_eq!(described, literal_described);
}

#[test]
fn bind_prepared_enum_where_equality_rejects_invalid_label_after_bind() {
    let path = unique_db_path("prepared-bind-enum-invalid-label");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_enum_documents_table(&path);
    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public]).expect("valid tenant");
    let mut session = SessionState::default();

    let prepared = core
        .parse_sql_prepared("SELECT id, mood FROM documents WHERE mood = $1 LIMIT 5")
        .expect("parse_sql_prepared should succeed");

    // Describe（Bind 前）は語彙照合をダミー値に対して省略するため成功する。
    core.describe_prepared_in_session(&session, &prepared)
        .expect("describe before bind must succeed regardless of the eventual bound value");

    // 語彙外ラベルを Bind すると Execute で `22P02` として検出される
    // （Describe をすり抜けたまま黙って通ってはならない）。
    let bound = core
        .bind_prepared(&prepared, &[some("not-a-real-mood")])
        .expect("bind_prepared should succeed (structural bind only)");
    let err = core
        .execute_parsed_in_session(&ctx, &mut session, &bound)
        .expect_err("executing an invalid ENUM label must fail");
    assert_eq!(err.wire_code(), "22P02");
}

// PR #1012 レビュー指摘の複合形: `$n` を含まない ENUM 等価述語（実リテラルが
// 語彙外）は、通常の Describe と同じく Bind 前の Describe でも必ず `22P02` に
// なる（ダミー値専用の縮退が実リテラルにまで及んではならない）。
#[test]
fn describe_prepared_rejects_real_invalid_enum_label_without_dollar_param() {
    let path = unique_db_path("prepared-describe-real-invalid-enum-label");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_enum_documents_table(&path);
    let session = SessionState::default();

    let prepared = core
        .parse_sql_prepared("SELECT id FROM documents WHERE mood = 'not-a-real-mood' LIMIT 5")
        .expect("parse_sql_prepared should succeed (structural validation only)");

    let err = core
        .describe_prepared_in_session(&session, &prepared)
        .expect_err(
            "describe_prepared_in_session must reject an invalid real enum label, \
             not defer detection to Execute",
        );
    assert_eq!(err.wire_code(), "22P02");

    let literal = core
        .parse_sql("SELECT id FROM documents WHERE mood = 'not-a-real-mood' LIMIT 5")
        .expect("parse_sql should succeed");
    let literal_err = core
        .describe_parsed_in_session(&session, &literal)
        .expect_err("describe_parsed_in_session should reject the same invalid label");
    assert_eq!(err.wire_code(), literal_err.wire_code());
}

// 同上（複合形）: `$n` 由来のダミー等価述語と、実リテラルの語彙外 ENUM 等価
// 述語が同一文に共存する場合でも、`$n` の存在自体が Describe 全体の検証を
// 無効化してはならない。
#[test]
fn describe_prepared_rejects_real_invalid_enum_label_alongside_unrelated_dollar_param() {
    let path = unique_db_path("prepared-describe-enum-mixed-param-and-invalid-label");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_enum_documents_table(&path);
    let session = SessionState::default();

    let prepared = core
        .parse_sql_prepared(
            "SELECT id FROM documents WHERE lang = $1 AND mood = 'not-a-real-mood' LIMIT 5",
        )
        .expect("parse_sql_prepared should succeed (structural validation only)");

    let err = core
        .describe_prepared_in_session(&session, &prepared)
        .expect_err("an unrelated $n in WHERE must not suppress the real ENUM label validation");
    assert_eq!(err.wire_code(), "22P02");
}

// PR #1012 レビュー指摘対応: 同じ根本原因（`bind_where_predicates` の値検証を
// Prepared Describe が無条件にスキップし得る）は集計 SELECT・広域取得
// （scan）経由の Describe でも同様に起こり得るため、`bind_aggregate`・
// `bind_scan` へも同じ `dummy_equality_flags` を結線してある（`sql::parser`
// のドキュメント参照）。ここでは集計経路（`COUNT(*) ... GROUP BY` なし）の
// Describe が ENUM の `$n` 等価述語で同様に成功することを固定する。
#[test]
fn describe_prepared_enum_where_equality_succeeds_for_aggregate_before_bind() {
    let path = unique_db_path("prepared-describe-enum-aggregate-dollar-param");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_enum_documents_table(&path);
    let session = SessionState::default();

    let prepared = core
        .parse_sql_prepared("SELECT COUNT(*) AS n FROM documents WHERE mood = $1")
        .expect("parse_sql_prepared should succeed (structural validation only)");

    core.describe_prepared_in_session(&session, &prepared)
        .expect(
            "aggregate Describe must not reject a dummy-substituted ENUM equality \
             value before Bind",
        );
}

// --- 副作用ゼロ（Parse／Describe は行・台帳・世代に触れない） -----------------

#[test]
fn parse_and_describe_prepared_insert_have_no_side_effects() {
    let path = unique_db_path("prepared-insert-no-side-effects");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let session = SessionState::default();

    let prepared = core
        .parse_sql_prepared(
            "INSERT INTO documents (id, embedding, body, lang) VALUES (1, $1, $2, $3) USING OPERATION_ID $4",
        )
        .expect("parse_sql_prepared should succeed");
    core.describe_prepared_in_session(&session, &prepared)
        .expect("describe_prepared_in_session should succeed");

    let read_ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let result = core
        .execute_sql(
            &read_ctx,
            "SELECT id FROM documents WHERE lang = 'en' LIMIT 5",
        )
        .expect("select should succeed");
    assert_eq!(
        result.rows.len(),
        0,
        "Parse/Describe must not insert any row"
    );
    let _ = ctx; // ctx は本テストでは未使用（read_ctx のみ使う）だが、
                 // 意図を明示するために保持する。
}

// PR #1012 codex-review P1 指摘の回帰: 公開 API `sql::parser::bind_scan`／
// `bind_aggregate` は従来どおりの 3 引数シグネチャを維持し、検証省略フラグを
// 受け取らない（クレート外から ENUM ラベル検証を省略させる経路を作らない）。
// `$n` に由来しない語彙外 ENUM ラベルは、公開束縛 API・通常 Describe・
// Prepared Describe のいずれでも従来どおり `22P02` で拒否される。
#[test]
fn public_bind_and_describe_reject_invalid_enum_label_without_dollar_param() {
    use engine::sql::allowlist::{validate_sql, Statement};
    use engine::sql::parser::{bind_aggregate, bind_scan};
    use engine::sql::udf_call::UdfRegistry;

    let scan_invalid = "SELECT id FROM documents WHERE mood = 'not-a-real-mood' LIMIT 5";
    let scan_valid = "SELECT id FROM documents WHERE mood = 'happy' LIMIT 5";
    let agg_invalid = "SELECT COUNT(*) FROM documents WHERE mood = 'not-a-real-mood'";
    let agg_valid = "SELECT COUNT(*) FROM documents WHERE mood = 'happy'";

    // 公開束縛 API: `EngineCore` と同じ手順で作った ENUM 列付きテーブルの
    // `Storage` を許可リスト照会・スキーマ取得に直接使う。
    {
        let path = unique_db_path("public-bind-rejects-invalid-enum-label");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let mood = storage
            .create_enum_type("mood", vec!["happy".to_string(), "sad".to_string()])
            .expect("create enum type");
        storage
            .create_table(&TableSchema::new(
                "documents",
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(3), false),
                    ColumnDef::new("mood", ColumnType::Enum(mood), true),
                ],
            ))
            .expect("create table");
        let schema = storage
            .get_table_schema("documents")
            .expect("get_table_schema");
        let udfs = UdfRegistry::default();

        let bind_scan_sql = |sql: &str| match validate_sql(sql, &storage).expect("validate_sql") {
            Statement::Scan(v) => bind_scan(&v, &schema, &udfs).map(|_| ()),
            other => panic!("expected Statement::Scan, got {other:?}"),
        };
        let bind_agg_sql = |sql: &str| match validate_sql(sql, &storage).expect("validate_sql") {
            Statement::Aggregate(v) => bind_aggregate(&v, &schema, &udfs).map(|_| ()),
            other => panic!("expected Statement::Aggregate, got {other:?}"),
        };
        assert_eq!(
            bind_scan_sql(scan_invalid).unwrap_err().wire_code(),
            "22P02"
        );
        assert_eq!(bind_agg_sql(agg_invalid).unwrap_err().wire_code(), "22P02");
        bind_scan_sql(scan_valid).expect("valid label must bind (scan)");
        bind_agg_sql(agg_valid).expect("valid label must bind (aggregate)");
    }

    // Describe（通常・Prepared）: `$n` を含まない文の実リテラルは必ず検証される。
    let path = unique_db_path("public-describe-rejects-invalid-enum-label");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_enum_documents_table(&path);
    let session = SessionState::default();

    for sql in [scan_invalid, agg_invalid] {
        let parsed = core.parse_sql(sql).expect("parse_sql should succeed");
        let err = core
            .describe_parsed_in_session(&session, &parsed)
            .expect_err("describe must reject an invalid real enum label");
        assert_eq!(err.wire_code(), "22P02", "sql = {sql}");

        let prepared = core
            .parse_sql_prepared(sql)
            .expect("parse_sql_prepared should succeed");
        let err = core
            .describe_prepared_in_session(&session, &prepared)
            .expect_err("prepared describe must reject an invalid real enum label");
        assert_eq!(err.wire_code(), "22P02", "sql = {sql}");
    }
    for sql in [scan_valid, agg_valid] {
        let parsed = core.parse_sql(sql).expect("parse_sql should succeed");
        core.describe_parsed_in_session(&session, &parsed)
            .expect("describe must accept a valid enum label");
    }
}

// --- 型推論・0 パラメータ文アクセサ（Issue #1171・WIRE-12） ----------------------

#[test]
fn param_types_are_inferred_per_position() {
    use engine::sql::exec::ColumnMeta;
    use engine::sql::params::PreparedParamType;

    let path = unique_db_path("prepared-param-types");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);

    let types = |sql: &str| {
        core.parse_sql_prepared(sql)
            .expect("parse_sql_prepared should succeed")
            .param_types()
            .to_vec()
    };
    assert_eq!(
        types("SELECT id FROM documents ORDER BY embedding <=> $1 LIMIT 3"),
        vec![PreparedParamType::VectorText]
    );
    assert_eq!(
        types("SELECT id FROM documents WHERE body = $1 LIMIT 10"),
        vec![PreparedParamType::Column(ColumnMeta::Scalar {
            name: "body".to_string(),
            ty: ColumnType::Text,
        })]
    );
    assert_eq!(
        types("SELECT id FROM documents WHERE id = $1 LIMIT 10"),
        vec![PreparedParamType::Column(ColumnMeta::Id)]
    );
    // INSERT の行内 ordinal は明示列リストの列へ解決される。
    assert_eq!(
        types(
            "INSERT INTO documents (id, embedding, body, lang) VALUES (9, $1, $2, 'x') USING OPERATION_ID $3"
        ),
        vec![
            PreparedParamType::Column(ColumnMeta::Scalar {
                name: "embedding".to_string(),
                ty: ColumnType::Vector(3),
            }),
            PreparedParamType::Column(ColumnMeta::Scalar {
                name: "body".to_string(),
                ty: ColumnType::Text,
            }),
            PreparedParamType::Text,
        ]
    );
    // 未参照の番号は Text。
    assert_eq!(
        types("SELECT id FROM documents WHERE lang = $2 LIMIT 10"),
        vec![
            PreparedParamType::Text,
            PreparedParamType::Column(ColumnMeta::Scalar {
                name: "lang".to_string(),
                ty: ColumnType::Text,
            })
        ]
    );
}

#[test]
fn parsed_if_unparameterized_matches_parse_sql_only_without_params() {
    let path = unique_db_path("prepared-unparameterized-accessor");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);

    let sql = "SELECT id FROM documents WHERE lang = 'en' LIMIT 5";
    let prepared = core
        .parse_sql_prepared(sql)
        .expect("parse_sql_prepared should succeed");
    let expected = core.parse_sql(sql).expect("parse_sql should succeed");
    assert_eq!(prepared.param_count(), 0);
    assert_eq!(
        format!("{:?}", prepared.parsed_if_unparameterized()),
        format!("{:?}", Some(&expected))
    );

    let with_param = core
        .parse_sql_prepared("SELECT id FROM documents WHERE lang = $1 LIMIT 5")
        .expect("parse_sql_prepared should succeed");
    assert!(with_param.parsed_if_unparameterized().is_none());
}
// --- 型付き置換（Issue #1342・WIRE-12）: 行 id・INTEGER・BIGINT・BOOLEAN 列 -----

/// `id`・`n INTEGER`・`b BIGINT`・`flag BOOLEAN`・`mood ENUM`・`body TEXT` を持つ表。
fn new_core_with_typed_table(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    let mood = storage
        .create_enum_type("mood", vec!["happy".to_string(), "sad".to_string()])
        .expect("create enum type");
    storage
        .create_table(&TableSchema::new(
            "typed",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("body", ColumnType::Text, false),
                ColumnDef::new("n", ColumnType::Integer, true),
                ColumnDef::new("b", ColumnType::BigInt, true),
                ColumnDef::new("flag", ColumnType::Boolean, true),
                ColumnDef::new("mood", ColumnType::Enum(mood), true),
            ],
        ))
        .expect("create table");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn assert_bind_matches_literal(
    core: &EngineCore,
    template: &str,
    values: &[&str],
    literal_sql: &str,
) {
    let prepared = core
        .parse_sql_prepared(template)
        .expect("parse_sql_prepared should succeed");
    let vals: Vec<Option<Vec<u8>>> = values.iter().map(|v| some(v)).collect();
    let bound = core
        .bind_prepared(&prepared, &vals)
        .expect("bind_prepared should succeed");
    let literal = core
        .parse_sql(literal_sql)
        .expect("parse_sql should succeed");
    assert_eq!(bound, literal, "bound form must equal literal form");
    // Describe(S) も成功し、リテラル形と同じ列メタを返す。
    let session = SessionState::default();
    let described = core
        .describe_prepared_in_session(&session, &prepared)
        .expect("describe_prepared_in_session should succeed");
    let literal_described = core
        .describe_parsed_in_session(&session, &literal)
        .expect("describe_parsed_in_session should succeed");
    assert_eq!(described, literal_described);
}

#[test]
fn typed_bind_matches_literal_form_for_id_integer_bigint_boolean() {
    let path = unique_db_path("prepared-typed-parity");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_typed_table(&path);

    assert_bind_matches_literal(
        &core,
        "SELECT id, body FROM typed WHERE id = $1 LIMIT 5",
        &["5"],
        "SELECT id, body FROM typed WHERE id = 5 LIMIT 5",
    );
    assert_bind_matches_literal(
        &core,
        "SELECT id, n FROM typed WHERE n = $1 LIMIT 5",
        &["7"],
        "SELECT id, n FROM typed WHERE n = 7 LIMIT 5",
    );
    assert_bind_matches_literal(
        &core,
        "SELECT id, b FROM typed WHERE b = $1 LIMIT 5",
        &["9000000000"],
        "SELECT id, b FROM typed WHERE b = 9000000000 LIMIT 5",
    );
    for (text, lit) in [
        ("t", "true"),
        ("TRUE", "true"),
        ("1", "true"),
        (" yes ", "true"),
        ("f", "false"),
        ("off", "false"),
        ("0", "false"),
    ] {
        assert_bind_matches_literal(
            &core,
            "SELECT id, flag FROM typed WHERE flag = $1 LIMIT 5",
            &[text],
            &format!("SELECT id, flag FROM typed WHERE flag = {lit} LIMIT 5"),
        );
    }
    assert_bind_matches_literal(
        &core,
        "SELECT count(*) FROM typed WHERE n = $1",
        &["7"],
        "SELECT count(*) FROM typed WHERE n = 7",
    );
    assert_bind_matches_literal(
        &core,
        "DELETE FROM typed WHERE id = $1 USING OPERATION_ID $2",
        &["5", "op-1"],
        "DELETE FROM typed WHERE id = 5 USING OPERATION_ID 'op-1'",
    );
    assert_bind_matches_literal(
        &core,
        "UPDATE typed SET body = 'x' WHERE id = $1 USING OPERATION_ID $2",
        &["5", "op-2"],
        "UPDATE typed SET body = 'x' WHERE id = 5 USING OPERATION_ID 'op-2'",
    );
    assert_bind_matches_literal(
        &core,
        "INSERT INTO typed (id, embedding, body, n, flag) VALUES ($1, '[0.1,0.2,0.3]', 'hi', $2, $3) USING OPERATION_ID $4",
        &["1", "-5", "t", "op-3"],
        "INSERT INTO typed (id, embedding, body, n, flag) VALUES (1, '[0.1,0.2,0.3]', 'hi', -5, true) USING OPERATION_ID 'op-3'",
    );
}

#[test]
fn typed_bind_rejects_malformed_values_with_22p02_and_no_value_echo() {
    let path = unique_db_path("prepared-typed-22p02");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_typed_table(&path);

    let int_stmt = core
        .parse_sql_prepared("SELECT id FROM typed WHERE n = $1 LIMIT 5")
        .expect("parse");
    for bad in [
        "abc",
        "5.0",
        "+5",
        " 5",
        "",
        "-",
        "1e3",
        "1 OR 1=1",
        "5; DROP TABLE typed",
    ] {
        let err = core
            .bind_prepared(&int_stmt, &[some(bad)])
            .expect_err("malformed integer must be rejected");
        assert_eq!(err.wire_code(), "22P02", "value {bad:?}");
        if !bad.is_empty() {
            assert!(!err.to_string().contains(bad), "must not echo {bad:?}");
        }
    }
    let bool_stmt = core
        .parse_sql_prepared("SELECT id FROM typed WHERE flag = $1 LIMIT 5")
        .expect("parse");
    for bad in ["maybe", "o", "", "true OR", "true; DROP TABLE typed"] {
        let err = core
            .bind_prepared(&bool_stmt, &[some(bad)])
            .expect_err("malformed boolean must be rejected");
        assert_eq!(err.wire_code(), "22P02", "value {bad:?}");
    }
    // テーブルは残っている。
    core.parse_sql("SELECT id FROM typed LIMIT 1")
        .expect("table must remain");
}

#[test]
fn typed_bind_keeps_literal_form_limits() {
    let path = unique_db_path("prepared-typed-literal-limits");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_typed_table(&path);
    // 負値 WHERE はリテラル形でも同じエラー（式項の単項マイナス未対応。既存制約）。
    let prepared = core
        .parse_sql_prepared("SELECT id FROM typed WHERE n = $1 LIMIT 5")
        .expect("parse");
    for (value, literal_sql) in [
        ("-5", "SELECT id FROM typed WHERE n = -5 LIMIT 5"),
        (
            "2147483648",
            "SELECT id FROM typed WHERE n = 2147483648 LIMIT 5",
        ),
    ] {
        let bound = core.bind_prepared(&prepared, &[some(value)]);
        let literal = core.parse_sql(literal_sql);
        assert_eq!(
            bound.as_ref().map_err(|e| e.wire_code()),
            literal.as_ref().map_err(|e| e.wire_code()),
            "value {value}"
        );
    }
}

#[test]
fn typed_params_do_not_shift_enum_dummy_flags() {
    let path = unique_db_path("prepared-typed-ordinal");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_typed_table(&path);
    let session = SessionState::default();

    // 実リテラルの不正 ENUM ラベルは、型付き `$n` が前後にあっても Describe で弾く。
    for sql in [
        "SELECT id FROM typed WHERE mood = 'bogus' AND n = $1 LIMIT 5",
        "SELECT id FROM typed WHERE n = $1 AND mood = 'bogus' LIMIT 5",
        "SELECT id FROM typed WHERE flag = $1 AND mood = 'bogus' LIMIT 5",
    ] {
        let prepared = core.parse_sql_prepared(sql).expect("parse");
        let err = core
            .describe_prepared_in_session(&session, &prepared)
            .expect_err("invalid enum label must not slip through Describe");
        assert_eq!(err.wire_code(), "22P02", "{sql}");
    }
    // `$n` の ENUM 等価と型付き `$n` の混在は Describe 成功。
    let prepared = core
        .parse_sql_prepared("SELECT id FROM typed WHERE mood = $1 AND n = $2 LIMIT 5")
        .expect("parse");
    core.describe_prepared_in_session(&session, &prepared)
        .expect("describe should succeed");
}

#[test]
fn typed_bind_executes_with_rls_boundary() {
    let path = unique_db_path("prepared-typed-exec-rls");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_typed_table(&path);
    let ctx_a =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let ctx_b = PolicyContext::new("tenant-b").expect("valid tenant");
    let mut session = SessionState::default();

    let insert = core
        .parse_sql_prepared(
            "INSERT INTO typed (id, embedding, body, n, flag) VALUES ($1, '[0.1,0.2,0.3]', 'row', $2, $3) USING OPERATION_ID $4",
        )
        .expect("parse");
    let bound = core
        .bind_prepared(&insert, &[some("1"), some("-5"), some("t"), some("op-ins")])
        .expect("bind");
    core.execute_parsed_in_session(&ctx_a, &mut session, &bound)
        .expect("insert should succeed");

    let rows = |ctx: &PolicyContext, session: &mut SessionState, sql: &str, v: &str| {
        let prepared = core.parse_sql_prepared(sql).expect("parse");
        let bound = core.bind_prepared(&prepared, &[some(v)]).expect("bind");
        match core
            .execute_parsed_in_session(ctx, session, &bound)
            .expect("execute")
        {
            SqlOutcome::Query(result) => result.rows.len(),
            other => panic!("expected Query, got {other:?}"),
        }
    };
    let by_id = "SELECT id FROM typed WHERE id = $1 LIMIT 5";
    let by_flag = "SELECT id FROM typed WHERE flag = $1 LIMIT 5";
    assert_eq!(rows(&ctx_a, &mut session, by_id, "1"), 1);
    assert_eq!(rows(&ctx_a, &mut session, by_flag, "t"), 1);
    assert_eq!(rows(&ctx_a, &mut session, by_flag, "f"), 0);
    assert_eq!(rows(&ctx_b, &mut session, by_id, "1"), 0);
    assert_eq!(rows(&ctx_b, &mut session, by_flag, "t"), 0);
}
