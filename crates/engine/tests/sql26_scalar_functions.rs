//! 文字列スカラー関数群の結合テスト（Issue #919、対象ビヘイビア: SQL-26
//! （検討中）・ポインタ: `docs/spec/05-tasks.md` TASK-210・
//! `docs/spec/04-behavior/sql-surface.md` SQL-26）。
//!
//! `tests/sql_scan.rs`（Issue #454・SQL-15）と同じ流儀（`unique_db_path`／
//! `CleanupGuard`、実 `Storage`＋`CpuScalarProvider`、`EngineCore::execute_sql`／
//! `execute_sql_in_session` を production 経路として検証）。`LOWER`・`UPPER`・
//! `LENGTH`・`SUBSTR`・`CONCAT`・`TRIM`・`REPLACE`・`POSITION` を投影・`WHERE`・
//! `CHECK` から使い、NULL 伝播（AC2）・予約名衝突（AC4）・型不一致の `wire_code`
//! 決定性を固定する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("label", ColumnType::Text, true),
        ],
    )
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// `id=1..=3` に `label = "Hello", "world", NULL` を投入する固定コーパス
/// （多バイト文字のケースは別途 `multibyte_functions_are_char_indexed` で扱う）。
fn seed(storage: &Storage, tenant: &str) {
    storage.create_table(&schema()).expect("create table");
    let ctx = ctx_for(tenant);
    let rows: [(u64, Option<&str>); 3] = [(1, Some("Hello")), (2, Some("world")), (3, None)];
    for (id, label) in rows {
        let value = match label {
            Some(s) => Value::Text(s.to_string()),
            None => Value::Null,
        };
        let op_id = engine::recovery::required_op_id::OperationId::parse(&format!("op-{id}"))
            .expect("valid operation_id");
        engine::tenant::insert_typed_row(
            storage,
            TABLE,
            &ctx,
            id,
            Visibility::Public,
            &[Value::Vector(vec![0.1, 0.2]), value],
            &op_id,
        )
        .expect("insert row");
    }
}

fn new_core(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    seed(&storage, "tenant-a");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn cell_for(result: &engine::sql::exec::QueryResult, id: u64, col: usize) -> Cell {
    result
        .rows
        .iter()
        .find(|r| r.id == id)
        .unwrap_or_else(|| panic!("row id={id} not found in result"))
        .cells
        .get(col)
        .cloned()
        .unwrap_or_else(|| panic!("column {col} missing for id={id}"))
}

// --- 投影段（AC1・AC2） -------------------------------------------------------

#[test]
fn upper_lower_length_project_expected_values_and_propagate_null() {
    let path = unique_db_path("sql26-projection");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let ctx = ctx_for("tenant-a");

    let result = core
        .execute_sql(
            &ctx,
            "SELECT id, upper(label), lower(label), length(label) FROM docs LIMIT 10",
        )
        .expect("SELECT with string functions should succeed");

    assert_eq!(cell_for(&result, 1, 1), Cell::Text("HELLO".to_string()));
    assert_eq!(cell_for(&result, 1, 2), Cell::Text("hello".to_string()));
    assert_eq!(cell_for(&result, 1, 3), Cell::Float(5.0));

    // Issue #919・SQL-26（AC2）: nullable TEXT 列が NULL の行は、strict な関数
    // （UPPER/LOWER/LENGTH）すべてで NULL を伝播する。
    assert_eq!(cell_for(&result, 3, 1), Cell::Null);
    assert_eq!(cell_for(&result, 3, 2), Cell::Null);
    assert_eq!(cell_for(&result, 3, 3), Cell::Null);
}

#[test]
fn substr_matches_postgres_window_semantics_in_projection() {
    let path = unique_db_path("sql26-substr");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let ctx = ctx_for("tenant-a");

    let result = core
        .execute_sql(
            &ctx,
            "SELECT id, substr(label, 2, 3), substr(label, 2) FROM docs WHERE id = 1 LIMIT 1",
        )
        .expect("SELECT with substr should succeed");
    assert_eq!(cell_for(&result, 1, 1), Cell::Text("ell".to_string()));
    assert_eq!(cell_for(&result, 1, 2), Cell::Text("ello".to_string()));
}

#[test]
fn concat_treats_null_as_empty_string_and_never_returns_null() {
    let path = unique_db_path("sql26-concat");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let ctx = ctx_for("tenant-a");

    let result = core
        .execute_sql(&ctx, "SELECT id, concat(label, '!') FROM docs LIMIT 10")
        .expect("SELECT with concat should succeed");
    assert_eq!(cell_for(&result, 1, 1), Cell::Text("Hello!".to_string()));
    // Issue #919・SQL-26（AC2）: CONCAT は NULL を空文字として扱い、常に非 NULL。
    assert_eq!(cell_for(&result, 3, 1), Cell::Text("!".to_string()));
}

#[test]
fn trim_replace_position_project_expected_values() {
    let path = unique_db_path("sql26-trim-replace-position");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let ctx = ctx_for("tenant-a");

    let result = core
        .execute_sql(
            &ctx,
            "SELECT id, trim(label), replace(label, 'l', 'L'), position('lo' IN label) \
             FROM docs WHERE id = 1 LIMIT 1",
        )
        .expect("SELECT with trim/replace/position should succeed");
    assert_eq!(cell_for(&result, 1, 1), Cell::Text("Hello".to_string()));
    assert_eq!(cell_for(&result, 1, 2), Cell::Text("HeLLo".to_string()));
    assert_eq!(cell_for(&result, 1, 3), Cell::Float(4.0));
}

#[test]
fn multibyte_functions_are_char_indexed() {
    let path = unique_db_path("sql26-multibyte");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let ctx = ctx_for("tenant-a");
    let op_id =
        engine::recovery::required_op_id::OperationId::parse("op-1").expect("valid operation_id");
    engine::tenant::insert_typed_row(
        &storage,
        TABLE,
        &ctx,
        1,
        Visibility::Public,
        &[
            Value::Vector(vec![0.1, 0.2]),
            Value::Text("あいうえお".to_string()),
        ],
        &op_id,
    )
    .expect("insert row");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));

    let result = core
        .execute_sql(
            &ctx,
            "SELECT id, length(label), substr(label, 2, 2) FROM docs LIMIT 1",
        )
        .expect("SELECT should succeed");
    assert_eq!(cell_for(&result, 1, 1), Cell::Float(5.0));
    assert_eq!(cell_for(&result, 1, 2), Cell::Text("いう".to_string()));
}

// --- WHERE 段（AC2: NULL は UNKNOWN=偽） --------------------------------------

#[test]
fn where_clause_with_string_function_excludes_null_rows() {
    let path = unique_db_path("sql26-where");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let ctx = ctx_for("tenant-a");

    let result = core
        .execute_sql(
            &ctx,
            "SELECT id FROM docs WHERE lower(label) = 'hello' LIMIT 10",
        )
        .expect("SELECT with WHERE lower(...) should succeed");
    let ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    assert_eq!(ids, vec![1]);

    // NULL 行を含む述語（`length(label) > 0`）は id=3（label NULL）を除外する。
    let result = core
        .execute_sql(&ctx, "SELECT id FROM docs WHERE length(label) > 0 LIMIT 10")
        .expect("SELECT with WHERE length(...) should succeed");
    let mut ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    ids.sort_unstable();
    assert_eq!(ids, vec![1, 2]);
}

/// PR 自己レビューで判明した回帰の固定テスト。`ReferencedColumns::derive`
/// （`sql::aggregate`）が集計関数引数（`AggregateInput::ScalarExpr`）の部分式に
/// 現れる `TEXT` 列参照（`BoundExpr::TextColumnRef`）を `scalar_mask` へ
/// 反映していなかったため、`SUM(LENGTH(text_col))` のように集計関数の直接引数が
/// 文字列スカラー関数である式は、返り値型こそ `ExprType::Scalar` でも
/// 評価時に `text_columns` から読めず常に `Internal`（fail-closed だが正当な
/// クエリを常に失敗させる回帰）になっていた。`COUNT(DISTINCT <expr>)` も
/// 同じ `ReferencedColumns::derive` を共有するため同様に固定する。
#[test]
fn aggregate_argument_containing_string_scalar_function_is_computed_correctly() {
    let path = unique_db_path("sql26-aggregate-string-arg");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let ctx = ctx_for("tenant-a");

    // label: "Hello"(5)・"world"(5)・NULL。SUM(LENGTH(label)) は NULL を
    // 除外して 5 + 5 = 10。
    let result = core
        .execute_sql(&ctx, "SELECT SUM(LENGTH(label)) FROM docs")
        .expect("SUM(LENGTH(...)) should succeed");
    match &result.rows[0].cells[0] {
        Cell::Float(v) => assert_eq!(*v, 10.0),
        other => panic!("expected Cell::Float, got {other:?}"),
    }

    // COUNT(DISTINCT LENGTH(label)) は "Hello"/"world" とも長さ 5 で同一・
    // NULL は除外するため異なり数は 1。
    let result = core
        .execute_sql(&ctx, "SELECT COUNT(DISTINCT LENGTH(label)) FROM docs")
        .expect("COUNT(DISTINCT LENGTH(...)) should succeed");
    match &result.rows[0].cells[0] {
        Cell::Integer(v) => assert_eq!(*v, 1),
        other => panic!("expected Cell::Integer, got {other:?}"),
    }
}

// --- 予約名・型不一致（AC4） ---------------------------------------------------

#[test]
fn builtin_string_function_names_are_reserved_against_udf_definition() {
    let path = unique_db_path("sql26-reserved-names");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();

    for name in [
        "lower", "upper", "length", "substr", "concat", "trim", "replace", "position",
    ] {
        let err = core
            .execute_sql_in_session(
                &ctx,
                &mut session,
                &format!("CREATE FUNCTION {name}(x) AS x"),
            )
            .expect_err(&format!("{name} should be reserved"));
        assert_eq!(err.wire_code(), "22000", "unexpected wire_code for {name}");
    }
}

#[test]
fn type_mismatches_are_rejected_with_22000() {
    let path = unique_db_path("sql26-type-mismatch");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let ctx = ctx_for("tenant-a");

    // 数値列に対する文字列関数呼び出し（`vec_norm` の Scalar 戻り値へ `upper` を
    // 適用）は型不一致で拒否する。
    let err = core
        .execute_sql(&ctx, "SELECT upper(vec_norm(embedding)) FROM docs LIMIT 1")
        .expect_err("upper(Scalar) should be rejected");
    assert_eq!(err.wire_code(), "22000");

    // 未知関数。
    let err = core
        .execute_sql(&ctx, "SELECT mystery_fn(label) FROM docs LIMIT 1")
        .expect_err("unknown function should be rejected");
    assert_eq!(err.wire_code(), "22000");

    // SUBSTR の引数個数不一致（1 個のみ）。
    let err = core
        .execute_sql(&ctx, "SELECT substr(label) FROM docs LIMIT 1")
        .expect_err("substr with 1 argument should be rejected");
    assert_eq!(err.wire_code(), "22000");
}

// --- CHECK 制約（Issue #919 の CHECK 経路への波及） ---------------------------

#[test]
fn check_constraint_using_length_enforces_at_write_time_and_treats_null_as_satisfied() {
    let path = unique_db_path("sql26-check");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();
    session.allow_ddl();

    core.execute_sql_in_session(
        &ctx,
        &mut session,
        "CREATE TABLE docs (label TEXT CHECK (length(label) > 0))",
    )
    .expect("CREATE TABLE with length(...) CHECK should succeed");

    // NULL の label は CHECK を満たしたとみなす（AC2・三値論理）。`INSERT ...
    // VALUES` の要素パーサーは `NULL` トークンを受理しないため、列リストから
    // `label` を省略して NULL 既定を使う（`tests/sql_not_null_default.rs` と
    // 同じ流儀）。
    core.execute_insert_sql(
        &ctx,
        "INSERT INTO docs (id) VALUES (10) USING OPERATION_ID 'op-check-null'",
    )
    .expect("NULL label should satisfy CHECK (UNKNOWN)");

    // 空文字列は CHECK 違反。
    let err = core
        .execute_insert_sql(
            &ctx,
            "INSERT INTO docs (id, label) VALUES (11, '') USING OPERATION_ID 'op-check-violation'",
        )
        .expect_err("empty label should violate length(label) > 0");
    assert_eq!(err.wire_code(), "23514");
}

/// codex-review（Cursor Bugbot）P1 指摘の回帰テスト: `POSITION` は組み込み関数の
/// 中で唯一カンマ区切りでない特殊構文（`POSITION(needle IN haystack)`）を持つ。
/// `sql::check_constraint::render_expr` が汎用のカンマ形（`POSITION(a, b)`。
/// 構文段が `42601` で拒否する形）で出力すると、`CHECK` 定義の永続化時点の
/// 往復検証（`validate_and_build` の設計 D2）が再パース不一致として
/// fail-closed に拒否し、`CREATE TABLE` 自体が失敗していた。
///
/// `tests/uuid_column.rs::uuid_column_roundtrips_through_storage_reopen` と
/// 同じ「`Storage` を閉じて同じパスで再オープンする」流儀で、`CHECK` 定義
/// テキスト（`predicate_sql`）がカタログへ永続化され、再オープン後の
/// `CompiledChecks::compile` による再パース・再束縛を経ても同じ評価結果
/// （enforce の成否）になることを固定する（実際の再起動を跨ぐ永続化と
/// 同じ経路——`schema.checks()` は `predicate_sql` の生テキストから
/// 都度再構築される）。
#[test]
fn check_constraint_using_position_round_trips_through_storage_reopen() {
    let path = unique_db_path("sql26-check-position-reopen");
    let _guard = CleanupGuard(path.clone());
    let ctx = ctx_for("tenant-a");

    {
        let storage = Storage::open(&path).expect("open storage");
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
        let mut session = SessionState::default();
        session.allow_ddl();

        core.execute_sql_in_session(
            &ctx,
            &mut session,
            "CREATE TABLE docs (label TEXT CHECK (POSITION('a' IN label) > 0))",
        )
        .expect("CREATE TABLE with POSITION(...) CHECK should succeed");

        // NULL の label は CHECK を満たしたとみなす（AC2・三値論理。既存の
        // length(...) CHECK テストと同じ流儀）。
        core.execute_insert_sql(
            &ctx,
            "INSERT INTO docs (id) VALUES (10) USING OPERATION_ID 'op-check-null'",
        )
        .expect("NULL label should satisfy CHECK (UNKNOWN)");

        // 'a' を含む label は CHECK を満たす。
        core.execute_insert_sql(
            &ctx,
            "INSERT INTO docs (id, label) VALUES (11, 'banana') USING OPERATION_ID 'op-check-pass'",
        )
        .expect("label containing 'a' should satisfy POSITION(...) CHECK");

        // 'a' を含まない label は CHECK 違反。
        let err = core
            .execute_insert_sql(
                &ctx,
                "INSERT INTO docs (id, label) VALUES (12, 'xyz') USING OPERATION_ID 'op-check-violation'",
            )
            .expect_err("label without 'a' should violate POSITION(...) CHECK");
        assert_eq!(err.wire_code(), "23514");
    }

    // 再オープン後（`CompiledChecks::compile` が永続化された `predicate_sql` を
    // 再パース・再束縛する）も同じ評価結果になることを確認する。
    let storage = Storage::open(&path).expect("reopen storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));

    core.execute_insert_sql(
        &ctx,
        "INSERT INTO docs (id) VALUES (20) USING OPERATION_ID 'op-check-null-after-reopen'",
    )
    .expect("NULL label should still satisfy CHECK (UNKNOWN) after reopen");

    core.execute_insert_sql(
        &ctx,
        "INSERT INTO docs (id, label) VALUES (21, 'apple') USING OPERATION_ID 'op-check-pass-after-reopen'",
    )
    .expect("label containing 'a' should still satisfy POSITION(...) CHECK after reopen");

    let err = core
        .execute_insert_sql(
            &ctx,
            "INSERT INTO docs (id, label) VALUES (22, 'xyz') USING OPERATION_ID 'op-check-violation-after-reopen'",
        )
        .expect_err("label without 'a' should still violate POSITION(...) CHECK after reopen");
    assert_eq!(err.wire_code(), "23514");
}

// --- codex-review（Cursor Bugbot）High 指摘の回帰テスト: COALESCE/NULLIF/CASE
// と NULL の TEXT 列の相互作用（`sql::expr_program::ExprStep::JumpIfNotNull` が
// `StackValue::TextColumnRef` を解決せず非 NULL 扱いしていた不具合。§3-7 に
// 追記済みのため対象ビヘイビアは引き続き SQL-26／Issue #919・#921）
// ---------------------------------------------------------------------------
//
// `seed()` のコーパス（id=1 label="Hello"・id=2 label="world"・id=3 label=NULL）
// をそのまま使う。`COALESCE`/`NULLIF`/`CASE` はいずれも構文段のトップレベル
// 式評価（投影・`WHERE`）でコンパイル済みステップ列（`ExprProgram`）を経由する
// ため、修正前は id=3（label が NULL）の行で `COALESCE(label, ...)` が
// フォールバック値ではなく NULL を返す・`WHERE COALESCE(...) = ...` が
// 誤って非該当になる、という回帰が投影・WHERE の両方で再現していた。

#[test]
fn coalesce_with_null_text_column_falls_back_in_projection() {
    let path = unique_db_path("sql26-coalesce-null-projection");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let ctx = ctx_for("tenant-a");

    let result = core
        .execute_sql(
            &ctx,
            "SELECT id, coalesce(label, 'default') FROM docs LIMIT 10",
        )
        .expect("SELECT with coalesce should succeed");
    // 非 NULL の TEXT 列（id=1, 2）は自身の値をそのまま返す（修正前から
    // 正しく動いていた経路。回帰していないことを確認する）。
    assert_eq!(cell_for(&result, 1, 1), Cell::Text("Hello".to_string()));
    assert_eq!(cell_for(&result, 2, 1), Cell::Text("world".to_string()));
    // NULL の TEXT 列（id=3）は 'default' へフォールバックする（修正前は
    // `TextColumnRef` マーカーを非 NULL と誤判定し NULL を返していた）。
    assert_eq!(cell_for(&result, 3, 1), Cell::Text("default".to_string()));
}

#[test]
fn coalesce_with_null_text_column_falls_back_in_where_clause() {
    let path = unique_db_path("sql26-coalesce-null-where");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let ctx = ctx_for("tenant-a");

    // label が NULL の行だけが 'fallback' へ解決される。修正前は
    // `COALESCE(label, 'fallback')` が誤って NULL を返し続けるため
    // `= 'fallback'` に一致せず、0 行になっていた。
    let result = core
        .execute_sql(
            &ctx,
            "SELECT id FROM docs WHERE coalesce(label, 'fallback') = 'fallback' LIMIT 10",
        )
        .expect("SELECT with WHERE coalesce should succeed");
    let ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    assert_eq!(ids, vec![3]);

    // 非 NULL の行（id=1, 2）は自身の値と比較され、'fallback' には一致しない。
    let result = core
        .execute_sql(
            &ctx,
            "SELECT id FROM docs WHERE coalesce(label, 'fallback') = 'Hello' LIMIT 10",
        )
        .expect("SELECT with WHERE coalesce should succeed");
    let ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    assert_eq!(ids, vec![1]);
}

#[test]
fn multi_stage_coalesce_skips_multiple_null_text_arguments() {
    let path = unique_db_path("sql26-coalesce-multi-stage");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let ctx = ctx_for("tenant-a");

    // id=3 は label が NULL のため、`label` を 2 回連続で NULL として
    // スキップし、3 番目の引数（リテラル）へたどり着く必要がある
    // （`JumpIfNotNull` が最初の `TextColumnRef` を誤って非 NULL と判定すると
    // 2 回目の引数へ進まずマーカーのまま止まってしまう）。
    let result = core
        .execute_sql(
            &ctx,
            "SELECT id, coalesce(label, label, 'final-default') FROM docs LIMIT 10",
        )
        .expect("SELECT with multi-stage coalesce should succeed");
    assert_eq!(cell_for(&result, 1, 1), Cell::Text("Hello".to_string()));
    assert_eq!(
        cell_for(&result, 3, 1),
        Cell::Text("final-default".to_string())
    );
}

#[test]
fn nullif_rejects_text_operands_and_propagates_null_via_derived_scalar_expr() {
    let path = unique_db_path("sql26-nullif-text");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let ctx = ctx_for("tenant-a");

    // NULLIF は `sql::udf_call::bind_nullif` が両辺を `ExprType::Scalar` に
    // 限定するため、`TEXT` 型（`BoundExpr::TextColumnRef` そのもの）は
    // オペランドとして構造的に受理されない（`=` 演算子と同じ既存契約）。
    // そのため NULLIF の直接オペランドとして生の `TextColumnRef` マーカーが
    // スタックへ積まれる経路は存在せず、本バグ（`JumpIfNotNull` の
    // 誤判定）の影響を受けようがないことをまず固定する。
    let err = core
        .execute_sql(&ctx, "SELECT nullif(label, 'Hello') FROM docs LIMIT 10")
        .expect_err("NULLIF must reject TEXT operands");
    assert_eq!(err.wire_code(), "42804");

    // TEXT 列由来の Scalar 式（`LENGTH(label)`）を経由した NULLIF は受理される。
    // `LENGTH` は Builtin ステップの `stack_to_expr_value` を経由して既に
    // 解決済みのスカラー値をスタックへ積むため生のマーカーは残らないが、
    // NULL 伝播（strict な `LENGTH` が NULL の `label` に対し NULL を返す）が
    // NULLIF まで正しく伝わることを固定する。
    let result = core
        .execute_sql(
            &ctx,
            "SELECT id, nullif(length(label), 100) FROM docs LIMIT 10",
        )
        .expect("SELECT with nullif(length(...), ...) should succeed");
    // id=1, 2: length は 5（'Hello'/'world'）で 100 と異なるため自身の値。
    assert_eq!(cell_for(&result, 1, 1), Cell::Float(5.0));
    assert_eq!(cell_for(&result, 2, 1), Cell::Float(5.0));
    // id=3: label が NULL のため length(label) も NULL、NULLIF 自体も NULL
    // （`(Null, _) => Null` 契約）。
    assert_eq!(cell_for(&result, 3, 1), Cell::Null);
}

#[test]
fn case_when_comparing_null_text_column_takes_else_branch() {
    let path = unique_db_path("sql26-case-text");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let ctx = ctx_for("tenant-a");

    // CASE の WHEN 条件は比較（`Expr::Binary`）に限定されるため、TEXT 列が
    // NULL の行では条件自体が NULL（UNKNOWN）に解決され、ELSE 分岐を取る
    // （`JumpIfNotTrue` は Binary 比較の結果〔Bool/Null〕のみを見るため、この
    // バグの影響は受けない経路だが、TEXT 列との組み合わせで回帰しないことを
    // 固定する）。
    let result = core
        .execute_sql(
            &ctx,
            "SELECT id, case when label = 'Hello' then 'yes' else 'no' end FROM docs LIMIT 10",
        )
        .expect("SELECT with case should succeed");
    assert_eq!(cell_for(&result, 1, 1), Cell::Text("yes".to_string()));
    assert_eq!(cell_for(&result, 2, 1), Cell::Text("no".to_string()));
    // id=3: label が NULL のため `label = 'Hello'` は UNKNOWN → ELSE 分岐。
    assert_eq!(cell_for(&result, 3, 1), Cell::Text("no".to_string()));
}
