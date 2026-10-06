//! 主キー・UNIQUE・FOREIGN KEY を構成する列への `ALTER COLUMN TYPE` の拡大変換
//! （`INTEGER`→`BIGINT`。TABLE-19・TABLE-17・TABLE-16・SQL-23・ERR-6、Issue #1402）の
//! 結合テスト。ポインタ: `docs/spec/04-behavior/data-model.md` TABLE-19。
//!
//! 正準キーは型タグ付きのため、片側だけ拡大した FK（参照元 `BIGINT`・参照先 `INTEGER` 等）
//! でも参照整合性検査・一意性検査の結果が変わらないことを production 経路
//! （`EngineCore::execute_sql_in_session`）で確認する。特に参照先の拡大後に
//! 参照中の親行の削除が `23503` になること（索引の型タグ不一致による fail-open の回帰）が要。
//!
//! Issue #1435: 混在 FK は宣言（CREATE TABLE・ADD FOREIGN KEY）でも作れる（TABLE-17・TABLE-22）。
//!
//! REAL／DOUBLE PRECISION（Issue #1434・TABLE-17・TABLE-19・TABLE-20）は FK 列になれない（D3）ため
//! FK 列の整数以外の混在は作れない。末尾の節で、その契約（宣言面が `42830`・副作用なし）と、
//! REAL の UNIQUE 列を拡大した後も値を保って一意性が働くことを固定する。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn new_core(label: &str) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn ddl_session() -> SessionState {
    let mut session = SessionState::default();
    session.allow_ddl();
    session
}

fn exec(
    core: &EngineCore,
    session: &mut SessionState,
    ctx: &PolicyContext,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(ctx, session, sql)
}

fn ok(core: &EngineCore, ctx: &PolicyContext, sql: &str) {
    let mut s = ddl_session();
    exec(core, &mut s, ctx, sql).unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
}

fn code(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> String {
    let mut s = ddl_session();
    exec(core, &mut s, ctx, sql)
        .expect_err(&format!("{sql} must fail"))
        .wire_code()
        .to_string()
}

fn ints(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Vec<i64> {
    let mut v: Vec<i64> = core
        .execute_sql(ctx, sql)
        .expect("select")
        .rows
        .into_iter()
        .map(|r| match r.cells.into_iter().next().expect("cell") {
            Cell::SignedInteger(v) => v,
            other => panic!("unexpected cell {other:?}"),
        })
        .collect();
    v.sort_unstable();
    v
}

const WIDE: i64 = i32::MAX as i64 + 1;

fn setup_parent_child(
    label: &str,
    parent_ty: &str,
    child_ty: &str,
    extra: &str,
) -> (EngineCore, std::path::PathBuf) {
    let (core, path) = new_core(label);
    let o = ctx("owner");
    ok(
        &core,
        &o,
        &format!("CREATE TABLE kp (k {parent_ty} PRIMARY KEY, u {parent_ty}, UNIQUE (u))"),
    );
    ok(
        &core,
        &o,
        &format!("CREATE TABLE kc (ref {child_ty}, FOREIGN KEY (ref) REFERENCES kp (k){extra})"),
    );
    for k in [1, 2, 3] {
        ok(
            &core,
            &o,
            &format!("INSERT INTO kp (id, k, u) VALUES ({k}, {k}, {k}0) USING OPERATION_ID 'p{k}'"),
        );
    }
    ok(
        &core,
        &o,
        "INSERT INTO kc (id, ref) VALUES (1, 1) USING OPERATION_ID 'c1'",
    );
    (core, path)
}

/// 主キー・UNIQUE・FK 参照元・FK 参照先のいずれの構成列でも拡大変換を受理し、値を保つ。
#[test]
fn widening_key_columns_is_accepted_and_preserves_values() {
    let (core, path) = setup_parent_child("akc-accept", "INTEGER", "INTEGER", "");
    let _g = CleanupGuard(path);
    let o = ctx("owner");
    for sql in [
        "ALTER TABLE kp ALTER COLUMN k TYPE BIGINT",
        "ALTER TABLE kp ALTER COLUMN u TYPE BIGINT",
        "ALTER TABLE kc ALTER COLUMN ref TYPE BIGINT",
    ] {
        ok(&core, &o, sql);
    }
    assert_eq!(ints(&core, &o, "SELECT k FROM kp LIMIT 10"), vec![1, 2, 3]);
    assert_eq!(
        ints(&core, &o, "SELECT u FROM kp LIMIT 10"),
        vec![10, 20, 30]
    );
    assert_eq!(ints(&core, &o, "SELECT ref FROM kc LIMIT 10"), vec![1]);
}

/// 参照先（親 PK）だけ拡大: 参照中の親行の削除・キー更新は `23503`、未参照は成功する。
#[test]
fn widening_referenced_column_keeps_referential_integrity() {
    let (core, path) = setup_parent_child("akc-parent", "INTEGER", "INTEGER", "");
    let _g = CleanupGuard(path);
    let o = ctx("owner");
    ok(&core, &o, "ALTER TABLE kp ALTER COLUMN k TYPE BIGINT");
    assert_eq!(
        code(
            &core,
            &o,
            "DELETE FROM kp WHERE id = 1 USING OPERATION_ID 'd1'"
        ),
        "23503"
    );
    assert_eq!(
        code(
            &core,
            &o,
            "UPDATE kp SET k = 100 WHERE id = 1 USING OPERATION_ID 'u1'"
        ),
        "23503"
    );
    ok(
        &core,
        &o,
        "DELETE FROM kp WHERE id = 2 USING OPERATION_ID 'd2'",
    );
    ok(
        &core,
        &o,
        "INSERT INTO kc (id, ref) VALUES (2, 3) USING OPERATION_ID 'c2'",
    );
    assert_eq!(
        code(
            &core,
            &o,
            "INSERT INTO kc (id, ref) VALUES (3, 2) USING OPERATION_ID 'c3'"
        ),
        "23503"
    );
    assert_eq!(
        code(
            &core,
            &o,
            "DELETE FROM kp WHERE id = 3 USING OPERATION_ID 'd3'"
        ),
        "23503"
    );
}

/// 参照元（子）だけ拡大: 親の値域外を参照する INSERT は `23503`、親行の削除も `23503`。
#[test]
fn widening_referencing_column_keeps_referential_integrity() {
    let (core, path) = setup_parent_child("akc-child", "INTEGER", "INTEGER", "");
    let _g = CleanupGuard(path);
    let o = ctx("owner");
    ok(&core, &o, "ALTER TABLE kc ALTER COLUMN ref TYPE BIGINT");
    ok(
        &core,
        &o,
        "INSERT INTO kc (id, ref) VALUES (2, 2) USING OPERATION_ID 'c2'",
    );
    assert_eq!(
        code(
            &core,
            &o,
            &format!("INSERT INTO kc (id, ref) VALUES (3, {WIDE}) USING OPERATION_ID 'c3'")
        ),
        "23503"
    );
    assert_eq!(
        code(
            &core,
            &o,
            "DELETE FROM kp WHERE id = 1 USING OPERATION_ID 'd1'"
        ),
        "23503"
    );
    ok(
        &core,
        &o,
        "DELETE FROM kp WHERE id = 3 USING OPERATION_ID 'd3'",
    );
}

/// 親→子の順に両方拡大して型が一致に戻っても検査結果は変わらない。
#[test]
fn widening_both_sides_converges_to_matching_types() {
    let (core, path) = setup_parent_child("akc-both", "INTEGER", "INTEGER", "");
    let _g = CleanupGuard(path);
    let o = ctx("owner");
    ok(&core, &o, "ALTER TABLE kp ALTER COLUMN k TYPE BIGINT");
    ok(&core, &o, "ALTER TABLE kc ALTER COLUMN ref TYPE BIGINT");
    assert_eq!(
        code(
            &core,
            &o,
            "DELETE FROM kp WHERE id = 1 USING OPERATION_ID 'd1'"
        ),
        "23503"
    );
    ok(
        &core,
        &o,
        &format!("INSERT INTO kp (id, k, u) VALUES (4, {WIDE}, 40) USING OPERATION_ID 'p4'"),
    );
    ok(
        &core,
        &o,
        &format!("INSERT INTO kc (id, ref) VALUES (2, {WIDE}) USING OPERATION_ID 'c2'"),
    );
}

/// 自己参照テーブルの PK 拡大は受理され、その後の参照整合性も保たれる。
#[test]
fn widening_self_referencing_primary_key_is_accepted() {
    let (core, path) = new_core("akc-self");
    let _g = CleanupGuard(path);
    let o = ctx("owner");
    ok(
        &core,
        &o,
        "CREATE TABLE nodes (k INTEGER PRIMARY KEY, up INTEGER, FOREIGN KEY (up) REFERENCES nodes (k))",
    );
    ok(
        &core,
        &o,
        "INSERT INTO nodes (id, k) VALUES (1, 1) USING OPERATION_ID 'n1'",
    );
    ok(
        &core,
        &o,
        "INSERT INTO nodes (id, k, up) VALUES (2, 2, 1) USING OPERATION_ID 'n2'",
    );
    ok(&core, &o, "ALTER TABLE nodes ALTER COLUMN k TYPE BIGINT");
    assert_eq!(
        code(
            &core,
            &o,
            "DELETE FROM nodes WHERE id = 1 USING OPERATION_ID 'd1'"
        ),
        "23503"
    );
    assert_eq!(
        code(
            &core,
            &o,
            "INSERT INTO nodes (id, k, up) VALUES (3, 3, 99) USING OPERATION_ID 'n3'"
        ),
        "23503"
    );
    ok(&core, &o, "ALTER TABLE nodes ALTER COLUMN up TYPE BIGINT");
    ok(
        &core,
        &o,
        "INSERT INTO nodes (id, k, up) VALUES (3, 3, 2) USING OPERATION_ID 'n3'",
    );
}

/// 型が混在した状態の参照アクション（ON DELETE CASCADE／SET NULL）。
#[test]
fn referential_actions_work_with_mixed_types() {
    for action in ["ON DELETE CASCADE", "ON DELETE SET NULL"] {
        let (core, path) =
            setup_parent_child("akc-actions", "INTEGER", "INTEGER", &format!(" {action}"));
        let _g = CleanupGuard(path);
        let o = ctx("owner");
        ok(&core, &o, "ALTER TABLE kp ALTER COLUMN k TYPE BIGINT");
        ok(
            &core,
            &o,
            "DELETE FROM kp WHERE id = 1 USING OPERATION_ID 'd1'",
        );
        if action.ends_with("CASCADE") {
            assert!(ints(&core, &o, "SELECT ref FROM kc LIMIT 10").is_empty());
        } else {
            let n = core
                .execute_sql(&o, "SELECT ref FROM kc LIMIT 10")
                .expect("select");
            assert!(matches!(n.rows[0].cells[0], Cell::Null));
        }
    }
}

/// ON UPDATE CASCADE: 親 `BIGINT`・子 `INTEGER` で子に収まる値は伝播し、収まらない値は
/// 副作用なしで拒否する。
#[test]
fn on_update_cascade_with_mixed_types() {
    let (core, path) = setup_parent_child("akc-upd", "INTEGER", "INTEGER", " ON UPDATE CASCADE");
    let _g = CleanupGuard(path);
    let o = ctx("owner");
    ok(&core, &o, "ALTER TABLE kp ALTER COLUMN k TYPE BIGINT");
    ok(
        &core,
        &o,
        "UPDATE kp SET k = 100 WHERE id = 1 USING OPERATION_ID 'u1'",
    );
    assert_eq!(ints(&core, &o, "SELECT ref FROM kc LIMIT 10"), vec![100]);
    let c = code(
        &core,
        &o,
        &format!("UPDATE kp SET k = {WIDE} WHERE id = 1 USING OPERATION_ID 'u2'"),
    );
    assert_eq!(c, "23503");
    assert_eq!(
        ints(&core, &o, "SELECT k FROM kp LIMIT 10"),
        vec![2, 3, 100]
    );
    assert_eq!(ints(&core, &o, "SELECT ref FROM kc LIMIT 10"), vec![100]);
}

/// ON UPDATE CASCADE: 参照する子行が無い親キーは、子の列に収まらない値へも更新できる
/// （値域検査は子行へ伝播する場合にだけ行う。codex-review 指摘・PR #1414）。
#[test]
fn on_update_cascade_allows_out_of_child_range_when_no_child_row() {
    let (core, path) = setup_parent_child("akc-upd2", "INTEGER", "INTEGER", " ON UPDATE CASCADE");
    let _g = CleanupGuard(path);
    let o = ctx("owner");
    ok(&core, &o, "ALTER TABLE kp ALTER COLUMN k TYPE BIGINT");
    // 親 id=2（k=2）を参照する子行は無い。
    ok(
        &core,
        &o,
        &format!("UPDATE kp SET k = {WIDE} WHERE id = 2 USING OPERATION_ID 'u3'"),
    );
    assert_eq!(
        ints(&core, &o, "SELECT k FROM kp LIMIT 10"),
        vec![1, 3, WIDE]
    );
    assert_eq!(ints(&core, &o, "SELECT ref FROM kc LIMIT 10"), vec![1]);
}

/// UNIQUE・PK 列の拡大後も、複数テナントで重複判定が正しい（索引の再構築）。
#[test]
fn unique_and_primary_key_checks_hold_across_tenants_after_widening() {
    let (core, path) = new_core("akc-uniq");
    let _g = CleanupGuard(path);
    let a = ctx("tenant-a");
    let b = ctx("tenant-b");
    ok(
        &core,
        &a,
        "CREATE TABLE ku (k INTEGER PRIMARY KEY, u INTEGER, UNIQUE (u))",
    );
    for (c, id) in [(&a, 1), (&b, 1)] {
        ok(
            &core,
            c,
            &format!("INSERT INTO ku (id, k, u) VALUES ({id}, 5, 7) USING OPERATION_ID 'i{id}'"),
        );
    }
    ok(&core, &a, "ALTER TABLE ku ALTER COLUMN k TYPE BIGINT");
    ok(&core, &a, "ALTER TABLE ku ALTER COLUMN u TYPE BIGINT");
    // ALTER 後に書き込んだテナントも、書き込んでいないテナントも既存値の重複を検出する。
    for c in [&a, &b] {
        assert_eq!(
            code(
                &core,
                c,
                "INSERT INTO ku (id, k, u) VALUES (2, 5, 8) USING OPERATION_ID 'x1'"
            ),
            "23505"
        );
        assert_eq!(
            code(
                &core,
                c,
                "INSERT INTO ku (id, k, u) VALUES (2, 6, 7) USING OPERATION_ID 'x2'"
            ),
            "23505"
        );
    }
    // 別テナントが同じ値を持つのは許される（拡大後の値でも）。
    ok(
        &core,
        &a,
        &format!("INSERT INTO ku (id, k, u) VALUES (3, {WIDE}, {WIDE}) USING OPERATION_ID 'w1'"),
    );
    ok(
        &core,
        &b,
        &format!("INSERT INTO ku (id, k, u) VALUES (3, {WIDE}, {WIDE}) USING OPERATION_ID 'w2'"),
    );
}

/// 縮小・異種の ALTER は `42804` のまま。INTEGER／BIGINT が混在する宣言は受理され、
/// 参照整合性が拡大変換後と同じ規則で働く。整数以外の混在宣言は `42830` のまま
/// （Issue #1435・TABLE-17・TABLE-22）。
#[test]
fn non_widening_stays_42804_and_mixed_integer_declarations_are_enforced() {
    let (core, path) = setup_parent_child("akc-strict", "BIGINT", "BIGINT", "");
    let _g = CleanupGuard(path);
    let o = ctx("owner");
    for sql in [
        "ALTER TABLE kp ALTER COLUMN k TYPE INTEGER",
        "ALTER TABLE kc ALTER COLUMN ref TYPE DOUBLE PRECISION",
    ] {
        assert_eq!(code(&core, &o, sql), "42804", "{sql}");
    }
    assert_eq!(ints(&core, &o, "SELECT k FROM kp LIMIT 10"), vec![1, 2, 3]);
    // 親 BIGINT・子 INTEGER の混在宣言（表制約・ADD FOREIGN KEY）は受理する。
    ok(
        &core,
        &o,
        "CREATE TABLE kx (ref INTEGER, FOREIGN KEY (ref) REFERENCES kp (k))",
    );
    ok(&core, &o, "CREATE TABLE ky (ref INTEGER)");
    ok(
        &core,
        &o,
        "ALTER TABLE ky ADD FOREIGN KEY (ref) REFERENCES kp (k)",
    );
    for t in ["kx", "ky"] {
        ok(
            &core,
            &o,
            &format!("INSERT INTO {t} (id, ref) VALUES (1, 2) USING OPERATION_ID '{t}ok'"),
        );
        assert_eq!(
            code(
                &core,
                &o,
                &format!("INSERT INTO {t} (id, ref) VALUES (2, 99) USING OPERATION_ID '{t}ng'")
            ),
            "23503",
            "{t}"
        );
    }
    assert_eq!(
        code(
            &core,
            &o,
            "DELETE FROM kp WHERE id = 2 USING OPERATION_ID 'dkp2'"
        ),
        "23503"
    );
    // 整数以外の混在は拒否のまま（副作用なし）。
    for sql in [
        "CREATE TABLE kz (ref TEXT, FOREIGN KEY (ref) REFERENCES kp (k))",
        "CREATE TABLE kz (ref REAL, FOREIGN KEY (ref) REFERENCES kp (k))",
        "CREATE TABLE kz (ref NUMERIC(10,2), FOREIGN KEY (ref) REFERENCES kp (k))",
    ] {
        assert_eq!(code(&core, &o, sql), "42830", "{sql}");
    }
    ok(&core, &o, "CREATE TABLE kz (ref INTEGER)");
    ok(&core, &o, "CREATE TABLE kw (ref TEXT)");
    assert_eq!(
        code(
            &core,
            &o,
            "ALTER TABLE kw ADD FOREIGN KEY (ref) REFERENCES kp (k)"
        ),
        "42830"
    );
}

/// 宣言による混在 FK（子 BIGINT・親 INTEGER）: 親の値域外は `23503`、ADD FOREIGN KEY の
/// 既存行検証も同じ規則で `23503`（FK は付かない）。自己参照・複合キー・列制約形も受理する。
#[test]
fn declared_mixed_foreign_keys_enforce_integrity() {
    let (core, path) = new_core("akc-declared");
    let _g = CleanupGuard(path);
    let o = ctx("owner");
    ok(&core, &o, "CREATE TABLE p (k INTEGER PRIMARY KEY)");
    ok(&core, &o, "CREATE TABLE c (ref BIGINT REFERENCES p (k))");
    ok(
        &core,
        &o,
        "INSERT INTO p (id, k) VALUES (1, 1) USING OPERATION_ID 'p1'",
    );
    ok(
        &core,
        &o,
        "INSERT INTO c (id, ref) VALUES (1, 1) USING OPERATION_ID 'c1'",
    );
    assert_eq!(
        code(
            &core,
            &o,
            &format!("INSERT INTO c (id, ref) VALUES (2, {WIDE}) USING OPERATION_ID 'c2'")
        ),
        "23503"
    );
    // ADD FOREIGN KEY の既存行検証: 値域外の既存行があれば 23503 で FK は付かない。
    ok(&core, &o, "CREATE TABLE d (ref BIGINT)");
    ok(
        &core,
        &o,
        &format!("INSERT INTO d (id, ref) VALUES (1, {WIDE}) USING OPERATION_ID 'd1'"),
    );
    assert_eq!(
        code(
            &core,
            &o,
            "ALTER TABLE d ADD FOREIGN KEY (ref) REFERENCES p (k)"
        ),
        "23503"
    );
    // 自己参照（親 INTEGER・子 BIGINT）。
    ok(
        &core,
        &o,
        "CREATE TABLE nodes (k INTEGER PRIMARY KEY, up BIGINT, FOREIGN KEY (up) REFERENCES nodes (k))",
    );
    ok(
        &core,
        &o,
        "INSERT INTO nodes (id, k) VALUES (1, 1) USING OPERATION_ID 'n1'",
    );
    ok(
        &core,
        &o,
        "INSERT INTO nodes (id, k, up) VALUES (2, 2, 1) USING OPERATION_ID 'n2'",
    );
    assert_eq!(
        code(
            &core,
            &o,
            "INSERT INTO nodes (id, k, up) VALUES (3, 3, 9) USING OPERATION_ID 'n3'"
        ),
        "23503"
    );
    assert_eq!(
        code(
            &core,
            &o,
            "DELETE FROM nodes WHERE id = 1 USING OPERATION_ID 'nd1'"
        ),
        "23503"
    );
    // 複合キー（一方の位置だけ混在）。
    ok(
        &core,
        &o,
        "CREATE TABLE cp (a BIGINT, b TEXT, PRIMARY KEY (a, b))",
    );
    ok(
        &core,
        &o,
        "CREATE TABLE cc (a INTEGER, b TEXT, FOREIGN KEY (a, b) REFERENCES cp (a, b))",
    );
    ok(
        &core,
        &o,
        "INSERT INTO cp (id, a, b) VALUES (1, 1, 'x') USING OPERATION_ID 'cp1'",
    );
    ok(
        &core,
        &o,
        "INSERT INTO cc (id, a, b) VALUES (1, 1, 'x') USING OPERATION_ID 'cc1'",
    );
    assert_eq!(
        code(
            &core,
            &o,
            "INSERT INTO cc (id, a, b) VALUES (2, 1, 'y') USING OPERATION_ID 'cc2'"
        ),
        "23503"
    );
}

fn floats(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Vec<f64> {
    let mut v: Vec<f64> = core
        .execute_sql(ctx, sql)
        .expect("select")
        .rows
        .into_iter()
        .map(|r| match r.cells.into_iter().next().expect("cell") {
            Cell::Float(v) => v,
            other => panic!("unexpected cell {other:?}"),
        })
        .collect();
    v.sort_by(f64::total_cmp);
    v
}

/// REAL／DOUBLE PRECISION は FK 列になれず（D3）、どの宣言面でも `42830` で拒否され副作用が無い。
/// 混在 FK（片側だけ REAL→DOUBLE PRECISION）の状態は構造的に到達できないことの固定（Issue #1434）。
#[test]
fn real_and_double_columns_cannot_form_foreign_keys() {
    let (core, path) = new_core("akc-fp-decl");
    let _g = CleanupGuard(path);
    let o = ctx("owner");
    ok(
        &core,
        &o,
        "CREATE TABLE fp (k INTEGER PRIMARY KEY, r REAL, d DOUBLE PRECISION, UNIQUE (r), UNIQUE (d))",
    );
    for sql in [
        "CREATE TABLE fc (x REAL, FOREIGN KEY (x) REFERENCES fp (r))",
        "CREATE TABLE fc (x DOUBLE PRECISION, FOREIGN KEY (x) REFERENCES fp (d))",
        "CREATE TABLE fc (x REAL, FOREIGN KEY (x) REFERENCES fp (d))",
        "CREATE TABLE fc (x DOUBLE PRECISION, FOREIGN KEY (x) REFERENCES fp (r))",
        "CREATE TABLE fc (x REAL REFERENCES fp (r))",
        "CREATE TABLE fc (x DOUBLE PRECISION REFERENCES fp (d))",
        "CREATE TABLE fc (x REAL, CONSTRAINT n FOREIGN KEY (x) REFERENCES fp (r))",
        "CREATE TABLE fc (r REAL, UNIQUE (r), up REAL, FOREIGN KEY (up) REFERENCES fc (r))",
    ] {
        assert_eq!(code(&core, &o, sql), "42830", "{sql}");
        // 拒否された CREATE TABLE はテーブルを残さない（同名の再作成が成功する）。
        ok(&core, &o, "CREATE TABLE fc (x REAL)");
        ok(&core, &o, "DROP TABLE fc");
    }
    for ty in ["REAL", "DOUBLE PRECISION"] {
        ok(&core, &o, &format!("CREATE TABLE fz (x {ty})"));
        for target in ["r", "d"] {
            assert_eq!(
                code(
                    &core,
                    &o,
                    &format!("ALTER TABLE fz ADD FOREIGN KEY (x) REFERENCES fp ({target})")
                ),
                "42830",
                "{ty} -> {target}"
            );
        }
        // FK が付いていない（親に無い値を挿入できる）。
        ok(
            &core,
            &o,
            "INSERT INTO fz (id, x) VALUES (1, 9.5) USING OPERATION_ID 'z1'",
        );
        ok(&core, &o, "DROP TABLE fz");
    }
}

/// REAL の UNIQUE 列（FK の参照先になり得る唯一の REAL 一意キー）を DOUBLE PRECISION へ広げた後も、
/// 値を保って一意性が働き、判定はテナント内に閉じる（Issue #1434）。
#[test]
fn widening_real_unique_column_preserves_values_and_uniqueness_per_tenant() {
    let (core, path) = new_core("akc-fp-uniq");
    let _g = CleanupGuard(path);
    let o = ctx("owner");
    ok(&core, &o, "CREATE TABLE fu (r REAL, UNIQUE (r))");
    let tenants = [ctx("tenant-a"), ctx("tenant-b")];
    for (t, c) in tenants.iter().enumerate() {
        for (i, v) in ["1.5", "0.1", "0.0"].iter().enumerate() {
            ok(
                &core,
                c,
                &format!("INSERT INTO fu (id, r) VALUES ({i}, {v}) USING OPERATION_ID 'u{t}{i}'"),
            );
        }
    }
    ok(
        &core,
        &o,
        "ALTER TABLE fu ALTER COLUMN r TYPE DOUBLE PRECISION",
    );
    for (t, c) in tenants.iter().enumerate() {
        assert_eq!(
            floats(&core, c, "SELECT r FROM fu LIMIT 10"),
            vec![0.0, f64::from(0.1f32), 1.5]
        );
        for (i, v) in ["1.5", "-0.0"].iter().enumerate() {
            let sql = format!(
                "INSERT INTO fu (id, r) VALUES ({}, {v}) USING OPERATION_ID 'd{t}{i}'",
                10 + i
            );
            assert_eq!(code(&core, c, &sql), "23505", "{sql}");
        }
        // 旧 REAL 表現とは一致しない DOUBLE の 0.1 は別の値として受理される。
        ok(
            &core,
            c,
            "INSERT INTO fu (id, r) VALUES (20, 0.1) USING OPERATION_ID 'e'",
        );
    }
    // テナント境界: 片方の新しい値は他方の判定に影響しない。
    ok(
        &core,
        &tenants[0],
        "INSERT INTO fu (id, r) VALUES (30, 2.25) USING OPERATION_ID 'f'",
    );
    ok(
        &core,
        &tenants[1],
        "INSERT INTO fu (id, r) VALUES (30, 2.25) USING OPERATION_ID 'f'",
    );
}
