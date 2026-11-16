//! `UPDATE ... FROM`／`DELETE ... USING` の FROM 内 `JOIN ... ON` と多方向結合（SQL-53 (b)(3)(4)(5)・RLS-10・
//! RECOVER-10/11・SQL-28・Issue #3294。親 #3290）の結合テスト。
//!
//! 役割: source 側（FROM／USING）が 2 表以上、または `JOIN ... ON` の段を持つ形を SQL 経由で実行し、
//! PG 17 と同じ件数・値・NULL 補完になること、各リレーションが書き込むセッションの RLS で独立に走査される
//! こと、結合実行器の上限（入力行数・出力行数・リレーション数）が `54000` で副作用ゼロになること、
//! 台帳（再送冪等）・明示 Tx・自己結合・`RETURNING` が単一表 source と同じ契約で動くことを固定する。
//! 期待値の source 行は一意に決まるデータで固定し、PG の「不定」の範囲には依存しない
//! （多重一致だけは「結合出力の先頭の一致行」を採る本実装の決定的な定義を固定する）。
//! 上限は `EngineCore` のビルダーで渡すだけで環境変数・共有状態は使わない。
//! ポインタ: `docs/design/update-from-delete-using.md`（「多方向結合」節）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::join_limits::JoinSizeLimits;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::{Cell, ReturningOutcome};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

/// t(k, x)・s(k, v)・u(k, w) の 3 表を持つコアを作る。
fn build(label: &str, tune: impl FnOnce(EngineCore) -> EngineCore) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    for (name, col) in [("t", "x"), ("s", "v"), ("u", "w")] {
        storage
            .create_table(&TableSchema::new(
                name,
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(2), false),
                    ColumnDef::new("k", ColumnType::Text, false),
                    ColumnDef::new(col, ColumnType::Integer, true),
                ],
            ))
            .expect("create table");
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (tune(core), CleanupGuard(path))
}

fn new_core(label: &str) -> (EngineCore, CleanupGuard) {
    build(label, |c| c)
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn run(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Result<String, SqlSurfaceError> {
    core.execute_sql_in_session(ctx, &mut SessionState::default(), sql)
        .map(|o| format!("{o:?}"))
}

fn code(r: Result<String, SqlSurfaceError>) -> String {
    r.expect_err("must be rejected").wire_code().to_string()
}

fn insert(core: &EngineCore, ctx: &PolicyContext, table: &str, id: u64, k: &str, n: i64) {
    let col = match table {
        "t" => "x",
        "s" => "v",
        _ => "w",
    };
    run(
        core,
        ctx,
        &format!(
            "INSERT INTO {table} (id, embedding, k, {col}) VALUES ({id}, '[0.1,0.2]', '{k}', {n})"
        ),
    )
    .expect("insert");
}

/// t: (1,a,10) (2,b,20) (3,c,30)。s: (1,a,100) (2,b,200)。u: (1,a,7) (2,b,8) (3,z,9)。
fn seed(core: &EngineCore, ctx: &PolicyContext) {
    for (id, k, x) in [(1, "a", 10), (2, "b", 20), (3, "c", 30)] {
        insert(core, ctx, "t", id, k, x);
    }
    for (id, k, v) in [(1, "a", 100), (2, "b", 200)] {
        insert(core, ctx, "s", id, k, v);
    }
    for (id, k, w) in [(1, "a", 7), (2, "b", 8), (3, "z", 9)] {
        insert(core, ctx, "u", id, k, w);
    }
}

/// 呼び出し元から見える `t` の `(id, x)` を id 昇順で返す。`x` が NULL の行は `None`。
fn xs(core: &EngineCore, ctx: &PolicyContext) -> Vec<(u64, Option<i64>)> {
    match core
        .execute_sql_in_session(
            ctx,
            &mut SessionState::default(),
            "SELECT id, x FROM t ORDER BY id LIMIT 100",
        )
        .expect("scan t")
    {
        SqlOutcome::Query(r) => r
            .rows
            .iter()
            .map(|row| {
                let id = match row.cells.first() {
                    Some(Cell::Integer(n)) => *n,
                    other => panic!("unexpected id cell {other:?}"),
                };
                let x = match row.cells.get(1) {
                    Some(Cell::SignedInteger(n)) => Some(*n),
                    Some(Cell::Null) => None,
                    other => panic!("unexpected x cell {other:?}"),
                };
                (id, x)
            })
            .collect(),
        other => panic!("expected a query result, got {other:?}"),
    }
}

fn tag(kind: &str, n: u64) -> String {
    match kind {
        "UPD" => format!("Update(UpdateOutcome {{ rows_affected: {n} }})"),
        _ => format!("Delete(DeleteOutcome {{ rows_affected: {n} }})"),
    }
}

fn returning(
    core: &EngineCore,
    ctx: &PolicyContext,
    sql: &str,
) -> Result<ReturningOutcome, SqlSurfaceError> {
    match core.execute_sql_in_session(ctx, &mut SessionState::default(), sql)? {
        SqlOutcome::Returning(r) => Ok(r),
        other => panic!("{sql}: expected Returning, got {other:?}"),
    }
}

/// 明示 Tx で 1 文だけ実行し、結果（または拒否コード）を返してから `finish`（COMMIT／ROLLBACK）で終える。
fn explicit_one(
    core: &EngineCore,
    ctx: &PolicyContext,
    sql: &str,
    finish: &str,
) -> Result<String, String> {
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(ctx, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let out = core
        .execute_sql_in_txn(ctx, &mut session, &mut txn, sql)
        .map(|o| format!("{o:?}"))
        .map_err(|e| e.wire_code().to_string());
    core.execute_sql_in_txn(ctx, &mut session, &mut txn, finish)
        .expect("finish");
    out
}

const UPD_JOIN: &str = "UPDATE t SET x = u.w FROM s JOIN u ON s.k = u.k WHERE t.k = s.k";
const DEL_JOIN: &str = "DELETE FROM t USING s JOIN u ON s.k = u.k WHERE t.k = s.k";

// ---- 1. FROM／USING 内の JOIN ... ON ----

/// `FROM a JOIN b ON ...` の UPDATE は、結合した行の値で対象行を 1 回ずつ更新する。突き合わせられない
/// 対象行（`c`）は変わらない。autocommit と明示 Tx で同じ結果になる。
#[test]
fn update_from_join_on_sets_values_from_the_joined_row() {
    let alice = ctx("alice");
    for explicit in [false, true] {
        let (core, _g) = new_core("sql53-mj-upd");
        seed(&core, &alice);
        let out = if explicit {
            explicit_one(&core, &alice, UPD_JOIN, "COMMIT").expect("explicit")
        } else {
            run(&core, &alice, UPD_JOIN).expect("autocommit")
        };
        assert_eq!(out, tag("UPD", 2));
        assert_eq!(
            xs(&core, &alice),
            vec![(1, Some(7)), (2, Some(8)), (3, Some(30))]
        );
    }
}

/// 明示 Tx の ROLLBACK は結合 UPDATE の変更を残さない。
#[test]
fn explicit_rollback_discards_the_joined_update() {
    let alice = ctx("alice");
    let (core, _g) = new_core("sql53-mj-rollback");
    seed(&core, &alice);
    let out = explicit_one(&core, &alice, UPD_JOIN, "ROLLBACK").expect("update");
    assert_eq!(out, tag("UPD", 2));
    assert_eq!(
        xs(&core, &alice),
        vec![(1, Some(10)), (2, Some(20)), (3, Some(30))]
    );
}

/// `USING a JOIN b ON ...` の DELETE。source だけの述語（`u.k = 'a'`）は結合の後に評価される。
#[test]
fn delete_using_join_on_removes_matched_targets_once() {
    let alice = ctx("alice");
    let (core, _g) = new_core("sql53-mj-del");
    seed(&core, &alice);
    assert_eq!(run(&core, &alice, DEL_JOIN).expect("delete"), tag("DEL", 2));
    assert_eq!(xs(&core, &alice), vec![(3, Some(30))]);

    let (core, _g) = new_core("sql53-mj-del-pred");
    seed(&core, &alice);
    let sql = "DELETE FROM t USING s JOIN u ON s.k = u.k WHERE t.k = s.k AND u.k = 'a'";
    assert_eq!(run(&core, &alice, sql).expect("delete"), tag("DEL", 1));
    assert_eq!(xs(&core, &alice), vec![(2, Some(20)), (3, Some(30))]);
}

/// 結合種別: INNER・LEFT・RIGHT・FULL の ON 句を受理し、結合器と同じ意味論で一致を決める。
#[test]
fn join_kinds_follow_the_join_executor_semantics() {
    let alice = ctx("alice");
    for kind in ["INNER", "LEFT", "RIGHT", "FULL OUTER"] {
        let (core, _g) = new_core("sql53-mj-kinds");
        seed(&core, &alice);
        let sql = format!("DELETE FROM t USING s {kind} JOIN u ON s.k = u.k WHERE t.k = s.k");
        // RIGHT／FULL で加わる u だけの行は s.k が NULL で t.k = s.k に一致しないため、件数は変わらない。
        assert_eq!(
            run(&core, &alice, &sql).expect(&sql),
            tag("DEL", 2),
            "{sql}"
        );
    }
}

/// LEFT JOIN の NULL 補完: 一致しない行の source 値は NULL で、SET も NULL になる（キーが NULL なら
/// 一致しない）。NOT NULL 列への代入は `23502` で、行は変わらない。
#[test]
fn left_join_null_extension_sets_null_and_respects_not_null() {
    let alice = ctx("alice");
    let (core, _g) = new_core("sql53-mj-left");
    seed(&core, &alice);
    let sql = "UPDATE t SET x = u.w FROM s LEFT JOIN u ON s.k = u.k AND u.k = 'a' WHERE t.k = s.k";
    assert_eq!(run(&core, &alice, sql).expect("update"), tag("UPD", 2));
    assert_eq!(
        xs(&core, &alice),
        vec![(1, Some(7)), (2, None), (3, Some(30))]
    );

    let (core, _g) = new_core("sql53-mj-left-notnull");
    seed(&core, &alice);
    let before = xs(&core, &alice);
    let sql = "UPDATE t SET k = u.k FROM s LEFT JOIN u ON s.k = u.k AND u.k = 'a' WHERE t.k = s.k";
    assert_eq!(code(run(&core, &alice, sql)), "23502");
    assert_eq!(before, xs(&core, &alice));
}

// ---- 2. 多方向結合 ----

/// カンマ区切りの 3 表と `CROSS JOIN`。source 内部の結合を経由して対象行と突き合わせる。
#[test]
fn three_way_comma_and_cross_joins() {
    let alice = ctx("alice");
    let (core, _g) = new_core("sql53-mj-comma");
    seed(&core, &alice);
    let sql = "DELETE FROM t USING s, u WHERE t.k = s.k AND s.k = u.k";
    assert_eq!(run(&core, &alice, sql).expect("delete"), tag("DEL", 2));
    assert_eq!(xs(&core, &alice), vec![(3, Some(30))]);

    // CROSS JOIN: s × u(z) の各組が t と突き合わせられる。
    let (core, _g) = new_core("sql53-mj-cross");
    seed(&core, &alice);
    let sql = "UPDATE t SET x = u.w FROM s CROSS JOIN u WHERE t.k = s.k AND u.k = 'z'";
    assert_eq!(run(&core, &alice, sql).expect("update"), tag("UPD", 2));
    assert_eq!(
        xs(&core, &alice),
        vec![(1, Some(9)), (2, Some(9)), (3, Some(30))]
    );
}

/// 疑似列 `id` を結合キーに使う畳み込み source（符号なしの `Cell::Integer` を運ぶ）でも、SET が他の
/// source 列を読む正常な文が `XX000` にならず、一致した行だけが更新される。
#[test]
fn pseudo_id_join_key_with_folded_source_set_from_other_column() {
    let alice = ctx("alice");
    let (core, _g) = new_core("sql53-mj-idkey");
    seed(&core, &alice);
    let sql = "UPDATE t SET x = u.w FROM s JOIN u ON s.k = u.k WHERE t.id = s.id";
    assert_eq!(run(&core, &alice, sql).expect("update"), tag("UPD", 2));
    assert_eq!(
        xs(&core, &alice),
        vec![(1, Some(7)), (2, Some(8)), (3, Some(30))]
    );

    // 疑似列 `id` が結合の ON と WHERE の両方に現れる形（DELETE）。
    let (core, _g) = new_core("sql53-mj-idkey-del");
    seed(&core, &alice);
    let sql = "DELETE FROM t USING s JOIN u ON s.id = u.id WHERE t.id = u.id";
    assert_eq!(run(&core, &alice, sql).expect("delete"), tag("DEL", 2));
    assert_eq!(xs(&core, &alice), vec![(3, Some(30))]);
}

/// 4 表（対象 + source 3 表）の連鎖。`ON` で連結した source を対象表と突き合わせる。
#[test]
fn four_way_chain_with_self_join_inside_source() {
    let alice = ctx("alice");
    let (core, _g) = new_core("sql53-mj-four");
    seed(&core, &alice);
    // 自己結合: s が別名で 2 回現れる。
    let sql = "UPDATE t SET x = u.w FROM s s1 JOIN s s2 ON s1.k = s2.k JOIN u ON s2.k = u.k \
               WHERE t.k = s1.k";
    assert_eq!(run(&core, &alice, sql).expect("update"), tag("UPD", 2));
    assert_eq!(
        xs(&core, &alice),
        vec![(1, Some(7)), (2, Some(8)), (3, Some(30))]
    );
}

/// 対象表が source の中に別名で現れる自己結合。
#[test]
fn target_table_may_appear_inside_the_join_under_an_alias() {
    let alice = ctx("alice");
    let (core, _g) = new_core("sql53-mj-selfjoin");
    seed(&core, &alice);
    let sql = "DELETE FROM t USING t AS t2 JOIN s ON t2.k = s.k WHERE t.id = t2.id";
    assert_eq!(run(&core, &alice, sql).expect("delete"), tag("DEL", 2));
    assert_eq!(xs(&core, &alice), vec![(3, Some(30))]);
}

/// 多重一致: 対象行 1 件に結合出力の複数行が一致しても、影響行数は対象行数で、UPDATE の値は結合出力の
/// 先頭の一致行から採る（走査順。PG の「不定」の範囲内で決定的）。
#[test]
fn multiple_matches_update_each_target_once_with_the_first_joined_row() {
    let alice = ctx("alice");
    let (core, _g) = new_core("sql53-mj-multi");
    seed(&core, &alice);
    insert(&core, &alice, "s", 3, "a", 901);
    insert(&core, &alice, "u", 4, "a", 77);
    let sql = "UPDATE t SET x = s.v FROM s JOIN u ON s.k = u.k WHERE t.k = s.k";
    assert_eq!(run(&core, &alice, sql).expect("update"), tag("UPD", 2));
    assert_eq!(
        xs(&core, &alice),
        vec![(1, Some(100)), (2, Some(200)), (3, Some(30))]
    );
    // DELETE も対象行を 1 回だけ数える。
    let (core, _g) = new_core("sql53-mj-multi-del");
    seed(&core, &alice);
    insert(&core, &alice, "s", 3, "a", 901);
    insert(&core, &alice, "u", 4, "a", 77);
    assert_eq!(run(&core, &alice, DEL_JOIN).expect("delete"), tag("DEL", 2));
}

// ---- 3. 結合キーの無い形（純粋な交差結合） ----

/// 対象表と source の間に等値キーが無い形は交差結合として受理し、一致ペアの総数が上限内なら実行する。
/// source が空なら 0 件。source だけの述語で絞れる。
#[test]
fn keyless_cross_join_counts_every_target_row_once() {
    let alice = ctx("alice");
    let (core, _g) = new_core("sql53-mj-keyless");
    seed(&core, &alice);
    let sql = "UPDATE t SET x = s.v FROM s WHERE s.k = 'b'";
    assert_eq!(run(&core, &alice, sql).expect("update"), tag("UPD", 3));
    assert_eq!(
        xs(&core, &alice),
        vec![(1, Some(200)), (2, Some(200)), (3, Some(200))]
    );
    assert_eq!(
        run(&core, &alice, "DELETE FROM t USING s WHERE s.k = 'zzz'").expect("none"),
        tag("DEL", 0)
    );
    assert_eq!(xs(&core, &alice).len(), 3);
    // キー無しで source が 2 表の形。
    assert_eq!(
        run(&core, &alice, "DELETE FROM t USING s JOIN u ON s.k = u.k").expect("join"),
        tag("DEL", 3)
    );
    assert!(xs(&core, &alice).is_empty());
}

/// source が空の交差結合は 0 件（成功の no-op）。
#[test]
fn keyless_cross_join_with_an_empty_source_is_a_noop() {
    let alice = ctx("alice");
    let (core, _g) = new_core("sql53-mj-keyless-empty");
    for (id, k, x) in [(1, "a", 10), (2, "b", 20)] {
        insert(&core, &alice, "t", id, k, x);
    }
    assert_eq!(
        run(&core, &alice, "DELETE FROM t USING s JOIN u ON s.k = u.k").expect("delete"),
        tag("DEL", 0)
    );
    assert_eq!(xs(&core, &alice).len(), 2);
}

// ---- 4. 上限（54000・副作用ゼロ） ----

/// 結合実行器の入力行数・出力行数・リレーション数の上限は SELECT の JOIN と同じ契約で、超過は `54000`、
/// 対象表は変わらない。上限ちょうどは成功する。
#[test]
fn join_limits_reject_over_limit_without_side_effects() {
    let alice = ctx("alice");
    // 出力行数: s JOIN u は 2 行。上限 1 は超過。
    let (core, _g) = build("sql53-mj-lim-out", |c| {
        c.with_join_limits(JoinSizeLimits::new(3, 100_000, 1))
    });
    seed(&core, &alice);
    let before = xs(&core, &alice);
    for sql in [UPD_JOIN, DEL_JOIN] {
        assert_eq!(code(run(&core, &alice, sql)), "54000", "{sql}");
        assert_eq!(before, xs(&core, &alice), "{sql}");
    }
    let (core, _g) = build("sql53-mj-lim-out-ok", |c| {
        c.with_join_limits(JoinSizeLimits::new(3, 100_000, 2))
    });
    seed(&core, &alice);
    assert_eq!(run(&core, &alice, UPD_JOIN).expect("update"), tag("UPD", 2));

    // 入力行数: s・u の可視行数（2 と 3）。上限 2 は u（3 行）で超過。
    let (core, _g) = build("sql53-mj-lim-in", |c| {
        c.with_join_limits(JoinSizeLimits::new(3, 2, 100_000))
    });
    seed(&core, &alice);
    let before = xs(&core, &alice);
    for sql in [UPD_JOIN, DEL_JOIN] {
        assert_eq!(code(run(&core, &alice, sql)), "54000", "{sql}");
        assert_eq!(before, xs(&core, &alice), "{sql}");
    }

    // リレーション数: 対象 + source 2 表 = 3。上限 2 は超過、3 は成功。
    let (core, _g) = build("sql53-mj-lim-rel", |c| {
        c.with_join_limits(JoinSizeLimits::new(2, 100_000, 100_000))
    });
    seed(&core, &alice);
    let before = xs(&core, &alice);
    for sql in [UPD_JOIN, DEL_JOIN] {
        assert_eq!(code(run(&core, &alice, sql)), "54000", "{sql}");
        assert_eq!(before, xs(&core, &alice), "{sql}");
    }
}

/// 結合キーの無い交差結合は、一致ペアの累計（対象行 × source 行）が出力行数の上限を超えたら `54000`。
#[test]
fn keyless_cross_join_pairs_are_bounded_by_the_output_limit() {
    let alice = ctx("alice");
    // 対象 3 行 × source 2 行 = 6 ペア。上限 5 は超過、6 は成功。
    let (core, _g) = build("sql53-mj-lim-pairs", |c| {
        c.with_join_limits(JoinSizeLimits::new(2, 100_000, 5))
    });
    seed(&core, &alice);
    let before = xs(&core, &alice);
    for sql in [
        "DELETE FROM t USING s",
        "UPDATE t SET x = s.v FROM s",
        "DELETE FROM t USING s, u WHERE s.k = u.k AND t.k = 'a'",
    ] {
        let r = run(&core, &alice, sql);
        // 最後の形は source が 2 表で relation 上限 2 を超えるため同じ 54000。
        assert_eq!(code(r), "54000", "{sql}");
        assert_eq!(before, xs(&core, &alice), "{sql}");
    }
    let (core, _g) = build("sql53-mj-lim-pairs-ok", |c| {
        c.with_join_limits(JoinSizeLimits::new(2, 100_000, 6))
    });
    seed(&core, &alice);
    assert_eq!(
        run(&core, &alice, "DELETE FROM t USING s").expect("delete"),
        tag("DEL", 3)
    );
}

/// 参照数の超過（対象表 + 8 個）は構文段で `54000`。
#[test]
fn too_many_table_references_in_join_chain_is_rejected() {
    let alice = ctx("alice");
    let (core, _g) = new_core("sql53-mj-refs");
    seed(&core, &alice);
    let before = xs(&core, &alice);
    let sql = "DELETE FROM t USING s a JOIN s b ON a.k = b.k JOIN s c ON b.k = c.k \
               JOIN s d ON c.k = d.k JOIN s e ON d.k = e.k JOIN s f ON e.k = f.k \
               JOIN s g ON f.k = g.k JOIN s h ON g.k = h.k WHERE t.k = a.k";
    assert_eq!(code(run(&core, &alice, sql)), "54000");
    assert_eq!(before, xs(&core, &alice));
}

// ---- 5. RLS ----

/// RLS-10: 結合の各リレーションは書き込むセッションの RLS で独立に走査される。他テナントの Private な行は
/// どの source 位置でも一致・件数・結果に現れない（他テナントの行の有無で結果が同一）。
#[test]
fn other_tenant_private_rows_never_join_in_any_source_position() {
    let alice = ctx("alice");
    let bob = ctx("bob");
    for explicit in [false, true] {
        let (clean, _g1) = new_core("sql53-mj-rls-clean");
        seed(&clean, &alice);
        let (noisy, _g2) = new_core("sql53-mj-rls-noisy");
        seed(&noisy, &alice);
        // bob の行: c に一致する s・u、a に一致する別の u、自分の t。
        insert(&noisy, &bob, "s", 10, "c", 777);
        insert(&noisy, &bob, "u", 11, "c", 888);
        insert(&noisy, &bob, "u", 12, "a", 999);
        insert(&noisy, &bob, "t", 20, "a", 5);
        let (a, b) = if explicit {
            (
                explicit_one(&clean, &alice, UPD_JOIN, "COMMIT").expect("clean"),
                explicit_one(&noisy, &alice, UPD_JOIN, "COMMIT").expect("noisy"),
            )
        } else {
            (
                run(&clean, &alice, UPD_JOIN).expect("clean"),
                run(&noisy, &alice, UPD_JOIN).expect("noisy"),
            )
        };
        assert_eq!(a, b, "affected count must not depend on other tenants");
        assert_eq!(a, tag("UPD", 2));
        assert_eq!(xs(&clean, &alice), xs(&noisy, &alice));
        assert_eq!(xs(&noisy, &bob), vec![(20, Some(5))]);

        // 3 表のカンマ形でも同じ。
        let sql = "DELETE FROM t USING s, u WHERE t.k = s.k AND s.k = u.k";
        assert_eq!(
            run(&clean, &alice, sql).expect("clean"),
            run(&noisy, &alice, sql).expect("noisy")
        );
        assert_eq!(xs(&noisy, &bob), vec![(20, Some(5))]);
    }
}

/// 他テナントしか持たない表が source の途中にあると、結合は 0 件（成功の no-op）で、他テナントの存在は
/// エラーにも件数にも現れない。
#[test]
fn a_source_table_owned_only_by_another_tenant_matches_nothing() {
    let alice = ctx("alice");
    let bob = ctx("bob");
    let (core, _g) = new_core("sql53-mj-rls-only-other");
    for (id, k, x) in [(1, "a", 10), (2, "b", 20)] {
        insert(&core, &alice, "t", id, k, x);
        insert(&core, &alice, "s", id, k, 100);
    }
    insert(&core, &bob, "u", 1, "a", 7);
    insert(&core, &bob, "u", 2, "b", 8);
    assert_eq!(run(&core, &alice, UPD_JOIN).expect("update"), tag("UPD", 0));
    assert_eq!(run(&core, &alice, DEL_JOIN).expect("delete"), tag("DEL", 0));
    assert_eq!(xs(&core, &alice), vec![(1, Some(10)), (2, Some(20))]);
}

// ---- 6. RETURNING ----

/// RETURNING で 2 番目・3 番目の source 表の列と `*`・NULL 補完を返す。
#[test]
fn returning_reads_columns_of_every_joined_source_table() {
    let alice = ctx("alice");
    let (core, _g) = new_core("sql53-mj-ret");
    seed(&core, &alice);
    let r = returning(
        &core,
        &alice,
        "UPDATE t SET x = 1 FROM s JOIN u ON s.k = u.k WHERE t.k = s.k RETURNING t.id, s.v, u.w",
    )
    .expect("returning");
    assert_eq!(r.rows_affected, 2);
    let rows: Vec<Vec<Cell>> = r.result.rows.iter().map(|r| r.cells.clone()).collect();
    assert_eq!(
        rows,
        vec![
            vec![
                Cell::Integer(1),
                Cell::SignedInteger(100),
                Cell::SignedInteger(7)
            ],
            vec![
                Cell::Integer(2),
                Cell::SignedInteger(200),
                Cell::SignedInteger(8)
            ],
        ]
    );

    // NULL 補完側は NULL。疑似列 id・`*` も返せる。
    let (core, _g) = new_core("sql53-mj-ret-left");
    seed(&core, &alice);
    let r = returning(
        &core,
        &alice,
        "DELETE FROM t USING s LEFT JOIN u ON s.k = u.k AND u.k = 'a' WHERE t.k = s.k \
         RETURNING t.id, u.w, u.id",
    )
    .expect("returning");
    let rows: Vec<Vec<Cell>> = r.result.rows.iter().map(|r| r.cells.clone()).collect();
    assert_eq!(
        rows,
        vec![
            vec![Cell::Integer(1), Cell::SignedInteger(7), Cell::Integer(1)],
            vec![Cell::Integer(2), Cell::Null, Cell::Null],
        ]
    );
    let (core, _g) = new_core("sql53-mj-ret-star");
    seed(&core, &alice);
    let r = returning(
        &core,
        &alice,
        "DELETE FROM t USING s, u WHERE t.k = s.k AND s.k = u.k RETURNING *",
    )
    .expect("returning star");
    // t（id, embedding, k, x）→ s（id, embedding, k, v）→ u（id, embedding, k, w）。
    assert_eq!(r.result.columns.len(), 12);
    assert_eq!(r.result.rows.len(), 2);
}

// ---- 7. 台帳（再送冪等） ----

fn op(sql: &str, id: &str) -> String {
    format!("{sql} USING OPERATION_ID '{id}'")
}

/// 同じ op id の再送は `23505`、別文への再利用は `22023`。拒否された文は op id を消費しない。
#[test]
fn operation_id_resend_and_reuse_follow_the_ledger_contract() {
    let alice = ctx("alice");
    let (core, _g) = new_core("sql53-mj-ledger");
    seed(&core, &alice);
    assert_eq!(
        run(&core, &alice, &op(UPD_JOIN, "mj-1")).expect("first"),
        tag("UPD", 2)
    );
    assert_eq!(code(run(&core, &alice, &op(UPD_JOIN, "mj-1"))), "23505");
    assert_eq!(code(run(&core, &alice, &op(DEL_JOIN, "mj-1"))), "22023");
    // 拒否された文（ON が対象表を参照）は op id を消費しない。
    let bad = "DELETE FROM t USING s JOIN u ON t.k = u.k WHERE t.k = s.k";
    assert_eq!(code(run(&core, &alice, &op(bad, "mj-2"))), "42P01");
    assert_eq!(
        run(&core, &alice, &op(DEL_JOIN, "mj-2")).expect("unused id"),
        tag("DEL", 2)
    );
}

// ---- 8. fail-closed の拒否 ----

/// 未対応・不正な形は固定の SQLSTATE で拒否され、行は変わらない。
// intentional: docs/design/update-from-delete-using.md
// tracked: #3295
#[test]
fn unsupported_and_invalid_join_forms_fail_closed_without_side_effects() {
    let alice = ctx("alice");
    let (core, _g) = new_core("sql53-mj-reject");
    seed(&core, &alice);
    let before = xs(&core, &alice);
    for (sql, want) in [
        // ON が対象表を参照する（PG と同じく拒否）
        (
            "DELETE FROM t USING s JOIN u ON t.k = u.k WHERE t.k = s.k",
            "42P01",
        ),
        (
            "DELETE FROM t USING s JOIN u ON s.k = u.k AND x = 'q' WHERE t.k = s.k",
            "42703",
        ),
        // JOIN ... USING・NATURAL JOIN・カンマの後の明示 JOIN（意図的な拒否）
        (
            "DELETE FROM t USING s JOIN u USING (k) WHERE t.k = s.k",
            "0A000",
        ),
        (
            "DELETE FROM t USING s NATURAL JOIN u WHERE t.k = s.k",
            "0A000",
        ),
        (
            "DELETE FROM t USING s, u JOIN s s2 ON u.k = s2.k WHERE t.k = s.k",
            "0A000",
        ),
        // 派生表・関数呼び出し
        (
            "DELETE FROM t USING s JOIN (SELECT k FROM u) q ON s.k = q.k WHERE t.k = s.k",
            "42601",
        ),
        // 対象表と source をまたぐ非等値・OR（tracked: #3295）
        (
            "DELETE FROM t USING s JOIN u ON s.k = u.k WHERE t.k = s.k AND t.x < u.w",
            "0A000",
        ),
        (
            "DELETE FROM t USING s JOIN u ON s.k = u.k WHERE t.k = s.k OR t.k = u.k",
            "0A000",
        ),
        // 未知の表・列
        (
            "DELETE FROM t USING s JOIN nope ON s.k = nope.k WHERE t.k = s.k",
            "42P01",
        ),
        (
            "DELETE FROM t USING s JOIN u ON s.k = u.nope WHERE t.k = s.k",
            "42703",
        ),
        // 型クラスの不一致
        (
            "DELETE FROM t USING s JOIN u ON s.k = u.w WHERE t.k = s.k",
            "42804",
        ),
    ] {
        assert_eq!(code(run(&core, &alice, sql)), want, "{sql}");
        assert_eq!(before, xs(&core, &alice), "rows changed by {sql}");
    }
}

/// Describe（拡張クエリ）と Execute は同じ拒否コードを返し、受理形は Describe も通る。
#[test]
fn describe_and_execute_agree_on_join_forms() {
    let (core, _g) = new_core("sql53-mj-describe");
    let session = SessionState::default();
    for sql in [
        UPD_JOIN,
        DEL_JOIN,
        "DELETE FROM t USING s, u WHERE s.k = u.k",
    ] {
        let parsed = core.parse_sql(sql).expect("parse");
        core.describe_parsed_in_session(&session, &parsed)
            .unwrap_or_else(|e| panic!("describe failed for {sql}: {e:?}"));
    }
    for (sql, want) in [
        (
            "DELETE FROM t USING s JOIN u ON t.k = u.k WHERE t.k = s.k",
            "42P01",
        ),
        (
            "DELETE FROM t USING s JOIN u ON s.k = u.w WHERE t.k = s.k",
            "42804",
        ),
    ] {
        let parsed = core.parse_sql(sql).expect("parse");
        let got = match core.describe_parsed_in_session(&session, &parsed) {
            Ok(v) => panic!("expected an error for {sql}, got {v:?}"),
            Err(e) => e.wire_code().to_string(),
        };
        assert_eq!(got, want, "{sql}");
    }
}

/// `search_path` が `public` を隠すセッションでは、JOIN の先の非修飾表も `42P01` のゲートに掛かる。
#[test]
fn search_path_gate_covers_tables_added_by_join() {
    let alice = ctx("alice");
    let (core, _g) = new_core("sql53-mj-searchpath");
    seed(&core, &alice);
    let mut session = SessionState::default();
    core.execute_sql_in_session(&alice, &mut session, "SET search_path TO other")
        .expect("set search_path");
    let before = xs(&core, &alice);
    let sql = "DELETE FROM public.t USING public.s JOIN u ON s.k = u.k WHERE t.k = s.k";
    let err = core
        .execute_sql_in_session(&alice, &mut session, sql)
        .expect_err("gate");
    assert_eq!(err.wire_code(), "42P01", "{sql}");
    assert_eq!(before, xs(&core, &alice));
}
