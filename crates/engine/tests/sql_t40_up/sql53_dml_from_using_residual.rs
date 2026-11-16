//! `UPDATE ... FROM`／`DELETE ... USING` の非等値・`OR` をまたぐ述語（残余述語）の SQL 経由の結合テスト
//! （SQL-53 (b)・RLS-10・RECOVER-10/11・Issue #3295。親 #3290）。
//!
//! 役割: 等値キーが取れない部分を対象行 × source 行のネストループで評価する経路を、三値論理（NULL を含む
//! 比較は一致しない）・数値リテラルと `IS [NOT] NULL` の葉・多重一致の決定性・自己結合・RLS・明示 Tx・
//! 台帳・上限（`54000`）・fail-closed の観点で固定する。期待値は PostgreSQL の意味論から手で求めた行集合。
//! ポインタ: `docs/design/update-from-delete-using.md`。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::join_limits::JoinSizeLimits;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn build(label: &str, tune: impl FnOnce(EngineCore) -> EngineCore) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    for (name, col) in [("t", "x"), ("s", "v")] {
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

fn tag(kind: &str, n: u64) -> String {
    match kind {
        "UPD" => format!("Update(UpdateOutcome {{ rows_affected: {n} }})"),
        _ => format!("Delete(DeleteOutcome {{ rows_affected: {n} }})"),
    }
}

fn op(sql: &str, id: &str) -> String {
    format!("{sql} USING OPERATION_ID '{id}'")
}

fn insert(core: &EngineCore, ctx: &PolicyContext, table: &str, id: u64, k: &str, n: Option<i64>) {
    let col = if table == "t" { "x" } else { "v" };
    let n = n.map_or("NULL".to_string(), |v| v.to_string());
    run(
        core,
        ctx,
        &format!(
            "INSERT INTO {table} (id, embedding, k, {col}) VALUES ({id}, '[0.1,0.2]', '{k}', {n})"
        ),
    )
    .expect("insert");
}

/// t: (1,a,10) (2,b,20) (3,c,30) (4,d,NULL)。s: (1,a,15) (2,b,20) (3,z,NULL)。
fn seed(core: &EngineCore, ctx: &PolicyContext) {
    for (id, k, x) in [
        (1, "a", Some(10)),
        (2, "b", Some(20)),
        (3, "c", Some(30)),
        (4, "d", None),
    ] {
        insert(core, ctx, "t", id, k, x);
    }
    for (id, k, v) in [(1, "a", Some(15)), (2, "b", Some(20)), (3, "z", None)] {
        insert(core, ctx, "s", id, k, v);
    }
}

fn rows_t(core: &EngineCore, ctx: &PolicyContext) -> String {
    run(core, ctx, "SELECT id, k, x FROM t ORDER BY id LIMIT 100").expect("scan t")
}

/// `seed` 直後の新しいコアで 1 文を実行し、タグと実行後の t の行を返す。
fn exec_fresh(label: &str, sql: &str) -> (String, String) {
    let alice = ctx("alice");
    let (core, _g) = new_core(label);
    seed(&core, &alice);
    let out = run(&core, &alice, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    (out, rows_t(&core, &alice))
}

/// 影響行数（`UPDATE n`／`DELETE n` の n）だけを確かめる。
fn assert_count(sql: &str, want: u64) {
    let kind = if sql.starts_with("UPDATE") {
        "UPD"
    } else {
        "DEL"
    };
    let (out, _) = exec_fresh("sql53-res-count", sql);
    assert_eq!(out, tag(kind, want), "{sql}");
}

/// キー無しの非等値。NULL を含む比較は一致しない（t4 の x は NULL、s3 の v は NULL）。
#[test]
fn keyless_non_equi_predicates_match_the_postgres_row_set() {
    // t.x < s.v: t1(10) < s1(15) だけ。t2(20)・t3(30) はどの s.v よりも小さくなく、NULL は一致しない。
    assert_count("DELETE FROM t USING s WHERE t.x < s.v", 1);
    // t.x >= s.v: t2(20>=15)・t3(30>=15)。t1(10) は不一致、t4 は NULL。
    assert_count("UPDATE t SET x = 0 FROM s WHERE t.x >= s.v", 2);
    // 末尾 `;`（psql の単独送信）でも同じ。
    assert_count("DELETE FROM t USING s WHERE t.x < s.v;", 1);
    let (_, rows) = exec_fresh("sql53-res-rows", "DELETE FROM t USING s WHERE t.x < s.v");
    assert!(!rows.contains("\"a\""), "{rows}");
    assert!(
        rows.contains("\"b\"") && rows.contains("\"c\"") && rows.contains("\"d\""),
        "{rows}"
    );
}

#[test]
fn equi_key_plus_residual_and_or_across_relations() {
    // k が一致する組のうち t.x < s.v: (t1,s1) だけ。
    assert_count("UPDATE t SET x = 0 FROM s WHERE t.k = s.k AND t.x < s.v", 1);
    // 一致後の OR: (t1,s1): t.x=10 → 真 / (t2,s2): s.v=20 → 真。
    assert_count(
        "UPDATE t SET x = 0 FROM s WHERE t.k = s.k AND (t.x = 10 OR s.v = 20)",
        2,
    );
    // relation をまたぐ OR: (t1,s1)・(t2,s2) は k 一致、(t2,s2) は x = v も真。対象行は 1 回だけ更新される。
    assert_count("UPDATE t SET x = 1 FROM s WHERE t.k = s.k OR t.x = s.v", 2);
    assert_count("DELETE FROM t USING s WHERE t.k = s.k OR t.x = s.v", 2);
    // 数値リテラル・IN・IS NULL を含む OR。
    assert_count(
        "DELETE FROM t USING s WHERE t.k = s.k OR t.x IN (30, 99)",
        3,
    );
}

/// OR をまたぐ述語は内部結合の意味論。source が空なら、対象側だけで真になる分岐があっても 0 行。
#[test]
fn or_with_empty_source_matches_nothing() {
    let alice = ctx("alice");
    let (core, _g) = new_core("sql53-res-empty-source");
    for (id, k, x) in [(1, "a", Some(10)), (2, "b", Some(20))] {
        insert(&core, &alice, "t", id, k, x);
    }
    let before = rows_t(&core, &alice);
    for sql in [
        "UPDATE t SET x = 1 FROM s WHERE t.k = s.k OR t.x = 10",
        "DELETE FROM t USING s WHERE t.x < s.v OR t.x = 10",
    ] {
        let kind = if sql.starts_with("UPDATE") {
            "UPD"
        } else {
            "DEL"
        };
        assert_eq!(run(&core, &alice, sql).expect(sql), tag(kind, 0), "{sql}");
    }
    assert_eq!(before, rows_t(&core, &alice));
}

/// NULL の真理値は通常の WHERE と一致する（`IS [NOT] NULL` を含む）。
#[test]
fn three_valued_logic_matches_plain_where() {
    // s3.v が NULL なので `s.v IS NULL` は全ての t 行と真の組を作る: t の 4 行すべて。
    assert_count(
        "UPDATE t SET x = 0 FROM s WHERE t.x < s.v OR s.v IS NULL",
        4,
    );
    // t4.x が NULL の行は `t.x IS NULL` で真。k 一致の t1・t2 と合わせて 3 行。
    assert_count("DELETE FROM t USING s WHERE t.k = s.k OR t.x IS NULL", 3);
    // NULL を含む比較は偽: t.x = s.v は (t2,s2) だけ。
    assert_count("DELETE FROM t USING s WHERE t.x = s.v", 1);
    // IS NOT NULL（対象側・source 側）。
    assert_count(
        "DELETE FROM t USING s WHERE t.k = s.k AND t.x IS NOT NULL",
        2,
    );
    assert_count("DELETE FROM t USING s WHERE t.k = s.k AND s.v IS NULL", 0);
    // 対象側だけの部分は、同じ述語の単一表 WHERE の結果と一致する。
    let alice = ctx("alice");
    let (core, _g) = new_core("sql53-res-plain");
    seed(&core, &alice);
    let plain = run(
        &core,
        &alice,
        "SELECT id FROM t WHERE x IS NULL OR x > 15 ORDER BY id LIMIT 10",
    )
    .expect("plain");
    assert!(
        plain.contains('2') && plain.contains('3') && plain.contains('4'),
        "{plain}"
    );
    // 同じ述語（対象側だけの OR）に等値キーを足すと、一致する t2(x=20>15) だけが対象になる。
    let sql = "DELETE FROM t USING s WHERE (t.x IS NULL OR t.x > 15) AND t.k = s.k";
    assert_eq!(run(&core, &alice, sql).expect(sql), tag("DEL", 1));
}

/// 数値リテラル（`>`・`= -1`・整数列に対する `= 1.5`・`IN`）。
#[test]
fn numeric_literals_follow_plain_where() {
    assert_count("DELETE FROM t USING s WHERE t.k = s.k AND t.x > 5", 2);
    assert_count("DELETE FROM t USING s WHERE t.k = s.k AND t.x = -1", 0);
    assert_count("DELETE FROM t USING s WHERE t.k = s.k AND t.x = 1.5", 0);
    assert_count(
        "DELETE FROM t USING s WHERE t.k = s.k AND t.x IN (10, 30)",
        1,
    );
    assert_count("DELETE FROM t USING s WHERE t.x < s.v AND s.v <= 15", 1);
    // 文字列列との数値比較は、通常の単一表 WHERE と同じ成否・エラーコード。
    let alice = ctx("alice");
    let (core, _g) = new_core("sql53-res-numeric-text");
    seed(&core, &alice);
    let plain = run(&core, &alice, "SELECT id FROM t WHERE k > 1 LIMIT 5");
    let joined = run(
        &core,
        &alice,
        "DELETE FROM t USING s WHERE t.k = s.k AND t.k > 1",
    );
    assert_eq!(plain.is_err(), joined.is_err());
    if let (Err(p), Err(j)) = (&plain, &joined) {
        assert_eq!(p.wire_code(), j.wire_code());
    }
}

/// 多重一致: 対象行は 1 回だけ更新され、採る source 行は残余が真になった走査位置の最初（決定的）。
#[test]
fn multiple_matches_take_the_first_true_source_row() {
    let alice = ctx("alice");
    let (core, _g) = new_core("sql53-res-first");
    seed(&core, &alice);
    // t1(10) は s1(15)・s2(20) の両方で真。先頭は s1 → x = 15。t2(20) は s 内に 20 より大きい v が
    // 無いので不変。
    let out = run(&core, &alice, "UPDATE t SET x = s.v FROM s WHERE t.x < s.v").expect("update");
    assert_eq!(out, tag("UPD", 1));
    let rows = rows_t(&core, &alice);
    assert!(rows.contains("15"), "{rows}");
    assert!(
        !rows.contains("Integer(10)") && !rows.contains("SignedInteger(10)"),
        "{rows}"
    );
}

/// 自己結合（別名で公開名を分ける）の非等値。
#[test]
fn self_join_non_equi() {
    let alice = ctx("alice");
    let (core, _g) = new_core("sql53-res-self");
    seed(&core, &alice);
    // t2.x = 30 の行（t3）より小さい t 行: t1(10)・t2(20)。
    let out = run(
        &core,
        &alice,
        "UPDATE t SET x = t2.x FROM t AS t2 WHERE t.x < t2.x AND t2.x = 30",
    )
    .expect("self join");
    assert_eq!(out, tag("UPD", 2));
}

/// RLS（P0）: 他テナントの Private 行は source 側にも現れず、他テナントの対象行は変わらない。
#[test]
fn other_tenant_rows_never_join_or_change() {
    let alice = ctx("alice");
    let bob = ctx("bob");
    let (core, _g) = new_core("sql53-res-rls");
    // alice: t(1,a,10)・s(1,a,5)。bob: s(10,a,999)（他テナント Private）と t(20,a,5)。
    insert(&core, &alice, "t", 1, "a", Some(10));
    insert(&core, &alice, "s", 1, "a", Some(5));
    insert(&core, &bob, "s", 10, "a", Some(999));
    insert(&core, &bob, "t", 20, "a", Some(5));
    let (before_a, before_b) = (rows_t(&core, &alice), rows_t(&core, &bob));
    // bob の s(999) が漏れていれば t1(10 < 999) が一致してしまう。alice の s は v=5 だけなので 0 件。
    for sql in [
        "UPDATE t SET x = 0 FROM s WHERE t.x < s.v",
        "DELETE FROM t USING s WHERE t.x < s.v OR s.v IS NULL AND t.k = 'zzz'",
    ] {
        let kind = if sql.starts_with("UPDATE") {
            "UPD"
        } else {
            "DEL"
        };
        assert_eq!(run(&core, &alice, sql).expect(sql), tag(kind, 0), "{sql}");
    }
    assert_eq!(before_a, rows_t(&core, &alice));
    assert_eq!(before_b, rows_t(&core, &bob));
    // alice が自分の s で一致を作れば、変わるのは alice の行だけ。
    let out = run(&core, &alice, "UPDATE t SET x = 0 FROM s WHERE t.x > s.v").expect("update");
    assert_eq!(out, tag("UPD", 1));
    assert_eq!(before_b, rows_t(&core, &bob));
}

/// 明示 Tx: COMMIT で反映・ROLLBACK で破棄・同一 Tx 内の read-your-writes。
#[test]
fn explicit_transaction_commit_and_rollback() {
    let alice = ctx("alice");
    let (core, _g) = new_core("sql53-res-txn");
    seed(&core, &alice);
    let sql = "DELETE FROM t USING s WHERE t.x < s.v";
    let before = rows_t(&core, &alice);
    for (finish, changed) in [("ROLLBACK", false), ("COMMIT", true)] {
        let mut session = SessionState::default();
        let mut txn = core.new_session_transaction();
        core.execute_sql_in_txn(&alice, &mut session, &mut txn, "BEGIN")
            .expect("begin");
        let out = core
            .execute_sql_in_txn(&alice, &mut session, &mut txn, sql)
            .map(|o| format!("{o:?}"))
            .expect("delete");
        assert_eq!(out, tag("DEL", 1));
        // read-your-writes: 同じ Tx の中では削除済みの行が見えない。
        let inside = core
            .execute_sql_in_txn(
                &alice,
                &mut session,
                &mut txn,
                "SELECT id FROM t ORDER BY id LIMIT 10",
            )
            .map(|o| format!("{o:?}"))
            .expect("select");
        assert!(!inside.contains("15") && inside != before, "{inside}");
        core.execute_sql_in_txn(&alice, &mut session, &mut txn, finish)
            .expect("finish");
        if changed {
            assert_ne!(before, rows_t(&core, &alice));
        } else {
            assert_eq!(before, rows_t(&core, &alice));
        }
    }
}

/// 台帳（RECOVER-10/11）: 再送は `23505` で二重適用されず、別文への再利用は `22023`、
/// 拒否された文は op id を消費しない。
#[test]
fn operation_id_resend_reuse_and_rejection() {
    let alice = ctx("alice");
    let (core, _g) = new_core("sql53-res-ledger");
    seed(&core, &alice);
    let sql = "UPDATE t SET x = 0 FROM s WHERE t.x >= s.v";
    assert_eq!(
        run(&core, &alice, &op(sql, "res-1")).expect("first"),
        tag("UPD", 2)
    );
    let after = rows_t(&core, &alice);
    assert_eq!(code(run(&core, &alice, &op(sql, "res-1"))), "23505");
    assert_eq!(
        code(run(
            &core,
            &alice,
            &op("DELETE FROM t USING s WHERE t.x < s.v", "res-1")
        )),
        "22023"
    );
    assert_eq!(after, rows_t(&core, &alice));
    // 拒否された文（VECTOR 列を参照する残余）は op id を消費しない。
    assert_eq!(
        code(run(
            &core,
            &alice,
            &op("UPDATE t SET x = 1 FROM s WHERE t.x < s.embedding", "res-2")
        )),
        "0A000"
    );
    assert_eq!(
        run(
            &core,
            &alice,
            &op("DELETE FROM t USING s WHERE t.k = s.k", "res-2")
        )
        .expect("unused"),
        tag("DEL", 2)
    );
}

/// 上限: 一致ペア数の上限超過は `54000` で副作用ゼロ。上限内なら成功する。
#[test]
fn pair_limit_is_enforced_without_side_effects() {
    let alice = ctx("alice");
    // t.x < s.v の一致ペアは (t1,s1)・(t1,s2) の 2 組。上限 1 は超過。
    let (core, _g) = build("sql53-res-limit", |c| {
        c.with_join_limits(JoinSizeLimits::new(2, 100_000, 1))
    });
    seed(&core, &alice);
    let before = rows_t(&core, &alice);
    for sql in [
        "UPDATE t SET x = 0 FROM s WHERE t.x < s.v",
        "DELETE FROM t USING s WHERE t.x < s.v",
    ] {
        assert_eq!(code(run(&core, &alice, sql)), "54000", "{sql}");
        assert_eq!(before, rows_t(&core, &alice), "{sql}");
    }
    let (core, _g) = build("sql53-res-limit-ok", |c| {
        c.with_join_limits(JoinSizeLimits::new(2, 100_000, 2))
    });
    seed(&core, &alice);
    assert_eq!(
        run(&core, &alice, "DELETE FROM t USING s WHERE t.x < s.v").expect("within the limit"),
        tag("DEL", 1)
    );
}

/// fail-closed: VECTOR 列・FROM 側の疑似列 id を使う残余は `0A000`、サブクエリ・`NOT`・`IS TRUE` は
/// `42601`。Describe と Execute のコードが一致し、行は変わらない。
// intentional: docs/design/update-from-delete-using.md
#[test]
fn unsupported_residual_forms_fail_closed() {
    let alice = ctx("alice");
    let (core, _g) = new_core("sql53-res-reject");
    seed(&core, &alice);
    let before = rows_t(&core, &alice);
    for (sql, want) in [
        ("UPDATE t SET x = 1 FROM s WHERE t.x < s.embedding", "0A000"),
        ("DELETE FROM t USING s WHERE t.x < s.id", "0A000"),
        ("DELETE FROM t USING s WHERE t.k = s.k OR s.id = 1", "0A000"),
        (
            "DELETE FROM t USING s WHERE t.x < s.v AND NOT t.k = s.k",
            "42601",
        ),
        (
            "DELETE FROM t USING s WHERE t.x < s.v AND t.x IN (SELECT 1)",
            "42601",
        ),
        (
            "DELETE FROM t USING s WHERE t.x < s.v AND t.x IS TRUE",
            "42601",
        ),
        // 等値キーも残余も無い片側だけの述語は従来どおり。
        ("DELETE FROM t USING s WHERE t.x IS NULL", "0A000"),
    ] {
        assert_eq!(code(run(&core, &alice, sql)), want, "{sql}");
        let parsed = core.parse_sql(sql);
        if let Ok(parsed) = parsed {
            assert_eq!(
                code(
                    core.describe_parsed_in_session(&SessionState::default(), &parsed)
                        .map(|m| format!("{m:?}"))
                ),
                want,
                "{sql}"
            );
        }
    }
    assert_eq!(before, rows_t(&core, &alice));
}

/// 残余を持つ形の `EXPLAIN` は成功し（`Internal` にならない）、行を変えない。
#[test]
fn explain_with_residual_succeeds_without_side_effects() {
    let alice = ctx("alice");
    let (core, _g) = new_core("sql53-res-explain");
    seed(&core, &alice);
    let before = rows_t(&core, &alice);
    for sql in [
        "EXPLAIN UPDATE t SET x = s.v FROM s WHERE t.x < s.v",
        "EXPLAIN DELETE FROM t USING s WHERE t.k = s.k OR t.x IS NULL",
    ] {
        run(&core, &alice, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    }
    assert_eq!(before, rows_t(&core, &alice));
}
