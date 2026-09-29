//! `REAL`／`DOUBLE PRECISION` 列型（Issue #882・TABLE-13・TASK-196）の SQL 表層
//! 結合テスト。ポインタ: `docs/spec/05-tasks.md` TASK-196・
//! `docs/spec/04-behavior/data-model.md`。
//!
//! ファイル名は `scalar_types_roundtrip.rs`（並行 Issue #881／#883 が使う可能性
//! がある名前）と意図的に衝突を避けている。Issue #897（型横断回帰）では本
//! ファイルを削除・統合せず、`column_type_codec_roundtrip.rs`・
//! `column_type_catalog_regression.rs`・`column_type_sql_rejection_matrix.rs`
//! を型横断の別ファイルとして追加する方針を採った（既存の網羅性を保つため）。
//!
//! `EngineCore::execute_sql_in_session`（production の SQL 実行経路）を直接
//! 検証する（`sql_update_single_row.rs` と同じ流儀。実 `Storage`＋
//! `CpuScalarProvider`、`unique_db_path`／`CleanupGuard`）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId as TypedOperationId;
use engine::row_codec::Value;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "metrics";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("score", ColumnType::Real, false),
            ColumnDef::new("weight", ColumnType::Double, true),
        ],
    )
}

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("scalar-types-float-roundtrip");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// `ORDER BY`（DISTANCE）を伴わない広域取得（SQL-15。`sql::scan`）で 1 行を
/// 取得する（本テストのテーブルは `id` 等価条件のみで十分なため、DISTANCE
/// 検索は使わない）。
fn select_row(core: &EngineCore, ctx: &PolicyContext, id: u64) -> Vec<Cell> {
    let result = core
        .execute_sql(
            ctx,
            &format!("SELECT score, weight FROM {TABLE} WHERE id = {id} LIMIT 1"),
        )
        .expect("select should succeed");
    assert_eq!(result.rows.len(), 1);
    result.rows[0].cells.clone()
}

/// [`select_row`] の 0 行版（存在しない・不可視の行を確認する用途）。
fn select_rows(core: &EngineCore, ctx: &PolicyContext, id: u64) -> Vec<Vec<Cell>> {
    let result = core
        .execute_sql(
            ctx,
            &format!("SELECT score, weight FROM {TABLE} WHERE id = {id} LIMIT 1"),
        )
        .expect("select should succeed");
    result.rows.iter().map(|r| r.cells.clone()).collect()
}

fn as_float(cell: &Cell) -> f64 {
    match cell {
        Cell::Float(v) => *v,
        other => panic!("expected Cell::Float, got {other:?}"),
    }
}

// --- 基本往復 ---------------------------------------------------------------

#[test]
fn insert_select_roundtrips_real_and_double_values() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let mut session = SessionState::default();

    core.execute_sql_in_session(
        &alice,
        &mut session,
        &format!(
            "INSERT INTO {TABLE} (id, embedding, score, weight) \
             VALUES (1, '[0.1,0.2]', 1.5, 2.25) USING OPERATION_ID 'op-1'"
        ),
    )
    .expect("INSERT should succeed");

    let cells = select_row(&core, &alice, 1);
    assert_eq!(as_float(&cells[0]), 1.5f32 as f64);
    assert_eq!(as_float(&cells[1]), 2.25);
}

/// 指数表記リテラル（`1.5e3`・`2E-2`）が字句解析から列束縛まで通る
/// （Issue #1187。`scalar_float::parse_real`／`parse_double` の受理文法と揃える）。
#[test]
fn insert_accepts_exponent_notation_literals() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let mut session = SessionState::default();

    core.execute_sql_in_session(
        &alice,
        &mut session,
        &format!(
            "INSERT INTO {TABLE} (id, embedding, score, weight) \
             VALUES (1, '[0.1,0.2]', 1.5e3, 2E-2) USING OPERATION_ID 'op-1'"
        ),
    )
    .expect("INSERT with exponent literals should succeed");

    let cells = select_row(&core, &alice, 1);
    assert_eq!(as_float(&cells[0]), 1500.0);
    assert_eq!(as_float(&cells[1]), 0.02);

    // 式（WHERE の算術比較）でも指数表記が 1 トークンとして読まれる。
    let result = core
        .execute_sql(
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE id + 1e0 = 2 LIMIT 1"),
        )
        .expect("exponent literal in WHERE expression should succeed");
    assert_eq!(result.rows.len(), 1);
}

/// 指数部に数字が無い `1e` は構文エラーで拒否される（PostgreSQL と同様）。
#[test]
fn insert_rejects_exponent_without_digits() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let mut session = SessionState::default();

    let sql = format!(
        "INSERT INTO {TABLE} (id, embedding, score) VALUES (1, '[0.1,0.2]', 1e) \
         USING OPERATION_ID 'op-1'"
    );
    let err = core
        .execute_sql_in_session(&alice, &mut session, &sql)
        .expect_err("exponent without digits must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

/// 負値・境界値（`-0.0` の正規化を含む）の往復。
#[test]
fn insert_select_roundtrips_negative_and_boundary_values() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let mut session = SessionState::default();

    core.execute_sql_in_session(
        &alice,
        &mut session,
        &format!(
            "INSERT INTO {TABLE} (id, embedding, score, weight) \
             VALUES (1, '[0.1,0.2]', -3.5, -0.0) USING OPERATION_ID 'op-1'"
        ),
    )
    .expect("INSERT should succeed");

    let cells = select_row(&core, &alice, 1);
    assert_eq!(as_float(&cells[0]), -3.5f32 as f64);
    // -0.0 は +0.0 へ正規化される（F4）。
    assert_eq!(as_float(&cells[1]).to_bits(), 0.0f64.to_bits());
}

/// `weight`（nullable）を省略した行は NULL として読める。
#[test]
fn nullable_double_column_defaults_to_null_when_omitted() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let mut session = SessionState::default();

    core.execute_sql_in_session(
        &alice,
        &mut session,
        &format!(
            "INSERT INTO {TABLE} (id, embedding, score) VALUES (1, '[0.1,0.2]', 1.0) \
             USING OPERATION_ID 'op-1'"
        ),
    )
    .expect("INSERT should succeed");

    let cells = select_row(&core, &alice, 1);
    assert!(matches!(cells[1], Cell::Null));
}

// --- ALTER TABLE ADD COLUMN（TABLE-5）----------------------------------------

/// `ADD COLUMN` 以前の既存行は、新設 nullable REAL/DOUBLE 列を NULL として
/// 読める（TABLE-5）。
#[test]
fn add_column_existing_rows_read_new_float_column_as_null() {
    let path = unique_db_path("scalar-types-float-add-column");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let base_schema = TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("score", ColumnType::Real, false),
        ],
    );
    storage.create_table(&base_schema).expect("create table");

    let alice = ctx_for("alice");
    let op_id = TypedOperationId::parse("op-1").expect("valid operation_id");
    engine::tenant::insert_typed_row(
        &storage,
        TABLE,
        &alice,
        1,
        Visibility::Public,
        &[Value::Vector(vec![0.1, 0.2]), Value::Real(1.0)],
        &op_id,
    )
    .expect("seed insert should succeed");

    // `ADD COLUMN` を書き込み済み行の後に適用する（TABLE-5 の想定順序）。
    storage
        .alter_table_add_column(TABLE, ColumnDef::new("weight", ColumnType::Double, true))
        .expect("ALTER TABLE ADD COLUMN should succeed");

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let cells = select_row(&core, &alice, 1);
    assert!(matches!(cells[1], Cell::Null));
}

// --- エラー系（fail-closed）--------------------------------------------------

/// オーバーフローするリテラルは `22003`（NumericOutOfRange）で拒否し、書き込み
/// トランザクション開始前の拒否のためテーブル世代・台帳のいずれも変化しない
/// ことを、直後の再送が成功することで間接的に確認する。
#[test]
fn insert_rejects_out_of_range_real_literal_with_22003() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let mut session = SessionState::default();

    let huge = "4".to_string() + &"0".repeat(39);
    let sql = format!(
        "INSERT INTO {TABLE} (id, embedding, score) VALUES (1, '[0.1,0.2]', {huge}) \
         USING OPERATION_ID 'op-1'"
    );
    let err = core
        .execute_sql_in_session(&alice, &mut session, &sql)
        .expect_err("oversized REAL literal must be rejected");
    assert_eq!(err.wire_code(), "22003");

    // 拒否後に同一 operation_id で正常な値を再送すると成功する（台帳未記録・
    // テーブル世代不変の間接確認）。
    let retry_sql = format!(
        "INSERT INTO {TABLE} (id, embedding, score) VALUES (1, '[0.1,0.2]', 1.0) \
         USING OPERATION_ID 'op-1'"
    );
    core.execute_sql_in_session(&alice, &mut session, &retry_sql)
        .expect("retry with the same operation_id should succeed after the rejected attempt");
}

/// 文字列リテラルは形式不正として `22P02`（InvalidTextRepresentation。Issue #1187）で拒否する
/// （F7: 文字列からの暗黙変換はしない）。
#[test]
fn insert_rejects_string_literal_for_real_column_with_22p02() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let mut session = SessionState::default();

    let sql = format!(
        "INSERT INTO {TABLE} (id, embedding, score) VALUES (1, '[0.1,0.2]', '1.5') \
         USING OPERATION_ID 'op-1'"
    );
    let err = core
        .execute_sql_in_session(&alice, &mut session, &sql)
        .expect_err("string literal for REAL column must be rejected");
    assert_eq!(err.wire_code(), "22P02");
}

/// `WHERE score = 1` は fail-closed に拒否される（F10: REAL/DOUBLE は算術と
/// 組み合わせる式評価経路〔レーン A〕の対象で、#891・TASK-199 が対応した
/// 非数値型〔DATE/TIMESTAMP/NUMERIC/UUID/BYTEA〕の範囲外のまま。詳細は
/// `docs/design/scalar-types-predicates.md` 参照）。
#[test]
fn select_where_on_real_column_is_rejected() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let mut session = SessionState::default();

    core.execute_sql_in_session(
        &alice,
        &mut session,
        &format!(
            "INSERT INTO {TABLE} (id, embedding, score) VALUES (1, '[0.1,0.2]', 1.0) \
             USING OPERATION_ID 'op-1'"
        ),
    )
    .expect("INSERT should succeed");

    let err = core
        .execute_sql(
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE score = 1 LIMIT 1"),
        )
        .expect_err("WHERE on REAL column must be rejected");
    assert_eq!(err.wire_code(), "22000");
}

/// `SUM(score)` は Issue #892 で受理された（TABLE-13・SQL-13）。結果は
/// Issue #1173 以降 `REAL`（f32 へ丸めた `Cell::Float`。PostgreSQL の `sum(real)`
/// と同じ）で、列メタも `Computed { ty: Some(Real) }` になる。
#[test]
fn sum_aggregate_on_real_column_succeeds_after_issue_892() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let mut session = SessionState::default();

    core.execute_sql_in_session(
        &alice,
        &mut session,
        &format!(
            "INSERT INTO {TABLE} (id, embedding, score) VALUES (1, '[0.1,0.2]', 1.0) \
             USING OPERATION_ID 'op-1'"
        ),
    )
    .expect("INSERT should succeed");
    core.execute_sql_in_session(
        &alice,
        &mut session,
        &format!(
            "INSERT INTO {TABLE} (id, embedding, score) VALUES (2, '[0.1,0.2]', 2.5) \
             USING OPERATION_ID 'op-2'"
        ),
    )
    .expect("INSERT should succeed");

    let result = core
        .execute_sql(&alice, &format!("SELECT SUM(score) FROM {TABLE}"))
        .expect("SUM on REAL column must succeed after Issue #892");
    assert_eq!(result.rows[0].cells, vec![Cell::Float(3.5)]);
    assert_eq!(
        result.columns[0],
        engine::sql::exec::ColumnMeta::Computed {
            name: "sum".to_string(),
            ty: Some(engine::catalog::ColumnType::Real),
        }
    );
}

/// `id = -1` の応答コードは REAL/DOUBLE 追加の前後で変わらない（既存契約の
/// 非退行確認）。
#[test]
fn negative_id_pseudo_column_rejection_is_unchanged() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");

    let err = core
        .execute_sql(&alice, &format!("SELECT id FROM {TABLE} WHERE id = -1"))
        .expect_err("negative id literal must still be rejected");
    // 既存の型不一致・構文不正のいずれかの応答であればよい（本 Issue はこの
    // 応答コード自体を変えないことのみを確認する）。
    assert_ne!(err.wire_code(), "22003");
}

// --- UPDATE / RLS -------------------------------------------------------

/// UPDATE ... SET による REAL/DOUBLE 列の往復。
#[test]
fn update_set_roundtrips_real_and_double_values() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let mut session = SessionState::default();

    core.execute_sql_in_session(
        &alice,
        &mut session,
        &format!(
            "INSERT INTO {TABLE} (id, embedding, score, weight) \
             VALUES (1, '[0.1,0.2]', 1.0, 1.0) USING OPERATION_ID 'op-1'"
        ),
    )
    .expect("INSERT should succeed");

    let outcome = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            &format!("UPDATE {TABLE} SET score = -2.5 WHERE id = 1 USING OPERATION_ID 'op-update'"),
        )
        .expect("UPDATE should succeed");
    match outcome {
        SqlOutcome::Update(o) => assert_eq!(o.rows_affected, 1),
        other => panic!("expected SqlOutcome::Update, got {other:?}"),
    }

    let cells = select_row(&core, &alice, 1);
    assert_eq!(as_float(&cells[0]), -2.5f32 as f64);
    assert_eq!(as_float(&cells[1]), 1.0);
}

/// 他テナントからは不可視（RLS）。
#[test]
fn other_tenant_cannot_see_the_row() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let bob = ctx_for("bob");
    let mut session = SessionState::default();

    core.execute_sql_in_session(
        &alice,
        &mut session,
        &format!(
            "INSERT INTO {TABLE} (id, embedding, score) VALUES (1, '[0.1,0.2]', 1.0) \
             USING OPERATION_ID 'op-1'"
        ),
    )
    .expect("INSERT should succeed");

    let rows = select_rows(&core, &bob, 1);
    assert!(rows.is_empty());
}

/// SET 値の事前検証（非有限・型不一致）は、対象行の有無にかかわらず同じ
/// 応答になる（存在情報を漏らさない。RLS-9 と同じ判断）。
#[test]
fn update_set_finite_check_gives_the_same_response_regardless_of_row_existence() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let mut session = SessionState::default();

    // id=1 は存在しない。REAL リテラルは範囲外リテラルは束縛段の
    // `parse_real` で `22003` に落ちるため、ここでは型不一致（`22000`）で
    // 「文字列を渡す」ケースを使い、存在有無で応答が変わらないことを見る。
    let sql =
        format!("UPDATE {TABLE} SET score = '1.5' WHERE id = 1 USING OPERATION_ID 'op-missing'");
    let err_missing = core
        .execute_sql_in_session(&alice, &mut session, &sql)
        .expect_err("type-mismatched SET must be rejected regardless of row existence");

    core.execute_sql_in_session(
        &alice,
        &mut session,
        &format!(
            "INSERT INTO {TABLE} (id, embedding, score) VALUES (1, '[0.1,0.2]', 1.0) \
             USING OPERATION_ID 'op-seed'"
        ),
    )
    .expect("INSERT should succeed");

    let sql2 =
        format!("UPDATE {TABLE} SET score = '1.5' WHERE id = 1 USING OPERATION_ID 'op-present'");
    let err_present = core
        .execute_sql_in_session(&alice, &mut session, &sql2)
        .expect_err("type-mismatched SET must be rejected regardless of row existence");

    assert_eq!(err_missing.wire_code(), err_present.wire_code());
}
