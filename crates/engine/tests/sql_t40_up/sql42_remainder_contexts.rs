//! `%`（剰余）演算子を各式文脈（WHERE・ORDER BY・GROUP BY・HAVING・CHECK・UDF・UPDATE SET）で
//! 使える形を固定する結合テスト（SQL-42 のポインタのみ。Issue #3274、親 #3272）。
//! 構文・型付き評価の基礎は `sql42_remainder_operator.rs`（Issue #3273）が担い、本ファイルは
//! 文脈ごとの先読み・束縛環境の差で `%` だけ通らなくならないこと、未対応の形が fail-closed で
//! 拒否されること、値依存エラー（22012・23514）が自テナントの可視行にだけ出ること（他テナントの
//! 行の有無を応答から推測できないこと）を `EngineCore::execute_sql_in_session` 経由で確かめる。
//! 素の走査の `ORDER BY` が算術で始まるキーを `+ - * /` と同じく 42601 で拒否する既存制限は
//! `%` にも同様に適用される（拒否は意図した制限であり `tracked:` の受け皿は未起票）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::{Cell, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn run_opt(
    core: &EngineCore,
    tenant: &str,
    sql: &str,
) -> Result<SqlOutcome, engine::sql::allowlist::SqlSurfaceError> {
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(&ctx_for(tenant), &mut s, sql)
}

fn ok(core: &EngineCore, tenant: &str, sql: &str) -> SqlOutcome {
    run_opt(core, tenant, sql).unwrap_or_else(|e| panic!("{sql:?} should succeed: {e:?}"))
}

fn query(core: &EngineCore, tenant: &str, sql: &str) -> QueryResult {
    match ok(core, tenant, sql) {
        SqlOutcome::Query(r) => r,
        other => panic!("expected Query for {sql:?}, got {other:?}"),
    }
}

fn code(core: &EngineCore, tenant: &str, sql: &str) -> String {
    match run_opt(core, tenant, sql) {
        Ok(o) => panic!("{sql:?} should be rejected, got {o:?}"),
        Err(e) => e.wire_code().to_string(),
    }
}

/// 成功なら `"ok"`、失敗なら `wire_code`（応答が他テナントの行に依存しないことの比較用）。
fn outcome_code(core: &EngineCore, tenant: &str, sql: &str) -> String {
    match run_opt(core, tenant, sql) {
        Ok(_) => "ok".to_string(),
        Err(e) => e.wire_code().to_string(),
    }
}

/// 結果行の `id` を返却順のまま集める（ORDER BY の順序検査用）。
fn ids_in_order(r: &QueryResult) -> Vec<u64> {
    r.rows.iter().map(|row| row.id).collect()
}

fn ids_sorted(r: &QueryResult) -> Vec<u64> {
    let mut v = ids_in_order(r);
    v.sort_unstable();
    v
}

fn signed(c: &Cell) -> i64 {
    match c {
        Cell::SignedInteger(i) => *i,
        Cell::Integer(i) => i64::try_from(*i).expect("fits i64"),
        other => panic!("expected integer cell, got {other:?}"),
    }
}

/// 各行の (先頭列, 2 列目) を整数で集める。
fn pairs(r: &QueryResult) -> Vec<(i64, i64)> {
    r.rows
        .iter()
        .map(|row| (signed(&row.cells[0]), signed(&row.cells[1])))
        .collect()
}

/// 列 `col` の値を `id` 昇順で返す。
fn col_by_id(core: &EngineCore, tenant: &str, col: &str) -> Vec<i64> {
    let r = query(
        core,
        tenant,
        &format!("SELECT {col} FROM t ORDER BY id LIMIT 100"),
    );
    r.rows.iter().map(|row| signed(&row.cells[0])).collect()
}

fn open_core(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

/// t1: id 1..=5 の n = 1, 7, -7, 4, 6（b・w も同値）。t2: id 9 の n = 0（`%` の結果や
/// 0 除算の有無に現れてはならない他テナントの行）。
fn setup(label: &str) -> (EngineCore, std::path::PathBuf, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "t",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("n", ColumnType::Integer, false),
                ColumnDef::new("b", ColumnType::BigInt, false),
                ColumnDef::new("w", ColumnType::SmallInt, false),
                ColumnDef::new("r", ColumnType::Real, false),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    for (id, n) in [(1u64, 1i64), (2, 7), (3, -7), (4, 4), (5, 6)] {
        ok(
            &core,
            "t1",
            &format!(
                "INSERT INTO t (id, embedding, n, b, w, r) VALUES \
                 ({id}, '[0.{id},0.1]', {n}, {n}, {n}, 1.5) USING OPERATION_ID 'seed-{id}'"
            ),
        );
    }
    ok(
        &core,
        "t2",
        "INSERT INTO t (id, embedding, n, b, w, r) VALUES \
         (9, '[0.9,0.1]', 0, 0, 0, 1.5) USING OPERATION_ID 'seed-other'",
    );
    (core, path.clone(), CleanupGuard(path))
}

#[test]
fn where_accepts_remainder_across_integer_types_and_knn() {
    let (core, _p, _g) = setup("sql42-rem-where");
    for (sql, expected) in [
        ("SELECT id FROM t WHERE n % 2 = 0 LIMIT 10", vec![4, 5]),
        (
            "SELECT id FROM t WHERE (n % 3) = 1 AND b % 2 = 1 LIMIT 10",
            vec![1, 2],
        ),
        (
            "SELECT id FROM t WHERE w % 4 <> 0 LIMIT 10",
            vec![1, 2, 3, 5],
        ),
        ("SELECT id FROM t WHERE id % 2 = 0 LIMIT 10", vec![2, 4]),
    ] {
        assert_eq!(ids_sorted(&query(&core, "t1", sql)), expected, "{sql}");
    }
    // kNN との組み合わせ。他テナントの id 9（n = 0）は偶数でも現れない。
    let r = query(
        &core,
        "t1",
        "SELECT id FROM t WHERE n % 2 = 1 ORDER BY embedding <=> '[0.1,0.1]' LIMIT 2",
    );
    assert_eq!(ids_sorted(&r), vec![1, 2]);
}

#[test]
fn order_by_expression_keys_accept_remainder() {
    let (core, _p, _g) = setup("sql42-rem-order");
    // abs(n) % 3: id5→0、id1→1、id2→1、id3→1、id4→1（同値は id 昇順）。
    let r = query(
        &core,
        "t1",
        "SELECT id FROM t ORDER BY abs(n) % 3, id LIMIT 10",
    );
    assert_eq!(ids_in_order(&r), vec![5, 1, 2, 3, 4]);
    let r = query(
        &core,
        "t1",
        "SELECT id FROM t ORDER BY CASE WHEN n % 2 = 0 THEN 0 ELSE 1 END, id LIMIT 10",
    );
    assert_eq!(ids_in_order(&r), vec![4, 5, 1, 2, 3]);
    // 序数・別名でも `%` の投影値で並ぶ（PostgreSQL 17 では -7 % 3 = -1）。
    for sql in [
        "SELECT n % 3 AS m FROM t ORDER BY 1 LIMIT 10",
        "SELECT n % 3 AS m FROM t ORDER BY m LIMIT 10",
    ] {
        let r = query(&core, "t1", sql);
        let vals: Vec<i64> = r.rows.iter().map(|row| signed(&row.cells[0])).collect();
        assert_eq!(vals, vec![-1, 0, 1, 1, 1], "{sql}");
    }
}

/// 素の走査の `ORDER BY` が算術で始まるキーを拒否する既存制限は `%` にも `* /` と同じコードで
/// 適用する（`%` だけ受理範囲を広げない。fail-closed）。
/// intentional: docs/design/order-by-having-expressions.md
/// 根拠の節: 広域取得の ORDER BY 式キー
#[test]
fn plain_scan_order_by_arithmetic_key_is_rejected_alike() {
    let (core, _p, _g) = setup("sql42-rem-order-reject");
    for sql in [
        "SELECT id FROM t ORDER BY n % 2 LIMIT 5",
        "SELECT id FROM t ORDER BY n * 2 LIMIT 5",
        "SELECT id FROM t ORDER BY n / 2 LIMIT 5",
    ] {
        assert_eq!(code(&core, "t1", sql), "42601", "{sql}");
    }
}

#[test]
fn group_by_having_accept_remainder_keys_within_tenant() {
    let (core, _p, _g) = setup("sql42-rem-group");
    // n % 3: id1→1、id2→1、id3→-1、id4→1、id5→0。他テナントの n = 0 は 0 の群に混ざらない。
    for sql in [
        "SELECT n % 3, count(*) FROM t GROUP BY n % 3 ORDER BY 1",
        "SELECT n % 3, count(*) FROM t GROUP BY (n % 3) ORDER BY 1",
    ] {
        let r = query(&core, "t1", sql);
        assert_eq!(pairs(&r), vec![(-1, 1), (0, 1), (1, 3)], "{sql}");
    }
    let r = query(
        &core,
        "t1",
        "SELECT n % 3, count(*) FROM t GROUP BY n % 3 ORDER BY n % 3 DESC",
    );
    assert_eq!(pairs(&r), vec![(1, 3), (0, 1), (-1, 1)]);
    // HAVING でも集計と `%` を組み合わせられる。
    let r = query(
        &core,
        "t1",
        "SELECT n % 3, count(*) FROM t GROUP BY n % 3 HAVING count(*) % 2 = 1 ORDER BY 1",
    );
    assert_eq!(pairs(&r), vec![(-1, 1), (0, 1), (1, 3)]);
    let r = query(
        &core,
        "t1",
        "SELECT n % 3, count(*) FROM t GROUP BY n % 3 HAVING n % 3 > 0 ORDER BY 1",
    );
    assert_eq!(pairs(&r), vec![(1, 3)]);
    // 投影とキーの式が一致しなければ既存の照合拒否（42803）と同じ。
    assert_eq!(
        code(&core, "t1", "SELECT n % 2, count(*) FROM t GROUP BY n % 3"),
        "42803"
    );
}

#[test]
fn check_constraint_with_remainder_is_enforced_and_survives_reopen() {
    let path = unique_db_path("sql42-rem-check");
    let _g = CleanupGuard(path.clone());
    {
        let core = open_core(&path);
        ok(&core, "t1", "CREATE TABLE c (n INTEGER, CHECK (n % 2 = 0))");
        ok(
            &core,
            "t1",
            "INSERT INTO c (n) VALUES (4) USING OPERATION_ID 'c1'",
        );
        assert_eq!(
            code(
                &core,
                "t1",
                "INSERT INTO c (n) VALUES (5) USING OPERATION_ID 'c2'"
            ),
            "23514"
        );
        assert_eq!(
            code(
                &core,
                "t1",
                "UPDATE c SET n = n + 1 WHERE n = 4 USING OPERATION_ID 'c3'"
            ),
            "23514"
        );
    }
    // 保存された制約文字列（`%`）の再解析後も同じ制約が効く。
    let core = open_core(&path);
    assert_eq!(
        code(
            &core,
            "t1",
            "INSERT INTO c (n) VALUES (7) USING OPERATION_ID 'c4'"
        ),
        "23514"
    );
    ok(
        &core,
        "t1",
        "UPDATE c SET n = n + 2 WHERE n = 4 USING OPERATION_ID 'c5'",
    );
    let r = query(&core, "t1", "SELECT n FROM c LIMIT 10");
    assert_eq!(signed(&r.rows[0].cells[0]), 6);
}

/// float 型の `%` は PostgreSQL でも未定義（42883）。CHECK の定義時に拒否する。
/// PostgreSQL 17 にも浮動小数点の `%` は無い（Issue #3273）。
/// intentional: docs/design/modulo-operator.md
#[test]
fn check_constraint_rejects_float_remainder_and_zero_divisor_is_22012() {
    let (core, _p, _g) = setup("sql42-rem-check-reject");
    assert_eq!(
        code(&core, "t1", "CREATE TABLE c2 (r REAL, CHECK (r % 2 = 0))"),
        "42883"
    );
    ok(
        &core,
        "t1",
        "CREATE TABLE c3 (n INTEGER, CHECK (n % 0 = 0))",
    );
    assert_eq!(
        code(
            &core,
            "t1",
            "INSERT INTO c3 (n) VALUES (4) USING OPERATION_ID 'z1'"
        ),
        "22012"
    );
}

#[test]
fn udf_body_remainder_is_usable_from_where_projection_and_order_by() {
    let (core, _p, _g) = setup("sql42-rem-udf");
    // UDF はセッション単位の登録なので、同一セッションで定義と呼び出しを行う。
    let mut s = SessionState::default();
    s.allow_ddl();
    let ctx = ctx_for("t1");
    core.execute_sql_in_session(
        &ctx,
        &mut s,
        "CREATE FUNCTION f(x integer) RETURNS integer AS x % 3",
    )
    .expect("create function");
    let mut q = |sql: &str| match core
        .execute_sql_in_session(&ctx, &mut s, sql)
        .unwrap_or_else(|e| panic!("{sql:?} should succeed: {e:?}"))
    {
        SqlOutcome::Query(r) => r,
        other => panic!("expected Query for {sql:?}, got {other:?}"),
    };
    let r = q("SELECT id FROM t WHERE f(n) = 1 LIMIT 10");
    assert_eq!(ids_sorted(&r), vec![1, 2, 4]);
    let r = q("SELECT f(n) FROM t ORDER BY id LIMIT 10");
    let vals: Vec<i64> = r.rows.iter().map(|row| signed(&row.cells[0])).collect();
    assert_eq!(vals, vec![1, 1, -1, 1, 0]);
    let r = q("SELECT id FROM t ORDER BY f(n), id LIMIT 10");
    assert_eq!(ids_in_order(&r), vec![3, 5, 1, 2, 4]);
}

#[test]
fn update_set_remainder_covers_single_row_predicate_and_integer_types() {
    let (core, _p, _g) = setup("sql42-rem-update");
    // 単一行形。
    ok(
        &core,
        "t1",
        "UPDATE t SET n = n % 3 WHERE id = 2 USING OPERATION_ID 'u1'",
    );
    // 述語形（n > 0 の id 1・2・4・5。他テナントの n = 0 は対象外）。
    match ok(
        &core,
        "t1",
        "UPDATE t SET n = n % 3 WHERE n > 0 USING OPERATION_ID 'u2'",
    ) {
        SqlOutcome::Update(o) => assert_eq!(o.rows_affected, 4),
        other => panic!("expected Update, got {other:?}"),
    }
    assert_eq!(col_by_id(&core, "t1", "n"), vec![1, 1, -7, 1, 0]);
    // BIGINT・SMALLINT 列への代入（符号は被除数に従う）。
    ok(
        &core,
        "t1",
        "UPDATE t SET b = b % 3 WHERE id = 5 USING OPERATION_ID 'u3'",
    );
    ok(
        &core,
        "t1",
        "UPDATE t SET w = w % 4 WHERE id = 3 USING OPERATION_ID 'u4'",
    );
    assert_eq!(col_by_id(&core, "t1", "b"), vec![1, 7, -7, 4, 0]);
    assert_eq!(col_by_id(&core, "t1", "w"), vec![1, 7, -3, 4, 6]);
    // 0 除算は 22012。
    assert_eq!(
        code(
            &core,
            "t1",
            "UPDATE t SET n = n % 0 WHERE id = 1 USING OPERATION_ID 'u6'"
        ),
        "22012"
    );
    // 他テナントの行は一切変わらない。
    assert_eq!(col_by_id(&core, "t2", "n"), vec![0]);
}

/// 値依存エラー（22012）は自テナントの可視行にだけ出る。他テナントに `n = 0` の行があっても
/// 応答は変わらず、存在オラクルにならない。
#[test]
fn update_set_zero_divisor_is_not_an_existence_oracle() {
    let (with_other, _p1, _g1) = setup("sql42-rem-oracle-a");
    let (without_other, _p2, _g2) = setup("sql42-rem-oracle-b");
    ok(
        &without_other,
        "t2",
        "DELETE FROM t WHERE id = 9 USING OPERATION_ID 'del-other'",
    );
    // t1 の可視行に n = 0 は無い。述語 n >= 0 は他テナントの n = 0 の行も満たすため、可視性検査だけがそれを除外する（分離が欠ければ 22012 になる）。
    let sql = "UPDATE t SET n = 1 % n WHERE n >= 0 USING OPERATION_ID 'o1'";
    assert_eq!(outcome_code(&with_other, "t1", sql), "ok");
    assert_eq!(outcome_code(&without_other, "t1", sql), "ok");
    assert_eq!(
        col_by_id(&with_other, "t1", "n"),
        col_by_id(&without_other, "t1", "n")
    );
}

#[test]
fn update_set_resend_distinguishes_remainder_from_division() {
    let (core, _p, _g) = setup("sql42-rem-resend");
    let sql = "UPDATE t SET n = n % 3 WHERE id = 2 USING OPERATION_ID 'r1'";
    ok(&core, "t1", sql);
    // 同じ文の再送は適用済み（23505）で二重適用しない。
    assert_eq!(code(&core, "t1", sql), "23505");
    // `%` を `/` に変えた文を同じ operation_id で送ると内容不一致（22023）で副作用ゼロ。
    assert_eq!(
        code(
            &core,
            "t1",
            "UPDATE t SET n = n / 3 WHERE id = 2 USING OPERATION_ID 'r1'"
        ),
        "22023"
    );
    assert_eq!(col_by_id(&core, "t1", "n"), vec![1, 1, -7, 4, 6]);
}

/// 未対応の形は fail-closed で拒否する（Issue #3273）。ケースごとに根拠が異なる。
/// - 構文不完全形（`n % = 0`）は通常の構文エラー `42601`。
/// - float 列の `%`（`42883`）は PostgreSQL にも無い意図的な拒否で、根拠は設計記録 modulo-operator.md。
/// - 小数リテラルの `%`（`0A000`）は未対応分で、受け皿 #3534 で追跡する。
/// intentional: docs/design/modulo-operator.md
/// tracked: #3534
#[test]
fn unsupported_forms_are_rejected_fail_closed() {
    let (core, _p, _g) = setup("sql42-rem-reject");
    for (sql, expected) in [
        ("SELECT id FROM t WHERE n % = 0 LIMIT 10", "42601"),
        ("SELECT id FROM t WHERE r % 2 = 0 LIMIT 10", "42883"),
        ("SELECT id FROM t WHERE 0.3 % 0.1 = 0 LIMIT 10", "0A000"),
    ] {
        assert_eq!(code(&core, "t1", sql), expected, "{sql}");
    }
}
