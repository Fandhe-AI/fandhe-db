//! 述語つき `UPDATE ... WHERE`／`DELETE ... WHERE`（SQL-19・TASK-192、Issue #871）
//! の実行結線の結合テスト。ポインタ: `docs/spec/05-tasks.md` TASK-192・
//! `docs/spec/04-behavior/sql-surface.md` SQL-19・`docs/spec/04-behavior/
//! recovery.md` RECOVER-11（検討中）・RECOVER-9・RECOVER-10・
//! `docs/spec/04-behavior/rls.md` RLS-7・RLS-9・RLS-10・TABLE-12。
//!
//! `EngineCore::execute_sql_in_session` を production 経路として検証する
//! （`sql_delete_single_row.rs`・`truncate_table.rs` と同じ流儀。実 `Storage`
//! ＋ `CpuScalarProvider`、`unique_db_path`／`CleanupGuard`）。ADR
//! `docs/design/multi-row-dml-operation-id.md`（Issue #868。ステータス
//! Proposed・オーナー承認待ち）を作業前提として実行結線した旨を
//! `docs/design/predicate-dml-exec.md` に記録済み。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::{encode_scalar_columns, Value};
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{RowInput, Storage, Visibility};
use engine::tenant;

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "documents";

fn schema(name: &str) -> TableSchema {
    TableSchema::new(
        name,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    )
}

fn new_core_with_table() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("sql-predicate-dml-exec");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema(TABLE)).expect("create table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn ctx_for(tenant: &str, allow_private: bool) -> PolicyContext {
    if allow_private {
        PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
            .expect("valid tenant")
    } else {
        PolicyContext::new(tenant).expect("valid tenant")
    }
}

fn insert_row(
    core: &EngineCore,
    ctx: &PolicyContext,
    table: &str,
    id: u64,
    lang: &str,
    body: &str,
    seq: &str,
) {
    let mut session = SessionState::default();
    core.execute_sql_in_session(
        ctx,
        &mut session,
        &format!(
            "INSERT INTO {table} (id, embedding, lang, body) VALUES \
             ({id}, '[0.1,0.2]', '{lang}', '{body}') USING OPERATION_ID 'seed-{table}-{id}-{seq}'"
        ),
    )
    .expect("insert row");
}

fn count_star(core: &EngineCore, ctx: &PolicyContext, table: &str) -> u64 {
    let result = core
        .execute_sql(ctx, &format!("SELECT COUNT(*) FROM {table}"))
        .expect("count(*) should succeed");
    assert_eq!(result.rows.len(), 1);
    match &result.rows[0].cells[0] {
        Cell::Integer(v) => *v,
        other => panic!("expected Cell::Integer, got {other:?}"),
    }
}

fn scan_lang_rows(core: &EngineCore, ctx: &PolicyContext, table: &str) -> Vec<(u64, String)> {
    core.execute_sql(ctx, &format!("SELECT id, lang FROM {table} LIMIT 100"))
        .expect("scan should succeed")
        .rows
        .iter()
        .map(|r| {
            let lang = match &r.cells[1] {
                Cell::Text(s) => s.clone(),
                other => panic!("expected Cell::Text, got {other:?}"),
            };
            (r.id, lang)
        })
        .collect()
}

fn execute(
    core: &EngineCore,
    ctx: &PolicyContext,
    sql: &str,
) -> Result<SqlOutcome, engine::sql::allowlist::SqlSurfaceError> {
    let mut session = SessionState::default();
    core.execute_sql_in_session(ctx, &mut session, sql)
}

/// 候補列挙・応答件数の同値性: `SELECT` の候補集合と `DELETE ... WHERE` の
/// `rows_affected`・削除後残存集合が一致する（第 2 の述語評価器を作らない
/// 契約の外部観測）。
#[test]
fn predicate_delete_removes_exactly_the_matching_rows() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    insert_row(&core, &alice, TABLE, 1, "ja", "a", "1");
    insert_row(&core, &alice, TABLE, 2, "en", "b", "2");
    insert_row(&core, &alice, TABLE, 3, "ja", "c", "3");

    let outcome = execute(
        &core,
        &alice,
        &format!("DELETE FROM {TABLE} WHERE lang = 'ja' USING OPERATION_ID 'op-del-1'"),
    )
    .expect("predicate DELETE should succeed");
    match outcome {
        SqlOutcome::Delete(o) => assert_eq!(o.rows_affected, 2),
        other => panic!("expected SqlOutcome::Delete, got {other:?}"),
    }

    let remaining = scan_lang_rows(&core, &alice, TABLE);
    assert_eq!(remaining, vec![(2, "en".to_string())]);
    assert_eq!(count_star(&core, &alice, TABLE), 1);
}

/// 候補列挙・SET 反映の同値性: `UPDATE ... WHERE` は一致行のみ SET 対象列を
/// 更新し、非対象列・他行は不変のまま。
#[test]
fn predicate_update_changes_exactly_the_matching_rows() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    insert_row(&core, &alice, TABLE, 1, "ja", "a", "1");
    insert_row(&core, &alice, TABLE, 2, "en", "b", "2");
    insert_row(&core, &alice, TABLE, 3, "ja", "c", "3");

    let outcome = execute(
        &core,
        &alice,
        &format!("UPDATE {TABLE} SET lang = 'fr' WHERE lang = 'ja' USING OPERATION_ID 'op-upd-1'"),
    )
    .expect("predicate UPDATE should succeed");
    match outcome {
        SqlOutcome::Update(o) => assert_eq!(o.rows_affected, 2),
        other => panic!("expected SqlOutcome::Update, got {other:?}"),
    }

    let mut rows = scan_lang_rows(&core, &alice, TABLE);
    rows.sort();
    assert_eq!(
        rows,
        vec![
            (1, "fr".to_string()),
            (2, "en".to_string()),
            (3, "fr".to_string()),
        ]
    );
}

/// RLS 境界（RLS-9・RLS-10）: alice が bob の `Public` 行と一致する述語で
/// DELETE/UPDATE しても bob 行は不変。応答（成否・件数）は「bob 行が存在
/// しない DB」で同じ文を実行した場合と一致する（bob 行の有無が alice の
/// 応答へ一切影響しない）。
#[test]
fn predicate_delete_and_update_do_not_affect_other_tenant_rows() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let bob = ctx_for("bob", true);
    insert_row(&core, &alice, TABLE, 1, "ja", "a", "1");
    insert_row(&core, &bob, TABLE, 2, "ja", "b", "1");

    let outcome = execute(
        &core,
        &alice,
        &format!("DELETE FROM {TABLE} WHERE lang = 'ja' USING OPERATION_ID 'op-del-cross'"),
    )
    .expect("predicate DELETE should succeed");
    match outcome {
        SqlOutcome::Delete(o) => assert_eq!(o.rows_affected, 1),
        other => panic!("expected SqlOutcome::Delete, got {other:?}"),
    }
    // bob 行は不変（bob 自身の視点で確認）。
    assert_eq!(count_star(&core, &bob, TABLE), 1);
    // alice の視点では自テナント行のみが対象になり、bob 行は候補にすら
    // ならない（`rows_affected` が bob 行の有無に依存しない）。
    assert_eq!(count_star(&core, &alice, TABLE), 0);

    let outcome = execute(
        &core,
        &alice,
        &format!(
            "UPDATE {TABLE} SET lang = 'fr' WHERE lang = 'ja' USING OPERATION_ID 'op-upd-cross'"
        ),
    )
    .expect("predicate UPDATE should succeed");
    match outcome {
        SqlOutcome::Update(o) => assert_eq!(o.rows_affected, 0),
        other => panic!("expected SqlOutcome::Update, got {other:?}"),
    }
    let bob_rows = scan_lang_rows(&core, &bob, TABLE);
    assert_eq!(bob_rows, vec![(2, "ja".to_string())]);
}

/// 0 行一致: 台帳は commit され（世代は進まない）、同一 `operation_id` の
/// 再送は対象行の有無に依存せず `23505` へ収束する（RECOVER-10・RECOVER-11）。
#[test]
fn predicate_delete_zero_match_still_records_ledger_and_rejects_resend() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    insert_row(&core, &alice, TABLE, 1, "en", "a", "1");

    let outcome = execute(
        &core,
        &alice,
        &format!("DELETE FROM {TABLE} WHERE lang = 'ja' USING OPERATION_ID 'op-zero'"),
    )
    .expect("zero-match predicate DELETE should still succeed");
    match outcome {
        SqlOutcome::Delete(o) => assert_eq!(o.rows_affected, 0),
        other => panic!("expected SqlOutcome::Delete, got {other:?}"),
    }
    assert_eq!(count_star(&core, &alice, TABLE), 1);

    let err = execute(
        &core,
        &alice,
        &format!("DELETE FROM {TABLE} WHERE lang = 'ja' USING OPERATION_ID 'op-zero'"),
    )
    .expect_err("resend of a recorded operation_id must be rejected");
    assert_eq!(err.wire_code(), "23505");
    assert_eq!(count_star(&core, &alice, TABLE), 1);
}

/// 内容照合ハッシュ（RECOVER-11）: `WHERE` 述語の宣言順を入れ替えた再送は
/// 内容不一致として `22023` を返す（`AND` 平坦列挙・並べ替えない契約）。
#[test]
fn predicate_delete_resend_with_reordered_predicates_is_content_mismatch() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    insert_row(&core, &alice, TABLE, 1, "ja", "a", "1");

    execute(
        &core,
        &alice,
        &format!(
            "DELETE FROM {TABLE} WHERE lang = 'ja' AND id > 0 USING OPERATION_ID 'op-reorder'"
        ),
    )
    .expect("first DELETE should succeed");

    // 同じ operation_id・述語順を入れ替えた文を再送する（行は既に削除済み
    // だが、台帳照合は列挙より先に行われるため実削除の有無に依存しない）。
    let err = execute(
        &core,
        &alice,
        &format!(
            "DELETE FROM {TABLE} WHERE id > 0 AND lang = 'ja' USING OPERATION_ID 'op-reorder'"
        ),
    )
    .expect_err("reordered predicate resend must be a content mismatch");
    assert_eq!(err.wire_code(), "22023");
}

/// SQL-24・TASK-208、Issue #914: `LIKE` の一般形（中間一致）を含む述語つき
/// `DELETE ... WHERE` も件数一致・再送契約（`23505`／`22023`）に従う
/// （`declarative_filter::DeclarativeFilter::like` 経由での束縛が第 2 の
/// 述語評価器を作らない契約を守ることの回帰防止）。
#[test]
fn predicate_delete_with_like_general_form_matches_count_and_resend_contract() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    insert_row(&core, &alice, TABLE, 1, "ja", "src/a.rs", "1");
    insert_row(&core, &alice, TABLE, 2, "en", "src/b.rs", "2");
    insert_row(&core, &alice, TABLE, 3, "ja", "README.md", "3");

    let outcome = execute(
        &core,
        &alice,
        &format!("DELETE FROM {TABLE} WHERE body LIKE '%.rs' USING OPERATION_ID 'op-like-del'"),
    )
    .expect("predicate DELETE with LIKE general form should succeed");
    match outcome {
        SqlOutcome::Delete(o) => assert_eq!(o.rows_affected, 2),
        other => panic!("expected SqlOutcome::Delete, got {other:?}"),
    }
    assert_eq!(count_star(&core, &alice, TABLE), 1);

    // 同一 `operation_id`・同一パターンの再送は `23505`。
    let err = execute(
        &core,
        &alice,
        &format!("DELETE FROM {TABLE} WHERE body LIKE '%.rs' USING OPERATION_ID 'op-like-del'"),
    )
    .expect_err("resend of a recorded operation_id must be rejected");
    assert_eq!(err.wire_code(), "23505");

    // 同一 `operation_id`・パターン違いの再送は内容不一致（`22023`）。
    let err = execute(
        &core,
        &alice,
        &format!("DELETE FROM {TABLE} WHERE body LIKE '%.md' USING OPERATION_ID 'op-like-del'"),
    )
    .expect_err("same operation_id with a different pattern must be a content mismatch");
    assert_eq!(err.wire_code(), "22023");
}

/// `WHERE visible()` のみの述語つき DELETE は自テナント全行を候補にする
/// （#870 の決定を継承。歯止めは影響行数上限のみ。本テストは #870 の
/// 決定を再決定しない）。
#[test]
fn predicate_delete_visible_only_matches_all_own_rows() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    insert_row(&core, &alice, TABLE, 1, "ja", "a", "1");
    insert_row(&core, &alice, TABLE, 2, "en", "b", "2");
    insert_row(&core, &alice, TABLE, 3, "fr", "c", "3");

    let outcome = execute(
        &core,
        &alice,
        &format!("DELETE FROM {TABLE} WHERE visible() USING OPERATION_ID 'op-visible'"),
    )
    .expect("visible()-only predicate DELETE should succeed");
    match outcome {
        SqlOutcome::Delete(o) => assert_eq!(o.rows_affected, 3),
        other => panic!("expected SqlOutcome::Delete, got {other:?}"),
    }
    assert_eq!(count_star(&core, &alice, TABLE), 0);
}

/// fail-closed: `operation_id` 欠落は `23502`（構造検証・束縛のいずれよりも
/// 先に短絡する。TASK-92・RECOVER-1）。
#[test]
fn predicate_delete_missing_operation_id_is_rejected() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    insert_row(&core, &alice, TABLE, 1, "ja", "a", "1");

    let err = execute(
        &core,
        &alice,
        &format!("DELETE FROM {TABLE} WHERE lang = 'ja'"),
    )
    .expect_err("missing operation_id must be rejected");
    assert_eq!(err.wire_code(), "23502");
    assert_eq!(count_star(&core, &alice, TABLE), 1);
}

/// fail-closed: `WHERE visible()` のみの述語つき UPDATE は束縛段で拒否する
/// （#869 の既存契約。実行に到達しない）。
#[test]
fn predicate_update_visible_only_is_rejected_at_bind() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    insert_row(&core, &alice, TABLE, 1, "ja", "a", "1");

    let err = execute(
        &core,
        &alice,
        &format!(
            "UPDATE {TABLE} SET lang = 'fr' WHERE visible() USING OPERATION_ID 'op-visible-upd'"
        ),
    )
    .expect_err("visible()-only predicate UPDATE must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

/// `execute_sql`（セッション無し）は引き続き `DELETE`／`UPDATE` を拒否する
/// （既存契約は本 Issue で変更しない）。
#[test]
fn execute_sql_without_session_still_rejects_predicate_delete_and_update() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);

    let err = core
        .execute_sql(
            &alice,
            &format!("DELETE FROM {TABLE} WHERE lang = 'ja' USING OPERATION_ID 'op-x'"),
        )
        .expect_err("execute_sql must reject DELETE");
    assert_eq!(err.wire_code(), "42601");

    let err = core
        .execute_sql(
            &alice,
            &format!("UPDATE {TABLE} SET lang = 'fr' WHERE lang = 'ja' USING OPERATION_ID 'op-y'"),
        )
        .expect_err("execute_sql must reject UPDATE");
    assert_eq!(err.wire_code(), "42601");
}

/// `MAX_DML_AFFECTED_ROWS`（1,000）件を超える一致は `54000` で拒否され、
/// 副作用がゼロのまま（行不変・台帳未記録＝再送しても再び `54000`）となる
/// （ADR §6「4.」。`engine::sql::parser::MAX_DML_AFFECTED_ROWS` を直接参照し、
/// 本リポ独自の実装既定値が変わっても追随する）。多数行の投入は
/// `MAX_INSERT_ROWS_PER_STATEMENT`（1,000）に収まるよう複数行 `VALUES`
/// （SQL-16・TASK-190）2 文に分割する。
#[test]
fn predicate_delete_over_limit_is_rejected_with_no_side_effects() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);

    let limit = engine::sql::parser::MAX_DML_AFFECTED_ROWS;
    let over = limit + 1; // 1,001 件（1,000 件ちょうどは成功する対照として別テストへ）

    // 複数行 `VALUES`（SQL-16・TASK-190）の実行結線は INDEX-4 の一括投入上限
    // （既定 64 ファイル相当）を経由するため、本テストの目的（DML 影響行数
    // 上限 `MAX_DML_AFFECTED_ROWS`＝1,000 の検証）には使えない。単一行 `INSERT`
    // をループで投入する（`insert_row` ヘルパーと同じ経路）。
    for id in 1..=over {
        insert_row(
            &core,
            &alice,
            TABLE,
            id as u64,
            "ja",
            "b",
            &format!("over-{id}"),
        );
    }
    assert_eq!(count_star(&core, &alice, TABLE), over as u64);

    let err = execute(
        &core,
        &alice,
        &format!("DELETE FROM {TABLE} WHERE lang = 'ja' USING OPERATION_ID 'op-over-limit'"),
    )
    .expect_err("over-limit predicate DELETE must be rejected");
    assert_eq!(err.wire_code(), "54000");
    // 副作用ゼロ: 行数不変・台帳未記録（同一 operation_id は再送しても
    // 再び 54000 になる。23505/22023 にはならない＝台帳に痕跡が残っていない）。
    assert_eq!(count_star(&core, &alice, TABLE), over as u64);
    let err_again = execute(
        &core,
        &alice,
        &format!("DELETE FROM {TABLE} WHERE lang = 'ja' USING OPERATION_ID 'op-over-limit'"),
    )
    .expect_err("resend of an unrecorded operation_id must be the same over-limit rejection");
    assert_eq!(err_again.wire_code(), "54000");
}

/// 指定 `id` の行が持つ `embedding`（`VECTOR` 列）の現在値を直接読み取る
/// ヘルパー（広域取得 `SELECT embedding FROM {table} WHERE id = {id} LIMIT 1`。
/// SQL-15・Issue #454）。`ORDER BY <=> ...` の近傍探索（ランキングが未正規化
/// 内積によるため、大きさの異なるベクトル間では「値が完全一致する行」が
/// Top-1 に来るとは限らない）を経由せず、値そのものを比較する。
fn embedding_of(core: &EngineCore, ctx: &PolicyContext, table: &str, id: u64) -> Vec<f32> {
    let result = core
        .execute_sql(
            ctx,
            &format!("SELECT embedding FROM {table} WHERE id = {id} LIMIT 1"),
        )
        .expect("select should succeed");
    assert_eq!(result.rows.len(), 1, "expected exactly one row for id={id}");
    match &result.rows[0].cells[0] {
        Cell::Vector(v) => v.clone(),
        other => panic!("expected Cell::Vector, got {other:?}"),
    }
}

/// 述語つき UPDATE が `VECTOR` 列（`embedding`）への SET も一括適用できる
/// ことを確認する（`tenant::update_rows_where_unchecked` の
/// `Some(vector_idx) == col_idx` 分岐。read-merge-write で埋め込みを丸ごと
/// 差し替え、TEXT 列は無変更のまま残る）。
#[test]
fn predicate_update_set_vector_column_replaces_embedding_for_matching_rows() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    insert_row(&core, &alice, TABLE, 1, "ja", "a", "1");
    insert_row(&core, &alice, TABLE, 2, "en", "b", "2");

    let outcome = execute(
        &core,
        &alice,
        &format!(
            "UPDATE {TABLE} SET embedding = '[9.0,9.0]' WHERE lang = 'ja' \
             USING OPERATION_ID 'op-upd-vec'"
        ),
    )
    .expect("predicate UPDATE with VECTOR column SET should succeed");
    match outcome {
        SqlOutcome::Update(o) => assert_eq!(o.rows_affected, 1),
        other => panic!("expected SqlOutcome::Update, got {other:?}"),
    }

    // id=1（一致行）の embedding は更新後の値に置き換わる（クエリ [9.0,9.0]
    // の Top-1 が id=1 になる）。TEXT 列（lang）は無変更のまま。
    assert_eq!(embedding_of(&core, &alice, TABLE, 1), vec![9.0, 9.0]);
    let rows = scan_lang_rows(&core, &alice, TABLE);
    assert!(rows.contains(&(1, "ja".to_string())));

    // id=2（非一致行）の embedding は無変更のまま（クエリ [0.1,0.2] の Top-1
    // が id=2 になる。元の値のまま残っている証跡）。
    assert_eq!(embedding_of(&core, &alice, TABLE, 2), vec![0.1, 0.2]);
}

/// `VECTOR` 列への SET でリテラルの次元がスキーマと不一致なら束縛段で `22000`
/// （`sql::parser::bind_set_assignments` を単一行 UPDATE と共有するため、
/// `sql_update_single_row.rs::set_embedding_with_wrong_dimension_is_rejected_with_22000`
/// と同じ契約。対象行の実在有無に関わらず束縛段で拒否されるため副作用ゼロ）。
#[test]
fn predicate_update_set_vector_column_with_wrong_dimension_is_rejected_with_22000() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    insert_row(&core, &alice, TABLE, 1, "ja", "a", "1");

    let err = execute(
        &core,
        &alice,
        &format!(
            "UPDATE {TABLE} SET embedding = '[1.0,2.0,3.0]' WHERE lang = 'ja' \
             USING OPERATION_ID 'op-upd-vec-dim'"
        ),
    )
    .expect_err("dimension mismatch must be rejected at bind time");
    assert_eq!(err.wire_code(), "22000");
    // 副作用ゼロ: 行不変・台帳未記録（同一 operation_id の再送も同じ束縛
    // エラーになる。23505/22023 にはならない）。
    assert_eq!(embedding_of(&core, &alice, TABLE, 1), vec![0.1, 0.2]);
    let err_again = execute(
        &core,
        &alice,
        &format!(
            "UPDATE {TABLE} SET embedding = '[1.0,2.0,3.0]' WHERE lang = 'ja' \
             USING OPERATION_ID 'op-upd-vec-dim'"
        ),
    )
    .expect_err("resend of an unrecorded operation_id must be the same bind-time rejection");
    assert_eq!(err_again.wire_code(), "22000");
}

/// `VECTOR` 列への SET に非ベクトル値（数値リテラル）を与えると束縛段で
/// `22000`（`bind_set_assignments` の `(ColumnType::Vector(_), InsertLiteral::Number(_))`
/// 分岐）。
#[test]
fn predicate_update_set_vector_column_with_non_vector_value_is_rejected_with_22000() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    insert_row(&core, &alice, TABLE, 1, "ja", "a", "1");

    let err = execute(
        &core,
        &alice,
        &format!(
            "UPDATE {TABLE} SET embedding = 42 WHERE lang = 'ja' \
             USING OPERATION_ID 'op-upd-vec-nonvec'"
        ),
    )
    .expect_err("non-vector literal for a VECTOR column must be rejected");
    assert_eq!(err.wire_code(), "22000");
    assert_eq!(embedding_of(&core, &alice, TABLE, 1), vec![0.1, 0.2]);
}

/// DELETE 側（[`predicate_delete_over_limit_is_rejected_with_no_side_effects`]）
/// と同じ契約が述語つき UPDATE 側にも成立することを固定する（`§6` の上限 API
/// 並立〔`MAX_DML_AFFECTED_ROWS`＋`check_dml_affected_rows`〕は DELETE 側の
/// `DEFAULT_MAX_DML_AFFECTED_ROWS`＋`check_affected_row_count`と値は同じ
/// 1,000 だが別 API のため、UPDATE 側でも独立に検証する）。
#[test]
fn predicate_update_over_limit_is_rejected_with_no_side_effects() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);

    let limit = engine::sql::parser::MAX_DML_AFFECTED_ROWS;
    let over = limit + 1; // 1,001 件（1,000 件ちょうどは成功する対照として別テストへ）

    for id in 1..=over {
        insert_row(
            &core,
            &alice,
            TABLE,
            id as u64,
            "ja",
            "b",
            &format!("over-{id}"),
        );
    }
    assert_eq!(count_star(&core, &alice, TABLE), over as u64);

    let err = execute(
        &core,
        &alice,
        &format!(
            "UPDATE {TABLE} SET lang = 'fr' WHERE lang = 'ja' USING OPERATION_ID 'op-upd-over-limit'"
        ),
    )
    .expect_err("over-limit predicate UPDATE must be rejected");
    assert_eq!(err.wire_code(), "54000");
    // 副作用ゼロ: 行不変（lang 列は書き換わっていない。1,001 行全件を
    // `scan_lang_rows` の `LIMIT 100` で確認するのは非現実的なため、
    // `WHERE lang = 'fr'` の一致数が 0 のままであることで代替確認する）
    // ・台帳未記録（同一 operation_id は再送しても再び 54000 になる。
    // 23505/22023 にはならない）。
    assert_eq!(count_star(&core, &alice, TABLE), over as u64);
    let unchanged = core
        .execute_sql(
            &alice,
            &format!("SELECT COUNT(*) FROM {TABLE} WHERE lang = 'fr'"),
        )
        .expect("count(*) should succeed");
    match &unchanged.rows[0].cells[0] {
        Cell::Integer(v) => assert_eq!(*v, 0, "no row should have been updated to lang = 'fr'"),
        other => panic!("expected Cell::Integer, got {other:?}"),
    }
    let err_again = execute(
        &core,
        &alice,
        &format!(
            "UPDATE {TABLE} SET lang = 'fr' WHERE lang = 'ja' USING OPERATION_ID 'op-upd-over-limit'"
        ),
    )
    .expect_err("resend of an unrecorded operation_id must be the same over-limit rejection");
    assert_eq!(err_again.wire_code(), "54000");
}

/// [`enumerate_dml_candidates`]（`UPDATE`／`DELETE` 両経路が共有する候補列挙本体。
/// `crate::tenant`）の総走査行数上限（`tenant::MAX_SCANNED_ROWS`＝1,000,000。
/// `visible_rows` と同じ値・同じ判断）が独立して効くことを固定する
/// （codex-review P1 指摘・Issue #871）。一致件数上限（`MAX_DML_AFFECTED_ROWS`）は
/// 述語に一致した行にしか作用しないため、一致行が 0 件のままだと `limit` には
/// 到達しない。この総走査上限が無いと、他テナントを含む任意規模の全表走査を
/// 単一 writer を占有したまま繰り返せてしまう。
///
/// 走査対象の投入は `crate::tenant::insert_rows` の単一バッチ呼び出しで行う
/// （SQL `INSERT` を 1,000,001 回ループさせると本テストが極端に重くなるため。
/// `insert_row` ヘルパーとは異なる bulk 経路だが、書き込み認可
/// （`PolicyContext::is_owner`）・`operation_id` 必須化ガードは同一）。
/// `tenant::insert_rows` は `&Storage` を取るため、`EngineCore::from_storage`
/// （所有権を奪う）へ渡す**前**に呼び出す必要がある（`scan_stage_profile_accept.rs`
/// の `seed_storage` と同じ順序）。
fn new_core_with_bulk_seeded_table(
    ctx: &PolicyContext,
    table: &str,
    n: u64,
) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("sql-predicate-dml-exec-scan-limit");
    let storage = Storage::open(&path).expect("open storage");
    let table_schema = schema(table);
    storage.create_table(&table_schema).expect("create table");

    let embedding = vec![1.0f32, 0.0f32];
    // `WHERE lang = 'zzz-nomatch'` に一致しない固定値。全走査中ずっと述語が
    // 偽のまま推移し、`MAX_DML_AFFECTED_ROWS`（一致件数上限）ではなく
    // `MAX_SCANNED_ROWS`（総走査行数上限）を先に踏むことを保証する。
    let metadata = encode_scalar_columns(
        &table_schema,
        &[
            Value::Vector(embedding.clone()),
            Value::Text("never-matches".to_string()),
            Value::Text("body".to_string()),
        ],
    )
    .expect("encode_scalar_columns");
    let rows: Vec<(u64, RowInput<'_>)> = (1..=n)
        .map(|id| {
            (
                id,
                RowInput {
                    tenant_id: ctx.tenant_id(),
                    // `Private` にする: `Public` だと他テナントの `SELECT`
                    // からも可視になり、`count_star` が「対象テナント自身の
                    // 行数」ではなく「visibility 縮約後の可視件数」を返して
                    // しまい、他テナントの行数超過をテナント越しに観測できて
                    // しまう（本テストが固定したい契約と矛盾する）。
                    visibility: Visibility::Private,
                    embedding: &embedding,
                    metadata: &metadata,
                },
            )
        })
        .collect();
    let op_id = OperationId::parse("seed-bulk-scan-limit").expect("valid operation_id");
    tenant::insert_rows(&storage, table, ctx, &rows, &op_id).expect("bulk seed rows");

    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

/// 総走査行数上限超過時、`DELETE ... WHERE` は `54000` で拒否され副作用ゼロ
/// （行不変・台帳未記録＝再送しても再び `54000`）のまま終端する。
#[test]
fn predicate_delete_over_scan_limit_is_rejected_with_no_side_effects() {
    let alice = ctx_for("alice", true);
    // `tenant::MAX_SCANNED_ROWS`（1,000,000）を 1 件超える総走査行数。
    let total_rows: u64 = 1_000_001;
    let (core, path) = new_core_with_bulk_seeded_table(&alice, TABLE, total_rows);
    let _guard = CleanupGuard(path);
    assert_eq!(count_star(&core, &alice, TABLE), total_rows);

    let err = execute(
        &core,
        &alice,
        &format!(
            "DELETE FROM {TABLE} WHERE lang = 'zzz-nomatch' USING OPERATION_ID 'op-scan-limit-del'"
        ),
    )
    .expect_err("over-scan-limit predicate DELETE must be rejected");
    assert_eq!(err.wire_code(), "54000");
    // 副作用ゼロ: 行数不変・台帳未記録（同一 operation_id を再送しても再び
    // 54000 になる。23505/22023 にはならない＝台帳に痕跡が残っていない）。
    assert_eq!(count_star(&core, &alice, TABLE), total_rows);
    let err_again = execute(
        &core,
        &alice,
        &format!(
            "DELETE FROM {TABLE} WHERE lang = 'zzz-nomatch' USING OPERATION_ID 'op-scan-limit-del'"
        ),
    )
    .expect_err("resend of an unrecorded operation_id must be the same over-scan-limit rejection");
    assert_eq!(err_again.wire_code(), "54000");
}

/// 総走査行数上限超過時、`UPDATE ... WHERE` も [`enumerate_dml_candidates`]
/// を共有するため同じ契約（`54000`・副作用ゼロ・台帳未記録）となる。
#[test]
fn predicate_update_over_scan_limit_is_rejected_with_no_side_effects() {
    let alice = ctx_for("alice", true);
    let total_rows: u64 = 1_000_001;
    let (core, path) = new_core_with_bulk_seeded_table(&alice, TABLE, total_rows);
    let _guard = CleanupGuard(path);
    assert_eq!(count_star(&core, &alice, TABLE), total_rows);

    let err = execute(
        &core,
        &alice,
        &format!(
            "UPDATE {TABLE} SET lang = 'fr' WHERE lang = 'zzz-nomatch' USING OPERATION_ID 'op-scan-limit-upd'"
        ),
    )
    .expect_err("over-scan-limit predicate UPDATE must be rejected");
    assert_eq!(err.wire_code(), "54000");
    assert_eq!(count_star(&core, &alice, TABLE), total_rows);
    let unchanged = core
        .execute_sql(
            &alice,
            &format!("SELECT COUNT(*) FROM {TABLE} WHERE lang = 'fr'"),
        )
        .expect("count(*) should succeed");
    match &unchanged.rows[0].cells[0] {
        Cell::Integer(v) => assert_eq!(*v, 0, "no row should have been updated to lang = 'fr'"),
        other => panic!("expected Cell::Integer, got {other:?}"),
    }
    let err_again = execute(
        &core,
        &alice,
        &format!(
            "UPDATE {TABLE} SET lang = 'fr' WHERE lang = 'zzz-nomatch' USING OPERATION_ID 'op-scan-limit-upd'"
        ),
    )
    .expect_err("resend of an unrecorded operation_id must be the same over-scan-limit rejection");
    assert_eq!(err_again.wire_code(), "54000");
}

/// codex-review P0 対応（Issue #871）: [`enumerate_dml_candidates`] の総走査
/// 行数上限は対象テナントの物理キー名前空間内でのみ計数される。他テナント
/// （bob）が [`tenant::MAX_SCANNED_ROWS`] を超える行を保持していても、対象
/// テナント（alice）自身の行数が少なければ述語つき `DELETE ... WHERE` は
/// `54000` にならず成功し、bob の行は一切変化しない（bob の行数超過の有無を
/// alice への応答から推測できないことの回帰固定。修正前は他テナントの行も
/// `is_owner` 判定前に総走査カウンタへ加算していたため、この呼び出しは誤って
/// `54000` になっていた）。
#[test]
fn predicate_delete_succeeds_when_only_other_tenant_exceeds_scan_limit() {
    let bob = ctx_for("bob", true);
    let alice = ctx_for("alice", true);
    let other_tenant_rows: u64 = 1_000_001;
    let (core, path) = new_core_with_bulk_seeded_table(&bob, TABLE, other_tenant_rows);
    let _guard = CleanupGuard(path);
    assert_eq!(count_star(&core, &bob, TABLE), other_tenant_rows);

    // alice 自身の行は少数（総走査上限を踏まない）。
    insert_row(&core, &alice, TABLE, 1, "ja", "doc-1", "a1");
    insert_row(&core, &alice, TABLE, 2, "ja", "doc-2", "a2");
    insert_row(&core, &alice, TABLE, 3, "fr", "doc-3", "a3");

    let outcome = execute(
        &core,
        &alice,
        &format!(
            "DELETE FROM {TABLE} WHERE lang = 'ja' USING OPERATION_ID 'op-del-other-tenant-excess'"
        ),
    )
    .expect(
        "predicate DELETE scoped to alice's small row set must succeed \
         regardless of bob's row count",
    );
    match outcome {
        SqlOutcome::Delete(o) => assert_eq!(o.rows_affected, 2),
        other => panic!("expected SqlOutcome::Delete, got {other:?}"),
    }
    assert_eq!(count_star(&core, &alice, TABLE), 1);
    assert_eq!(
        count_star(&core, &bob, TABLE),
        other_tenant_rows,
        "bob's rows must remain untouched"
    );
}

/// 上記 DELETE 版と同じ回帰固定を `UPDATE ... WHERE` 側でも行う
/// （[`enumerate_dml_candidates`] を共有するため）。
#[test]
fn predicate_update_succeeds_when_only_other_tenant_exceeds_scan_limit() {
    let bob = ctx_for("bob", true);
    let alice = ctx_for("alice", true);
    let other_tenant_rows: u64 = 1_000_001;
    let (core, path) = new_core_with_bulk_seeded_table(&bob, TABLE, other_tenant_rows);
    let _guard = CleanupGuard(path);
    assert_eq!(count_star(&core, &bob, TABLE), other_tenant_rows);

    insert_row(&core, &alice, TABLE, 1, "ja", "doc-1", "a1");
    insert_row(&core, &alice, TABLE, 2, "ja", "doc-2", "a2");
    insert_row(&core, &alice, TABLE, 3, "fr", "doc-3", "a3");

    let outcome = execute(
        &core,
        &alice,
        &format!(
            "UPDATE {TABLE} SET lang = 'en' WHERE lang = 'ja' USING OPERATION_ID 'op-upd-other-tenant-excess'"
        ),
    )
    .expect(
        "predicate UPDATE scoped to alice's small row set must succeed \
         regardless of bob's row count",
    );
    match outcome {
        SqlOutcome::Update(o) => assert_eq!(o.rows_affected, 2),
        other => panic!("expected SqlOutcome::Update, got {other:?}"),
    }
    let mut remaining_langs: Vec<String> = scan_lang_rows(&core, &alice, TABLE)
        .into_iter()
        .map(|(_, lang)| lang)
        .collect();
    remaining_langs.sort();
    assert_eq!(
        remaining_langs,
        vec!["en".to_string(), "en".to_string(), "fr".to_string()]
    );
    assert_eq!(
        count_star(&core, &bob, TABLE),
        other_tenant_rows,
        "bob's rows must remain untouched"
    );
}

/// codex-review P0 対応（Issue #871・[`enumerate_dml_candidates`] の range 終端を
/// `Bound::Unbounded` から対象テナントの閉区間へ限定した修正の回帰固定）:
/// 対象テナント（alice）が `TABLE` に行を 1 件も持たず、かつ物理キー順で
/// alice より後ろに位置するテナント（bob。文字列比較で `"alice" < "bob"`）が
/// [`tenant::MAX_SCANNED_ROWS`] を超える行を保持している場合でも、述語つき
/// `DELETE ... WHERE` は alice の空の名前空間だけを走査して `54000` にならず
/// 成功し（0 件一致）、bob の行は一切変化しない。修正前は `range` の終端が
/// `Unbounded` だったため、alice の名前空間走査が 0 件のまま尽きた直後の
/// 反復で辞書順で後続する bob の先頭エントリを取得してしまい、`key_tenant
/// != tenant` の break 判定に至る前に bob 領域のキー・値へ触れていた
/// （「他テナント領域のキー・値に一切触れない」契約への抵触）。
#[test]
fn predicate_delete_succeeds_when_target_tenant_has_no_rows_and_following_tenant_exceeds_scan_limit(
) {
    let bob = ctx_for("bob", true);
    let alice = ctx_for("alice", true);
    let other_tenant_rows: u64 = 1_000_001;
    let (core, path) = new_core_with_bulk_seeded_table(&bob, TABLE, other_tenant_rows);
    let _guard = CleanupGuard(path);
    assert_eq!(count_star(&core, &bob, TABLE), other_tenant_rows);
    // alice は `TABLE` に行を 1 件も持たない。
    assert_eq!(count_star(&core, &alice, TABLE), 0);

    let outcome = execute(
        &core,
        &alice,
        &format!(
            "DELETE FROM {TABLE} WHERE lang = 'ja' USING OPERATION_ID 'op-del-empty-target-tenant'"
        ),
    )
    .expect(
        "predicate DELETE over alice's empty namespace must succeed with zero \
         matches regardless of bob's row count",
    );
    match outcome {
        SqlOutcome::Delete(o) => assert_eq!(o.rows_affected, 0),
        other => panic!("expected SqlOutcome::Delete, got {other:?}"),
    }
    assert_eq!(count_star(&core, &alice, TABLE), 0);
    assert_eq!(
        count_star(&core, &bob, TABLE),
        other_tenant_rows,
        "bob's rows must remain untouched"
    );
}

/// 上記 DELETE 版と同じ回帰固定を `UPDATE ... WHERE` 側でも行う
/// （[`enumerate_dml_candidates`] を共有するため）。
#[test]
fn predicate_update_succeeds_when_target_tenant_has_no_rows_and_following_tenant_exceeds_scan_limit(
) {
    let bob = ctx_for("bob", true);
    let alice = ctx_for("alice", true);
    let other_tenant_rows: u64 = 1_000_001;
    let (core, path) = new_core_with_bulk_seeded_table(&bob, TABLE, other_tenant_rows);
    let _guard = CleanupGuard(path);
    assert_eq!(count_star(&core, &bob, TABLE), other_tenant_rows);
    assert_eq!(count_star(&core, &alice, TABLE), 0);

    let outcome = execute(
        &core,
        &alice,
        &format!(
            "UPDATE {TABLE} SET lang = 'en' WHERE lang = 'ja' USING OPERATION_ID 'op-upd-empty-target-tenant'"
        ),
    )
    .expect(
        "predicate UPDATE over alice's empty namespace must succeed with zero \
         matches regardless of bob's row count",
    );
    match outcome {
        SqlOutcome::Update(o) => assert_eq!(o.rows_affected, 0),
        other => panic!("expected SqlOutcome::Update, got {other:?}"),
    }
    assert_eq!(count_star(&core, &alice, TABLE), 0);
    assert_eq!(
        count_star(&core, &bob, TABLE),
        other_tenant_rows,
        "bob's rows must remain untouched"
    );
}
