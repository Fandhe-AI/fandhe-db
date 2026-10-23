//! `core/dml_join_exec.rs` の単体テスト（SQL-53 (b)・RLS-10。Issue #1755）。
//!
//! 構文・束縛（Issue #1754）が未マージの間も検証できるよう、束縛済みの実行入力（[`JoinedDmlPlan`]）を
//! 手で組んで実行入口へ渡す（#1754 のマージ後は SQL 文字列による結合テストを別ファイルに追加する）。
//! 多重一致の一意化・各上限の fail-closed（副作用ゼロ）・テナント境界（他テナントの Private 行が
//! 一致・SET 値・件数に影響しない）・ゲート（autocommit・RETURNING の source 参照・台帳）を固定する。

use super::dml_join_exec::JoinedDmlOutcome;
use super::EngineCore;
use crate::catalog::{ColumnDef, ColumnType, TableSchema};
use crate::kernel::CpuScalarProvider;
use crate::policy::PolicyContext;
use crate::recovery::required_op_id::{LedgerMode, OperationId};
use crate::row_codec::Value;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::exec::Cell;
use crate::sql::join::dml::{
    EquiKey, JoinedDmlPlan, JoinedDmlSource, JoinedDmlTarget, JoinedSetItem, JoinedUpdateSet,
};
use crate::sql::join::JoinLimits;
use crate::sql::mode::SessionState;
use crate::sql::udf_call::{BinOp, Expr};
use crate::storage::{Storage, Visibility};
use crate::tenant::WriteTarget;
use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

fn t_schema() -> TableSchema {
    TableSchema::new(
        "t",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("x", ColumnType::BigInt, false),
            ColumnDef::new("k", ColumnType::Text, true),
        ],
    )
}

fn s_schema() -> TableSchema {
    TableSchema::new(
        "s",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("k", ColumnType::Text, true),
            ColumnDef::new("v", ColumnType::BigInt, true),
        ],
    )
}

/// 結合スカラービュー（対象列 ++ source の取り出し列）に対応する束縛用スキーマ。
fn combined_schema() -> TableSchema {
    TableSchema::new(
        "t_s",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("x", ColumnType::BigInt, false),
            ColumnDef::new("k", ColumnType::Text, true),
            ColumnDef::new("s_k", ColumnType::Text, true),
            ColumnDef::new("s_v", ColumnType::BigInt, true),
        ],
    )
}

fn new_core() -> (EngineCore, CleanupGuard) {
    new_core_with(LedgerMode::default())
}

fn new_core_with(mode: LedgerMode) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path("dml-join-exec");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&t_schema()).expect("create t");
    storage.create_table(&s_schema()).expect("create s");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_ledger_mode(mode),
        CleanupGuard(path),
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn run(core: &EngineCore, ctx: &PolicyContext, sql: &str) {
    let mut session = SessionState::default();
    core.execute_sql_in_session(ctx, &mut session, sql)
        .expect("sql");
}

fn seed_t(core: &EngineCore, ctx: &PolicyContext, id: u64, x: i64, k: Option<&str>) {
    let k = k.map_or("NULL".to_string(), |k| format!("'{k}'"));
    run(
        core,
        ctx,
        &format!("INSERT INTO t (id, embedding, x, k) VALUES ({id}, '[0.1,0.2]', {x}, {k})"),
    );
}

fn seed_s(core: &EngineCore, ctx: &PolicyContext, id: u64, k: &str, v: i64) {
    run(
        core,
        ctx,
        &format!("INSERT INTO s (id, embedding, k, v) VALUES ({id}, '[0.1,0.2]', '{k}', {v})"),
    );
}

/// 他テナント（bob）が `Public` で持つ source 行を挿入する。
fn seed_s_public(core: &EngineCore, ctx: &PolicyContext, id: u64, k: &str, v: i64) {
    crate::tenant::insert_typed_row(
        &core.storage,
        "s",
        ctx,
        id,
        Visibility::Public,
        &[
            Value::Vector(vec![0.1, 0.2]),
            Value::Text(k.to_string()),
            Value::BigInt(v),
        ],
        &OperationId::parse(&format!("seed-public-{id}")).expect("op id"),
    )
    .expect("insert public row");
}

/// `t` の `(id, x)` を id 昇順で返す（呼び出し元テナントから見える行）。
fn xs(core: &EngineCore, ctx: &PolicyContext) -> Vec<(u64, i64)> {
    let mut v: Vec<(u64, i64)> = core
        .execute_sql(ctx, "SELECT x FROM t LIMIT 100")
        .expect("scan")
        .rows
        .iter()
        .map(|r| match &r.cells[0] {
            Cell::SignedInteger(x) => (r.id, *x),
            other => panic!("unexpected cell {other:?}"),
        })
        .collect();
    v.sort();
    v
}

fn key_k() -> EquiKey {
    EquiKey {
        target_column: Some(2),
        source_column: Some(1),
    }
}

fn plan(keys: Vec<EquiKey>, update: Option<JoinedUpdateSet>) -> JoinedDmlPlan {
    JoinedDmlPlan {
        target: JoinedDmlTarget {
            table: "t".to_string(),
            schema: t_schema(),
            metadata_filters: Vec::new(),
            expr_filters: Vec::new(),
            or_filters: Vec::new(),
        },
        source: JoinedDmlSource {
            table: "s".to_string(),
            schema: s_schema(),
            where_predicates: Vec::new(),
            // 結合ビュー上の位置: s_k = 3、s_v = 4。
            columns: vec![1, 2],
        },
        keys,
        update,
        returning_references_source: false,
    }
}

fn set_x(expr: Expr) -> JoinedUpdateSet {
    let col = t_schema().columns[1].clone();
    let set = crate::sql::set_expr::bind_set_expr("x", &expr, &col, &combined_schema())
        .expect("bind SET");
    JoinedUpdateSet {
        literal_assignments: Vec::new(),
        items: vec![JoinedSetItem {
            column_index: 1,
            set,
        }],
    }
}

fn update_from_s_v() -> JoinedDmlPlan {
    plan(vec![key_k()], Some(set_x(Expr::Ident("s_v".to_string()))))
}

fn update_x_plus_one() -> JoinedDmlPlan {
    plan(
        vec![key_k()],
        Some(set_x(Expr::Binary {
            op: BinOp::Add,
            lhs: Box::new(Expr::Ident("x".to_string())),
            rhs: Box::new(Expr::Number("1".to_string())),
        })),
    )
}

fn delete_using() -> JoinedDmlPlan {
    plan(vec![key_k()], None)
}

fn exec_with(
    core: &EngineCore,
    ctx: &PolicyContext,
    plan: &JoinedDmlPlan,
    limits: &JoinLimits,
    max_affected: Option<usize>,
    max_scanned: usize,
) -> Result<JoinedDmlOutcome, SqlSurfaceError> {
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    txn.begin_implicit(&core.storage, &session)?;
    let write_txn = txn.write_txn().expect("write txn");
    let out = core.execute_joined_dml_with_limits(
        WriteTarget::InTxn(write_txn),
        ctx,
        &session,
        plan,
        None,
        None,
        limits,
        max_affected.and_then(std::num::NonZeroUsize::new),
        max_scanned,
    );
    // 失敗時は txn を drop して abort する（呼び出し元の暗黙トランザクションと同じ扱い）。
    let out = out?;
    // 呼び出し元（#1754 の dispatch）の責務: 書き込んだ表を暗黙トランザクションへ記録する。
    txn.mark_written(ctx.tenant_id(), &plan.target.table);
    txn.commit_implicit(&mut session)?;
    Ok(out)
}

fn exec(
    core: &EngineCore,
    ctx: &PolicyContext,
    plan: &JoinedDmlPlan,
) -> Result<JoinedDmlOutcome, SqlSurfaceError> {
    exec_with(core, ctx, plan, &JoinLimits::default(), None, 1_000_000)
}

/// alice: t = {1:(10,a), 2:(20,b), 3:(30,c)}、s = {1:(a,100), 2:(a,200), 3:(b,300)}。
fn setup() -> (EngineCore, CleanupGuard, PolicyContext) {
    let (core, guard) = new_core();
    let alice = ctx("alice");
    seed_t(&core, &alice, 1, 10, Some("a"));
    seed_t(&core, &alice, 2, 20, Some("b"));
    seed_t(&core, &alice, 3, 30, Some("c"));
    seed_s(&core, &alice, 1, "a", 100);
    seed_s(&core, &alice, 2, "a", 200);
    seed_s(&core, &alice, 3, "b", 300);
    (core, guard, alice)
}

#[test]
fn multi_match_update_applies_once_with_the_first_source_row() {
    let (core, _g, alice) = setup();
    let out = exec(&core, &alice, &update_from_s_v()).expect("update");
    // 対象行 2 件（a は source 2 行に一致するが 1 回だけ更新）。影響行数は対象行数。
    assert_eq!(out.rows_affected, 2);
    // 一致位置の昇順の先頭（id 昇順の走査順）の source 行の値が入る。c は不一致で不変。
    assert_eq!(xs(&core, &alice), vec![(1, 100), (2, 300), (3, 30)]);
    assert_eq!(out.matches, vec![(1, 0), (2, 2)]);
}

#[test]
fn multi_match_update_with_self_reference_increments_exactly_once() {
    let (core, _g, alice) = setup();
    seed_s(&core, &alice, 4, "a", 400);
    let out = exec(&core, &alice, &update_x_plus_one()).expect("update");
    assert_eq!(out.rows_affected, 2);
    // a は source 3 行に一致するが +1 は 1 回だけ。
    assert_eq!(xs(&core, &alice), vec![(1, 11), (2, 21), (3, 30)]);
}

#[test]
fn update_is_deterministic_across_repeated_runs() {
    for _ in 0..3 {
        let (core, _g, alice) = setup();
        exec(&core, &alice, &update_from_s_v()).expect("update");
        assert_eq!(xs(&core, &alice), vec![(1, 100), (2, 300), (3, 30)]);
    }
}

#[test]
fn multi_match_delete_removes_each_target_row_once() {
    let (core, _g, alice) = setup();
    seed_s(&core, &alice, 4, "a", 400);
    let out = exec(&core, &alice, &delete_using()).expect("delete");
    assert_eq!(out.rows_affected, 2);
    assert_eq!(xs(&core, &alice), vec![(3, 30)]);
}

#[test]
fn null_keys_and_unmatched_rows_are_untouched() {
    let (core, _g, alice) = setup();
    seed_t(&core, &alice, 4, 40, None);
    run(
        &core,
        &alice,
        "INSERT INTO s (id, embedding, k, v) VALUES (9, '[0.1,0.2]', NULL, 900)",
    );
    let out = exec(&core, &alice, &delete_using()).expect("delete");
    // NULL 同士は一致しない。
    assert_eq!(out.rows_affected, 2);
    assert_eq!(xs(&core, &alice), vec![(3, 30), (4, 40)]);
}

#[test]
fn pseudo_id_key_joins_against_an_integer_column() {
    let (core, _g, alice) = setup();
    // t.id = s.v: s.v が 2 の行は無い。v を 2 にした行を足す。
    seed_s(&core, &alice, 7, "z", 2);
    let keys = vec![EquiKey {
        target_column: None,
        source_column: Some(2),
    }];
    let out = exec(&core, &alice, &plan(keys, None)).expect("delete");
    assert_eq!(out.rows_affected, 1);
    assert_eq!(xs(&core, &alice), vec![(1, 10), (3, 30)]);
}

#[test]
fn key_type_mismatch_is_rejected_without_side_effects() {
    let (core, _g, alice) = setup();
    // 対象の text 列 k と source の bigint 列 v は結合できない（42804）。
    let keys = vec![EquiKey {
        target_column: Some(2),
        source_column: Some(2),
    }];
    let err = exec(&core, &alice, &plan(keys, None))
        .err()
        .expect("reject");
    assert_eq!(err.wire_code(), "42804");
    assert_eq!(xs(&core, &alice).len(), 3);
}

#[test]
fn input_row_limit_rejects_with_54000_and_no_side_effects() {
    let (core, _g, alice) = setup();
    let limits = JoinLimits {
        max_input_rows: 2,
        ..JoinLimits::default()
    };
    let err = exec_with(&core, &alice, &update_from_s_v(), &limits, None, 1_000_000)
        .err()
        .expect("reject");
    assert_eq!(err.wire_code(), "54000");
    assert_eq!(xs(&core, &alice), vec![(1, 10), (2, 20), (3, 30)]);
}

#[test]
fn output_row_limit_counts_pairs_before_deduplication() {
    let (core, _g, alice) = setup();
    // 対象行は 2 件だが一致ペアは 3 件（a×2、b×1）。上限 2 はペア数で超過する。
    let limits = JoinLimits {
        max_output_rows: 2,
        ..JoinLimits::default()
    };
    let err = exec_with(&core, &alice, &update_from_s_v(), &limits, None, 1_000_000)
        .err()
        .expect("reject");
    assert_eq!(err.wire_code(), "54000");
    assert_eq!(xs(&core, &alice), vec![(1, 10), (2, 20), (3, 30)]);
    // 上限ちょうどなら通る。
    let limits = JoinLimits {
        max_output_rows: 3,
        ..JoinLimits::default()
    };
    exec_with(&core, &alice, &update_from_s_v(), &limits, None, 1_000_000).expect("at the limit");
}

#[test]
fn scanned_row_limit_rejects_with_54000_and_no_side_effects() {
    let (core, _g, alice) = setup();
    let err = exec_with(
        &core,
        &alice,
        &update_from_s_v(),
        &JoinLimits::default(),
        None,
        2,
    )
    .err()
    .expect("reject");
    assert_eq!(err.wire_code(), "54000");
    assert_eq!(xs(&core, &alice), vec![(1, 10), (2, 20), (3, 30)]);
}

#[test]
fn relation_and_byte_budget_limits_reject_with_54000() {
    let (core, _g, alice) = setup();
    let limits = JoinLimits {
        max_relations: 1,
        ..JoinLimits::default()
    };
    let err = exec_with(&core, &alice, &update_from_s_v(), &limits, None, 1_000_000)
        .err()
        .expect("relations");
    assert_eq!(err.wire_code(), "54000");
    let limits = JoinLimits {
        budget_cap: 16,
        ..JoinLimits::default()
    };
    let err = exec_with(&core, &alice, &update_from_s_v(), &limits, None, 1_000_000)
        .err()
        .expect("budget");
    assert_eq!(err.wire_code(), "54000");
    assert_eq!(xs(&core, &alice), vec![(1, 10), (2, 20), (3, 30)]);
}

#[test]
fn affected_row_limit_applies_to_deduplicated_target_rows() {
    let (core, _g, alice) = setup();
    seed_s(&core, &alice, 4, "a", 400);
    // 一致ペアは 4 件だが対象行は 2 件。上限 2 は通り、上限 1 は拒否される。
    exec_with(
        &core,
        &alice,
        &update_from_s_v(),
        &JoinLimits::default(),
        Some(2),
        1_000_000,
    )
    .expect("two target rows fit");
    let (core, _g, alice) = setup();
    let err = exec_with(
        &core,
        &alice,
        &update_from_s_v(),
        &JoinLimits::default(),
        Some(1),
        1_000_000,
    )
    .err()
    .expect("reject");
    assert_eq!(err.wire_code(), "54000");
    assert_eq!(xs(&core, &alice), vec![(1, 10), (2, 20), (3, 30)]);
}

#[test]
fn other_tenant_private_rows_never_affect_match_value_or_count() {
    let (core, _g, alice) = setup();
    let bob = ctx("bob");
    // bob の Private な source は alice の一致・SET 値・件数に現れない。
    seed_s(&core, &bob, 10, "c", 777);
    seed_s(&core, &bob, 11, "a", 888);
    seed_t(&core, &bob, 1, 5, Some("a"));
    let out = exec(&core, &alice, &update_from_s_v()).expect("update");
    assert_eq!(out.rows_affected, 2);
    assert_eq!(xs(&core, &alice), vec![(1, 100), (2, 300), (3, 30)]);
    // bob の対象行は更新されない。
    assert_eq!(xs(&core, &bob), vec![(1, 5)]);
}

#[test]
fn other_tenant_public_source_row_may_match_but_target_rows_stay_owned() {
    let (core, _g, alice) = setup();
    let bob = ctx("bob");
    // bob の Public な source 行は alice から読める（SELECT の JOIN と同じ読み取り意味論）。
    seed_s_public(&core, &bob, 20, "c", 555);
    seed_t(&core, &bob, 5, 50, Some("c"));
    let out = exec(&core, &alice, &update_from_s_v()).expect("update");
    assert_eq!(out.rows_affected, 3);
    assert_eq!(xs(&core, &alice), vec![(1, 100), (2, 300), (3, 555)]);
    // 対象側は所有行だけ。bob の行は更新されない。
    assert_eq!(xs(&core, &bob), vec![(5, 50)]);
}

#[test]
fn self_join_matches_against_the_statement_start_snapshot() {
    let (core, _g, alice) = setup();
    // 同じ表 t を source にする（t.k = t.k）。全行が自分自身に一致し、適用前の値で SET が決まる。
    let mut p = plan(
        vec![EquiKey {
            target_column: Some(2),
            source_column: Some(2),
        }],
        None,
    );
    p.source.table = "t".to_string();
    p.source.schema = t_schema();
    p.source.columns = vec![1, 2];
    let col = t_schema().columns[1].clone();
    // 結合ビュー: 対象 3 列 ++ source(x=3, k=4)。
    let combined = TableSchema::new(
        "t_t",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("x", ColumnType::BigInt, false),
            ColumnDef::new("k", ColumnType::Text, true),
            ColumnDef::new("t2_x", ColumnType::BigInt, false),
            ColumnDef::new("t2_k", ColumnType::Text, true),
        ],
    );
    let set = crate::sql::set_expr::bind_set_expr(
        "x",
        &Expr::Binary {
            op: BinOp::Add,
            lhs: Box::new(Expr::Ident("t2_x".to_string())),
            rhs: Box::new(Expr::Number("1".to_string())),
        },
        &col,
        &combined,
    )
    .expect("bind");
    p.update = Some(JoinedUpdateSet {
        literal_assignments: Vec::new(),
        items: vec![JoinedSetItem {
            column_index: 1,
            set,
        }],
    });
    let out = exec(&core, &alice, &p).expect("self join");
    assert_eq!(out.rows_affected, 3);
    assert_eq!(xs(&core, &alice), vec![(1, 11), (2, 21), (3, 31)]);
}

#[test]
fn autocommit_target_is_rejected_with_0a000() {
    let (core, _g, alice) = setup();
    let session = SessionState::default();
    let err = core
        .execute_joined_dml_in(
            WriteTarget::Autocommit(&core.storage),
            &alice,
            &session,
            &delete_using(),
            None,
            None,
        )
        .err()
        .expect("reject");
    assert_eq!(err.wire_code(), "0A000");
    assert_eq!(xs(&core, &alice).len(), 3);
}

#[test]
fn returning_that_references_the_source_is_rejected_with_0a000() {
    let (core, _g, alice) = setup();
    let mut p = delete_using();
    p.returning_references_source = true;
    let err = exec(&core, &alice, &p).err().expect("reject");
    assert_eq!(err.wire_code(), "0A000");
    assert_eq!(xs(&core, &alice).len(), 3);
}

#[test]
fn operation_id_is_gated_until_the_ledger_hash_is_wired() {
    let (core, _g) = new_core_with(LedgerMode::Ledgered);
    let alice = ctx("alice");
    seed_t_with_op(&core, &alice);
    let session = SessionState::default();
    let mut txn = core.new_session_transaction();
    let s = SessionState::default();
    txn.begin_implicit(&core.storage, &s).expect("begin");
    let write_txn = txn.write_txn().expect("write txn");
    let op = OperationId::parse("join-op-1").expect("op id");
    let err = core
        .execute_joined_dml_in(
            WriteTarget::InTxn(write_txn),
            &alice,
            &session,
            &delete_using(),
            Some(&op),
            None,
        )
        .err()
        .expect("gate");
    assert_eq!(err.wire_code(), "0A000");
    // operation_id を省略した Ledgered 構成は従来どおり 23502。
    let err = core
        .execute_joined_dml_in(
            WriteTarget::InTxn(write_txn),
            &alice,
            &session,
            &delete_using(),
            None,
            None,
        )
        .err()
        .expect("missing op id");
    assert_eq!(err.wire_code(), "23502");
}

fn seed_t_with_op(core: &EngineCore, ctx: &PolicyContext) {
    crate::tenant::insert_typed_row(
        &core.storage,
        "t",
        ctx,
        1,
        Visibility::Private,
        &[
            Value::Vector(vec![0.1, 0.2]),
            Value::BigInt(1),
            Value::Text("a".to_string()),
        ],
        &OperationId::parse("seed-t-1").expect("op id"),
    )
    .expect("seed");
}

#[test]
fn explicit_transaction_sees_uncommitted_source_rows_and_rolls_back() {
    let (core, _g, alice) = setup();
    let session = SessionState::default();
    let mut txn = core.new_session_transaction();
    txn.begin_implicit(&core.storage, &session).expect("begin");
    let write_txn = txn.write_txn().expect("write txn");
    // 同じ txn 内で追加した source 行（未 commit）が結合に見える。
    crate::tenant::insert_typed_row_unchecked(
        WriteTarget::InTxn(write_txn),
        "s",
        &alice,
        50,
        Visibility::Private,
        &[
            Value::Vector(vec![0.1, 0.2]),
            Value::Text("c".to_string()),
            Value::BigInt(4242),
        ],
        crate::recovery::ledger::LedgerWrite::Omitted,
        None,
        &[],
        false,
    )
    .expect("insert uncommitted source row");
    let out = core
        .execute_joined_dml_with_limits(
            WriteTarget::InTxn(write_txn),
            &alice,
            &session,
            &update_from_s_v(),
            None,
            None,
            &JoinLimits::default(),
            None,
            1_000_000,
        )
        .expect("update in txn");
    assert_eq!(out.rows_affected, 3);
    // commit せずに drop（ROLLBACK 相当）すると何も残らない。
    drop(txn);
    assert_eq!(xs(&core, &alice), vec![(1, 10), (2, 20), (3, 30)]);
}

/// 回帰（PR #2655 指摘）: source 列が TIME／TIMESTAMPTZ／INTERVAL／CHAR(n)／配列の `SET col = s.col` が
/// `cell_scalar` 未対応で `XX000` にならず、値が代入される。
#[test]
fn update_from_assigns_time_timestamptz_interval_bpchar_and_array_source_columns() {
    use crate::catalog::{ArrayElemType, ArrayType};
    fn cols(prefix: &str) -> Vec<ColumnDef> {
        let arr = ColumnType::Array(ArrayType::new(ArrayElemType::Integer, 8).expect("array ty"));
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("k", ColumnType::Text, true),
            ColumnDef::new(format!("{prefix}tm"), ColumnType::Time, true),
            ColumnDef::new(format!("{prefix}tz"), ColumnType::TimestampTz, true),
            ColumnDef::new(format!("{prefix}iv"), ColumnType::Interval, true),
            ColumnDef::new(format!("{prefix}bp"), ColumnType::Bpchar(4), true),
            ColumnDef::new(format!("{prefix}arr"), arr, true),
        ]
    }
    let tt = TableSchema::new("tt", cols(""));
    let ss = TableSchema::new("ss", cols(""));
    let (core, _g) = new_core();
    core.storage.create_table(&tt).expect("create tt");
    core.storage.create_table(&ss).expect("create ss");
    let alice = ctx("alice");
    run(
        &core,
        &alice,
        "INSERT INTO tt (id, embedding, k) VALUES (1, '[0.1,0.2]', 'a')",
    );
    run(
        &core,
        &alice,
        "INSERT INTO ss (id, embedding, k, tm, tz, iv, bp, arr) VALUES (1, '[0.1,0.2]', 'a', \
         '12:34:56', '2020-01-02 03:04:05+00', '1 day', 'ab', ARRAY[1, 2])",
    );
    // 結合ビュー: 対象列 ++ source の取り出し列（束縛用スキーマは s_ 接頭辞）。
    let mut combined = cols("");
    combined.extend(cols("s_").into_iter().skip(2));
    let combined = TableSchema::new("tt_ss", combined);
    let items = ["tm", "tz", "iv", "bp", "arr"]
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let col = tt.columns[2 + i].clone();
            let set = crate::sql::set_expr::bind_set_expr(
                name,
                &Expr::Ident(format!("s_{name}")),
                &col,
                &combined,
            )
            .expect("bind SET");
            JoinedSetItem {
                column_index: 2 + i,
                set,
            }
        })
        .collect();
    let plan = JoinedDmlPlan {
        target: JoinedDmlTarget {
            table: "tt".to_string(),
            schema: tt.clone(),
            metadata_filters: Vec::new(),
            expr_filters: Vec::new(),
            or_filters: Vec::new(),
        },
        source: JoinedDmlSource {
            table: "ss".to_string(),
            schema: ss,
            where_predicates: Vec::new(),
            columns: vec![2, 3, 4, 5, 6],
        },
        keys: vec![EquiKey {
            target_column: Some(1),
            source_column: Some(1),
        }],
        update: Some(JoinedUpdateSet {
            literal_assignments: Vec::new(),
            items,
        }),
        returning_references_source: false,
    };
    let out = exec(&core, &alice, &plan).expect("update");
    assert_eq!(out.rows_affected, 1);
    let got = core
        .execute_sql(&alice, "SELECT tm, tz, iv, bp, arr FROM tt LIMIT 10")
        .expect("select");
    let want = core
        .execute_sql(&alice, "SELECT tm, tz, iv, bp, arr FROM ss LIMIT 10")
        .expect("select");
    assert_eq!(got.rows.len(), 1);
    assert_eq!(got.rows[0].cells, want.rows[0].cells);
    assert!(got.rows[0].cells.iter().all(|c| !matches!(c, Cell::Null)));
}
